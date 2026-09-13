//! The `summarise` tool: run a command whose output is expected to be large or
//! noisy and return a **delegate-written summary** of it instead of the raw
//! text, so the bulk never enters the tech lead's context.
//!
//! The full output is preserved on disk (under `<root>/.comrade/artifacts/`)
//! and its path is returned, so nothing is lost — the tech lead can read the
//! parts it actually needs. The command runs behind the same approval gate as
//! the `shell` tool; the summary is one non-streaming chat round-trip to a
//! configured delegate model (`[[delegates]]`).

use crate::config::{Autonomy, DelegateCfg};
use crate::delegate::{Target, build_targets};
use crate::llm::{ChatMessage, LlmClient, Role};
use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{TaskRunner, Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Name of the tool advertised to the tech lead model.
pub const TOOL_NAME: &str = "summarise";

/// Cap on the characters of raw command output handed to the summariser model.
/// The whole output still goes to the artifact file; this only bounds what the
/// summariser reads.
const MAX_RAW_CHARS: usize = 24_000;

/// Directory (under the project root) the full command output is written to.
const ARTIFACT_DIR: &str = ".comrade/artifacts";

/// A tool that runs one command (a shell command or a project task) and returns
/// a delegate-written summary of its output, saving the full output to a file.
pub struct SummariseTool {
    spec: ToolSpec,
    targets: Vec<Target>,
    /// Delegate used when the caller does not name one: the one whose blurb best
    /// fits summarising (see [`pick_default_model`]).
    default_model: String,
    /// Runs project tasks (`task` arg). `None` when the caller built the tool
    /// without a runner (tests); the `task` path then refuses, `command` works.
    task_runner: Option<Arc<dyn TaskRunner>>,
}

/// The system prompt given to the summariser model. Kept short and strict so a
/// small/cheap delegate still produces a faithful, actionable digest.
const SYSTEM_PROMPT: &str = "\
You are a summariser. You are given the raw output of a shell command. Reply \
with a concise summary that KEEPS every error, failure, warning, test name, \
file:line and numeric total, and DROPS repetition and progress noise. Use short \
headings and bullets when they help. Never invent facts and never add an \
introduction or closing: reply with the summary only.";

impl SummariseTool {
    /// Build the tool from the configured `[[delegates]]` entries. Returns
    /// `Ok(None)` when no delegate is configured (the tool is then not
    /// advertised). The delegate models are the same ones the `delegate` and
    /// `ask_advise` tools use.
    ///
    /// `task_runner` resolves project tasks for the `task` arg; pass `None` to
    /// offer only the raw `command` path (the crate stays agnostic to the tool
    /// crates that provide it).
    pub fn new(
        delegates: &[DelegateCfg],
        task_runner: Option<Arc<dyn TaskRunner>>,
    ) -> Result<Option<Self>> {
        let targets = build_targets(delegates)?;
        if targets.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = targets.iter().map(|t| t.cfg.name.clone()).collect();
        let default_model = pick_default_model(&targets);
        let listing = delegates
            .iter()
            .filter(|d| d.enabled)
            .map(crate::delegate::cfg_line)
            .collect::<Vec<_>>()
            .join("\n");

        let description = format!(
            "Run ONE command whose output you expect to be large, noisy or low-value (a full \
             build/test log, a big diff, a verbose command) and return a DELEGATE-WRITTEN SUMMARY \
             of that output instead of the raw text — keeping the bulk out of your context. Pass \
             EITHER `command` (an arbitrary shell command, run with your approval exactly like \
             `shell`) OR `task` (a project task verb or alias, e.g. build/check/clippy, resolved \
             in the project's build ecosystem and optionally scoped with `subproject`); the two \
             are mutually exclusive. The full, uncapped output is written under \
             .comrade/artifacts/ and its path is returned, so you can read the exact parts you \
             still need. Prefer a dedicated tool (pom_run_tests, pom_check) when it already \
             returns a tight summary; use this when you must run a raw command or task and only \
             its gist matters.\n\n\
             The summary is written by the delegate whose description best fits the summarising \
             job (override with `model`); configured delegates:\n{listing}"
        );
        let schema = json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Arbitrary shell command to run (bash -c); its raw output is summarised. Mutually exclusive with `task`."
                },
                "task": {
                    "type": "string",
                    "description": "Project task verb or alias (e.g. \"build\", \"check\", \"clippy\") resolved via the build ecosystem; its raw output is summarised. Mutually exclusive with `command`."
                },
                "subproject": {
                    "type": "string",
                    "description": "Optional subproject directory for `task`, e.g. \"crates/app\"."
                },
                "ecosystem": {
                    "type": "string",
                    "description": "Which build ecosystem to use for `task` in a polyglot repo, \"cargo\" or \"npm\". Defaults to the only one present, or the one that supports the task."
                },
                "focus": {
                    "type": "string",
                    "description": "Optional hint for the summariser: what the summary must cover (e.g. \"which tests failed and why\")."
                },
                "dir": {
                    "type": "string",
                    "description": "Optional directory relative to the project root to run `command` in."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 300,
                    "description": "Kill the command/task after this many seconds."
                },
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": "Which delegate model writes the summary. Defaults to the delegate whose description best fits summarising."
                }
            },
            "oneOf": [
                { "required": ["command"] },
                { "required": ["task"] }
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
            default_model,
            task_runner,
        }))
    }

    /// The delegate client that writes the summary, by name.
    fn client_for(&self, name: &str) -> Option<(&LlmClient, &DelegateCfg)> {
        self.targets
            .iter()
            .find(|t| t.cfg.name == name)
            .map(|t| (&t.client, &t.cfg))
    }

    fn model_names(&self) -> Vec<String> {
        self.targets.iter().map(|t| t.cfg.name.clone()).collect()
    }
}

#[async_trait]
impl Tool for SummariseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            command: Option<String>,
            #[serde(default)]
            task: Option<String>,
            #[serde(default)]
            subproject: Option<String>,
            #[serde(default)]
            ecosystem: Option<String>,
            #[serde(default)]
            focus: Option<String>,
            #[serde(default)]
            dir: Option<String>,
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
            #[serde(default)]
            model: Option<String>,
        }
        fn default_timeout() -> u64 {
            300
        }

        let args: Args = serde_json::from_value(args)?;
        let command = args
            .command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let task = args
            .task
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match (&command, &task) {
            (None, None) => bail!("provide `command` (a shell command) or `task` (a project task)"),
            (Some(_), Some(_)) => bail!("`command` and `task` are mutually exclusive"),
            _ => {}
        }

        // Which delegate writes the summary.
        let model = args
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.default_model.clone());
        let Some((client, cfg)) = self.client_for(&model) else {
            bail!(
                "unknown summariser model {model:?}; choose one of {:?}",
                self.model_names()
            );
        };
        // A delegate banned from `delegate`/`ask_advise` must not be quietly
        // usable here to spawn a model chat either.
        if cfg.approval == Autonomy::Deny {
            bail!(
                "delegate {model:?} is configured `approval = \"deny\"`: refusing to use it to \
                 summarise"
            );
        }

        // Run the command (or project task) and capture the uncapped output.
        let (label, out) = if let Some(task) = task {
            let Some(runner) = &self.task_runner else {
                bail!("no project task runner is available; use `command` instead of `task`");
            };
            let run = runner
                .run_task(
                    &ctx.project_root,
                    &task,
                    args.subproject.as_deref(),
                    args.ecosystem.as_deref(),
                    &[],
                    args.timeout_secs,
                )
                .await?;
            (
                run.describe.clone(),
                RawOutput {
                    success: run.success,
                    code: run.code,
                    body: run.body,
                    elapsed: run.elapsed,
                },
            )
        } else {
            let command = command.expect("checked: exactly one of command/task is set");
            // Policy gate BEFORE approval: a denied command never even prompts.
            comrade_tool::check_command(&command, &comrade_tool::policy())?;
            let cwd = resolve_dir(&ctx.project_root, args.dir.as_deref())?;
            ctx.confirm(format!("run (output summarised): {command}"), None)
                .await?;
            let out = run_command(&cwd, &command, args.timeout_secs).await?;
            (command, out)
        };

        let artifact = write_artifact(&ctx.project_root, &label, &out.body).ok();
        let summary =
            summarise_output(client, &label, out.code, args.focus.as_deref(), &out.body).await?;

        let status = if out.success { "ok" } else { "failed" };
        let mut result = format!(
            "{label} {status} (exit {}, {:.1}s)\n",
            out.code,
            out.elapsed.as_secs_f32()
        );
        match artifact {
            Some(path) => result.push_str(&format!(
                "full output ({} bytes) saved to {}\n",
                out.body.len(),
                path.display()
            )),
            None => result.push_str("full output could not be saved\n"),
        }
        if let Some(focus) = args
            .focus
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
        {
            result.push_str(&format!("focus: {focus}\n"));
        }
        result.push_str(&format!("\nSUMMARY (by {model}):\n{summary}"));
        Ok(result)
    }
}

/// Description-hint keywords that mark a delegate as a good fit for bulk,
/// low-stakes summarisation work. A `[[delegates]]` `description` is the
/// config's stated purpose for the model — the same text the tech lead reads to
/// pick a delegate — so it is the signal the tool uses to choose one itself.
const SUMMARISER_HINTS: &[&str] = &[
    "summar", "cheap", "fast", "quick", "small", "light", "econom", "budget",
];

/// Choose the delegate that best fits summarisation: the enabled one whose
/// name, model id or description matches the most [`SUMMARISER_HINTS`], with
/// config order breaking ties. Falls back to the first enabled delegate when no
/// blurb hints at a preference.
fn pick_default_model(targets: &[Target]) -> String {
    let mut best: Option<(usize, &str)> = None;
    for t in targets {
        let hay =
            format!("{} {} {}", t.cfg.name, t.cfg.llm.model, t.cfg.description).to_lowercase();
        let score: usize = SUMMARISER_HINTS
            .iter()
            .map(|h| hay.matches(h).count())
            .sum();
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, &t.cfg.name));
        }
    }
    best.map(|(_, name)| name.to_string()).unwrap_or_default()
}

/// Resolve the working directory from the project root + an optional relative
/// `dir`, refusing anything that escapes the root (same rule as the shell tool).
fn resolve_dir(root: &Path, dir: Option<&str>) -> Result<PathBuf> {
    let Some(dir) = dir else {
        return Ok(root.to_path_buf());
    };
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        return Ok(root.to_path_buf());
    }
    let path = root.join(dir);
    if !path.starts_with(root) || dir.contains("..") {
        bail!("dir {dir:?} escapes the project root");
    }
    Ok(path)
}

/// Result of running the raw command.
struct RawOutput {
    success: bool,
    code: i32,
    body: String,
    elapsed: Duration,
}

/// Run `command` with `bash -c` in `cwd`, capturing stdout+stderr UNCAPPED.
async fn run_command(cwd: &Path, command: &str, timeout_secs: u64) -> Result<RawOutput> {
    use std::process::Stdio;

    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn bash for {command:?}"))?;

    let started = std::time::Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("command timed out after {timeout_secs}s and was killed"))?
        .context("command failed to produce output")?;

    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
    body.push_str(&String::from_utf8_lossy(&output.stderr));

    Ok(RawOutput {
        success: output.status.success(),
        code: output.status.code().unwrap_or(-1),
        body: body.trim_end().to_string(),
        elapsed: started.elapsed(),
    })
}

/// Write the full command output to `<root>/.comrade/artifacts/<secs>-<slug>.txt`
/// and return the path.
fn write_artifact(root: &Path, command: &str, body: &str) -> Result<PathBuf> {
    let dir = root.join(ARTIFACT_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{secs}-{}.txt", slug(command)));
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// A short, filesystem-safe slug of a command (first 40 alphanumeric chars).
fn slug(command: &str) -> String {
    let mut out = String::new();
    for c in command.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "output".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Ask the delegate model to summarise the raw output.
async fn summarise_output(
    client: &LlmClient,
    command: &str,
    code: i32,
    focus: Option<&str>,
    raw: &str,
) -> Result<String> {
    let mut body: String = raw.chars().take(MAX_RAW_CHARS).collect();
    if raw.chars().count() > MAX_RAW_CHARS {
        body.push_str(
            "\n…[output truncated for the summariser; read the artifact file for the rest]",
        );
    }
    let focus_line = match focus.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("What the summary must cover: {f}\n"),
        None => String::new(),
    };
    let user =
        format!("Command: {command}\nExit code: {code}\n{focus_line}\n--- raw output ---\n{body}");
    let messages = [
        ChatMessage::new(Role::System, SYSTEM_PROMPT),
        ChatMessage::new(Role::User, user),
    ];
    let summary = client.chat(&messages).await?;
    if summary.trim().is_empty() {
        bail!("the summariser model returned an empty summary");
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryUndo;
    use crate::config::LlmCfg;
    use crate::session::AgentSession;
    use comrade_tool::tool::{UserIo, UserPrompt, UserReply};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A one-shot fake OpenAI-compatible server that captures the request body
    /// and replies with `content`. Mirrors the helper in delegate.rs's tests.
    fn fake_summariser(content: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut tmp = [0u8; 2048];
            let mut body_len: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            while body_len.is_none_or(|len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&tmp[..n]);
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
            let body = match (header_end, body_len) {
                (Some(he), Some(len)) => {
                    let start = he + 4;
                    let end = (start + len).min(data.len());
                    String::from_utf8_lossy(&data[start..end]).into_owned()
                }
                _ => String::new(),
            };
            let _ = tx.send(body);
            let resp_body = format!(
                "{{\"choices\":[{{\"message\":{{\"content\":\"{}\"}}}}]}}",
                content.replace('"', "\\\"")
            );
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
            description: format!("{name} summariser"),
            enabled: true,
            approval: Autonomy::Auto,
            llm: LlmCfg {
                base_url: base_url.into(),
                model: "summariser-model".into(),
                ..LlmCfg::default()
            },
        }
    }

    /// A task runner double: returns a fixed sentinel body for any task, so a
    /// test can prove the task's raw output is summarised, not returned.
    struct FakeRunner;
    #[async_trait]
    impl comrade_tool::TaskRunner for FakeRunner {
        async fn run_task(
            &self,
            _root: &Path,
            task: &str,
            _subproject: Option<&str>,
            _ecosystem: Option<&str>,
            _extra: &[String],
            _timeout_secs: u64,
        ) -> Result<comrade_tool::TaskRun> {
            Ok(comrade_tool::TaskRun {
                describe: format!("task {task}"),
                success: true,
                code: 0,
                body: "SECRET_TASK_OUTPUT".to_string(),
                elapsed: Duration::from_millis(1),
            })
        }
    }

    /// The tool only reads the root and (under auto_approve) never asks, so a
    /// no-op IO double plus a fresh session is all the tests need.
    struct NoopIo;
    #[async_trait]
    impl UserIo for NoopIo {
        async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
            Ok(UserReply::Answer(String::new()))
        }
    }

    fn ctx(root: &Path) -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            project_root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            session: Arc::new(AgentSession::new(tx)).as_control(),
            user: Arc::new(NoopIo),
            undo: Arc::new(MemoryUndo::new(root.to_path_buf())),
            auto_approve: true,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("comrade-summarise-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_delegates_yield_no_tool() {
        assert!(SummariseTool::new(&[], None).unwrap().is_none());
    }

    #[tokio::test]
    async fn runs_a_command_and_returns_the_summary_and_artifact_path() {
        let root = scratch("basic");
        // The command's *output* comes from a file, so the sentinel text is not
        // part of the command string (and so not in the header/slug): this lets
        // the test prove the raw text never reaches the caller.
        std::fs::write(root.join("sentinel.txt"), "SECRET_RAW_PAYLOAD").unwrap();
        let (url, rx) = fake_summariser("3 failures: a, b, c");
        let tool = SummariseTool::new(&[delegate("cheap", &url)], None)
            .unwrap()
            .unwrap();
        let ctx = ctx(&root);

        let out = tool
            .invoke(
                &ctx,
                json!({ "command": "cat sentinel.txt", "focus": "greet" }),
            )
            .await
            .unwrap();

        // The raw command output was never returned to the caller...
        assert!(!out.contains("SECRET_RAW_PAYLOAD"), "{out}");
        // ...the delegate's summary was.
        assert!(out.contains("SUMMARY (by cheap):"), "{out}");
        assert!(out.contains("3 failures: a, b, c"), "{out}");
        // ...and the full output is preserved on disk and its path reported.
        assert!(out.contains(".comrade/artifacts/"), "{out}");
        let artifact = std::fs::read_dir(root.join(ARTIFACT_DIR))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(&artifact).unwrap(),
            "SECRET_RAW_PAYLOAD"
        );

        // The summariser actually received the raw output and the focus hint.
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(request.contains("SECRET_RAW_PAYLOAD"), "{request}");
        assert!(request.contains("greet"), "{request}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_denied_model_is_refused() {
        let root = scratch("deny");
        let (url, _rx) = fake_summariser("nope");
        let mut d = delegate("banned", &url);
        d.approval = Autonomy::Deny;
        let tool = SummariseTool::new(&[d], None).unwrap().unwrap();
        let err = tool
            .invoke(&ctx(&root), json!({ "command": "true" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn runs_a_project_task_via_the_runner() {
        let root = scratch("task");
        let (url, rx) = fake_summariser("build succeeded cleanly");
        let tool = SummariseTool::new(&[delegate("cheap", &url)], Some(Arc::new(FakeRunner)))
            .unwrap()
            .unwrap();

        let out = tool
            .invoke(&ctx(&root), json!({ "task": "build" }))
            .await
            .unwrap();

        // The task's raw output was summarised, not returned...
        assert!(!out.contains("SECRET_TASK_OUTPUT"), "{out}");
        assert!(out.contains("task build"), "{out}");
        assert!(out.contains("build succeeded cleanly"), "{out}");
        // ...and the summariser received it.
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(request.contains("SECRET_TASK_OUTPUT"), "{request}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn task_is_refused_without_a_runner() {
        let root = scratch("norunner");
        let (url, _rx) = fake_summariser("x");
        let tool = SummariseTool::new(&[delegate("cheap", &url)], None)
            .unwrap()
            .unwrap();
        let err = tool
            .invoke(&ctx(&root), json!({ "task": "build" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("task runner"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn command_and_task_are_mutually_exclusive() {
        let root = scratch("both");
        let (url, _rx) = fake_summariser("x");
        let tool = SummariseTool::new(&[delegate("cheap", &url)], Some(Arc::new(FakeRunner)))
            .unwrap()
            .unwrap();
        let err = tool
            .invoke(&ctx(&root), json!({ "command": "true", "task": "build" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn picker_prefers_the_delegate_blurbed_for_summarising() {
        let mut frontier = delegate("frontier", "http://x/v1");
        frontier.description = "strongest model, use for hard reasoning".into();
        frontier.llm.model = "big-model".into();
        let mut mini = delegate("mini", "http://y/v1");
        mini.description = "cheap fast model, ideal for summarising logs".into();
        mini.llm.model = "mini-2b".into();
        let targets = crate::delegate::build_targets(&[frontier, mini]).unwrap();
        assert_eq!(pick_default_model(&targets), "mini");
    }

    #[test]
    fn picker_breaks_ties_by_config_order() {
        let mut alpha = delegate("alpha", "http://a/v1");
        alpha.description = "general purpose".into();
        alpha.llm.model = "model-a".into();
        let mut beta = delegate("beta", "http://b/v1");
        beta.description = "another one".into();
        beta.llm.model = "model-b".into();
        let targets = crate::delegate::build_targets(&[alpha, beta]).unwrap();
        assert_eq!(pick_default_model(&targets), "alpha");
    }

    #[test]
    fn slug_is_filesystem_safe_and_bounded() {
        assert_eq!(slug("cargo test --all"), "cargo_test_all");
        assert_eq!(slug(""), "output");
        assert!(slug(&"x".repeat(200)).len() <= 40);
    }
}
