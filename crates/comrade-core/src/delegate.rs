//! The `delegate` tool: hand a single, self-contained sub-task to another
//! model — typically a cheaper or faster one on a different provider.
//!
//! The main ("planner") model keeps orchestrating with its own tools and
//! context, but can offload a well-defined piece of work to a delegate model
//! configured in `config.toml` under `[[delegates]]`. Delegates have **no**
//! tools and no repository access: they work purely from the prompt they are
//! given, which keeps them cheap and their blast radius zero.

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{PlanStatus, PlanTarget, Tool, ToolContext, ToolSpec};
use serde_json::{Value, json};

use crate::config::DelegateCfg;
use crate::llm::{ChatMessage, LlmClient, Role};

/// Name of the tool advertised to the orchestrating model.
pub const TOOL_NAME: &str = "delegate";

/// A delegated plan step gets one attempt from the delegate; if the parent's
/// verification then fails, the parent may re-delegate the same step with
/// `feedback` so the delegate can fix its work — up to this many fix rounds.
/// Rounds are recorded on the step's note (`working: <model> (fix N/5)`), and
/// further fix requests are refused once the limit is reached so the parent
/// takes the step over and does it itself.
const MAX_FIX_ROUNDS: u64 = 5;

/// System prompt for the delegated model. It must be self-sufficient and
/// return the deliverable, because its reply goes straight back to the
/// orchestrator as a tool observation. The delegate is also the first line of
/// verification: it must re-check its own deliverable and report the result,
/// even though it has no tools to run real checks (the parent does that).
const SUBAGENT_SYSTEM: &str = "\
You are a focused sub-agent of Comrade, a software engineering agent. The \
orchestrating agent delegated ONE self-contained task to you. You have no \
tools and no repository access: work only from the context and task below. \
Complete the task to the best of your ability and reply with ONLY the final \
deliverable (the code, patch, text, or answer) — no preamble, no meta-commentary, \
no questions.

Before replying, verify your own deliverable as far as you can: re-read it \
against the task's verification (when one is given), trace the logic and hunt \
for mistakes — the parent agent will run its own verification and cannot take \
your word for it. Close your reply with a single line starting with \
`VERIFICATION:` that states what you checked and whether the deliverable passes.";

/// One configured delegate model plus the HTTP client that talks to it.
struct Target {
    cfg: DelegateCfg,
    client: LlmClient,
}

/// A tool that runs a prompt on one of the configured delegate models.
pub struct DelegateTool {
    spec: ToolSpec,
    targets: Vec<Target>,
}

impl DelegateTool {
    /// Build the delegate tool from the configured `[[delegates]]` entries.
    /// Returns `Ok(None)` when no delegates are configured (the tool is then
    /// not advertised at all). Fails on duplicate/blank names or a delegate
    /// that cannot build an HTTP client.
    pub fn new(delegates: &[DelegateCfg]) -> Result<Option<Self>> {
        if delegates.is_empty() {
            return Ok(None);
        }

        let mut names: Vec<String> = Vec::new();
        let mut targets: Vec<Target> = Vec::with_capacity(delegates.len());
        for (i, cfg) in delegates.iter().enumerate() {
            if cfg.name.trim().is_empty() {
                bail!("delegates[{i}]: every delegate needs a `name`");
            }
            if cfg.llm.model.trim().is_empty() {
                bail!(
                    "delegates[{i}] ({}): every delegate needs a `model`",
                    cfg.name
                );
            }
            if names.iter().any(|n| n == &cfg.name) {
                bail!("delegates: duplicate delegate name {:?}", cfg.name);
            }
            names.push(cfg.name.clone());
            let client = LlmClient::new(&cfg.llm)
                .with_context(|| format!("delegate {:?} failed to build client", cfg.name))?;
            targets.push(Target {
                cfg: cfg.clone(),
                client,
            });
        }

        let name_list = names
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let description = format!(
            "\
Run one single, self-contained piece of work on another model while you keep \
planning and orchestrating. Use this to offload tasks that are cheaper, faster \
or better done by a specialist model — never use it for work that needs further \
tool calls or repository state, because the delegated model has NO tools and NO \
repository access: it answers purely from the prompt you send.

To execute one of your plan steps, pass `step` (the plan step id): the task and \
context then come from that step and `model` must match the step's model. \
Otherwise delegate ad-hoc work with `model` + `task` (+ optional `context`).

Delegate ONLY well-bounded jobs: writing one self-contained function or file \
with tests, a regex, a data transform, a translation, a rewrite of a code \
snippet, a focused explanation. Put every path, identifier and code snippet the \
delegate needs inside `task`; use `context` for background material it should \
consider. The delegate's reply is returned to you verbatim to verify and apply.

The plan shows who is working: delegating a step marks it in_progress with a \
`working: <model>` note, and fix rounds show up as `(fix N/5)`. Both you and \
the delegate verify. The delegate is asked to self-check and close with a \
`VERIFICATION:` line, but it has no tools, so that line is never proof. After \
the delegate replies, run the step's verification yourself with your tools \
(e.g. run_tests); if it fails, re-delegate the SAME step with `feedback` set \
to the failure output so the delegate fixes it — up to 5 fix rounds per step. \
After 5 the tool refuses further fix requests and you must do the step yourself.

Configured delegates:
{name_list}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "step": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Plan step id to execute. The task comes from the step's goal (+verification) and the context from the step's summarised context; the step's own model is used."
                },
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": "Which configured delegate model should do the work (must match the step's model when `step` is given)"
                },
                "task": {
                    "type": "string",
                    "description": "The exact, self-contained job for the delegate, with all needed details (paths, code, identifiers, expected output). Mutually exclusive with `step`."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background material the delegate should consider (existing code, error logs, constraints). Mutually exclusive with `step`."
                },
                "feedback": {
                    "type": "string",
                    "description": "Verification failure output from the parent for a delegate that previously attempted `step`: the delegate must fix its deliverable until it passes. Counts as one fix round (max 5 per step, tracked on the step's note). Only valid with `step`."
                }
            },
            "oneOf": [
                { "required": ["step"] },
                { "required": ["model", "task"] }
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
        }))
    }
}

#[async_trait]
impl Tool for DelegateTool {
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
        let feedback = args
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !feedback.is_empty() && step_id.is_none() {
            bail!(
                "`feedback` is only valid with `step`: it reports a failed verification to a \
                 delegate that previously attempted that plan step"
            );
        }

        // When a plan step is delegated, remember its previous status so the
        // plan can be restored if the delegate call itself fails.
        let mut delegated_step: Option<(u64, PlanStatus)> = None;

        // Resolve what to run: either an explicit ad-hoc task, or one plan step
        // (whose goal/verification/context/model all come from the plan).
        let (task, context, model) = match step_id {
            Some(id) => {
                for (key, label) in [("task", "task"), ("context", "context")] {
                    let present = args
                        .get(key)
                        .map(|v| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(true))
                        .unwrap_or(false);
                    if present {
                        bail!(
                            "cannot pass `{label}` together with `step`: the {label} comes from the plan step"
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
                        "`model` {model_arg:?} does not match the model assigned to plan step {id} \
                         ({:?})",
                        found.model
                    );
                }
                if found.model.trim().is_empty() {
                    bail!(
                        "plan step {id} has no delegate model assigned; it runs on the main model"
                    );
                }
                let goal = found.goal.trim();
                let verify = found.verification.trim();
                let task = if verify.is_empty() {
                    goal.to_string()
                } else {
                    format!("{goal}\n\nVerify your work: {verify}")
                };

                // Reflect the delegation in the plan so the UI shows which
                // delegate is working, and count the fix rounds. A step gets one
                // attempt from the delegate, then up to MAX_FIX_ROUNDS repairs
                // requested via `feedback`; past that the parent must take over.
                let fixes = fix_rounds_in_note(found.note.as_deref(), &found.model);
                if fixes >= MAX_FIX_ROUNDS {
                    bail!(
                        "plan step {id} already had {MAX_FIX_ROUNDS} failed fix round(s) with \
                         delegate {:?}; stop delegating and do the step yourself",
                        found.model
                    );
                }
                let note = if feedback.is_empty() {
                    if fixes == 0 {
                        format!("working: {}", found.model)
                    } else {
                        // a bare re-run keeps the fix count intact
                        format!("working: {} (fix {fixes}/{MAX_FIX_ROUNDS})", found.model)
                    }
                } else {
                    format!(
                        "working: {} (fix {}/{MAX_FIX_ROUNDS})",
                        found.model,
                        fixes + 1
                    )
                };
                delegated_step = Some((id, found.status));
                ctx.session
                    .update_plan(PlanTarget::Id(id), PlanStatus::InProgress, Some(note));

                (task, found.context, found.model)
            }
            None => {
                let task = args
                    .get("task")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if task.trim().is_empty() {
                    bail!("`task` must not be empty (or pass `step` to delegate a plan step)");
                }
                let context = args
                    .get("context")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (task, context, model_arg)
            }
        };

        let Some(target) = self.targets.iter().find(|t| t.cfg.name == model) else {
            let known = self
                .targets
                .iter()
                .map(|t| t.cfg.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("unknown delegate model {model:?}. Configured: {known}");
        };

        let user_prompt = if feedback.is_empty() {
            if context.trim().is_empty() {
                format!("Task:\n{task}")
            } else {
                format!("Context:\n{context}\n\nTask:\n{task}")
            }
        } else {
            // A fix round: the parent ran the verification, it failed, and the
            // delegate must repair its earlier deliverable.
            let header = if context.trim().is_empty() {
                format!("Task:\n{task}")
            } else {
                format!("Context:\n{context}\n\nTask:\n{task}")
            };
            format!(
                "{header}\n\nYour previous attempt did not pass the parent's verification of \
                 this step. The parent ran the verification and observed:\n{feedback}\n\n\
                 Fix the deliverable so it passes, and reply with the complete corrected version \
                 followed by the VERIFICATION: line."
            )
        };
        let messages = vec![
            ChatMessage::new(Role::System, SUBAGENT_SYSTEM),
            ChatMessage::new(Role::User, user_prompt),
        ];

        let display = target.cfg.llm.display();
        let reply = target
            .client
            .chat(&messages)
            .await
            .with_context(|| format!("delegate {model} ({display}) failed"));
        let reply = match reply {
            Ok(reply) => reply,
            Err(err) => {
                // The delegate never ran: pull the step back from "working" so
                // the plan does not claim a delegate is on the job.
                if let Some((id, previous)) = delegated_step {
                    ctx.session.update_plan(
                        PlanTarget::Id(id),
                        previous,
                        Some(format!("delegate {model} failed to run")),
                    );
                }
                return Err(err);
            }
        };

        Ok(format!("delegate {model} ({display}) replied:\n{reply}"))
    }
}

/// How many failed fix rounds a plan step has already been through, read from
/// the note this tool writes while a delegate is working (`working: <model>`
/// for the first attempt, `working: <model> (fix N/5)` for each repair).
fn fix_rounds_in_note(note: Option<&str>, model: &str) -> u64 {
    let Some(note) = note else {
        return 0;
    };
    let Some(rest) = note.strip_prefix(&format!("working: {model}")) else {
        return 0;
    };
    let Some(start) = rest.find("(fix ") else {
        return 0;
    };
    rest[start + "(fix ".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use comrade_tool::PlanStepDraft;
    use comrade_tool::ToolContext;
    use comrade_tool::tool::{UserIo, UserPrompt, UserReply};

    use super::*;
    use crate::MemoryUndo;
    use crate::config::{Config, DelegateCfg, LlmCfg};
    use crate::session::AgentSession;

    /// The delegate tool never touches the session/user, so a no-op IO double
    /// is all the tests need.
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
        }
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
    /// channel so tests can assert what the delegate was actually asked.
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
            // Read until the whole body (per Content-Length) is buffered.
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
        let tool = DelegateTool::new(&[]).unwrap();
        assert!(tool.is_none());
    }

    #[test]
    fn duplicate_or_blank_names_are_rejected() {
        let dup = vec![delegate("a", "http://x/v1"), delegate("a", "http://x/v1")];
        assert!(DelegateTool::new(&dup).is_err());

        let blank = vec![DelegateCfg {
            name: "  ".into(),
            ..delegate("ignored", "http://x/v1")
        }];
        assert!(DelegateTool::new(&blank).is_err());

        let no_model = vec![DelegateCfg {
            llm: LlmCfg {
                model: "".into(),
                ..LlmCfg::default()
            },
            ..delegate("m", "http://x/v1")
        }];
        assert!(DelegateTool::new(&no_model).is_err());
    }

    #[test]
    fn schema_advertises_models_and_required_args() {
        let cfg = Config {
            delegates: vec![
                delegate("groq-fast", "http://x/v1"),
                delegate("mistral", "http://x/v1"),
            ],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates).unwrap().unwrap();
        assert_eq!(tool.spec().name, "delegate");
        let schema = &tool.spec().json_schema;
        let models = schema["properties"]["model"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(models, vec!["groq-fast", "mistral"]);
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["step"].is_object());
        // `step` alone, or `model` + `task` (oneOf), are the two call shapes.
        let one_of = schema["oneOf"].as_array().unwrap();
        let requires = |needle: &str| {
            one_of.iter().any(|o| {
                o["required"]
                    .as_array()
                    .map_or(false, |r| r.iter().any(|v| v.as_str() == Some(needle)))
            })
        };
        assert!(requires("step"));
        assert!(requires("model") && requires("task"));
    }

    #[tokio::test]
    async fn invoke_queries_the_chosen_delegate() {
        let base = fake_chat_server("here is the finished function");
        let cfg = Config {
            delegates: vec![
                delegate("cheap", &base),
                delegate("other", "http://127.0.0.1:1/v1"),
            ],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(
                &ctx,
                json!({
                    "model": "cheap",
                    "task": "Write a double() function.",
                    "context": "Rust, must be pure."
                }),
            )
            .await
            .unwrap();
        assert!(out.contains("delegate cheap"));
        assert!(out.contains("here is the finished function"));
    }

    #[tokio::test]
    async fn invoke_rejects_unknown_model_and_empty_task() {
        let base = fake_chat_server("ignored");
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        let err = tool
            .invoke(&ctx, json!({"model": "nope", "task": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown delegate model"));

        let err = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "   "}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("task"));
    }

    #[tokio::test]
    async fn plan_step_delegation_sends_goal_verification_and_context() {
        let (base, spy) = request_spy();
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write a double() function.".into(),
            verification: "cargo test double passes".into(),
            model: "cheap".into(),
            context: "Pure Rust, no dependencies.".into(),
        }]);

        let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        assert!(out.contains("delegate cheap"), "{out}");

        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(body.contains("Write a double() function."), "{body}");
        assert!(
            body.contains("Verify your work: cargo test double passes"),
            "{body}"
        );
        assert!(body.contains("Pure Rust, no dependencies."), "{body}");
    }

    #[tokio::test]
    async fn plan_step_delegation_validates_args() {
        let (base, _spy) = request_spy();
        let cfg = Config {
            delegates: vec![
                delegate("cheap", &base),
                delegate("other", "http://127.0.0.1:1/v1"),
            ],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![
            PlanStepDraft {
                goal: "run on cheap".into(),
                verification: "".into(),
                model: "cheap".into(),
                context: "".into(),
            },
            PlanStepDraft {
                goal: "run on main".into(),
                verification: "".into(),
                model: "".into(),
                context: "".into(),
            },
        ]);

        // `model` must match the step's assigned model.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "model": "other"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        // A step without an assigned delegate cannot be delegated.
        let err = tool.invoke(&ctx, json!({"step": 2})).await.unwrap_err();
        assert!(
            err.to_string().contains("no delegate model assigned"),
            "{err}"
        );

        // `step` is exclusive with an explicit `task`.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "task": "nope"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot pass `task`"), "{err}");

        // Unknown step id.
        let err = tool.invoke(&ctx, json!({"step": 99})).await.unwrap_err();
        assert!(err.to_string().contains("no plan step with id 99"), "{err}");
    }

    #[tokio::test]
    async fn plan_step_delegation_marks_step_in_progress_with_working_note() {
        let (base, _spy) = request_spy();
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "cargo test double passes".into(),
            model: "cheap".into(),
            context: "".into(),
        }]);

        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::InProgress);
        let note = step.note.as_deref().unwrap_or_default();
        assert!(note.contains("working: cheap"), "{note}");
        assert!(!note.contains("fix"), "{note}");
    }

    fn cheap_tool(base_url: &str) -> DelegateTool {
        let cfg = Config {
            delegates: vec![delegate("cheap", base_url)],
            ..Config::default()
        };
        DelegateTool::new(&cfg.delegates).unwrap().unwrap()
    }

    #[tokio::test]
    async fn fix_feedback_is_forwarded_and_rounds_are_counted_on_the_step() {
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "cargo test double passes".into(),
            model: "cheap".into(),
            context: "".into(),
        }]);
        let note = || ctx.session.plan()[0].note.clone();

        // first attempt: no feedback, note carries no round yet
        let (base1, spy1) = request_spy();
        let tool = cheap_tool(&base1);
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let body1 = spy1
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            !body1.contains("did not pass the parent's verification"),
            "{body1}"
        );
        assert_eq!(note().as_deref(), Some("working: cheap"));

        // fix round 1: feedback reaches the delegate and the note counts it
        let (base2, spy2) = request_spy();
        let tool = cheap_tool(&base2);
        tool.invoke(
            &ctx,
            json!({"step": 1, "feedback": "cargo test fails: double(0) returned 1"}),
        )
        .await
        .unwrap();
        let body2 = spy2
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            body2.contains("did not pass the parent's verification"),
            "{body2}"
        );
        assert!(body2.contains("double(0) returned 1"), "{body2}");
        assert_eq!(note().as_deref(), Some("working: cheap (fix 1/5)"));

        // fix round 2 keeps counting
        let (base3, _spy3) = request_spy();
        let tool = cheap_tool(&base3);
        tool.invoke(&ctx, json!({"step": 1, "feedback": "still failing"}))
            .await
            .unwrap();
        assert_eq!(note().as_deref(), Some("working: cheap (fix 2/5)"));
    }

    #[tokio::test]
    async fn delegate_is_refused_after_five_fix_rounds() {
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "".into(),
            model: "cheap".into(),
            context: "".into(),
        }]);
        // simulate five failed fix rounds already recorded on the step
        ctx.session.update_plan(
            PlanTarget::Id(1),
            PlanStatus::InProgress,
            Some("working: cheap (fix 5/5)".into()),
        );

        let (base, _spy) = request_spy();
        let tool = cheap_tool(&base);
        let err = tool
            .invoke(&ctx, json!({"step": 1, "feedback": "still red"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("5 failed fix round(s)"), "{msg}");
        assert!(msg.contains("do the step yourself"), "{msg}");

        // a bare re-run is refused too once the limit is reached
        let (base2, _spy2) = request_spy();
        let tool = cheap_tool(&base2);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("do the step yourself"), "{err}");
    }

    #[tokio::test]
    async fn feedback_without_a_step_is_rejected() {
        let (base, _spy) = request_spy();
        let tool = cheap_tool(&base);
        let ctx = test_ctx();
        let err = tool
            .invoke(
                &ctx,
                json!({"model": "cheap", "task": "write double()", "feedback": "nope"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("`feedback` is only valid with `step`"),
            "{err}"
        );
    }
}
