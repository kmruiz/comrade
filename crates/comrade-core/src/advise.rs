//! The `ask_advise` tool: consult one configured delegate model for ADVICE.
//!
//! The main ("tech lead") model keeps a task on its own plate but wants a
//! second opinion before committing to an approach — how to plan or split a
//! task, which delegate fits a piece of work, whether a design/plan is sound,
//! what to watch out for. Unlike [`crate::delegate::DelegateTool`] nothing is
//! handed off: no plan step is marked working, no fix rounds, no human
//! approval, and the consulted delegate cannot change anything.
//!
//! The advisor gets a READ-ONLY sub-agent loop over the repository (see
//! [`AskAdviseTool::read_only_for_advice`] and the `advise_registry()` builder
//! in comrade-tui): it can list/read/search files, inspect git history, use the
//! project model and the memory/web tools to ground its advice — but it has no
//! write/edit/apply/rename/shell/run/commit/plan tools, so advice can never
//! mutate the workspace or the session. Delegates whose protocol is ReAct-text
//! still run through the same loop as `delegate`; the reply is the advisor's
//! final text, which the lead reads and decides on.

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolRegistry, ToolSpec};
use serde_json::{Value, json};

use crate::config::DelegateCfg;
use crate::delegate::{
    DelegateLimits, Target, build_targets, delegate_line, render_subagent_system,
    run_delegate_subagent,
};

/// Name of the tool advertised to the tech lead model.
pub const TOOL_NAME: &str = "ask_advise";

/// Wording of the read-guard nudge for an advisor: unlike a working `delegate`
/// it must not implement — it should stop browsing and give its advice.
const ADVICE_READ_NUDGE: &str = "You have performed {count} read-only calls in a row without \
     answering. You have enough context - give your advice now as your final answer (what to do, \
     in what order, what to avoid). Do not keep reading.";

/// A tool that asks one of the configured delegate models for advice.
pub struct AskAdviseTool {
    spec: ToolSpec,
    targets: Vec<Target>,
    /// Read-only repository tools the advisor may browse.
    tools: ToolRegistry,
    /// Iteration/token caps for the advisor's read-only sub-agent run.
    limits: DelegateLimits,
}

impl AskAdviseTool {
    /// Whether `name` is a read-only repository tool an advisor may browse.
    /// Advisors get exactly the tools the main agent loop classifies as
    /// read-only (agent.rs), so the advice can never change state. Kept as a
    /// method so comrade-tui's registry builder and this tool share one
    /// source of truth.
    pub fn read_only_for_advice(name: &str) -> bool {
        crate::agent::is_read_only(name)
    }

    /// Build the advise tool from the configured `[[delegates]]` entries.
    /// Returns `Ok(None)` when no delegates are configured (the tool is then
    /// not advertised at all).
    ///
    /// `tools` is the registry the advisor may call: pass a view of the main
    /// registry filtered to [`AskAdviseTool::read_only_for_advice`] tools so
    /// the advice can be grounded in the code without any side effects.
    pub fn new(
        delegates: &[DelegateCfg],
        tools: ToolRegistry,
        limits: DelegateLimits,
    ) -> Result<Option<Self>> {
        if delegates.is_empty() {
            return Ok(None);
        }
        let targets = build_targets(delegates)?;
        let names: Vec<String> = targets.iter().map(|t| t.cfg.name.clone()).collect();
        let listing = delegates
            .iter()
            .map(|d| delegate_line(&d.name, &d.description))
            .collect::<Vec<_>>()
            .join("\n");
        let description = format!(
            "\
Ask one of the configured delegate models for ADVICE — a second opinion — while you keep the \
task yourself. Unlike `delegate`, nothing is handed off: no plan step is marked working, nothing \
runs on the repo, and the delegate only answers you (e.g. how to plan or split a task, which \
approach is sound, what could go wrong, a review of your plan or design).

The consulted delegate gets READ-ONLY repository tools (read/search files, git status/diff/log, \
project_model, memory lookups, web_search) so the advice can be grounded in the actual code, but \
it has no write/edit/shell/run/commit/plan tools and cannot change anything. The call is NOT \
approval-gated and does not touch the plan; it only costs one extra model conversation.

Ask with `model` + a self-contained `question` (+ optional `context` for anything the delegate \
cannot discover itself, e.g. your plan draft or design notes). You get advice back — you still \
decide and do the work.

Configured delegates — pick the one whose description best fits the advice you need:
{listing}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": format!(
                        "Which configured delegate model should give the advice. Choose the delegate whose description best fits the question:\n{listing}"
                    )
                },
                "question": {
                    "type": "string",
                    "description": "The thing you want advice on, self-contained: what you are about to do, the situation, and what you need judged (e.g. how to plan/split a task, whether an approach is sound)."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background the delegate cannot discover itself: your plan draft, design notes, error output, constraints — anything that would let it advise without re-reading the repo."
                }
            },
            "required": ["model", "question"],
            "additionalProperties": false
        });

        Ok(Some(Self {
            spec: ToolSpec {
                name: TOOL_NAME.into(),
                description,
                json_schema: schema,
            },
            targets,
            tools,
            limits,
        }))
    }
}

#[async_trait]
impl Tool for AskAdviseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if question.is_empty() {
            bail!("`question` must not be empty: tell the delegate what you want advice on");
        }
        let context = args
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        let Some(target) = self.targets.iter().find(|t| t.cfg.name == model) else {
            let listed = self
                .targets
                .iter()
                .map(|t| delegate_line(&t.cfg.name, &t.cfg.description))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("unknown delegate model {model:?}. Configured delegates:\n{listed}");
        };

        let user_prompt = if context.is_empty() {
            format!("Question:\n{question}")
        } else {
            format!("Context:\n{context}\n\nQuestion:\n{question}")
        };
        let display = target.cfg.llm.display();
        let native = target.cfg.llm.protocol.native_enabled();
        let system = render_subagent_system(
            include_str!("../prompts/advise-system.md"),
            &ctx.project_root.to_string_lossy(),
            &self.tools,
            native,
        );
        let reply = run_delegate_subagent(
            &target.client,
            &self.tools,
            ctx,
            system,
            user_prompt,
            &target.cfg.name,
            native,
            &self.limits,
            ADVICE_READ_NUDGE,
        )
        .await
        .with_context(|| format!("delegate {model} ({display}) failed to give advice"))?;

        Ok(format!("advice from {model} ({display}):\n{reply}"))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use comrade_tool::ToolContext;
    use comrade_tool::tool::{UserIo, UserPrompt, UserReply};

    use super::*;
    use crate::MemoryUndo;
    use crate::config::{DelegateCfg, LlmCfg};
    use crate::session::AgentSession;

    /// The advise tool never touches the session/user (nothing is handed off),
    /// so a no-op IO double is all the tests need.
    struct NoopIo;
    #[async_trait]
    impl UserIo for NoopIo {
        async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
            Ok(UserReply::Answer(String::new()))
        }
    }

    fn test_ctx() -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            project_root: "/tmp/x".into(),
            cwd: "/tmp/x".into(),
            session: Arc::new(AgentSession::new(tx)).as_control(),
            user: Arc::new(NoopIo),
            undo: Arc::new(MemoryUndo::new("/tmp/x".into())),
            auto_approve: true,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            stop: None,
        }
    }

    /// Build an advise tool with an empty registry (no repository tools) so
    /// tests exercise the sub-agent loop without touching the real filesystem.
    fn mk_advise(delegates: &[DelegateCfg]) -> Result<Option<AskAdviseTool>> {
        AskAdviseTool::new(delegates, ToolRegistry::new(), DelegateLimits::default())
    }

    /// Spawn a fake OpenAI-compatible `/v1/chat/completions` server that
    /// answers every request with `content`. Returns its base URL.
    fn fake_chat_server(content: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let mut used = 0usize;
            loop {
                let n = stream.read(&mut buf[used..]).unwrap();
                if n == 0 {
                    break;
                }
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = format!(
                "{{\"choices\":[{{\"message\":{{\"content\":\"{}\"}}}}]}}",
                content.replace('"', "\\\"")
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// Like [`fake_chat_server`], but also ships the raw request body to a
    /// channel so tests can assert what the advisor was actually asked.
    fn request_spy() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut tmp = [0u8; 2048];
            let mut body_len: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            while body_len.map_or(true, |len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(p);
                        let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                        body_len = head.lines().find_map(|l| {
                            l.trim()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        });
                    }
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
            let resp_body = "{\"choices\":[{\"message\":{\"content\":\"ok\"}}]}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        (format!("http://127.0.0.1:{port}/v1"), rx)
    }

    fn delegate(name: &str, base_url: &str) -> DelegateCfg {
        DelegateCfg {
            name: name.into(),
            description: format!("{name} test delegate"),
            llm: LlmCfg {
                base_url: base_url.into(),
                model: "delegate-model".into(),
                ..LlmCfg::default()
            },
        }
    }

    #[test]
    fn empty_delegate_list_yields_no_tool() {
        let tool = mk_advise(&[]).unwrap();
        assert!(tool.is_none());
    }

    #[test]
    fn spec_advertises_model_enum_and_requires_question() {
        let tool = mk_advise(&[
            delegate("planner", "http://127.0.0.1:1/v1"),
            delegate("reviewer", "http://127.0.0.1:1/v1"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(tool.spec().name, "ask_advise");
        let schema = &tool.spec().json_schema;
        assert_eq!(schema["required"], serde_json::json!(["model", "question"]));
        let models = schema["properties"]["model"]["enum"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(models, vec!["planner", "reviewer"]);
    }

    #[test]
    fn read_only_for_advice_allows_only_read_only_tools() {
        // Exactly the main loop's read-only classification: advisors may browse
        // but must never be able to change the workspace or session.
        for name in [
            "read_file",
            "read_ranges",
            "rgrep",
            "list_dir",
            "git_diff",
            "git_log",
            "project_model",
            "find_decisions",
            "web_search",
        ] {
            assert!(
                AskAdviseTool::read_only_for_advice(name),
                "{name} should be browsable"
            );
        }
        for name in [
            "write_file",
            "apply_edit",
            "apply_patch",
            "rename",
            "shell",
            "run_task",
            "run_tests",
            "format_code",
            "git_commit",
            "remember",
            "amend_decision",
            "ask_advise",
            "delegate",
            "set_plan",
        ] {
            assert!(
                !AskAdviseTool::read_only_for_advice(name),
                "{name} must not be browsable"
            );
        }
    }

    #[tokio::test]
    async fn invoke_returns_advice_from_the_chosen_delegate() {
        let base = fake_chat_server("split the task into three small steps");
        let tool = mk_advise(&[
            delegate("cheap", &base),
            delegate("other", "http://127.0.0.1:1/v1"),
        ])
        .unwrap()
        .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(
                &ctx,
                json!({
                    "model": "cheap",
                    "question": "How should I plan this task?",
                    "context": "A Rust refactor touching two crates."
                }),
            )
            .await
            .unwrap();
        assert!(out.contains("advice from cheap"), "{out}");
        assert!(
            out.contains("split the task into three small steps"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn the_question_and_context_reach_the_advisor_verbatim() {
        let (base, spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = test_ctx();
        tool.invoke(
            &ctx,
            json!({
                "model": "cheap",
                "question": "How should I plan this task?",
                "context": "Background the advisor cannot discover."
            }),
        )
        .await
        .unwrap();
        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(body.contains("How should I plan this task?"), "{body}");
        assert!(
            body.contains("Background the advisor cannot discover."),
            "{body}"
        );
        // The advisor system prompt frames it as advice, not delegated work.
        assert!(body.contains("ADVICE"), "{body}");
    }

    #[tokio::test]
    async fn unknown_model_and_empty_question_are_rejected() {
        let base = fake_chat_server("ignored");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = test_ctx();
        let err = tool
            .invoke(&ctx, json!({"model": "nope", "question": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown delegate model"));
        assert!(
            err.to_string().contains("cheap: cheap test delegate"),
            "{err}"
        );

        let err = tool
            .invoke(&ctx, json!({"model": "cheap", "question": "   "}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("question"));
    }
}
