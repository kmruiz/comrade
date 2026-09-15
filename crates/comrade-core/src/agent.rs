use anyhow::{Context as _, Result, bail};
use comrade_tool::{Steer, ToolContext, ToolRegistry};
use futures_util::future::join_all;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::context::ContextManager;
use crate::hooks::Hooks;
use crate::llm::{ChatMessage, LlmClient, Role, Usage};
use crate::react::{build_system_prompt, parse_turn, render_observation};
use crate::redact::Redactor;
use crate::session::AgentEvent;

/// Total real tokens a request cost, when the endpoint reported any usage.
fn usage_total(u: &Usage) -> Option<usize> {
    if u.total_tokens > 0 {
        Some(u.total_tokens)
    } else if u.prompt_tokens + u.completion_tokens > 0 {
        Some(u.prompt_tokens + u.completion_tokens)
    } else {
        None
    }
}

/// Tools that mutate the workspace (used by the loop tracker to tell "repeat
/// but state changed" from "repeat doing nothing"). Keep in sync with the tool
/// crates.
const MUTATING_TOOLS: &[&str] = &[
    "fs_edit",
    "fs_write_file",
    "ts_rename",
    "git_commit",
    "git_stash",
    "git_branch",
    "git_checkout",
    "delegate",
    "delegate_parallel",
    "pom_run_task",
    "record_adr",
    "amend_adr",
    "merge_adr",
    "record_glossary",
    "rename_glossary",
    "delete_glossary",
    "pom_format_code",
    "pom_run_tests",
    "pom_check",
    "run_bg",
    "bg_kill",
    "shell",
];

/// Whether a tool call mutates the workspace (used to tell "repeat but state
/// changed" apart from "repeat doing nothing").
pub(crate) fn is_mutating(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// Whether a tool call only gathers information (never changes state). Shared
/// with the advisor registry for `ask_advise`: advisors may browse every
/// read-only tool but nothing that writes, runs or commits.
pub fn is_read_only(name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&name)
}

/// Tools that only gather information (never change state).
const READ_ONLY_TOOLS: &[&str] = &[
    "fs_list_dir",
    "fs_list_files",
    "fs_rgrep",
    "fs_read_file",
    "fs_read_ranges",
    "ts_list_symbols",
    "ts_structural_map",
    "ts_find_symbol",
    "ts_read_symbol",
    "ts_find_references",
    "ts_test_impact",
    "git_status",
    "git_diff",
    "git_show",
    "git_log",
    "git_blame",
    "pom_model",
    "find_adr",
    "list_adr",
    "read_adr",
    "find_glossary",
    "read_glossary",
    "stale_memory",
    "semantic_search",
    "web_search",
    "web_fetch",
];

/// After this many consecutive reads with no state change, we refuse another.
const READ_GUARD_THRESHOLD: usize = 20;

/// After ≥ this many consecutive non-progress calls following the first
/// workspace change, nudge the model once to finish (small models otherwise
/// keep re-verifying until `max_iterations`).
const STALL_NUDGE_AT: usize = 8;

/// At ≥ this many, end the run gracefully instead of burning the budget.
const STALL_END_AT: usize = 18;

/// Tools that push a task forward: they change the repo or the plan. Anything
/// else (reads, tests, checks, shell, jobs) only inspects state, so a long run
/// of them after the first change means the model is spinning.
const PROGRESS_TOOLS: &[&str] = &[
    "fs_edit",
    "fs_write_file",
    "ts_rename",
    "self_set_plan",
    "self_update_plan",
    "self_set_step_model",
    "self_set_step_context",
    "self_finish_plan",
    "self_rename_session",
    "ask_form",
    "delegate",
    "delegate_parallel",
    "record_adr",
    "amend_adr",
    "merge_adr",
    "record_glossary",
    "rename_glossary",
    "delete_glossary",
    "git_commit",
    "git_stash",
    "git_branch",
    "git_checkout",
    // A stuck sub-agent escalating to its parent is a recovery move: it counts
    // as progress, so the stall guard gives it a fresh window.
    "ask_upwards",
];

/// Whether a tool call moves the task forward (see [`PROGRESS_TOOLS`]).
pub(crate) fn is_progress(name: &str) -> bool {
    PROGRESS_TOOLS.contains(&name)
}

/// Injected (once) when the model keeps verifying after it has already changed
/// the repo — the classic small-model failure to recognise completion.
pub(crate) const STALL_NUDGE: &str = "You have already made your change and are now only re-checking. \
     STOP investigating. If the requested change is implemented and your last verification passed, \
     finish NOW: call `self_finish_plan` (if you made a plan), then reply with your final summary and \
     NO tool call. Only continue if the last verification actually FAILED - then fix the code with \
     `fs_edit`/`fs_write_file` first.";

/// Tools that verify the work (a test or type check run).
const VERIFY_TOOLS: &[&str] = &["pom_run_tests", "pom_check", "pom_run_task"];

/// Files that an edit tool rewrites.
const EDIT_TOOLS: &[&str] = &["fs_edit", "fs_write_file", "ts_rename"];

/// After this many edits with no verification in between, tell the model to run
/// the tests (a small model otherwise rewrites a broken file over and over).
const VERIFY_NUDGE_AFTER_EDITS: usize = 3;

/// Injected (once) when the model keeps editing without ever verifying.
pub(crate) const VERIFY_NUDGE: &str = "You have edited the code several times without running the \
     tests. STOP editing. Run `pom_run_tests` NOW, read its result, and only then edit again to fix \
     what it reports. If it passes, you are done.";


/// Process monitor: if the model keeps reading without doing anything, stop it.
/// Returns `true` when the tool may run (and updates the counter); `false` when
/// the read should be refused as "enough context".
/// Shared with the delegate sub-agent loop (delegate.rs).
pub(crate) fn allow_read_step(name: &str, consecutive_reads: &mut usize) -> bool {
    if is_read_only(name) {
        if *consecutive_reads >= READ_GUARD_THRESHOLD {
            return false;
        }
        *consecutive_reads += 1;
    } else {
        // any action (edit, plan change, question) resets the counter
        *consecutive_reads = 0;
    }
    true
}

fn read_guard_message(count: usize) -> String {
    format!(
        "You have performed {count} reads in a row with no changes. You have enough context - \
         implement now (write or edit a file), or call update_plan to revise your steps. Do not \
         keep reading."
    )
}

/// Classify a failed tool output and attach one short corrective hint.
fn failure_hint(name: &str, output: &str) -> String {
    let _ = name;
    let o = output.to_lowercase();
    if o.contains("denied") || o.contains("user denied") {
        return String::new(); // do not nag about human decisions
    }
    if o.contains("timed out") || o.contains("timeout") {
        return "the action timed out - split it into smaller steps or raise the timeout, then retry"
            .to_string();
    }
    if o.contains("connection refused")
        || o.contains("error sending request")
        || o.contains("request failed")
        || o.contains("network")
    {
        return "transient network/provider issue - check connectivity and retry once".to_string();
    }
    if o.contains("command not found") || o.contains("no such file") || o.contains("not found") {
        return "a command or path is missing - install it or point at the correct file"
            .to_string();
    }
    if o.contains("test result: failed") || (o.contains("failures:") && o.contains("panicked")) {
        return "the tests failed - fix the code (or the test) and rerun pom_run_tests".to_string();
    }
    if o.contains("error[")
        || o.contains("cannot find")
        || o.contains("mismatched types")
        || o.contains("expected ")
        || o.contains("--> ")
    {
        return "looks like a compile/type error in the code you changed - fix it before rerunning"
            .to_string();
    }
    if o.contains("exit code") || o.contains("exit ") {
        return "the command exited non-zero - read its output and fix the underlying issue"
            .to_string();
    }
    "read the error, form one hypothesis, change something or update the plan before retrying"
        .to_string()
}

/// Render a tool observation, appending a corrective hint on failures.
fn observation_with_failure_hint(tool_name: &str, ok: bool, output: &str) -> String {
    let mut text = output.to_string();
    if !ok {
        let hint = failure_hint(tool_name, output);
        if !hint.is_empty() {
            text.push_str("\n\nHINT: ");
            text.push_str(&hint);
        }
    }
    render_observation(tool_name, &text)
}

pub(crate) const LOOP_WINDOW: usize = 8;
pub(crate) const MAX_LOOP_REFUSALS: usize = 3;

/// Detects no-progress loops: the same exact tool call repeated while nothing
/// changed in between. Each detected repeat is refused; after several refusals
/// the run aborts instead of burning the whole budget.
#[derive(Default)]
pub(crate) struct LoopTracker {
    /// (canonical call signature, mutation counter at the time it ran).
    recent: std::collections::VecDeque<(String, u64)>,
    /// How many mutating calls have executed; identical calls on either side of
    /// a mutation are not considered a loop.
    mutation_seq: u64,
    /// Refusals per signature. Reset whenever the model makes any progress.
    refusals: std::collections::HashMap<String, usize>,
    /// When the model refuses to stop repeating, the run ends gracefully
    /// (instead of erroring out) with this message.
    stuck: Option<String>,
    /// Workspace/plan changes observed so far (see [`is_progress`]).
    progress: u64,
    /// Consecutive non-progress calls since the last progress call.
    idle: usize,
    /// Whether the one-shot stall nudge has already been emitted.
    stall_nudged: bool,
    /// Edits since the last verification (test/check) run.
    edits_since_verify: usize,
    /// Whether the one-shot "verify now" nudge has already been emitted.
    verify_nudged: bool,
}

impl LoopTracker {
    /// Returns the refusal count when this exact call was already made with no
    /// state change since (i.e. a no-progress repeat), else `None`.
    pub(crate) fn check(&mut self, sig: &str) -> Option<usize> {
        let repeats_without_change = self
            .recent
            .iter()
            .any(|(s, seq)| s == sig && *seq == self.mutation_seq);
        if !repeats_without_change {
            return None;
        }
        let count = self.refusals.entry(sig.to_string()).or_insert(0);
        *count += 1;
        Some(*count)
    }

    /// The model made progress (a real tool execution): clear the refusal
    /// counter so a fresh mistake does not accumulate onto an old one.
    pub(crate) fn record(&mut self, name: &str, sig: String) {
        if is_mutating(name) {
            self.mutation_seq += 1;
            // A state change means repeats were legitimate; start counting again.
            self.refusals.clear();
        } else if !self.refusals.is_empty() {
            // Running a different tool is an attempt at progress: give the model
            // the benefit of the doubt instead of carrying old refusals over.
            if self.refusals.contains_key(&sig) {
                self.refusals.remove(&sig);
            } else {
                self.refusals.clear();
            }
        }
        self.recent.push_back((sig, self.mutation_seq));
        if self.recent.len() > LOOP_WINDOW {
            let dropped = self.recent.pop_front().map(|(s, _)| s);
            if let Some(dropped) = dropped
                && !self.recent.iter().any(|(s, _)| *s == dropped)
            {
                self.refusals.remove(&dropped);
            }
        }
        // Stall tracking: a progress call resets the idle run; anything else
        // extends it, but only once the model has changed something at all (a
        // long read-only exploration before the first edit is legitimate).
        if is_progress(name) {
            self.progress += 1;
            self.idle = 0;
        } else if self.progress > 0 {
            self.idle += 1;
        }
        // Anti-thrash: a verification resets the edit run; an edit extends it.
        if VERIFY_TOOLS.contains(&name) {
            self.edits_since_verify = 0;
            self.verify_nudged = false;
        } else if EDIT_TOOLS.contains(&name) {
            self.edits_since_verify += 1;
        }
    }

    /// True exactly once, when the model has edited several times in a row
    /// without ever running the tests. A small model otherwise rewrites a
    /// broken file over and over instead of seeing the compiler error.
    pub(crate) fn needs_verify_nudge(&mut self) -> bool {
        if !self.verify_nudged && self.edits_since_verify >= VERIFY_NUDGE_AFTER_EDITS {
            self.verify_nudged = true;
            return true;
        }
        false
    }

    /// True exactly once, when the model has changed the repo and then spent
    /// [`STALL_NUDGE_AT`] calls in a row without making another change. The
    /// caller then injects [`STALL_NUDGE`] so the model learns to stop.
    pub(crate) fn needs_stall_nudge(&mut self) -> bool {
        if !self.stall_nudged && self.progress > 0 && self.idle >= STALL_NUDGE_AT {
            self.stall_nudged = true;
            return true;
        }
        false
    }

    /// Some(reason) once the model has kept spinning well past the nudge, so
    /// the loop can end gracefully with an explanation instead of hitting
    /// `max_iterations`.
    pub(crate) fn stall_reason(&self) -> Option<String> {
        (self.progress > 0 && self.idle >= STALL_END_AT).then(|| {
            format!(
                "Stopped: {} calls in a row without a further change after making progress - the \
                 work looks complete. (Last step: re-run your verification only if it truly failed.)",
                self.idle
            )
        })
    }

    /// Stop the run gracefully (not an error) because the model kept repeating.
    pub(crate) fn mark_stuck(&mut self, sig: &str) {
        if self.stuck.is_none() {
            self.stuck = Some(format!(
                "Stopped: repeated identical action `{sig}` without making progress"
            ));
        }
    }

    pub(crate) fn stuck_reason(&self) -> Option<String> {
        self.stuck.clone()
    }
}

/// Message fed back when a tool call is refused as a no-progress repeat.
pub(crate) fn loop_refusal(tool: &str) -> String {
    format!(
        "tool `{tool}` was already called with exactly these arguments and nothing changed since. \
         Repeating it will not make progress. Change something first (edit a file, run a different \
         tool, verify state) or give your final answer. Do NOT call `{tool}` again with identical arguments."
    )
}

/// Result of a finished agent run.
#[derive(Debug)]
pub struct AgentOutcome {
    pub final_answer: String,
    pub iterations: usize,
}

/// Inject any steering messages the human typed while a run was in flight into
/// the model history. Steers are only ever applied at a loop's rest point —
/// the top of an iteration, before the next model request and after the
/// previous turn's tool results were pushed — so message ordering with
/// assistant tool calls stays API-valid. The root agent loop and a nested
/// delegate sub-loop drain the same [`Steer`] bus (shared via
/// [`ToolContext::steer`]); whichever owns the loop at the moment receives the
/// message, and leftovers are seen by the root once a delegate hands back.
pub(crate) async fn drain_steer(steer: Option<&Steer>, history: &mut ContextManager) {
    let Some(steer) = steer else {
        return;
    };
    for text in steer.drain().await {
        // Merged so a steer cannot produce two user turns in a row (which some
        // strict providers reject).
        history.push_user_merged(&text);
    }
}

/// Build the rolling message history (system prompt + budget manager) for one
/// session. Sessions that run many tasks keep the returned manager and hand it
/// to [`run_agent_with_history`] each time, so context survives task
/// boundaries instead of being wiped on every final answer.
pub fn build_session_context(
    cfg: &Config,
    project_root: &str,
    tools: &ToolRegistry,
) -> ContextManager {
    let budget = cfg.effective_budget();
    ContextManager::with_system(
        build_system_prompt(project_root, tools, budget),
        budget,
        cfg.context.max_tool_output_chars,
    )
}

/// Drive one agent session to completion against the given model.
///
/// `ctx` carries the session state, human channel, and undo log; `tx` streams
/// [`AgentEvent`]s for the UI; `stop` allows interrupting between iterations.
pub async fn run_agent(
    cfg: &Config,
    client: &LlmClient,
    ctx: ToolContext,
    tools: &ToolRegistry,
    user_input: String,
    tx: mpsc::Sender<AgentEvent>,
    stop: CancellationToken,
) -> Result<AgentOutcome> {
    let root = ctx.project_root.to_string_lossy().to_string();
    let mut history = build_session_context(cfg, &root, tools);
    run_agent_with_history(cfg, client, ctx, tools, user_input, &mut history, tx, stop).await
}

/// Run an agent task against a caller-owned history (see
/// [`build_session_context`]). The history is kept across calls, so a session
/// that processes many tasks retains the earlier conversation; the manager
/// compacts it automatically as it approaches the budget. Exactly one task
/// runs at a time against the manager.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_with_history(
    cfg: &Config,
    client: &LlmClient,
    ctx: ToolContext,
    tools: &ToolRegistry,
    user_input: String,
    history: &mut ContextManager,
    tx: mpsc::Sender<AgentEvent>,
    stop: CancellationToken,
) -> Result<AgentOutcome> {
    let _ = tx.send(AgentEvent::RunStart).await;

    // Install the configured filesystem/shell guardrails for this run (they are
    // read back by the tool crates through `comrade_tool::policy`).
    comrade_tool::set_policy(cfg.security.to_policy(&ctx.project_root));

    if ctx.session.title().is_empty() || ctx.session.title() == "New session" {
        let mut t = user_input.trim().to_string();
        if t.chars().count() > 60 {
            t = t.chars().take(60).collect::<String>() + "…";
        }
        ctx.session.set_title(&t);
    }
    let _ = tx.send(AgentEvent::User(user_input.clone())).await;

    history.push(ChatMessage::new(Role::User, user_input));
    history.enforce_budget();

    let result = run_agent_loop(cfg, client, tools, ctx, history, tx.clone(), &stop).await;

    // Guarantee the UI always sees an error (if any) and a terminal event, on
    // every exit path.
    if let Err(e) = &result {
        let _ = tx.send(AgentEvent::Error(format!("{e:#}"))).await;
    }
    let _ = tx.send(AgentEvent::RunEnd).await;
    result
}

async fn run_agent_loop(
    cfg: &Config,
    client: &LlmClient,
    tools: &ToolRegistry,
    ctx: ToolContext,
    ctxm: &mut ContextManager,
    tx: mpsc::Sender<AgentEvent>,
    stop: &CancellationToken,
) -> Result<AgentOutcome> {
    // Attach this run's UI event channel to the context so tools that run a
    // sub-agent loop (the `delegate` tool) can stream what that sub-agent is
    // doing into the chat as it happens.
    let mut ctx = ctx;
    ctx.events = std::sync::Arc::new(crate::session::SessionEvents(tx.clone()));
    // Hand the run's cancel token to tools: the `delegate` sub-agent races its
    // model requests against it, so a user cancel aborts a stuck delegate
    // instead of leaving the run frozen at "working".
    ctx.stop = Some(stop.clone());

    let max_iterations = cfg.agent.max_iterations;
    let mut iterations = 0usize;
    // Pre/post hooks, a per-tool timeout and output redaction, applied to every
    // tool call this run makes (see `Dispatch`).
    let hooks = Hooks::from_cfg(&cfg.hooks);
    let redactor = if cfg.security.redact_secrets {
        Redactor::from_env()
    } else {
        Redactor::none()
    };
    let dispatch = Dispatch {
        cfg,
        hooks: &hooks,
        redactor: &redactor,
    };
    // Optional wall-clock budget for the whole run (checked at each rest point).
    let deadline = (cfg.agent.run_timeout_secs > 0).then(|| {
        std::time::Instant::now() + std::time::Duration::from_secs(cfg.agent.run_timeout_secs)
    });
    let mut tracker = LoopTracker::default();
    // We nudge the model once per run to open with a plan.
    let mut plan_nudged = false;
    // Consecutive read-only calls since the last state change (read guard).
    let mut consecutive_reads = 0usize;
    // Automatic compaction is armed once per over-budget episode: when the
    // history crosses into the budget headroom we summarise it once, then wait
    // until it has been trimmed back under the threshold before summarising
    // again. This stops a persistently over-budget history (e.g. a budget
    // smaller than the system prompt) from firing a summariser call on every
    // iteration.
    let mut auto_compact_armed = true;

    loop {
        if stop.is_cancelled() {
            bail!("agent interrupted by user");
        }
        if iterations >= max_iterations {
            bail!("reached max_iterations ({max_iterations}) without a final answer");
        }
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            let msg = format!(
                "run stopped: exceeded the {}s time budget",
                cfg.agent.run_timeout_secs
            );
            let _ = tx.send(AgentEvent::FinalAnswer(msg.clone())).await;
            return Ok(AgentOutcome {
                final_answer: msg,
                iterations,
            });
        }
        iterations += 1;

        // End gracefully (not as an error) when the model kept repeating.
        if let Some(reason) = tracker.stuck_reason() {
            let _ = tx.send(AgentEvent::FinalAnswer(reason.clone())).await;
            return Ok(AgentOutcome {
                final_answer: reason,
                iterations,
            });
        }

        // End gracefully when the model keeps re-verifying long after it has
        // already changed the repo (the nudge above was ignored).
        if let Some(reason) = tracker.stall_reason() {
            let _ = tx.send(AgentEvent::FinalAnswer(reason.clone())).await;
            return Ok(AgentOutcome {
                final_answer: reason,
                iterations,
            });
        }

        // A steer typed while this run was in flight reaches the model at its
        // next rest point, injected BEFORE the budget is enforced so compaction
        // can still make room for it.
        drain_steer(ctx.steer.as_ref(), ctxm).await;

        // A compaction the user requested (M-c) is honoured at this rest point,
        // before the budget is enforced: replace the whole history with a
        // model-written summary of what has been done.
        if ctx.compact.as_ref().is_some_and(|c| c.take()) {
            match crate::compact::compact_history(client, ctxm).await {
                Ok(rep) => {
                    let _ = tx
                        .send(AgentEvent::ContextCompacted {
                            before_messages: rep.before_messages,
                            after_messages: ctxm.messages().len(),
                            before_tokens: rep.before_tokens,
                            after_tokens: rep.after_tokens,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(AgentEvent::Error(format!(
                            "context compaction failed: {e:#}"
                        )))
                        .await;
                }
            }
        } else if cfg.context.auto_compact
            && auto_compact_armed
            && ctxm.messages().iter().any(|m| m.role == Role::Assistant)
            && ctxm.needs_auto_compaction()
        {
            // Automatic compaction: rather than let `enforce_budget` silently
            // stub and evict the history, ask the model for a summary of what
            // has been done so far and replace the history with it. Armed once
            // per over-budget episode (see `auto_compact_armed` above) and never
            // before the model has taken a turn, so a fresh prompt is never
            // folded away into a summary of itself.
            auto_compact_armed = false;
            match crate::compact::compact_history(client, ctxm).await {
                Ok(rep) => {
                    let _ = tx
                        .send(AgentEvent::ContextCompacted {
                            before_messages: rep.before_messages,
                            after_messages: ctxm.messages().len(),
                            before_tokens: rep.before_tokens,
                            after_tokens: rep.after_tokens,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(AgentEvent::Error(format!(
                            "context compaction failed: {e:#}"
                        )))
                        .await;
                }
            }
        } else if !ctxm.needs_auto_compaction() {
            // Back under the threshold: re-arm so the next over-budget episode
            // summarises again.
            auto_compact_armed = true;
        }

        ctxm.enforce_budget();

        // One-shot stall nudge: the model has changed the repo and is now only
        // re-checking without editing again, so tell it to finish. Injected as
        // a user turn so it is seen by the model request below.
        if tracker.needs_stall_nudge() {
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: "loop_guard".into(),
                    output: STALL_NUDGE.to_string(),
                    ok: false,
                })
                .await;
            ctxm.push_user_merged(STALL_NUDGE);
        } else if tracker.needs_verify_nudge() {
            // Mirror nudge for the opposite pathology: editing forever without
            // ever running the tests.
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: "loop_guard".into(),
                    output: VERIFY_NUDGE.to_string(),
                    ok: false,
                })
                .await;
            ctxm.push_user_merged(VERIFY_NUDGE);
        }

        // Advertise native tools unless the protocol is strictly ReAct.
        let native = cfg.llm.protocol.native_enabled();
        let tool_specs: Option<Vec<comrade_tool::ToolSpec>> = if native {
            let specs: Vec<_> = tools.iter().map(|t| t.spec().clone()).collect();
            if specs.is_empty() { None } else { Some(specs) }
        } else {
            None
        };

        // Stream the model's reply to the UI as `Delta` events; the accumulated
        // turn (text + native tool calls) is returned separately. A fast local
        // model emits tokens far faster than the TUI can redraw them (freeze
        // notes #25/#29), so the raw per-token chunks are COALESCED here: the
        // forwarder ships at most one `Delta` per DELTA_FLUSH_MS (or per
        // DELTA_FLUSH_CHARS of buffered text), and uses `try_send` so a UI
        // that has fallen behind can never wedge the run task on a full event
        // channel. Deltas are purely cosmetic streaming text -- the full turn
        // is committed by its own ToolCall/FinalAnswer event, so dropping an
        // intermediate batch only skips on-screen animation, never content.
        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let events_tx = tx.clone();
        const DELTA_FLUSH_MS: u64 = 33; // ~30 fps ceiling of on-screen token animation
        const DELTA_FLUSH_CHARS: usize = 1024; // ...and never batch more than this
        let forwarder = tokio::spawn(async move {
            let mut flush = tokio::time::interval(std::time::Duration::from_millis(DELTA_FLUSH_MS));
            flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut buf = String::new();
            let ship = |buf: &mut String| {
                if buf.is_empty() {
                    return;
                }
                let batch = std::mem::take(buf);
                let _ = events_tx.try_send(AgentEvent::Delta(batch));
            };
            loop {
                tokio::select! {
                    delta = delta_rx.recv() => match delta {
                        Some(d) => {
                            buf.push_str(&d);
                            // `buf` never grows past DELTA_FLUSH_CHARS, so the
                            // char count below stays cheap.
                            if buf.chars().count() >= DELTA_FLUSH_CHARS {
                                ship(&mut buf);
                            }
                        }
                        None => {
                            ship(&mut buf);
                            break;
                        }
                    },
                    _ = flush.tick() => ship(&mut buf),
                }
            }
        });

        let stream_result = tokio::select! {
            r = client.chat_turn(ctxm.messages(), tool_specs.as_deref(), {
                let delta_tx = delta_tx.clone();
                move |piece: &str| { let _ = delta_tx.send(piece.to_string()); }
            }) => r.context("llm call failed"),
            _ = stop.cancelled() => {
                Err(anyhow::anyhow!("agent interrupted by user"))
            }
        };
        let turn = match stream_result {
            Ok(turn) => turn,
            Err(e) => {
                // Interrupted (usually user cancel): return immediately,
                // WITHOUT draining the delta forwarder. Once delta_tx is
                // dropped (on return) the forwarder's channel closes and it
                // exits; the forwarder never blocks on the UI (try_send), so
                // it cannot hold up the run anyway.
                return Err(e);
            }
        };
        drop(delta_tx);
        // Drain the delta forwarder so any buffered text is flushed to the UI
        // before the turn's own ToolCall/FinalAnswer event commits the full
        // text. With the try_send path above this cannot block on a slow UI.
        let _ = forwarder.await;

        // Real usage from the API when reported (prompt_tokens = context the
        // model actually saw); fall back to our estimate otherwise.
        let (tokens, estimated) = match turn.usage.as_ref() {
            Some(u) if u.prompt_tokens > 0 => (u.prompt_tokens, false),
            _ => (ctxm.total_tokens(), true),
        };
        let _ = tx
            .send(AgentEvent::ContextStats {
                tokens,
                budget: cfg.effective_budget(),
                estimated,
            })
            .await;

        // Native function calls: dispatch them (possibly several per turn).
        if !turn.tool_calls.is_empty() {
            // Every task opens with a plan (single nudge per run).
            if !plan_nudged && ctx.session.plan().is_empty() {
                let first = turn
                    .tool_calls
                    .first()
                    .map(|c| c.name.as_str())
                    .unwrap_or("");
                let is_plan_tool =
                    matches!(first, "self_set_plan" | "self_rename_session" | "ask_form");
                if !is_plan_tool {
                    plan_nudged = true;
                    let msg = "Every task starts with a plan. Call self_set_plan first with your steps - \
                               each step needs a goal, a verification and the `model` that will run it \
                               (\"self\" or a delegate name) - before taking any other action.";
                    ctxm.push(ChatMessage::new(Role::Assistant, turn.content.clone()));
                    let _ = tx
                        .send(AgentEvent::ToolResult {
                            name: first.to_string(),
                            output: msg.to_string(),
                            ok: false,
                        })
                        .await;
                    ctxm.push(ChatMessage::new(Role::User, render_observation(first, msg)));
                    continue;
                }
            }
            run_native_calls(
                ctxm,
                &tx,
                tools,
                &ctx,
                &mut tracker,
                &mut consecutive_reads,
                dispatch,
                turn,
            )
            .await?;
            continue;
        }

        // Plain text: final answer or (auto/react fallback) a ReAct tool turn.
        let response = turn.content;
        if response.trim().is_empty() {
            bail!("model returned an empty response");
        }

        ctxm.push(ChatMessage::new(Role::Assistant, response.clone()));
        let _ = tx.send(AgentEvent::AssistantText(response.clone())).await;

        let turn_p = match parse_turn(&response) {
            Ok(t) => t,
            Err(e) => {
                // Recoverable model mistake: don't kill the run, ask the model
                // to resend valid JSON in its next turn.
                let msg = format!(
                    "Your previous message could not be parsed ({e:#}). \
                     Resend the tool call with Args as VALID strict JSON: quote every key and string \
                     value, no single quotes, no trailing commas."
                );
                let _ = tx
                    .send(AgentEvent::ToolResult {
                        name: "model_output".into(),
                        output: msg.clone(),
                        ok: false,
                    })
                    .await;
                let obs = ctxm.truncate_observation(&format!("ERROR: {msg}"));
                ctxm.push(ChatMessage::new(
                    Role::User,
                    render_observation("model_output", &obs),
                ));
                continue;
            }
        };
        if let Some(t) = turn_p.thought.as_deref() {
            let _ = tx.send(AgentEvent::Thought(t.to_string())).await;
        }

        let Some(tool_call) = turn_p.tool_call else {
            let answer = turn_p.final_text.clone();
            let _ = tx.send(AgentEvent::FinalAnswer(answer.clone())).await;
            return Ok(AgentOutcome {
                final_answer: answer,
                iterations,
            });
        };

        let Some(_tool) = tools.get(&tool_call.name) else {
            let msg = format!(
                "unknown tool {:?}; choose from the listed tools",
                tool_call.name
            );
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: tool_call.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            let obs = ctxm.truncate_observation(&format!("ERROR: {msg}"));
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &obs),
            ));
            continue;
        };

        // Every task opens with a plan. Nudge once if the model started acting
        // without calling set_plan.
        if !plan_nudged && ctx.session.plan().is_empty() {
            let is_plan_tool = matches!(
                tool_call.name.as_str(),
                "self_set_plan" | "self_rename_session" | "ask_form"
            );
            if !is_plan_tool {
                plan_nudged = true;
                let msg = "Every task starts with a plan. Call self_set_plan first with your steps - each step \
                     needs a goal, a verification and the `model` that will run it (\"self\" or a delegate \
                     name) - before taking any other action.";
                let _ = tx
                    .send(AgentEvent::ToolResult {
                        name: tool_call.name.clone(),
                        output: msg.to_string(),
                        ok: false,
                    })
                    .await;
                ctxm.push(ChatMessage::new(
                    Role::User,
                    render_observation(&tool_call.name, msg),
                ));
                continue;
            }
        }

        // Read guard: stop the model from reading forever without doing work.
        if !allow_read_step(&tool_call.name, &mut consecutive_reads) {
            let count = consecutive_reads;
            let msg = read_guard_message(count);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: tool_call.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &msg),
            ));
            continue;
        }

        let args_pretty = serde_json::to_string(&tool_call.args).unwrap_or_default();
        let sig = format!("{} {args_pretty}", tool_call.name);
        if let Some(count) = tracker.check(&sig) {
            if count >= MAX_LOOP_REFUSALS {
                tracker.mark_stuck(&sig);
                let msg = format!(
                    "ERROR: detected a loop: `{sig}` repeated {}x without any state change",
                    count
                );
                let _ = tx
                    .send(AgentEvent::ToolResult {
                        name: tool_call.name.clone(),
                        output: msg.clone(),
                        ok: false,
                    })
                    .await;
                // Let the top of the loop end the run gracefully.
                continue;
            }
            let msg = loop_refusal(&tool_call.name);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: tool_call.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            let obs = ctxm.truncate_observation(&format!("ERROR: {msg}"));
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &obs),
            ));
            continue;
        }
        let _ = tx
            .send(AgentEvent::ToolCall {
                name: tool_call.name.clone(),
                args: args_pretty.clone(),
                tokens: turn.usage.as_ref().and_then(usage_total),
            })
            .await;
        let _ = tx
            .send(AgentEvent::ToolStart {
                name: tool_call.name.clone(),
                args: args_pretty,
            })
            .await;

        let output = dispatch
            .run(tools, &ctx, &tool_call.name, tool_call.args.clone())
            .await;
        let ok = !output.starts_with("ERROR:");
        let clamped = ctxm.truncate_observation(&output);
        let _ = tx
            .send(AgentEvent::ToolResult {
                name: tool_call.name.clone(),
                output: clamped.clone(),
                ok,
            })
            .await;

        ctxm.push(ChatMessage::new(
            Role::User,
            observation_with_failure_hint(&tool_call.name, ok, &clamped),
        ));
        tracker.record(&tool_call.name, sig);
        // The tool call is spent: strip its (potentially large) args from the
        // stored assistant message so they are not re-sent every later turn.
        ctxm.note_tool_done(&tool_call.name, turn_p.thought.as_deref());
    }
}

/// Shared tool dispatch used by both the ReAct and native paths: it runs the
/// configured pre/post hooks around the call, applies an optional per-tool
/// timeout, and redacts secret-looking values out of the result before it is
/// fed back to the model or shown in the transcript.
#[derive(Clone, Copy)]
struct Dispatch<'a> {
    cfg: &'a Config,
    hooks: &'a Hooks,
    redactor: &'a Redactor,
}

impl Dispatch<'_> {
    async fn run(
        &self,
        tools: &ToolRegistry,
        ctx: &ToolContext,
        name: &str,
        args: serde_json::Value,
    ) -> String {
        if !self.hooks.is_empty()
            && let Err(e) = self.hooks.pre(&ctx.project_root, name, &args).await
        {
            return format!("ERROR: {e:#}");
        }
        let Some(tool) = tools.get(name) else {
            return format!("ERROR: unknown tool {name:?}");
        };
        let fut = tool.invoke(ctx, args.clone());
        let res = if self.cfg.agent.tool_timeout_secs > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_secs(self.cfg.agent.tool_timeout_secs),
                fut,
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "tool `{name}` timed out after {}s and was killed",
                    self.cfg.agent.tool_timeout_secs
                )),
            }
        } else {
            fut.await
        };
        let mut out = self.redactor.redact(&match res {
            Ok(o) => o,
            Err(e) => format!("ERROR: {e:#}"),
        });
        if !self.hooks.is_empty() {
            let ok = !out.starts_with("ERROR:");
            if let Some(warn) = self.hooks.post(&ctx.project_root, name, &args, ok).await {
                out.push_str("\n\n");
                out.push_str(&warn);
            }
        }
        out
    }
}

/// Dispatch a turn's native function calls. The assistant message with all
/// `tool_calls` is recorded first; each call then gets a `Role::Tool` result.
#[allow(clippy::too_many_arguments)]
async fn run_native_calls(
    ctxm: &mut ContextManager,
    tx: &mpsc::Sender<AgentEvent>,
    tools: &ToolRegistry,
    ctx: &ToolContext,
    tracker: &mut LoopTracker,
    consecutive_reads: &mut usize,
    dispatch: Dispatch<'_>,
    turn: crate::llm::LlmTurn,
) -> Result<()> {
    if !turn.content.trim().is_empty() {
        let _ = tx
            .send(AgentEvent::AssistantText(turn.content.clone()))
            .await;
    }

    struct Prepared {
        id: String,
        name: String,
        args: serde_json::Value,
        /// False when the model's argument string was not parseable JSON.
        parse_ok: bool,
        raw_args: String,
    }

    let mut prepared = Vec::new();
    let mut calls = Vec::new();
    for mc in turn.tool_calls {
        // Small models often emit JSON-ish arguments (unescaped quotes, bare
        // keys, trailing commas). Repair with the same tolerant parser the
        // ReAct path uses before giving up on the call.
        let parsed = serde_json::from_str::<serde_json::Value>(&mc.arguments)
            .ok()
            .or_else(|| crate::react::parse_args_json(&mc.arguments).ok());
        let parse_ok = parsed.is_some();
        let args = parsed.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        calls.push(crate::llm::ToolCallMsg {
            id: mc.id.clone(),
            name: mc.name.clone(),
            arguments: args.clone(),
        });
        prepared.push(Prepared {
            id: mc.id,
            name: mc.name,
            args,
            parse_ok,
            raw_args: mc.arguments,
        });
    }

    ctxm.push(ChatMessage::assistant_with_calls(turn.content, calls));

    // A native batch may hold several calls. `delegate` calls are slow
    // sub-agent runs, so when the whole batch is delegates they run
    // concurrently below instead of one after the other. Mixed batches keep
    // the historical strictly-sequential behaviour. Parallel delegates share
    // the repo, session and undo log — they are expected to manage their own
    // file conflicts (the delegate tool description warns the parent not to
    // batch two delegates that touch the same files).
    let parallel_delegates = prepared.len() > 1
        && prepared
            .iter()
            .all(|p| p.name == crate::delegate::TOOL_NAME);
    let mut deferred: Vec<Prepared> = Vec::new();
    // Real usage of the request behind this batch; handed to the first tool
    // call that actually dispatches so per-run sums count each request once.
    let mut turn_tokens = turn.usage.as_ref().and_then(usage_total);

    'calls: for p in prepared {
        if !p.parse_ok {
            // Do not run a tool with a half-parsed argument set (that is how a
            // small model silently corrupts a file): tell it to resend.
            let msg = format!(
                "ERROR: could not parse the arguments of `{}` as JSON. Resend this call with VALID \
                 strict JSON: escape newlines as \\n, escape every double quote inside a string as \
                 \\\", quote every key, and use no trailing comma. Received: {}",
                p.name, p.raw_args
            );
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: p.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            ctxm.push(ChatMessage::tool_result(p.id, msg));
            continue;
        }
        let args_pretty = serde_json::to_string(&p.args).unwrap_or_default();
        let sig = format!("{} {args_pretty}", p.name);
        // Read guard: refuse further exploration once nothing has changed.
        if !allow_read_step(&p.name, consecutive_reads) {
            let count = *consecutive_reads;
            let msg = read_guard_message(count);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: p.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            ctxm.push(ChatMessage::tool_result(p.id, msg));
            continue;
        }
        let _ = tx
            .send(AgentEvent::ToolCall {
                name: p.name.clone(),
                args: args_pretty.clone(),
                tokens: turn_tokens.take(),
            })
            .await;

        if let Some(count) = tracker.check(&sig) {
            if count >= MAX_LOOP_REFUSALS {
                tracker.mark_stuck(&sig);
                let msg = format!(
                    "ERROR: detected a loop: `{sig}` repeated {count}x without any state change"
                );
                let _ = tx
                    .send(AgentEvent::ToolResult {
                        name: p.name.clone(),
                        output: msg.clone(),
                        ok: false,
                    })
                    .await;
                // Stop dispatching this batch; the top of the loop ends the run.
                break 'calls;
            }
            let msg = loop_refusal(&p.name);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: p.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            ctxm.push(ChatMessage::tool_result(p.id, msg));
            continue;
        }

        let Some(_tool) = tools.get(&p.name) else {
            let msg = format!("unknown tool {:?}; choose from the listed tools", p.name);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: p.name.clone(),
                    output: msg.clone(),
                    ok: false,
                })
                .await;
            ctxm.push(ChatMessage::tool_result(p.id, msg));
            continue;
        };

        // Pre-flight checks all passed. In a pure-delegate batch the invocation
        // is deferred so every call runs concurrently after the loop; otherwise
        // keep invoking inline, exactly as before.
        if parallel_delegates {
            deferred.push(p);
            continue;
        }

        let args_pretty = serde_json::to_string(&p.args).unwrap_or_default();
        let _ = tx
            .send(AgentEvent::ToolStart {
                name: p.name.clone(),
                args: args_pretty,
            })
            .await;
        let output = dispatch.run(tools, ctx, &p.name, p.args.clone()).await;
        let ok = !output.starts_with("ERROR:");
        let clamped = ctxm.truncate_observation(&output);
        let _ = tx
            .send(AgentEvent::ToolResult {
                name: p.name.clone(),
                output: clamped.clone(),
                ok,
            })
            .await;
        let content = if ok {
            clamped.clone()
        } else {
            let hint = failure_hint(&p.name, &clamped);
            if hint.is_empty() {
                clamped.clone()
            } else {
                format!("{clamped}\n\nHINT: {hint}")
            }
        };
        ctxm.push(ChatMessage::tool_result(p.id, content));
        tracker.record(&p.name, sig);
    }

    // Run the deferred delegates concurrently. They are all `delegate` calls;
    // results are streamed back and recorded in the original call order.
    // Parallel delegates may each mutate the workspace — the parent was told
    // not to batch delegates that touch the same files.
    if !deferred.is_empty() {
        for p in &deferred {
            let args_pretty = serde_json::to_string(&p.args).unwrap_or_default();
            let _ = tx
                .send(AgentEvent::ToolStart {
                    name: p.name.clone(),
                    args: args_pretty,
                })
                .await;
        }
        let futures: Vec<Pin<Box<dyn Future<Output = String> + Send + '_>>> = deferred
            .iter()
            .map(|p| {
                let ctx = ctx.clone();
                let args = p.args.clone();
                let name = p.name.clone();
                let fut: Pin<Box<dyn Future<Output = String> + Send + '_>> =
                    Box::pin(async move { dispatch.run(tools, &ctx, &name, args).await });
                fut
            })
            .collect();
        let results = join_all(futures).await;
        for (p, output) in deferred.into_iter().zip(results) {
            let ok = !output.starts_with("ERROR:");
            let clamped = ctxm.truncate_observation(&output);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: p.name.clone(),
                    output: clamped.clone(),
                    ok,
                })
                .await;
            let content = if ok {
                clamped.clone()
            } else {
                let hint = failure_hint(&p.name, &clamped);
                if hint.is_empty() {
                    clamped.clone()
                } else {
                    format!("{clamped}\n\nHINT: {hint}")
                }
            };
            ctxm.push(ChatMessage::tool_result(p.id, content));
            let sig = format!(
                "{} {}",
                p.name,
                serde_json::to_string(&p.args).unwrap_or_default()
            );
            tracker.record(&p.name, sig);
        }
    }
    ctxm.note_turn_done();
    Ok(())
}

/// Convenience wrapper so callers don't need the full signature when running a
/// one-shot headless session.
pub async fn run_headless(
    cfg: &Config,
    client: &LlmClient,
    ctx: ToolContext,
    tools: &ToolRegistry,
    user_input: String,
) -> Result<AgentOutcome> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let printer = tokio::task::spawn(async move {
        while let Some(event) = rx.recv().await {
            let line = match &event {
                AgentEvent::Delta(d) => {
                    use std::io::Write;
                    print!("{d}");
                    std::io::stdout().flush().ok();
                    continue;
                }
                AgentEvent::Thought(t) => format!("🧠 {t}"),
                AgentEvent::ToolStart { name, args } => format!("🔧 {name} {args}"),
                AgentEvent::ToolResult { output, ok, .. } => {
                    if *ok {
                        format!("   ↳ {output}")
                    } else {
                        format!("   ⚠ {output}")
                    }
                }
                AgentEvent::FinalAnswer(a) => format!("\n✅ {a}"),
                AgentEvent::Error(e) => format!("❌ {e}"),
                AgentEvent::User(u) => format!("🧑 {u}"),
                // Sub-agent activity (delegate/ask_advise): indented so a
                // delegate's own tool calls are visible in the run log.
                AgentEvent::DelegateToolCall { model, name, args } => {
                    format!("   ↳ {model} → {name} {args}")
                }
                AgentEvent::DelegateToolResult {
                    model,
                    name: _,
                    output,
                    ok,
                } => {
                    let mark = if *ok { "↳" } else { "⚠" };
                    format!("   {mark} {model}: {output}")
                }
                _ => continue,
            };
            println!("{line}");
        }
    });

    let outcome = run_agent(
        cfg,
        client,
        ctx,
        tools,
        user_input,
        tx,
        CancellationToken::new(),
    )
    .await;
    printer.abort();
    let _ = printer.await;
    outcome
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use comrade_tool::{ToolContext, ToolRegistry, UserIo, UserPrompt, UserReply};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::config::Config;
    use crate::context::ContextManager;
    use crate::llm::LlmClient;
    use crate::session::{AgentEvent, AgentSession};
    use crate::undo::MemoryUndo;

    use super::{run_agent, run_agent_with_history};

    struct FakeUser;
    #[async_trait]
    impl UserIo for FakeUser {
        async fn ask(&self, _p: UserPrompt) -> anyhow::Result<UserReply> {
            Ok(UserReply::Answer("yes".into()))
        }
    }

    /// Scripted model serving one reply per HTTP request, streamed in two SSE
    /// chunks to exercise the accumulator. Requests past the end get the last
    /// canned reply.
    fn spawn_model_with(responses: &[&str]) -> u16 {
        let responses: Vec<String> = responses.iter().map(|s| s.to_string()).collect();
        let responses = Arc::new(responses);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let calls = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let mut used = 0usize;
                loop {
                    match stream.read(&mut buf[used..]) {
                        Ok(0) => break,
                        Ok(r) => {
                            used += r;
                            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let content = responses
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| responses.last().map(String::as_str).unwrap_or("All done."));
                // Split into two SSE chunks to exercise the accumulator.
                let half = content.len() / 2;
                let (a, b) = content.split_at(half);
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":{:?}}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":{:?}}}}}]}}\n\n\
                     data: [DONE]\n\n",
                    a, b
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        port
    }

    fn spawn_fake_model() -> u16 {
        spawn_model_with(&[
            "Thought: try a tool\nTool: no_such_tool\nArgs: {\"x\": 1}",
            "All done.",
        ])
    }

    /// Like [`spawn_model_with`], but each raw REQUEST body (read per
    /// Content-Length) is forwarded to the returned channel first, so tests
    /// can assert what the agent was actually asked on every turn.
    fn spawn_model_spy(responses: &[&str]) -> (u16, std::sync::mpsc::Receiver<String>) {
        let responses: Vec<String> = responses.iter().map(|s| s.to_string()).collect();
        let responses = Arc::new(responses);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let calls = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let mut data = Vec::new();
                let mut tmp = [0u8; 8192];
                let mut body_len: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                while body_len.is_none_or(|len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(r) => {
                            data.extend_from_slice(&tmp[..r]);
                            if header_end.is_none()
                                && let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(p);
                                let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                                body_len = head.lines().find_map(|l| {
                                    l.trim()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse().ok())
                                });
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body = match (header_end, body_len) {
                    (Some(he), Some(len)) => {
                        let start = he + 4;
                        let end = (start + len).min(data.len());
                        String::from_utf8_lossy(&data[start..end]).into_owned()
                    }
                    _ => String::new(),
                };
                let _ = tx.send(body);
                let content = responses
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| responses.last().map(String::as_str).unwrap_or("All done."));
                // Split into two SSE chunks to exercise the accumulator.
                let half = content.len() / 2;
                let (a, b) = content.split_at(half);
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":{:?}}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":{:?}}}}}]}}\n\n\
                     data: [DONE]\n\n",
                    a, b
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (port, rx)
    }

    /// Wait (with a timeout) for the next request body the fake model saw.
    async fn recv_body(rx: &std::sync::mpsc::Receiver<String>) -> String {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(b) = rx.try_recv() {
                return b;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for a model request");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A message pushed into the run's steering pipe is drained at the top of
    /// the loop and injected into the model history as a user message, so the
    /// very first model request carries it (mid-run steers take the same
    /// per-iteration drain path).
    #[tokio::test]
    async fn queued_steer_reaches_the_first_model_request() {
        let (port, bodies) = spawn_model_spy(&[
            "Thought: try a tool\nTool: no_such_tool\nArgs: {\"x\": 1}",
            "All done.",
        ]);
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, _events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        let root = std::env::temp_dir().join(format!("comrade-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));

        let (steer, steer_tx) = comrade_tool::Steer::channel();
        steer_tx
            .send("stop reading and implement now".to_string())
            .unwrap();
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: Some(steer),
            stop: None,
            compact: None,
        };
        let tools = ToolRegistry::new();
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "do the thing".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");

        let first = recv_body(&bodies).await;
        assert!(
            first.contains("stop reading and implement now"),
            "the steer must ride in the first model request"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fake model that answers both protocols the compaction test needs:
    /// non-streaming JSON (the summariser's `chat` call) and SSE (the agent's
    /// `chat_turn`). Every raw request body is forwarded to the channel.
    fn spawn_compact_spy() -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut data = Vec::new();
                let mut tmp = [0u8; 8192];
                let mut body_len: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                while body_len.is_none_or(|len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(r) => {
                            data.extend_from_slice(&tmp[..r]);
                            if header_end.is_none()
                                && let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(p);
                                let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                                body_len = head.lines().find_map(|l| {
                                    l.trim()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse().ok())
                                });
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body = match (header_end, body_len) {
                    (Some(he), Some(len)) => {
                        let start = he + 4;
                        let end = (start + len).min(data.len());
                        String::from_utf8_lossy(&data[start..end]).into_owned()
                    }
                    _ => String::new(),
                };
                let streaming = body.contains("\"stream\":true");
                let _ = tx.send(body);
                let resp = if streaming {
                    let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"All done.\"}}]}\n\n\
                                   data: [DONE]\n\n";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                } else {
                    let payload = "{\"choices\":[{\"message\":{\"content\":\"did X\"}}]}";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                };
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (port, rx)
    }

    /// A pending `CompactRequest` makes the very first model call a summariser
    /// request (non-streaming `chat`) whose body carries the transcript; the run
    /// then continues normally with the summarised history.
    #[tokio::test]
    async fn pending_compaction_request_summarises_the_history() {
        let (port, bodies) = spawn_compact_spy();
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, _events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        let root = std::env::temp_dir().join(format!("comrade-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));

        let compact = comrade_tool::CompactRequest::new();
        assert!(!compact.is_pending());
        compact.request();
        assert!(compact.is_pending());
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: Some(compact),
            stop: None,
        };
        let tools = ToolRegistry::new();
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "do the thing".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");

        // Compaction runs before the first agent turn, so the first request the
        // model sees is the summariser asking for the notes.
        let first = recv_body(&bodies).await;
        assert!(
            first.contains("Conversation so far:"),
            "first request must be the summariser: {first}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fake model for the auto-compaction test: streaming requests
    /// (`chat_turn`) get the next scripted agent reply as SSE, non-streaming
    /// requests (the summariser's `chat`) get a JSON summary. Every request body
    /// is forwarded to the channel.
    fn spawn_auto_compact_spy(responses: &[&str]) -> (u16, std::sync::mpsc::Receiver<String>) {
        let responses: Vec<String> = responses.iter().map(|s| s.to_string()).collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stream_calls = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut data = Vec::new();
                let mut tmp = [0u8; 8192];
                let mut body_len: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                while body_len.is_none_or(|len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(r) => {
                            data.extend_from_slice(&tmp[..r]);
                            if header_end.is_none()
                                && let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(p);
                                let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                                body_len = head.lines().find_map(|l| {
                                    l.trim()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse().ok())
                                });
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body = match (header_end, body_len) {
                    (Some(he), Some(len)) => {
                        let start = he + 4;
                        let end = (start + len).min(data.len());
                        String::from_utf8_lossy(&data[start..end]).into_owned()
                    }
                    _ => String::new(),
                };
                let streaming = body.contains("\"stream\":true");
                let _ = tx.send(body);
                let resp = if streaming {
                    let n = stream_calls.fetch_add(1, Ordering::SeqCst);
                    let content = responses.get(n).map(String::as_str).unwrap_or("All done.");
                    let payload = format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":{content:?}}}}}]}}\n\n\
                         data: [DONE]\n\n"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                } else {
                    let payload = "{\"choices\":[{\"message\":{\"content\":\"did X\"}}]}";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                };
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (port, rx)
    }

    /// With a budget small enough that the history crosses into the headroom
    /// mid-run, the loop summarises it automatically (with no `CompactRequest`):
    /// the turn after the model has spoken issues a summariser request and the
    /// run still finishes normally.
    #[tokio::test]
    async fn over_budget_history_is_auto_compacted() {
        let (port, bodies) = spawn_auto_compact_spy(&[
            "Thought: try a tool\nTool: no_such_tool\nArgs: {\"x\": 1}",
            "All done.",
        ]);
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, _events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        let root = std::env::temp_dir().join(format!("comrade-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let tools = ToolRegistry::new();
        let client = LlmClient::new(&cfg.llm).unwrap();

        // A tiny budget: after the model's first (tool-calling) turn, the history
        // is over the cap, so the next rest point summarises it.
        let mut history = ContextManager::with_system("test system", 40, 5000);

        let outcome = run_agent_with_history(
            &cfg,
            &client,
            ctx,
            &tools,
            "do the thing".to_string(),
            &mut history,
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");

        // The run has finished, so every request it made is already buffered: at
        // least one must be the non-streaming summariser carrying the transcript.
        let mut summarised = false;
        while let Ok(b) = bodies.try_recv() {
            if b.contains("Conversation so far:") {
                summarised = true;
            }
        }
        assert!(
            summarised,
            "an over-budget history must be auto-summarised without a CompactRequest"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn agent_streams_and_loops_until_final() {
        let port = spawn_fake_model();
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, mut events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root = std::env::temp_dir().join(format!("comrade-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let tools = ToolRegistry::new();
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "do the thing".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");
        assert_eq!(outcome.iterations, 2);

        // The run must have emitted streamed deltas and an error observation
        // for the unknown tool before finishing.
        let mut saw_delta = false;
        let mut saw_unknown_tool = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await
        {
            match ev {
                AgentEvent::Delta(_) => saw_delta = true,
                AgentEvent::ToolResult { output, ok, .. } => {
                    if !ok && output.contains("no_such_tool") {
                        saw_unknown_tool = true;
                    }
                }
                AgentEvent::RunEnd => break,
                _ => {}
            }
        }
        assert!(saw_delta, "expected streamed deltas");
        assert!(
            saw_unknown_tool,
            "expected the unknown-tool error observation"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stand-in for an approval-gated tool: records invocations and asks for
    /// confirmation like the real fs_write_file would.
    struct RecordingWrite {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl comrade_tool::Tool for RecordingWrite {
        fn spec(&self) -> &comrade_tool::ToolSpec {
            static SPEC: std::sync::LazyLock<comrade_tool::ToolSpec> =
                std::sync::LazyLock::new(|| comrade_tool::ToolSpec {
                    name: "fs_write_file".into(),
                    description: "write a file (test)".into(),
                    json_schema: serde_json::json!({ "type": "object", "properties": {} }),
                });
            &SPEC
        }
        async fn invoke(
            &self,
            ctx: &ToolContext,
            _args: serde_json::Value,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ctx.confirm("fs_write_file (test)", None).await?;
            Ok("wrote file".into())
        }
    }

    fn gated_registry(calls: Arc<std::sync::atomic::AtomicUsize>) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(RecordingWrite { calls }));
        tools
    }

    #[tokio::test]
    async fn approval_gated_tool_runs_with_confirmation() {
        let port = spawn_model_with(&[
            "Thought: write it\nTool: fs_write_file\nArgs: {\"path\": \"x.rs\", \"content\": \"a\"}",
            "All done.",
        ]);
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, _events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root = std::env::temp_dir().join(format!("comrade-gated-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: false,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = gated_registry(calls.clone());
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "do it".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");
        // the tool now runs with no justification, after the human confirms
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tool that never returns in time, to exercise the per-tool timeout.
    struct SlowTool;
    #[async_trait]
    impl comrade_tool::Tool for SlowTool {
        fn spec(&self) -> &comrade_tool::ToolSpec {
            static SPEC: std::sync::LazyLock<comrade_tool::ToolSpec> =
                std::sync::LazyLock::new(|| comrade_tool::ToolSpec {
                    name: "slow_tool".into(),
                    description: "sleeps (test)".into(),
                    json_schema: serde_json::json!({ "type": "object", "properties": {} }),
                });
            &SPEC
        }
        async fn invoke(
            &self,
            _ctx: &ToolContext,
            _args: serde_json::Value,
        ) -> anyhow::Result<String> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok("never seen".into())
        }
    }

    /// A tool whose output carries a token-shaped secret.
    struct SecretEcho;
    #[async_trait]
    impl comrade_tool::Tool for SecretEcho {
        fn spec(&self) -> &comrade_tool::ToolSpec {
            static SPEC: std::sync::LazyLock<comrade_tool::ToolSpec> =
                std::sync::LazyLock::new(|| comrade_tool::ToolSpec {
                    name: "echo_secret".into(),
                    description: "echoes a secret (test)".into(),
                    json_schema: serde_json::json!({ "type": "object", "properties": {} }),
                });
            &SPEC
        }
        async fn invoke(
            &self,
            _ctx: &ToolContext,
            _args: serde_json::Value,
        ) -> anyhow::Result<String> {
            Ok("export API_TOKEN=sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ".into())
        }
    }

    fn policy_registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(SlowTool));
        tools.register(Box::new(SecretEcho));
        tools
    }

    async fn drain_events(mut events: mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await
        {
            let end = matches!(ev, AgentEvent::RunEnd);
            out.push(ev);
            if end {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn a_slow_tool_is_killed_by_the_per_tool_timeout() {
        let port = spawn_model_with(&["Thought: try it\nTool: slow_tool\nArgs: {}", "All done."]);
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();
        cfg.agent.tool_timeout_secs = 1;

        let (tx, events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root = std::env::temp_dir().join(format!("comrade-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: Arc::new(MemoryUndo::new(root.clone())),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let tools = policy_registry();
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "do it".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");

        let mut saw_timeout = false;
        for ev in drain_events(events).await {
            if let AgentEvent::ToolResult { output, ok, .. } = ev
                && !ok
                && output.contains("timed out")
            {
                saw_timeout = true;
            }
        }
        assert!(saw_timeout, "a slow tool must be reported as timed out");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn tool_output_is_redacted_before_it_reaches_the_model() {
        let port = spawn_model_with(&[
            "Thought: read env\nTool: echo_secret\nArgs: {}",
            "All done.",
        ]);
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root = std::env::temp_dir().join(format!("comrade-redact-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: Arc::new(MemoryUndo::new(root.clone())),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let tools = policy_registry();
        let client = LlmClient::new(&cfg.llm).unwrap();

        run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "do it".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let mut saw_redaction = false;
        for ev in drain_events(events).await {
            if let AgentEvent::ToolResult { output, .. } = ev {
                assert!(
                    !output.contains("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
                    "the secret leaked: {output}"
                );
                if output.contains("«redacted»") {
                    saw_redaction = true;
                }
            }
        }
        assert!(saw_redaction, "the tool output must be redacted");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Serve one native tool-call request (streamed `tool_calls`) for
    /// fs_write_file, then a final text answer.
    fn spawn_native_model() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let calls = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let mut used = 0usize;
                loop {
                    match stream.read(&mut buf[used..]) {
                        Ok(0) => break,
                        Ok(r) => {
                            used += r;
                            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body = if n == 0 {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Writing now.\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"fs_write_file\",\"arguments\":\"{\\\"path\\\": \\\"x.rs\\\", \\\"content\\\": \\\"a\\\"}\"}}]}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"All done.\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        port
    }

    #[tokio::test]
    async fn native_tool_calls_dispatch_and_then_finish() {
        let port = spawn_native_model();
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, mut events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root = std::env::temp_dir().join(format!("comrade-native-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = gated_registry(calls.clone());
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "write it".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // auto-approve path: tool executed without a confirm prompt

        let mut saw_final = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await
        {
            if let AgentEvent::FinalAnswer(a) = &ev {
                saw_final = a.contains("All done");
            }
            if let AgentEvent::RunEnd = ev {
                break;
            }
        }
        assert!(saw_final);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Main model that answers the first request with TWO native `delegate`
    /// tool calls in a single batch, then finishes with "All done.".
    fn spawn_two_delegate_main_model() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let calls = AtomicUsize::new(0);
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let mut used = 0usize;
                loop {
                    match stream.read(&mut buf[used..]) {
                        Ok(0) => break,
                        Ok(r) => {
                            used += r;
                            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body = if n == 0 {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Delegating two tasks.\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"delegate\",\"arguments\":\"{\\\"model\\\": \\\"a\\\", \\\"task\\\": \\\"alpha work\\\"}\"}}]}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"delegate\",\"arguments\":\"{\\\"model\\\": \\\"b\\\", \\\"task\\\": \\\"beta work\\\"}\"}}]}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"All done.\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        port
    }

    /// Fake delegate endpoint that answers only once BOTH delegate requests
    /// have arrived. A sequential dispatcher sends one request and then blocks
    /// waiting for its reply, so it deadlocks here and the enclosing test times
    /// out; parallel dispatch gets both requests in and both replies out.
    fn spawn_barrier_delegate_server() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let read_header = |mut stream: std::net::TcpStream| -> std::net::TcpStream {
                let mut buf = [0u8; 2048];
                let mut used = 0usize;
                loop {
                    match stream.read(&mut buf[used..]) {
                        Ok(0) => break,
                        Ok(r) => {
                            used += r;
                            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                stream
            };
            let (s1, _) = listener.accept().unwrap();
            let s1 = read_header(s1);
            let (s2, _) = listener.accept().unwrap();
            let s2 = read_header(s2);
            for (mut s, content) in [(s1, "RESULT-A"), (s2, "RESULT-B")] {
                let body =
                    format!("{{\"choices\":[{{\"message\":{{\"content\":\"{content}\"}}}}]}}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        port
    }

    #[tokio::test]
    async fn parallel_delegate_calls_in_one_turn_run_concurrently() {
        let main_port = spawn_two_delegate_main_model();
        let delegate_port = spawn_barrier_delegate_server();
        let delegate_url = format!("http://127.0.0.1:{delegate_port}/v1");
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{main_port}/v1");
        cfg.llm.model = "fake".into();
        cfg.delegates = vec![
            crate::config::DelegateCfg {
                name: "a".into(),
                description: "delegate a".into(),
                enabled: true,
                approval: crate::config::Autonomy::Auto,
                llm: crate::config::LlmCfg {
                    base_url: delegate_url.clone(),
                    model: "alpha".into(),
                    ..Default::default()
                },
            },
            crate::config::DelegateCfg {
                name: "b".into(),
                description: "delegate b".into(),
                enabled: true,
                approval: crate::config::Autonomy::Auto,
                llm: crate::config::LlmCfg {
                    base_url: delegate_url,
                    model: "beta".into(),
                    ..Default::default()
                },
            },
        ];

        let (tx, mut events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "split the work into two independent sub-tasks and delegate both".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root =
            std::env::temp_dir().join(format!("comrade-parallel-delegate-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let delegate_tool = crate::delegate::DelegateTool::new(
            &cfg.delegates,
            ToolRegistry::new(),
            crate::delegate::DelegateLimits::default(),
        )
        .unwrap()
        .unwrap();
        let mut tools = ToolRegistry::new();
        tools.extend(vec![Box::new(delegate_tool) as Box<dyn comrade_tool::Tool>]);
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_agent(
                &cfg,
                &client,
                ctx,
                &tools,
                "delegate both sub-tasks in parallel".to_string(),
                tx,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("agent stalled: the two delegate calls did not run in parallel")
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");

        let mut delegate_results = 0usize;
        let mut saw_result_a = false;
        let mut saw_result_b = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await
        {
            match ev {
                AgentEvent::ToolResult {
                    name, output, ok, ..
                } if name == "delegate" => {
                    delegate_results += 1;
                    assert!(ok, "delegate call failed: {output}");
                    saw_result_a |= output.contains("RESULT-A");
                    saw_result_b |= output.contains("RESULT-B");
                }
                AgentEvent::RunEnd => break,
                _ => {}
            }
        }
        assert_eq!(delegate_results, 2);
        assert!(
            saw_result_a && saw_result_b,
            "both delegate replies must reach the model"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn native_gated_tool_runs_with_confirmation() {
        let port = spawn_native_model();
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, _events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root =
            std::env::temp_dir().join(format!("comrade-native-gated-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: false,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = gated_registry(calls.clone());
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "write it".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");
        // the gated tool now runs with no justification, after the human confirms
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod loop_tests {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::Config;
    use crate::llm::LlmClient;
    use crate::session::{AgentEvent, AgentSession};
    use crate::undo::MemoryUndo;
    use comrade_tool::{ToolContext, ToolRegistry};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct IoNoop;
    #[async_trait::async_trait]
    impl comrade_tool::UserIo for IoNoop {
        async fn ask(
            &self,
            _p: comrade_tool::UserPrompt,
        ) -> anyhow::Result<comrade_tool::UserReply> {
            Ok(comrade_tool::UserReply::Answer("yes".into()))
        }
    }

    fn spawn_parse_model() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let calls = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let mut used = 0usize;
                loop {
                    match stream.read(&mut buf[used..]) {
                        Ok(0) => break,
                        Ok(r) => {
                            used += r;
                            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let content = if n == 0 {
                    "Thought: look\nTool: list_dir\nArgs: {\"path\": \"x\""
                } else {
                    "All done."
                };
                let half = content.len() / 2;
                let (a, b) = content.split_at(half);
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":{:?}}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":{:?}}}}}]}}\n\n\
                     data: [DONE]\n\n",
                    a, b
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        port
    }

    #[tokio::test]
    async fn unparseable_args_is_recoverable_not_fatal() {
        let port = spawn_parse_model();
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, mut events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
        comrade_tool::SessionControl::set_plan(
            &*session,
            vec![comrade_tool::PlanStepDraft {
                goal: "do it".into(),
                verification: "verifies".into(),
                model: "".into(),
                context: "".into(),
            }],
        );
        let root =
            std::env::temp_dir().join(format!("comrade-parse-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(IoNoop),
            undo: undo.clone(),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        let tools = ToolRegistry::new();
        let client = LlmClient::new(&cfg.llm).unwrap();

        let outcome = run_agent(
            &cfg,
            &client,
            ctx,
            &tools,
            "go".to_string(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.final_answer, "All done.");
        assert_eq!(outcome.iterations, 2);

        let mut saw_feedback = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await
        {
            if let AgentEvent::ToolResult { output, ok, .. } = &ev
                && !ok
                && output.contains("could not be parsed")
            {
                saw_feedback = true;
            }
            if let AgentEvent::RunEnd = ev {
                break;
            }
        }
        assert!(saw_feedback, "expected a parse-fallback observation");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn identical_call_without_change_is_a_loop() {
        let mut t = LoopTracker::default();
        let sig = "fs_read_file {\"path\":\"a.rs\"}".to_string();
        assert_eq!(t.check(&sig), None);
        t.record("fs_read_file", sig.clone());
        // same call again, nothing changed -> refused (count 1, then 2)
        assert_eq!(t.check(&sig), Some(1));
        assert_eq!(t.check(&sig), Some(2));
        assert_eq!(t.check(&sig), Some(3));
    }

    #[test]
    fn repeat_after_a_mutation_is_allowed() {
        let mut t = LoopTracker::default();
        let sig = "pom_run_task {\"task\":\"test\"}".to_string();
        t.record("pom_run_task", sig.clone());
        assert_eq!(t.check(&sig), Some(1));

        // a mutating call in between bumps the sequence; identical test re-run ok
        t.record("fs_edit", "fs_edit {..}".to_string());
        assert_eq!(t.check(&sig), None);
    }

    #[test]
    fn different_arguments_are_not_a_loop() {
        let mut t = LoopTracker::default();
        t.record("fs_read_file", "fs_read_file a".to_string());
        assert_eq!(t.check("fs_read_file b"), None);
    }
}

#[cfg(test)]
mod loop_tracker_tests {
    use super::*;

    #[test]
    fn refusals_reset_after_different_action() {
        let mut t = LoopTracker::default();
        let sig = "fs_read_file a".to_string();
        t.record("fs_read_file", sig.clone());
        assert_eq!(t.check(&sig), Some(1));
        // model tries something else (still no mutation) -> counter resets
        t.record("fs_rgrep", "fs_rgrep query".to_string());
        assert_eq!(t.check(&sig), Some(1));
        assert_eq!(t.check(&sig), Some(2));
    }

    #[test]
    fn refusals_reset_after_mutation() {
        let mut t = LoopTracker::default();
        let sig = "pom_run_task {\"task\":\"test\"}".to_string();
        t.record("pom_run_task", sig.clone());
        assert_eq!(t.check(&sig), Some(1));
        t.record("fs_edit", "fs_edit {..}".to_string());
        assert_eq!(t.check(&sig), None); // legit re-run after an edit
    }

    #[test]
    fn stuck_ends_gracefully() {
        let mut t = LoopTracker::default();
        assert!(t.stuck_reason().is_none());
        t.mark_stuck("pom_run_tests {..}");
        assert!(t.stuck_reason().unwrap().contains("Stopped"));
    }
}

#[cfg(test)]
mod read_guard_tests {
    use super::*;

    #[test]
    fn reads_count_up_then_guard_refuses() {
        let mut reads = 0usize;
        for _ in 0..READ_GUARD_THRESHOLD {
            assert!(
                allow_read_step("fs_read_file", &mut reads),
                "reads should be allowed until threshold"
            );
        }
        assert_eq!(reads, READ_GUARD_THRESHOLD);
        // the next read is refused (and does not bump the counter)
        assert!(!allow_read_step("fs_read_file", &mut reads));
        assert_eq!(reads, READ_GUARD_THRESHOLD);
    }

    #[test]
    fn any_state_change_resets_the_counter() {
        let mut reads = 0usize;
        for _ in 0..READ_GUARD_THRESHOLD {
            assert!(allow_read_step("fs_rgrep", &mut reads));
        }
        assert!(!allow_read_step("fs_rgrep", &mut reads));

        // an action (write/edit/plan) resets the read counter
        assert!(allow_read_step("fs_edit", &mut reads));
        assert_eq!(reads, 0);
        assert!(allow_read_step("fs_rgrep", &mut reads));
        assert_eq!(reads, 1);
    }
}

#[cfg(test)]
mod monitors_tests {
    use super::*;

    #[test]
    fn failure_hint_classifies_common_cases() {
        assert!(failure_hint("shell", "user denied request").is_empty());
        let t = failure_hint("shell", "the command timed out after 300s and was killed");
        assert!(t.contains("timed out"), "{t}");
        let c = failure_hint(
            "pom_run_task",
            "error[E0308]: mismatched types\n --> src/a.rs",
        );
        assert!(c.contains("compile"), "{c}");
        let f = failure_hint(
            "pom_run_tests",
            "test result: FAILED. 1 failed\npanicked at src/lib.rs",
        );
        assert!(f.contains("tests failed"), "{f}");
        let g = failure_hint("shell", "any generic failure here");
        assert!(!g.is_empty());
    }
}
#[cfg(test)]
mod usage_tests {
    use super::usage_total;
    use crate::llm::Usage;

    #[test]
    fn usage_total_prefers_total_then_falls_back_to_the_sum() {
        // Endpoint reports total_tokens directly.
        assert_eq!(
            usage_total(&Usage {
                total_tokens: 150,
                prompt_tokens: 100,
                completion_tokens: 50,
            }),
            Some(150)
        );
        // Only the parts are present: sum them.
        assert_eq!(
            usage_total(&Usage {
                total_tokens: 0,
                prompt_tokens: 90,
                completion_tokens: 10,
            }),
            Some(100)
        );
        // No usage at all: the UI falls back to "no tokens reported".
        assert_eq!(usage_total(&Usage::default()), None);
    }
}

#[cfg(test)]
mod stall_tests {
    use super::LoopTracker;

    fn read(t: &mut LoopTracker, i: usize) {
        t.record("fs_read_file", format!("r{i}"));
    }

    #[test]
    fn reads_before_any_change_never_arm_the_guard() {
        // A long read-only exploration before the first edit is legitimate.
        let mut t = LoopTracker::default();
        for i in 0..40 {
            read(&mut t, i);
        }
        assert!(!t.needs_stall_nudge());
        assert!(t.stall_reason().is_none());
    }

    #[test]
    fn a_progress_call_resets_the_idle_run_and_the_nudge_is_one_shot() {
        let mut t = LoopTracker::default();
        t.record("fs_edit", "e1".into());
        for i in 0..7 {
            read(&mut t, i);
        }
        assert!(!t.needs_stall_nudge(), "7 idle calls is below the nudge");
        read(&mut t, 7);
        assert!(t.needs_stall_nudge());
        assert!(!t.needs_stall_nudge(), "the nudge fires at most once");
        // A further edit is progress and resets the idle run.
        t.record("fs_edit", "e2".into());
        read(&mut t, 0);
        assert!(t.stall_reason().is_none());
    }

    #[test]
    fn stall_reason_only_after_the_end_threshold() {
        let mut t = LoopTracker::default();
        t.record("fs_write_file", "w1".into());
        for i in 0..17 {
            read(&mut t, i);
        }
        assert!(t.stall_reason().is_none());
        read(&mut t, 17);
        assert!(t.stall_reason().is_some());
    }

    #[test]
    fn test_runs_count_as_non_progress() {
        // `pom_run_tests` is "mutating" for the repeat tracker but it is not a
        // change to the repo, so re-running it must still arm the stall guard.
        let mut t = LoopTracker::default();
        t.record("fs_edit", "e1".into());
        for i in 0..8 {
            t.record("pom_run_tests", format!("t{i}"));
        }
        assert!(t.needs_stall_nudge());
    }
}
