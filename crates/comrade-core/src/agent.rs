use anyhow::{Context as _, Result, bail};
use comrade_tool::{ToolContext, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::context::ContextManager;
use crate::llm::{ChatMessage, LlmClient, Role};
use crate::react::{build_system_prompt, parse_turn, render_observation};
use crate::session::AgentEvent;

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

    let mut ctxm = ContextManager::with_system(
        build_system_prompt(
            ctx.project_root.to_string_lossy().as_ref(),
            tools,
            cfg.context.budget_tokens,
        ),
        cfg.context.budget_tokens,
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

        // Stream the model's reply: each content chunk is forwarded to the UI as
        // `Delta`, while the accumulated text is returned for parsing/history.
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
            r = client.chat_stream(ctxm.messages(), {
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

        let response = stream_result?;
        if response.trim().is_empty() {
            bail!("model returned an empty response");
        }

        ctxm.push(ChatMessage::new(Role::Assistant, response.clone()));
        let _ = tx.send(AgentEvent::AssistantText(response.clone())).await;

        let turn = parse_turn(&response).context("failed to parse model response")?;
        if let Some(t) = turn.thought {
            let _ = tx.send(AgentEvent::Thought(t)).await;
        }

        let Some(tool_call) = turn.tool_call else {
            let answer = turn.final_text.clone();
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

        // Hand the model's justification/risk to whatever confirmation the tool
        // asks for (mutating tools and run_task).
        let has_note = !turn
            .justification
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
            || turn
                .risk
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty() && !r.trim().eq_ignore_ascii_case("none"));
        if has_note {
            ctx.set_approval(comrade_tool::ApprovalNotes {
                justification: turn.justification.clone().unwrap_or_default(),
                risk: turn.risk.clone(),
            });
        }

        let args_pretty = serde_json::to_string(&tool_call.args).unwrap_or_default();
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
    }
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

    /// Scripted model: 1st call streams a ReAct turn that invokes an unknown
    /// tool (exercises the error/observation path), 2nd call streams a final
    /// answer.
    fn spawn_fake_model() -> u16 {
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
                    "Thought: try a tool\nTool: no_such_tool\nArgs: {\"x\": 1}"
                } else {
                    "All done."
                };
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
}
