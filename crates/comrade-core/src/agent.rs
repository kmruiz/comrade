use anyhow::{Context as _, Result, bail};
use comrade_tool::{ToolContext, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::context::ContextManager;
use crate::llm::{ChatMessage, LlmClient, Role};
use crate::react::{build_system_prompt, parse_turn, render_observation};
use crate::session::AgentEvent;

/// Tools whose side effects require human approval (and thus mandatory
/// Justification/Risk). Keep in sync with the tool crates.
const APPROVAL_GATED_TOOLS: &[&str] = &[
    "apply_edit",
    "write_file",
    "rename",
    "git_commit",
    "run_task",
    "remember",
    "amend_decision",
    "apply_patch",
    "format_code",
    "run_tests",
    "shell",
];

fn is_approval_gated(name: &str) -> bool {
    APPROVAL_GATED_TOOLS.contains(&name)
}

/// Whether a tool call mutates the workspace (used to tell "repeat but state
/// changed" apart from "repeat doing nothing").
fn is_mutating(name: &str) -> bool {
    APPROVAL_GATED_TOOLS.contains(&name)
}

const LOOP_WINDOW: usize = 8;
const MAX_LOOP_REFUSALS: usize = 3;

/// Detects no-progress loops: the same exact tool call repeated while nothing
/// changed in between. Each detected repeat is refused; after several refusals
/// the run aborts instead of burning the whole budget.
#[derive(Default)]
struct LoopTracker {
    /// (canonical call signature, mutation counter at the time it ran).
    recent: std::collections::VecDeque<(String, u64)>,
    /// How many mutating calls have executed; identical calls on either side of
    /// a mutation are not considered a loop.
    mutation_seq: u64,
    /// Consecutive refusals per signature.
    refusals: std::collections::HashMap<String, usize>,
}

impl LoopTracker {
    /// Returns the refusal count when this exact call was already made with no
    /// state change since (i.e. a no-progress repeat), else `None`.
    fn check(&mut self, sig: &str) -> Option<usize> {
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

    /// Note a tool call that actually ran (mutation or read).
    fn record(&mut self, name: &str, sig: String) {
        if is_mutating(name) {
            self.mutation_seq += 1;
        }
        self.recent.push_back((sig, self.mutation_seq));
        if self.recent.len() > LOOP_WINDOW {
            self.recent.pop_front();
        }
    }
}

/// Message fed back when a tool call is refused as a no-progress repeat.
fn loop_refusal(tool: &str) -> String {
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
    let _ = tx.send(AgentEvent::RunStart).await;

    if ctx.session.title().is_empty() || ctx.session.title() == "New session" {
        let mut t = user_input.trim().to_string();
        if t.chars().count() > 60 {
            t = t.chars().take(60).collect::<String>() + "…";
        }
        ctx.session.set_title(&t);
    }
    let _ = tx.send(AgentEvent::User(user_input.clone())).await;

    let budget = cfg.effective_budget();
    let mut ctxm = ContextManager::with_system(
        build_system_prompt(ctx.project_root.to_string_lossy().as_ref(), tools, budget),
        budget,
        cfg.context.max_tool_output_chars,
    );
    ctxm.push(ChatMessage::new(Role::User, user_input));

    let result = run_agent_loop(cfg, client, tools, ctx, ctxm, tx.clone(), &stop).await;

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
    mut ctxm: ContextManager,
    tx: mpsc::Sender<AgentEvent>,
    stop: &CancellationToken,
) -> Result<AgentOutcome> {
    let max_iterations = cfg.agent.max_iterations;
    let mut iterations = 0usize;
    let mut tracker = LoopTracker::default();

    loop {
        if stop.is_cancelled() {
            bail!("agent interrupted by user");
        }
        if iterations >= max_iterations {
            bail!("reached max_iterations ({max_iterations}) without a final answer");
        }
        iterations += 1;

        // Stale approval notes from a previous turn must not leak into a later
        // confirmation; the current turn sets them again below.
        ctx.clear_approval();

        ctxm.enforce_budget();

        // Advertise native tools unless the protocol is strictly ReAct.
        let native = cfg.llm.protocol.native_enabled();
        let tool_specs: Option<Vec<comrade_tool::ToolSpec>> = if native {
            let specs: Vec<_> = tools.iter().map(|t| t.spec().clone()).collect();
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
            run_native_calls(&mut ctxm, &tx, tools, &ctx, &mut tracker, turn).await?;
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

        let args_pretty = serde_json::to_string(&tool_call.args).unwrap_or_default();
        let sig = format!("{} {args_pretty}", tool_call.name);
        if let Some(count) = tracker.check(&sig) {
            if count >= MAX_LOOP_REFUSALS {
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
                bail!(
                    "detected a loop: agent repeated `{}` {count}x without making progress",
                    tool_call.name
                );
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

        ctxm.push(ChatMessage::new(
            Role::User,
            render_observation(&tool_call.name, &clamped),
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

    let total = prepared.len();
    for p in prepared {
        let args_pretty = serde_json::to_string(&p.args).unwrap_or_default();
        let sig = format!("{} {args_pretty}", p.name);
        let _ = tx
            .send(AgentEvent::ToolCall {
                name: p.name.clone(),
                args: args_pretty.clone(),
                justification: p.justification.clone(),
                risk: p.risk.clone(),
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
                justification: p.justification.unwrap_or_default(),
                risk: p.risk,
            });
        }

        if let Some(count) = tracker.check(&sig) {
            if count >= MAX_LOOP_REFUSALS {
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
                bail!(
                    "detected a loop: agent repeated `{}` {count}x without making progress",
                    p.name
                );
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
        ctxm.push(ChatMessage::tool_result(p.id, clamped));
        tracker.record(&p.name, sig);
    }
    ctxm.note_turn_done(total);
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

    #[tokio::test]
    async fn native_gated_tool_refused_without_justification_args() {
        let port = spawn_native_model();
        let mut cfg = Config::default();
        cfg.llm.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.llm.model = "fake".into();

        let (tx, mut events) = mpsc::channel(64);
        let session = Arc::new(AgentSession::new(tx.clone()));
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
