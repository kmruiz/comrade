//! Comrade cockpit: headless runner and ratatui TUI.

mod editor;
mod headless;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use comrade_core::{Config, DelegateLimits, DelegateTool, LlmClient, MemoryUndo};
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
    let mut reg = build_tools(&cfg)?;
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

fn build_tools(cfg: &Config) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.extend(comrade_tool_session::all());
    reg.extend(comrade_tool_project::all());
    reg.extend(comrade_tool_fs::all());
    reg.extend(comrade_tool_git::all());
    reg.extend(comrade_tool_syntax::all());
    reg.extend(comrade_tool_memory::all());
    reg.extend(comrade_tool_web::all());
    // Delegate models configured under [[delegates]] become the `delegate`
    // tool; absent delegates mean no tool is advertised. Delegates get a
    // second, restricted registry (everything except git_commit and the
    // session/UI tools) so they can do real work without ever committing.
    if let Some(delegate) = DelegateTool::new(
        &cfg.delegates,
        delegate_registry(),
        DelegateLimits {
            max_iterations: cfg.agent.max_iterations,
            budget_tokens: cfg.context.budget_tokens,
            max_tool_output_chars: cfg.context.max_tool_output_chars,
        },
    )? {
        reg.register(Box::new(delegate));
    }
    Ok(reg)
}

/// The tools a delegated sub-agent may call: every repository/memory/project
/// tool from the same crates as the main registry, minus the ones a delegate
/// must never see (git_commit, the session/UI tools, and `delegate` itself so
/// it cannot recurse). `deny` is shared with comrade-core's delegate module so
/// the tool description and this registry can never drift apart.
fn delegate_registry() -> ToolRegistry {
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
    reg
}

/// Session state + undo log wired to a fresh event channel. The caller chooses
/// how to drive `user` (TUI dialogs or headless stdin/policy).
struct SessionBundle {
    session: Arc<comrade_core::AgentSession>,
    ctx_base: ToolContext,
}

fn new_session(
    deps: &Deps,
    user: Arc<dyn comrade_tool::UserIo>,
) -> (
    SessionBundle,
    tokio::sync::mpsc::Sender<comrade_core::AgentEvent>,
    tokio::sync::mpsc::Receiver<comrade_core::AgentEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(512);
    let session = Arc::new(comrade_core::AgentSession::new(tx.clone()));
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
        stop: None,
    };
    (SessionBundle { session, ctx_base }, tx, rx)
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
