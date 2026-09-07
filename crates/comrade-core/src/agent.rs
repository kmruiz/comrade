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

    let max_iterations = cfg.agent.max_iterations;
    let mut iterations = 0usize;

    loop {
        if stop.is_cancelled() {
            let _ = tx
                .send(AgentEvent::Error("interrupted by user".into()))
                .await;
            let _ = tx.send(AgentEvent::RunEnd).await;
            bail!("agent interrupted by user");
        }
        if iterations >= max_iterations {
            let _ = tx
                .send(AgentEvent::Error(format!(
                    "reached max_iterations ({max_iterations}) without a final answer"
                )))
                .await;
            let _ = tx.send(AgentEvent::RunEnd).await;
            bail!("reached max_iterations ({max_iterations}) without a final answer");
        }
        iterations += 1;

        ctxm.enforce_budget();

        let response = tokio::select! {
            r = client.chat(ctxm.messages()) => r.context("llm call failed")?,
            _ = stop.cancelled() => {
                let _ = tx.send(AgentEvent::Error("interrupted by user".into())).await;
                let _ = tx.send(AgentEvent::RunEnd).await;
                bail!("agent interrupted by user");
            }
        };

        if response.trim().is_empty() {
            let _ = tx
                .send(AgentEvent::Error("model returned an empty response".into()))
                .await;
            let _ = tx.send(AgentEvent::RunEnd).await;
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
            let _ = tx.send(AgentEvent::RunEnd).await;
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
