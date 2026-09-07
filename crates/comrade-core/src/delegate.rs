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
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde_json::{Value, json};

use crate::config::DelegateCfg;
use crate::llm::{ChatMessage, LlmClient, Role};

/// Name of the tool advertised to the orchestrating model.
pub const TOOL_NAME: &str = "delegate";

/// System prompt for the delegated model. It must be self-sufficient and
/// return only the deliverable, because its reply goes straight back to the
/// orchestrator as a tool observation.
const SUBAGENT_SYSTEM: &str = "\
You are a focused sub-agent of Comrade, a software engineering agent. The \
orchestrating agent delegated ONE self-contained task to you. You have no \
tools and no repository access: work only from the context and task below. \
Complete the task to the best of your ability and reply with ONLY the final \
deliverable (the code, patch, text, or answer) — no preamble, no meta-commentary, \
no questions.";

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

Delegate ONLY well-bounded jobs: writing one self-contained function or file \
with tests, a regex, a data transform, a translation, a rewrite of a code \
snippet, a focused explanation. Put every path, identifier and code snippet the \
delegate needs inside `task`; use `context` for background material it should \
consider. The delegate's reply is returned to you verbatim to verify and apply.

Configured delegates:
{name_list}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": "Which configured delegate model should do the work"
                },
                "task": {
                    "type": "string",
                    "description": "The exact, self-contained job for the delegate, with all needed details (paths, code, identifiers, expected output)"
                },
                "context": {
                    "type": "string",
                    "description": "Optional background material the delegate should consider (existing code, error logs, constraints)"
                }
            },
            "required": ["model", "task"],
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

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(target) = self.targets.iter().find(|t| t.cfg.name == model) else {
            let known = self
                .targets
                .iter()
                .map(|t| t.cfg.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("unknown delegate model {model:?}. Configured: {known}");
        };

        let task = args.get("task").and_then(Value::as_str).unwrap_or_default();
        if task.trim().is_empty() {
            bail!("`task` must not be empty");
        }

        let user_prompt = match args
            .get("context")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
        {
            Some(context) => format!("Context:\n{context}\n\nTask:\n{task}"),
            None => format!("Task:\n{task}"),
        };
        let messages = vec![
            ChatMessage::new(Role::System, SUBAGENT_SYSTEM),
            ChatMessage::new(Role::User, user_prompt),
        ];

        let display = target
            .cfg
            .llm
            .provider
            .as_deref()
            .map(|p| format!("{p}/{}", target.cfg.llm.model))
            .unwrap_or_else(|| target.cfg.llm.model.clone());
        let reply = target
            .client
            .chat(&messages)
            .await
            .with_context(|| format!("delegate {model} ({display}) failed"))?;

        Ok(format!("delegate {model} ({display}) replied:\n{reply}"))
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
        let required = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(required, vec!["model", "task"]);
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
}
