//! The `ask_advise` tool: consult one configured delegate model for ADVICE.
//!
//! The main ("tech lead") model keeps a task on its own plate but wants a
//! second opinion before committing to an approach — how to plan or split a
//! task, which delegate fits a piece of work, whether a design/plan is sound,
//! what to watch out for.
//!
//! Besides free-form advice, the tool doubles as the *readiness handshake* for
//! delegated plan steps: pass `step` = a plan step id (instead of `model` +
//! `question`) and the step's OWN delegate is consulted about whether the
//! step's context (goal/verification/context) is enough for it to pick the step
//! up. When the delegate's reply says the context suffices (a final
//! `VERDICT: READY` line) the step is marked [`PlanStatus::Ready`] — ready to
//! be delegated; when the delegate reports it needs more, the step stays
//! `Pending` with an "awaiting context: ..." note and the lead can feed the
//! request back via `set_step_context` and re-ask. This lets the lead run the
//! readiness checks in parallel while doing other work.
//!
//! Unlike [`crate::delegate::DelegateTool`] nothing is handed off: no step is
//! marked working, no fix rounds, and the consulted delegate cannot change the
//! repository. Consultations normally need no approval, but a delegate
//! configured `approval = "ask"` pauses for human approval before the advice
//! runs (and one set to "deny" is refused outright) — same policy as
//! [`crate::delegate`].
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
use comrade_tool::{
    AGENT_MODEL, PlanStatus, PlanTarget, Tool, ToolContext, ToolRegistry, ToolSpec,
};
use serde_json::{Value, json};

use crate::config::DelegateCfg;
use crate::delegate::{
    DelegateLimits, Target, approval_preview, build_targets, cfg_line, enforce_approval,
    render_subagent_system, run_delegate_subagent,
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
            .map(cfg_line)
            .collect::<Vec<_>>()
            .join("\n");
        let body = [
            "Ask one of the configured delegate models for ADVICE - a second opinion - while you",
            "keep the task yourself. Two modes:",
            "",
            "1. Free-form advice: pass `model` + a self-contained `question` (+ optional `context`).",
            "Nothing is handed off and nothing runs; the advisor only answers. You get advice back -",
            "you still decide.",
            "",
            "2. Readiness check for a delegated plan step: pass `step` = a plan step id (do NOT",
            "pass `model`, `question` or `context`). The step's OWN delegate is consulted about",
            "whether the step's context suffices for it to pick the step up. VERDICT: READY marks",
            "the step `ready` to delegate; NEEDS_MORE keeps it `pending` with an \"awaiting",
            "context: ...\" note. Enrich with set_step_context, then re-ask until `ready`. Fire",
            "these checks in PARALLEL (one ask_advise step = <id> per delegated step, batched).",
            "",
            "The advisor is READ-ONLY: it can read/search files and git history and use memory/web",
            "tools, but has no write/edit/shell/run/commit/plan tools and cannot change anything.",
            "Consulting normally needs no approval; a delegate configured `approval = \"ask\"`",
            "pauses for human approval first, and `approval = \"deny\"` is refused.",
        ]
        .join("\n");
        let description = format!(
            "{body}\n\nConfigured delegates — pick the one whose description best fits the advice you need:\n{listing}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "step": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Plan step id for a readiness check: the step's own delegate is consulted about whether its context suffices. VERDICT: READY marks it `ready`; NEEDS_MORE keeps it `pending`. Do not pass `model`, `question` or `context` with `step`."
                },
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": "Which configured delegate model should give the advice."
                },
                "question": {
                    "type": "string",
                    "description": "The advice you want, self-contained: what you are about to do, what you need judged (e.g. how to plan/split a task, whether an approach is sound)."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background the advisor cannot discover itself: plan draft, design notes, error output, constraints."
                }
            },
            "oneOf": [
                { "required": ["step"] },
                { "required": ["model", "question"] }
            ],
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
        let step_id = args.get("step").and_then(Value::as_u64);
        let model_arg = args
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
        let context = args
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        // Two mutually exclusive modes: a readiness check on a plan step, or a
        // free-form advice consult.
        let (model, user_prompt, approval_title) = match step_id {
            Some(id) => {
                for (key, label) in [("question", "question"), ("context", "context")] {
                    let present = args
                        .get(key)
                        .map(|v| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(true))
                        .unwrap_or(false);
                    if present {
                        bail!(
                            "cannot pass `{label}` together with `step`: the {label} comes from \
                             the plan step"
                        );
                    }
                }
                let found = ctx
                    .session
                    .plan()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow::anyhow!("no plan step with id {id}"))?;
                if !model_arg.is_empty() && model_arg != found.model {
                    bail!(
                        "`model` {model_arg:?} does not match the delegate assigned to plan step \
                         {id} ({:?}) — a step's readiness is checked with its own delegate",
                        found.model
                    );
                }
                if found.model.trim().is_empty() {
                    bail!(
                        "plan step {id} has no delegate model assigned; it runs on the main model"
                    );
                }
                if found.model.trim() == AGENT_MODEL {
                    bail!(
                        "plan step {id} is assigned to the main agent model ({AGENT_MODEL:?}), not \
                         a delegate — there is no delegate to confirm its readiness"
                    );
                }
                if matches!(found.status, PlanStatus::InProgress | PlanStatus::Done) {
                    bail!(
                        "plan step {id} is {} — readiness is checked while the step is pending, \
                         ready or blocked, before a delegate picks it up",
                        found.status
                    );
                }
                let goal = found.goal.trim();
                let verify = found.verification.trim();
                let step_ctx = found.context.trim();
                let prompt = format!(
                    "Context readiness check for plan step {id} (delegate {}):\n\
                     Goal: {goal}\n\
                     Verification: {verify}\n\
                     Context: {}\n\n\
                     You are the delegate that will execute this step. Working directory: {} — \
                     browse the repository read-only if you need more to judge. Tell the tech lead \
                     whether the context above is ENOUGH for you to accomplish the goal, or \
                     exactly what is missing.\n\n\
                     Reply with your verdict as the FINAL line, exactly one of:\n\
                     VERDICT: READY\n\
                     VERDICT: NEEDS_MORE: <exactly what extra context you need>",
                    found.model,
                    if step_ctx.is_empty() {
                        "(none)"
                    } else {
                        step_ctx
                    },
                    ctx.project_root.display(),
                );
                let model = found.model.clone();
                (
                    model.clone(),
                    prompt,
                    format!("Ask delegate {model} about readiness of plan step {id}?"),
                )
            }
            None => {
                if question.is_empty() {
                    bail!(
                        "`question` must not be empty: tell the delegate what you want advice on"
                    );
                }
                let model = model_arg.clone();
                let prompt = if context.is_empty() {
                    format!("Question:\n{question}")
                } else {
                    format!("Context:\n{context}\n\nQuestion:\n{question}")
                };
                (
                    model,
                    prompt,
                    format!("Ask delegate {model_arg} for advice?"),
                )
            }
        };

        let Some(target) = self.targets.iter().find(|t| t.cfg.name == model) else {
            let listed = self
                .targets
                .iter()
                .map(|t| cfg_line(&t.cfg))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("unknown delegate model {model:?}. Configured delegates:\n{listed}");
        };

        // Per-delegate approval policy, same as `delegate`: "ask" pauses for
        // the human before the advice runs, "deny" refuses outright.
        enforce_approval(
            &target.cfg,
            ctx,
            approval_title,
            Some(approval_preview(&user_prompt, "Question:")),
        )
        .await?;

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

        // A readiness check resolves the delegate's reply into a plan status:
        // an explicit final VERDICT: READY marks the step ready to pick up;
        // anything else leaves it pending with an "awaiting context" note.
        if let Some(id) = step_id {
            match readiness_verdict(&reply) {
                Readiness::Ready => {
                    ctx.session.update_plan(
                        PlanTarget::Id(id),
                        PlanStatus::Ready,
                        Some(format!("ready: {model} confirmed the context")),
                    );
                    Ok(format!(
                        "delegate {model} ({display}) confirmed plan step {id} is READY to pick \
                         up — the context suffices.\n\n{reply}"
                    ))
                }
                Readiness::NeedsMore(request) => {
                    ctx.session.update_plan(
                        PlanTarget::Id(id),
                        PlanStatus::Pending,
                        Some(format!("awaiting context: {request}")),
                    );
                    Ok(format!(
                        "delegate {model} ({display}) needs more context for plan step {id}:\n\
                         {request}\n\nAdd it with `set_step_context` (index = {id}), then re-run \
                         ask_advise step = {id} until the step is `ready`.\n\n{reply}"
                    ))
                }
            }
        } else {
            Ok(format!("advice from {model} ({display}):\n{reply}"))
        }
    }
}

/// The delegate's answer to a context-readiness check for a plan step.
enum Readiness {
    Ready,
    NeedsMore(String),
}

/// Extract the verdict from an advisor's reply. The readiness prompt asks the
/// delegate to close with exactly one final `VERDICT:` line; the last one wins.
/// A reply without any `VERDICT:` line is treated as "needs more" (a step is
/// never marked ready without an explicit READY verdict).
fn readiness_verdict(reply: &str) -> Readiness {
    let mut verdict: Option<Readiness> = None;
    for line in reply.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("VERDICT:")
            .or_else(|| line.strip_prefix("verdict:"))
        else {
            continue;
        };
        let rest = rest.trim();
        let value = rest
            .split_once(':')
            .map(|(k, v)| (k.trim(), Some(v.trim())))
            .or_else(|| Some((rest, None)));
        let Some((kind, extra)) = value else {
            continue;
        };
        verdict = Some(if kind.eq_ignore_ascii_case("READY") {
            Readiness::Ready
        } else if kind.eq_ignore_ascii_case("NEEDS_MORE") {
            Readiness::NeedsMore(extra.filter(|e| !e.is_empty()).unwrap_or(rest).to_string())
        } else {
            // Unknown verdict keyword: be conservative.
            Readiness::NeedsMore(line.to_string())
        });
    }
    verdict.unwrap_or_else(|| {
        Readiness::NeedsMore(
            reply
                .lines()
                .last()
                .unwrap_or("(the delegate gave no verdict)")
                .trim()
                .to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use comrade_tool::{
        PlanStepDraft, ToolContext,
        tool::{UserIo, UserPrompt, UserReply},
    };

    use super::*;
    use crate::MemoryUndo;
    use crate::config::{Autonomy, DelegateCfg, LlmCfg};
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

    /// A context whose confirmations actually reach the UserIo (auto_approve
    /// false), for exercising the per-delegate approval gate.
    fn ask_ctx(user: Arc<dyn UserIo>) -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            project_root: "/tmp/x".into(),
            cwd: "/tmp/x".into(),
            session: Arc::new(AgentSession::new(tx)).as_control(),
            user,
            undo: Arc::new(MemoryUndo::new("/tmp/x".into())),
            auto_approve: false,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            stop: None,
        }
    }

    /// Records every confirm prompt's title and answers each one with `answer`
    /// ("yes" approves; anything else denies).
    struct RecordingIo {
        titles: Arc<Mutex<Vec<String>>>,
        answer: String,
    }
    #[async_trait]
    impl UserIo for RecordingIo {
        async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
            if let UserPrompt::Confirm { title, .. } = prompt {
                self.titles.lock().unwrap().push(title);
            }
            Ok(UserReply::Answer(self.answer.clone()))
        }
    }

    /// Fails the test if the context ever asks the human: used to prove that
    /// auto/ungated delegates run without an approval prompt.
    struct MustNotAsk;
    #[async_trait]
    impl UserIo for MustNotAsk {
        async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
            panic!("auto/ungated delegate must not prompt for approval");
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
                content
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
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
            approval: Autonomy::Auto,
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
        // Either a free-form consult (model + question) or a readiness check on
        // a plan step (step alone).
        assert_eq!(
            schema["oneOf"],
            serde_json::json!([
                { "required": ["step"] },
                { "required": ["model", "question"] }
            ])
        );
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
            "fs_read_file",
            "fs_read_ranges",
            "fs_rgrep",
            "fs_list_dir",
            "git_diff",
            "git_log",
            "pom_model",
            "find_adr",
            "web_search",
        ] {
            assert!(
                AskAdviseTool::read_only_for_advice(name),
                "{name} should be browsable"
            );
        }
        for name in [
            "fs_write_file",
            "fs_edit",
            "ts_rename",
            "shell",
            "pom_run_task",
            "pom_run_tests",
            "pom_format_code",
            "git_commit",
            "record_adr",
            "amend_adr",
            "ask_advise",
            "delegate",
            "self_set_plan",
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

    #[tokio::test]
    async fn ask_gated_advisor_pauses_for_approval_then_advises() {
        let base = fake_chat_server("split the task into three small steps");
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_advise(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "yes".into(),
        }));

        let out = tool
            .invoke(
                &ctx,
                json!({"model": "expensive", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap();
        let held = titles.lock().unwrap();
        assert_eq!(held.len(), 1, "exactly one approval prompt expected");
        assert!(
            held[0].contains("Ask delegate expensive"),
            "confirm title should name the delegate: {:?}",
            held[0]
        );
        drop(held);
        assert!(out.contains("advice from expensive"), "{out}");
    }

    #[tokio::test]
    async fn denied_approval_aborts_the_consultation() {
        let base = fake_chat_server("ignored");
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_advise(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "".into(), // not affirmative -> denied
        }));

        let err = tool
            .invoke(
                &ctx,
                json!({"model": "expensive", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("user denied"), "{err}");
        assert_eq!(titles.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deny_gated_advisor_is_refused_without_running() {
        let mut guarded = delegate("guarded", "http://127.0.0.1:1/v1");
        guarded.approval = Autonomy::Deny;
        let tool = mk_advise(&[guarded]).unwrap().unwrap();
        let ctx = test_ctx();

        let err = tool
            .invoke(
                &ctx,
                json!({"model": "guarded", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("approval = \"deny\"") && err.to_string().contains("guarded"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn auto_advisor_runs_without_any_prompt() {
        let base = fake_chat_server("your plan is fine");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = ask_ctx(Arc::new(MustNotAsk));
        let out = tool
            .invoke(
                &ctx,
                json!({"model": "cheap", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap();
        assert!(out.contains("advice from cheap"), "{out}");
    }

    /// A context whose session already holds one delegate-assigned plan step
    /// (id 1) so readiness checks have something to consult about.
    fn step_ctx() -> ToolContext {
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "refactor the helper fn".into(),
            verification: "cargo test passes".into(),
            model: "cheap".into(),
            context: "old signature lives in crates/x/src/lib.rs".into(),
        }]);
        ctx
    }

    #[tokio::test]
    async fn step_mode_marks_step_ready_when_delegate_confirms() {
        let base = fake_chat_server("That is enough for me.\nVERDICT: READY");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        assert!(out.contains("READY to pick up"), "{out}");
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Ready);
        assert_eq!(
            step.note.as_deref(),
            Some("ready: cheap confirmed the context")
        );
    }

    #[tokio::test]
    async fn step_mode_stays_pending_and_reports_request_when_delegate_needs_more() {
        let base = fake_chat_server(
            "I need more detail.\nVERDICT: NEEDS_MORE: the exact fn signature of the helper",
        );
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        assert!(out.contains("needs more context for plan step 1"), "{out}");
        assert!(
            out.contains("the exact fn signature of the helper"),
            "{out}"
        );
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Pending);
        assert_eq!(
            step.note.as_deref(),
            Some("awaiting context: the exact fn signature of the helper")
        );
    }

    #[tokio::test]
    async fn step_mode_readiness_prompt_reaches_the_delegate() {
        let (base, spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            body.contains("Context readiness check for plan step 1"),
            "{body}"
        );
        assert!(body.contains("refactor the helper fn"), "{body}");
        assert!(
            body.contains("old signature lives in crates/x/src/lib.rs"),
            "{body}"
        );
        // The advisor must close with an explicit verdict line.
        assert!(body.contains("VERDICT: NEEDS_MORE"), "{body}");
    }

    #[tokio::test]
    async fn step_mode_without_a_verdict_is_never_marked_ready() {
        // request_spy's canned answer ("ok") carries no VERDICT: line; the step
        // must stay pending rather than being optimistically confirmed.
        let (base, _spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Pending);
        let note = step.note.as_deref().unwrap_or_default();
        assert!(note.contains("awaiting context"), "note was {note:?}");
    }

    #[tokio::test]
    async fn step_mode_rejects_mixed_and_mismatched_args() {
        let base = fake_chat_server("ignored");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();

        // `step` together with `question` or `context` is ambiguous: they come
        // from the plan step.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "question": "is it enough?"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot pass `question`"), "{err}");

        // The consulted delegate must be the step's own assigned delegate.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "model": "other"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match the delegate assigned"),
            "{err}"
        );

        // Unknown step id.
        let err = tool.invoke(&ctx, json!({"step": 42})).await.unwrap_err();
        assert!(err.to_string().contains("no plan step with id 42"), "{err}");
    }

    #[tokio::test]
    async fn step_mode_refuses_steps_without_a_delegate_or_already_working() {
        let base = fake_chat_server("ignored");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();

        // A step assigned to the main model has no delegate to consult.
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "my own work".into(),
            verification: String::new(),
            model: AGENT_MODEL.into(),
            context: String::new(),
        }]);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("not a delegate"), "{err}");

        // An in-progress step is already being worked: no readiness check.
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "delegated work".into(),
            verification: String::new(),
            model: "cheap".into(),
            context: String::new(),
        }]);
        ctx.session
            .update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("in_progress"), "{err}");
    }
}
