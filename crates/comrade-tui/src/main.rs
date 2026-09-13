//! Comrade cockpit: headless runner and ratatui TUI.

mod colors;
mod editor;
mod headless;
mod session_store;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use comrade_core::{AskAdviseTool, Config, DelegateLimits, DelegateTool, LlmClient, MemoryUndo};
use comrade_tool::{ToolContext, ToolRegistry};

#[derive(Parser, Debug)]
#[command(name = "comrade", version, about = "Local-first LLM development agent")]
struct Cli {
    /// One-shot task to run headless (omit to open the TUI with an empty prompt).
    #[arg(default_value = "")]
    prompt: String,

    /// Run headless (no TUI), streaming the run to stdout.
    #[arg(long)]
    headless: bool,

    /// Path to a TOML config file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Project root to operate in (defaults to the current directory).
    #[arg(long)]
    dir: Option<PathBuf>,

    /// Convenience: set autonomy = "auto" (apply changes without asking).
    #[arg(long)]
    auto: bool,
}

/// Spawn the task that relays agent events from the run-facing bounded
/// channel to the UI's unbounded queue. The run task streams into `tx` and
/// awaits each send, so `rx` must be drained on its own task: otherwise a UI
/// that is busy repainting (or wedged) fills the bounded channel and parks
/// the run mid-turn (freeze notes #25/#29). The relay never blocks — the
/// unbounded `ui_tx` side cannot exert back-pressure — and it is the only
/// place a receive on the run's event channel waits.
fn spawn_event_relay(
    mut rx: tokio::sync::mpsc::Receiver<comrade_core::AgentEvent>,
    ui_tx: tokio::sync::mpsc::UnboundedSender<comrade_core::AgentEvent>,
) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if ui_tx.send(ev).is_err() {
                break; // UI is gone; drop the rest.
            }
        }
    });
}

/// Shared runtime dependencies for a session.
struct Deps {
    cfg: Arc<Config>,
    client: Arc<LlmClient>,
    tools: Arc<ToolRegistry>,
    root: PathBuf,
    /// DeepSeek account balance (total), when available.
    balance: Option<String>,
    /// Path the live config was read from (None when only defaults apply), so
    /// the TUI can re-read it on a config-reload command.
    config_source: Option<PathBuf>,
    /// True when the CLI forced autonomy=auto; re-applied on config reloads.
    auto_forced: bool,
}

async fn build_deps(cli: &Cli) -> Result<Deps> {
    let loaded = Config::load(cli.config.as_deref()).context("failed to load config")?;
    let config_source = loaded.source;
    let mut cfg = loaded.config;
    if cli.auto {
        cfg.security.autonomy = comrade_core::Autonomy::Auto;
    }

    let root = match &cli.dir {
        Some(d) => d.canonicalize().context("bad --dir")?,
        None => std::env::current_dir().context("no current dir")?,
    };

    let client = Arc::new(LlmClient::new(&cfg.llm)?);

    // Detect the model's real context window and display identity (unless
    // configured explicitly) so the model gauge is accurate.
    if cfg.llm.context_window.is_none() {
        if let Some(window) = client.fetch_context_window().await {
            cfg.llm.context_window = Some(window);
            eprintln!(
                "[comrade] model {} context window: {window} tokens",
                cfg.llm.model
            );
        }
    }
    if cfg.llm.model_version.is_none() {
        cfg.llm.model_version = client.fetch_model_version().await;
    }
    let balance = client.fetch_account_balance().await;
    if let Some(b) = &balance {
        eprintln!("[comrade] account balance: {b}");
    }
    let cfg = Arc::new(cfg);

    // Connect configured MCP servers and register their tools alongside the
    // built-ins. A dead/unreachable server is skipped with a warning instead
    // of aborting startup (see connect_all).
    let mut reg = build_tools(&cfg, &root)?;
    reg.extend(comrade_tool_mcp::connect_all(&cfg.mcp.servers).await);
    let tools = Arc::new(reg);
    Ok(Deps {
        cfg,
        client,
        tools,
        root,
        balance,
        config_source,
        auto_forced: cli.auto,
    })
}

fn build_tools(cfg: &Config, root: &std::path::Path) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.extend(comrade_tool_session::all());
    reg.extend(comrade_tool_project::all());
    reg.extend(comrade_tool_fs::all());
    reg.extend(comrade_tool_git::all());
    reg.extend(comrade_tool_syntax::all());
    reg.extend(comrade_tool_memory::all());
    reg.extend(comrade_tool_web::all());
    // Claude-format skills discovered under `.comrade/skills` (and the common
    // Claude places) become one `skill_<name>` tool each.
    reg.extend(comrade_tool_skill::all(root));
    // Delegate models configured under [[delegates]] become the `delegate`
    // tool; absent delegates mean no tool is advertised. Delegates get a
    // second, restricted registry (everything except git_commit and the
    // session/UI tools) so they can do real work without ever committing.
    if let Some(delegate) = DelegateTool::new(
        &cfg.delegates,
        delegate_registry(root),
        DelegateLimits {
            max_iterations: cfg.agent.max_iterations,
            budget_tokens: cfg.context.budget_tokens,
            max_tool_output_chars: cfg.context.max_tool_output_chars,
        },
    )? {
        reg.register(Box::new(delegate));
    }
    // The same [[delegates]] back the `ask_advise` tool: consulting one of them
    // for a second opinion. Advisors only get the read-only registry, so they
    // can ground advice in the code but never change anything.
    if let Some(advise) = AskAdviseTool::new(
        &cfg.delegates,
        advise_registry(root),
        DelegateLimits {
            max_iterations: cfg.agent.max_iterations,
            budget_tokens: cfg.context.budget_tokens,
            max_tool_output_chars: cfg.context.max_tool_output_chars,
        },
    )? {
        reg.register(Box::new(advise));
    }
    Ok(reg)
}

/// The tools a delegated sub-agent may call: every repository/memory/project
/// tool from the same crates as the main registry, minus the ones a delegate
/// must never see (git_commit, the session/UI tools, and `delegate` itself so
/// it cannot recurse). `deny` is shared with comrade-core's delegate module so
/// the tool description and this registry can never drift apart.
fn delegate_registry(root: &std::path::Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for tool in comrade_tool_fs::all()
        .into_iter()
        .chain(comrade_tool_git::all())
        .chain(comrade_tool_syntax::all())
        .chain(comrade_tool_memory::all())
        .chain(comrade_tool_project::all())
        .chain(comrade_tool_web::all())
    {
        let name = tool.spec().name.clone();
        if !DelegateTool::denied_for_delegates(&name) {
            reg.register(tool);
        }
    }
    // Skills are read-only text: delegates may load them too.
    reg.extend(comrade_tool_skill::all(root));
    reg
}

/// The tools an advisor consulted via `ask_advise` may browse: only the
/// read-only repository tools from the same crates as the main registry (no
/// write/edit/shell/run/commit/plan tools), so an advisor can ground its
/// advice in the code but can never mutate the workspace or the session.
/// `AskAdviseTool::read_only_for_advice` shares the main loop's read-only
/// classification (agent.rs) so the two can never drift apart.
fn advise_registry(root: &std::path::Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for tool in comrade_tool_fs::all()
        .into_iter()
        .chain(comrade_tool_git::all())
        .chain(comrade_tool_syntax::all())
        .chain(comrade_tool_memory::all())
        .chain(comrade_tool_project::all())
        .chain(comrade_tool_web::all())
    {
        let name = tool.spec().name.clone();
        if AskAdviseTool::read_only_for_advice(&name) {
            reg.register(tool);
        }
    }
    // Skills are read-only text: advisors may load them too.
    reg.extend(comrade_tool_skill::all(root));
    reg
}

/// An agent event tagged with the id of the session that produced it. The TUI
/// keeps several sessions open at once (each with its own run-facing channel)
/// and routes every event to the session that owns it.
pub(crate) type TaggedEvent = (u64, comrade_core::AgentEvent);

/// Spawn the relay task for one session: a bounded run-facing channel whose
/// events are forwarded, tagged with `id`, into the UI's central unbounded
/// queue. The run task streams into the returned bounded sender and awaits each
/// send, so the bounded side must always be drained on its own task (freeze
/// notes #25/#29: a wedged repaint filling the channel parked the run mid-turn).
/// The central unbounded queue cannot exert back-pressure, and it is the single
/// queue the UI drains at its own pace.
pub(crate) fn spawn_tagged_relay(
    id: u64,
    central: tokio::sync::mpsc::UnboundedSender<TaggedEvent>,
) -> tokio::sync::mpsc::Sender<comrade_core::AgentEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel::<comrade_core::AgentEvent>(512);
    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(ev) = rx.recv().await {
            if central.send((id, ev)).is_err() {
                break; // UI is gone; drop the rest.
            }
        }
    });
    tx
}

/// Session state + undo log wired to a fresh event channel. The caller chooses
/// how to drive `user` (TUI dialogs or headless stdin/policy).
struct SessionBundle {
    session: Arc<comrade_core::AgentSession>,
    ctx_base: ToolContext,
}

/// Wire a fresh [`comrade_core::AgentSession`] + [`ToolContext`] to `tx`, the
/// session's own run-facing event sender (see [`spawn_tagged_relay`]).
pub(crate) fn session_bundle(
    deps: &Deps,
    user: Arc<dyn comrade_tool::UserIo>,
    tx: tokio::sync::mpsc::Sender<comrade_core::AgentEvent>,
) -> SessionBundle {
    let session = Arc::new(comrade_core::AgentSession::new(tx));
    let undo = Arc::new(MemoryUndo::new(deps.root.clone()));
    let ctx_base = ToolContext {
        project_root: deps.root.clone(),
        cwd: deps.root.clone(),
        session: session.clone().as_control(),
        user,
        undo: undo.clone(),
        auto_approve: deps.cfg.security.autonomy == comrade_core::Autonomy::Auto,
        approval: Default::default(),
        events: Arc::new(comrade_tool::NoopEvents),
        steer: None,
        stop: None,
    };
    SessionBundle { session, ctx_base }
}

fn new_session(
    deps: &Deps,
    user: Arc<dyn comrade_tool::UserIo>,
) -> (
    SessionBundle,
    tokio::sync::mpsc::Sender<comrade_core::AgentEvent>,
    tokio::sync::mpsc::UnboundedReceiver<comrade_core::AgentEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(512);
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_event_relay(rx, ui_tx);
    (session_bundle(deps, user, tx.clone()), tx, ui_rx)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let deps = build_deps(&cli).await?;

    let interactive = !cli.headless && cli.prompt.is_empty();
    if interactive && !std::io::stdout().is_terminal() {
        anyhow::bail!("the TUI needs a terminal; pass a prompt or --headless for one-shot runs");
    }

    if interactive {
        tui::run(&deps).await
    } else {
        headless::run(&deps, &cli.prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run task streams AgentEvents into the bounded channel and awaits
    /// each send (agent.rs), so the relay must keep the bounded side drained
    /// even when nothing is consuming the UI queue yet. This asserts the
    /// freeze-#29 property: an awaited sender is never parked behind a full
    /// channel while the relay runs.
    #[tokio::test]
    async fn event_relay_keeps_awaited_senders_unblocked() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_event_relay(rx, ui_tx);

        // Far more events than the bounded capacity, each awaited as the real
        // run task would send them.
        let total = 2000usize;
        for i in 0..total {
            let _ = tx
                .send(comrade_core::AgentEvent::Delta(format!("{i}")))
                .await;
        }
        drop(tx); // Close the run-facing side so the relay exits when drained.

        let mut received = 0usize;
        while ui_rx.recv().await.is_some() {
            received += 1;
        }
        assert_eq!(received, total, "every sent event must reach the UI queue");
    }

    /// Each session's relay tags its events with that session's id, so the TUI
    /// can route an event to the session that produced it (even a background
    /// one running while another session is on screen).
    #[tokio::test]
    async fn tagged_relay_stamps_events_with_the_session_id() {
        let (central_tx, mut central_rx) = tokio::sync::mpsc::unbounded_channel::<TaggedEvent>();
        let tx = spawn_tagged_relay(7, central_tx);
        tx.send(comrade_core::AgentEvent::RunStart).await.unwrap();
        let (id, ev) = central_rx.recv().await.unwrap();
        assert_eq!(id, 7);
        assert!(matches!(ev, comrade_core::AgentEvent::RunStart));
        // Order is preserved across the relay.
        tx.send(comrade_core::AgentEvent::RunEnd).await.unwrap();
        let (id, ev) = central_rx.recv().await.unwrap();
        assert_eq!(id, 7);
        assert!(matches!(ev, comrade_core::AgentEvent::RunEnd));
    }
}
