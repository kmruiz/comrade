use anyhow::{Context as _, Result, bail};
use comrade_tool::{ToolContext, ToolRegistry};
use futures_util::future::join_all;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::context::ContextManager;
use crate::llm::{ChatMessage, LlmClient, Role, Usage};
use crate::react::{build_system_prompt, parse_turn, render_observation};
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

/// Tools whose side effects require human approval (and thus mandatory
/// Justification/Risk). Keep in sync with the tool crates.
/// Tools that mutate the workspace (used by the loop tracker to tell "repeat
/// but state changed" from "repeat doing nothing").
const MUTATING_TOOLS: &[&str] = &[
    "apply_edit",
    "apply_patch",
    "write_file",
    "rename",
    "git_commit",
    "delegate",
    "run_task",
    "remember",
    "amend_decision",
    "format_code",
    "run_tests",
    "shell",
];

/// Tools that are approval-gated: the model MUST provide `justification` and
/// `risk` before they run (a human approves based on them). `git_commit`,
/// `run_task` and `run_tests` deliberately are NOT gated: they run directly.
/// `delegate` IS gated: handing a task to a sub-agent that will edit the
/// workspace deserves the same one-shot approval as the edits themselves.
const APPROVAL_GATED_TOOLS: &[&str] = &[
    "write_file",
    "rename",
    "delegate",
    "remember",
    "amend_decision",
    "shell",
];

fn is_approval_gated(name: &str) -> bool {
    APPROVAL_GATED_TOOLS.contains(&name)
}

/// Whether a tool call mutates the workspace (used to tell "repeat but state
/// changed" apart from "repeat doing nothing").
pub(crate) fn is_mutating(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// Tools that only gather information (never change state).
const READ_ONLY_TOOLS: &[&str] = &[
    "list_dir",
    "list_files",
    "rgrep",
    "read_file",
    "read_ranges",
    "list_symbols",
    "structural_map",
    "find_symbol",
    "find_definition",
    "read_symbol",
    "references_count",
    "find_references",
    "git_status",
    "git_diff",
    "git_log",
    "project_model",
    "find_decisions",
    "read_decision",
    "web_search",
];

fn is_read_only(name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&name)
}

/// After this many consecutive reads with no state change, we refuse another.
const READ_GUARD_THRESHOLD: usize = 20;

/// Process monitor: if the model keeps reading without doing anything, stop it.
/// Returns `true` when the tool may run (and updates the counter); `false` when
/// the read should be refused as "enough context".
fn allow_read_step(name: &str, consecutive_reads: &mut usize) -> bool {
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

/// Tools that modify source code (used by the verify-then-commit monitor).
const CODE_CHANGES: &[&str] = &[
    "apply_edit",
    "apply_patch",
    "write_file",
    "rename",
    "format_code",
    "shell",
    "delegate",
];

/// Update the "is the current change verified?" state after a tool ran.
fn update_verify_state(name: &str, ok: bool, output: &str, verified_after_change: &mut bool) {
    if CODE_CHANGES.contains(&name) {
        *verified_after_change = false;
    } else if matches!(name, "run_tests" | "run_task") && ok && output.contains("test result: ok.")
    {
        *verified_after_change = true;
    }
}

fn verify_guard_message() -> String {
    "You have unverified code changes. Run run_tests (or run_task test) and get them green BEFORE \
     calling git_commit."
        .to_string()
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
        return "the tests failed - fix the code (or the test) and rerun run_tests".to_string();
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

/// Approval-gated tools advertise `justification` and `risk` as optional native
/// arguments so the model actually passes them (many models omit fields the
/// schema forbids via `additionalProperties: false`). The agent strips them
/// before invoking the tool.
fn augmented_spec(mut spec: comrade_tool::ToolSpec) -> comrade_tool::ToolSpec {
    if !is_approval_gated(&spec.name) {
        return spec;
    }
    let obj = spec.json_schema.as_object_mut();
    if let Some(obj) = obj {
        obj.remove("additionalProperties"); // allow the injected keys
        if let Some(props) = obj
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            props.insert(
                "justification".into(),
                serde_json::json!({
                    "type": "string",
                    "description": "Why this action should run (required for approval)."
                }),
            );
            props.insert(
                "risk".into(),
                serde_json::json!({
                    "type": "string",
                    "description": "What could go wrong, or \"none\" (required for approval)."
                }),
            );
        }
    }
    spec
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
            if let Some(dropped) = dropped {
                if !self.recent.iter().any(|(s, _)| *s == dropped) {
                    self.refusals.remove(&dropped);
                }
            }
        }
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
    let mut tracker = LoopTracker::default();
    // We nudge the model once per run to open with a plan.
    let mut plan_nudged = false;
    // Consecutive read-only calls since the last state change (read guard).
    let mut consecutive_reads = 0usize;
    // Verify-then-commit monitor: false once code changes and true again after
    // a green test run.
    let mut verified_after_change = true;

    loop {
        if stop.is_cancelled() {
            bail!("agent interrupted by user");
        }
        if iterations >= max_iterations {
            bail!("reached max_iterations ({max_iterations}) without a final answer");
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

        // Stale approval notes from a previous turn must not leak into a later
        // confirmation; the current turn sets them again below.
        ctx.clear_approval();

        ctxm.enforce_budget();

        // Advertise native tools unless the protocol is strictly ReAct.
        let native = cfg.llm.protocol.native_enabled();
        let tool_specs: Option<Vec<comrade_tool::ToolSpec>> = if native {
            let specs: Vec<_> = tools
                .iter()
                .map(|t| augmented_spec(t.spec().clone()))
                .collect();
            if specs.is_empty() { None } else { Some(specs) }
        } else {
            None
        };

        // Stream the model's reply: each content chunk is forwarded to the UI as
        // `Delta`; the accumulated turn (text + native tool calls) is returned.
        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let events_tx = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(delta) = delta_rx.recv().await {
                if events_tx.send(AgentEvent::Delta(delta)).await.is_err() {
                    break;
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
        drop(delta_tx);
        // Drain the delta queue so the UI sees every token, not just those the
        // forwarder got to before we moved on.
        let _ = forwarder.await;

        let turn = stream_result?;

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
                let is_plan_tool = matches!(first, "set_plan" | "rename_session" | "ask_question");
                if !is_plan_tool {
                    plan_nudged = true;
                    let msg = "Every task starts with a plan. Call set_plan first with your steps - \
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
                    ctxm.push(ChatMessage::new(
                        Role::User,
                        render_observation(first, &msg),
                    ));
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
                &mut verified_after_change,
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

        let Some(tool) = tools.get(&tool_call.name) else {
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

        // Approval-gated tools (mutations, task runs) MUST be accompanied by a
        // Justification and a Risk line, otherwise the human has nothing to
        // reason with. Ask the model to repeat instead of running them.
        if is_approval_gated(&tool_call.name) && !ctx.auto_approve {
            let has_justification = !turn_p
                .justification
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty();
            let has_risk = !turn_p.risk.as_deref().unwrap_or("").trim().is_empty();
            if !has_justification || !has_risk {
                let missing = [(has_justification, "Justification"), (has_risk, "Risk")]
                    .iter()
                    .filter(|(ok, _)| !ok)
                    .map(|(_, label)| *label)
                    .collect::<Vec<_>>()
                    .join(" and ");
                let msg = format!(
                    "tool `{tool}` is approval-gated and was called without {missing}. \
                     Do NOT call it again without first writing both lines above the Tool line:\n\
                     Justification: <why this action should run>\n\
                     Risk: <what could go wrong, or \"Risk: none\" if safe>\n\
                     Repeat the call with both fields present.",
                    tool = tool_call.name,
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
            }
            // Both fields present: surface them on the approval prompt.
            ctx.set_approval(comrade_tool::ApprovalNotes {
                justification: turn_p.justification.clone().unwrap_or_default(),
                risk: turn_p.risk.clone(),
            });
        }

        // Every task opens with a plan. Nudge once if the model started acting
        // without calling set_plan.
        if !plan_nudged && ctx.session.plan().is_empty() {
            let is_plan_tool = matches!(
                tool_call.name.as_str(),
                "set_plan" | "rename_session" | "ask_question"
            );
            if !is_plan_tool {
                plan_nudged = true;
                let msg = "Every task starts with a plan. Call set_plan first with your steps - each step \
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
                    render_observation(&tool_call.name, &msg),
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

        // Verify-then-commit monitor: refuse commits of unverified changes.
        if tool_call.name == "git_commit" && !verified_after_change {
            let msg = verify_guard_message();
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
                justification: turn_p.justification.clone(),
                risk: turn_p.risk.clone(),
                tokens: turn.usage.as_ref().and_then(usage_total),
            })
            .await;
        let _ = tx
            .send(AgentEvent::ToolStart {
                name: tool_call.name.clone(),
                args: args_pretty,
            })
            .await;

        let output = match tool.invoke(&ctx, tool_call.args.clone()).await {
            Ok(out) => out,
            Err(err) => format!("ERROR: {err:#}"),
        };
        let ok = !output.starts_with("ERROR:");
        let clamped = ctxm.truncate_observation(&output);
        let _ = tx
            .send(AgentEvent::ToolResult {
                name: tool_call.name.clone(),
                output: clamped.clone(),
                ok,
            })
            .await;

        update_verify_state(&tool_call.name, ok, &clamped, &mut verified_after_change);
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

/// Dispatch a turn's native function calls. The assistant message with all
/// `tool_calls` is recorded first; each call then gets a `Role::Tool` result.
/// Approval-gated calls require `justification`/`risk` in their arguments.
async fn run_native_calls(
    ctxm: &mut ContextManager,
    tx: &mpsc::Sender<AgentEvent>,
    tools: &ToolRegistry,
    ctx: &ToolContext,
    tracker: &mut LoopTracker,
    consecutive_reads: &mut usize,
    verified_after_change: &mut bool,
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
        justification: Option<String>,
        risk: Option<String>,
    }

    let mut prepared = Vec::new();
    let mut calls = Vec::new();
    for mc in turn.tool_calls {
        let parsed: serde_json::Value = serde_json::from_str(&mc.arguments)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let text = |k: &str| {
            parsed
                .get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let justification = text("justification");
        let risk = text("risk");
        let mut clean = parsed.clone();
        if let Some(obj) = clean.as_object_mut() {
            obj.remove("justification");
            obj.remove("risk");
        }
        calls.push(crate::llm::ToolCallMsg {
            id: mc.id.clone(),
            name: mc.name.clone(),
            arguments: clean.clone(),
        });
        prepared.push(Prepared {
            id: mc.id,
            name: mc.name,
            args: clean,
            justification,
            risk,
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
        // Verify-then-commit monitor (native).
        if p.name == "git_commit" && !*verified_after_change {
            let msg = verify_guard_message();
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
                justification: p.justification.clone(),
                risk: p.risk.clone(),
                tokens: turn_tokens.take(),
            })
            .await;
        if is_approval_gated(&p.name) && !ctx.auto_approve {
            let has_j = p.justification.is_some();
            let has_r = p.risk.is_some();
            if !has_j || !has_r {
                let missing = [(has_j, "justification"), (has_r, "risk")]
                    .iter()
                    .filter(|(ok, _)| !ok)
                    .map(|(_, l)| *l)
                    .collect::<Vec<_>>()
                    .join(" and ");
                let msg = format!(
                    "tool `{name}` is approval-gated and was called without {missing}. \
                     Repeat the call passing `justification` and `risk` as arguments.",
                    name = p.name,
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
            ctx.set_approval(comrade_tool::ApprovalNotes {
                justification: p.justification.clone().unwrap_or_default(),
                risk: p.risk.clone(),
            });
        }

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

        let Some(tool) = tools.get(&p.name) else {
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
        let output = match tool.invoke(ctx, p.args.clone()).await {
            Ok(out) => out,
            Err(err) => format!("ERROR: {err:#}"),
        };
        let ok = !output.starts_with("ERROR:");
        let clamped = ctxm.truncate_observation(&output);
        let _ = tx
            .send(AgentEvent::ToolResult {
                name: p.name.clone(),
                output: clamped.clone(),
                ok,
            })
            .await;
        update_verify_state(&p.name, ok, &clamped, verified_after_change);
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
        let futures: Vec<Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>> = deferred
            .iter()
            .map(|p| {
                let ctx = ctx.clone();
                let args = p.args.clone();
                let tool = tools.get(&p.name).expect("delegate tool checked above");
                let fut: Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> =
                    Box::pin(async move { tool.invoke(&ctx, args).await });
                fut
            })
            .collect();
        let results = join_all(futures).await;
        for (p, res) in deferred.into_iter().zip(results) {
            let output = match res {
                Ok(out) => out,
                Err(err) => format!("ERROR: {err:#}"),
            };
            let ok = !output.starts_with("ERROR:");
            let clamped = ctxm.truncate_observation(&output);
            let _ = tx
                .send(AgentEvent::ToolResult {
                    name: p.name.clone(),
                    output: clamped.clone(),
                    ok,
                })
                .await;
            update_verify_state(&p.name, ok, &clamped, verified_after_change);
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
    use crate::llm::LlmClient;
    use crate::session::{AgentEvent, AgentSession};
    use crate::undo::MemoryUndo;

    use super::run_agent;

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
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
    /// confirmation like the real write_file would.
    struct RecordingWrite {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl comrade_tool::Tool for RecordingWrite {
        fn spec(&self) -> &comrade_tool::ToolSpec {
            static SPEC: std::sync::LazyLock<comrade_tool::ToolSpec> =
                std::sync::LazyLock::new(|| comrade_tool::ToolSpec {
                    name: "write_file".into(),
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
            ctx.confirm("write_file (test)", None).await?;
            Ok("wrote file".into())
        }
    }

    fn gated_registry(calls: Arc<std::sync::atomic::AtomicUsize>) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(RecordingWrite { calls }));
        tools
    }

    #[tokio::test]
    async fn approval_gated_tool_refused_without_justification_and_risk() {
        let port = spawn_model_with(&[
            "Thought: write it\nTool: write_file\nArgs: {\"path\": \"x.rs\", \"content\": \"a\"}",
            "All done.",
        ]);
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
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
        // tool was never executed (no side effects, no confirm shown)
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        let mut saw_refusal = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await
        {
            match ev {
                AgentEvent::ToolResult { output, ok, .. } => {
                    if !ok && output.contains("approval-gated") {
                        saw_refusal = true;
                    }
                }
                AgentEvent::RunEnd => break,
                _ => {}
            }
        }
        assert!(saw_refusal, "expected an approval-gated refusal");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn approval_gated_tool_runs_when_notes_present() {
        let port = spawn_model_with(&[
            "Thought: write it\nJustification: needed to add the requested file\nRisk: none\nTool: write_file\nArgs: {\"path\": \"y.rs\", \"content\": \"b\"}",
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
        let root = std::env::temp_dir().join(format!("comrade-notes-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let undo = Arc::new(MemoryUndo::new(root.clone()));
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: session.clone().as_control(),
            user: Arc::new(FakeUser),
            undo: undo.clone(),
            auto_approve: false,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Serve one native tool-call request (streamed `tool_calls`), then a final
    /// text answer. When `gated` the tool call targets write_file without
    /// justification/risk.
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
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Writing now.\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\": \\\"x.rs\\\", \\\"content\\\": \\\"a\\\"}\"}}]}}]}\n\n",
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
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
                llm: crate::config::LlmCfg {
                    base_url: delegate_url.clone(),
                    model: "alpha".into(),
                    ..Default::default()
                },
            },
            crate::config::DelegateCfg {
                name: "b".into(),
                description: "delegate b".into(),
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
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
    async fn native_gated_tool_refused_without_justification_args() {
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
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
        // gated native call without justification/risk never reached the tool
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        let mut saw_refusal = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await
        {
            if let AgentEvent::ToolResult { output, ok, .. } = &ev {
                if !ok && output.contains("approval-gated") {
                    saw_refusal = true;
                }
            }
            if let AgentEvent::RunEnd = ev {
                break;
            }
        }
        assert!(saw_refusal, "expected native approval-gated refusal");
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
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
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
            if let AgentEvent::ToolResult { output, ok, .. } = &ev {
                if !ok && output.contains("could not be parsed") {
                    saw_feedback = true;
                }
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
        let sig = "read_file {\"path\":\"a.rs\"}".to_string();
        assert_eq!(t.check(&sig), None);
        t.record("read_file", sig.clone());
        // same call again, nothing changed -> refused (count 1, then 2)
        assert_eq!(t.check(&sig), Some(1));
        assert_eq!(t.check(&sig), Some(2));
        assert_eq!(t.check(&sig), Some(3));
    }

    #[test]
    fn repeat_after_a_mutation_is_allowed() {
        let mut t = LoopTracker::default();
        let sig = "run_task {\"task\":\"test\"}".to_string();
        t.record("run_task", sig.clone());
        assert_eq!(t.check(&sig), Some(1));

        // a mutating call in between bumps the sequence; identical test re-run ok
        t.record("apply_edit", "apply_edit {..}".to_string());
        assert_eq!(t.check(&sig), None);
    }

    #[test]
    fn different_arguments_are_not_a_loop() {
        let mut t = LoopTracker::default();
        t.record("read_file", "read_file a".to_string());
        assert_eq!(t.check(&"read_file b".to_string()), None);
    }
}

#[cfg(test)]
mod loop_tracker_tests {
    use super::*;

    #[test]
    fn refusals_reset_after_different_action() {
        let mut t = LoopTracker::default();
        let sig = "read_file a".to_string();
        t.record("read_file", sig.clone());
        assert_eq!(t.check(&sig), Some(1));
        // model tries something else (still no mutation) -> counter resets
        t.record("rgrep", "rgrep query".to_string());
        assert_eq!(t.check(&sig), Some(1));
        assert_eq!(t.check(&sig), Some(2));
    }

    #[test]
    fn refusals_reset_after_mutation() {
        let mut t = LoopTracker::default();
        let sig = "run_task {\"task\":\"test\"}".to_string();
        t.record("run_task", sig.clone());
        assert_eq!(t.check(&sig), Some(1));
        t.record("apply_edit", "apply_edit {..}".to_string());
        assert_eq!(t.check(&sig), None); // legit re-run after an edit
    }

    #[test]
    fn stuck_ends_gracefully() {
        let mut t = LoopTracker::default();
        assert!(t.stuck_reason().is_none());
        t.mark_stuck("run_tests {..}");
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
                allow_read_step("read_file", &mut reads),
                "reads should be allowed until threshold"
            );
        }
        assert_eq!(reads, READ_GUARD_THRESHOLD);
        // the next read is refused (and does not bump the counter)
        assert!(!allow_read_step("read_file", &mut reads));
        assert_eq!(reads, READ_GUARD_THRESHOLD);
    }

    #[test]
    fn any_state_change_resets_the_counter() {
        let mut reads = 0usize;
        for _ in 0..READ_GUARD_THRESHOLD {
            assert!(allow_read_step("rgrep", &mut reads));
        }
        assert!(!allow_read_step("rgrep", &mut reads));

        // an action (write/edit/plan) resets the read counter
        assert!(allow_read_step("apply_edit", &mut reads));
        assert_eq!(reads, 0);
        assert!(allow_read_step("rgrep", &mut reads));
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
        let c = failure_hint("run_task", "error[E0308]: mismatched types\n --> src/a.rs");
        assert!(c.contains("compile"), "{c}");
        let f = failure_hint(
            "run_tests",
            "test result: FAILED. 1 failed\npanicked at src/lib.rs",
        );
        assert!(f.contains("tests failed"), "{f}");
        let g = failure_hint("shell", "any generic failure here");
        assert!(!g.is_empty());
    }

    #[test]
    fn verify_state_tracks_change_and_green_tests() {
        let mut v = true;
        // a code edit invalidates verification
        update_verify_state("apply_edit", true, "Edited src/a.rs.", &mut v);
        assert!(!v);
        // a red test run does not re-verify
        update_verify_state(
            "run_tests",
            true,
            "test result: FAILED. 0 passed; 1 failed",
            &mut v,
        );
        assert!(!v);
        // a green run does
        update_verify_state("run_tests", true, "test result: ok. 4 passed", &mut v);
        assert!(v);
        // another edit invalidates again
        update_verify_state("write_file", true, "Wrote src/b.rs.", &mut v);
        assert!(!v);
    }

    #[test]
    fn verify_guard_message_is_actionable() {
        let m = verify_guard_message();
        assert!(m.contains("run_tests"));
        assert!(m.contains("git_commit"));
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
