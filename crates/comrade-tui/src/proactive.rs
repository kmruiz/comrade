//! Proactive mode: sensors poll a source on an interval and report changes so
//! Comrade can react — notify the human, or (in `auto` mode) open a session to
//! handle the change itself.
//!
//! A sensor is a `[[sensors]]` entry in `config.toml` (see
//! [`comrade_core::SensorCfg`]). It polls either:
//! - a shell `command`, run via `bash -c`, or
//! - a registered `tool` — any tool the harness knows: a built-in, a bridged MCP
//!   tool (e.g. one that lists a sprint's JIRA tickets), or a skill.
//!
//! Either way the polled *string* is what matters: the first successful poll
//! only establishes a *baseline* and emits nothing. Every later poll whose result
//! differs from the baseline emits a [`SensorEvent::Changed`] carrying the line
//! diff; a failing poll emits a [`SensorEvent::Error`] without disturbing the
//! baseline.
//!
//! The change detection itself ([`line_delta`]) is pure and unit-tested, and the
//! poll source is behind the small [`Probe`] trait, so the polling loop is
//! testable without touching the clock, the process table or a model.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use comrade_core::{SensorCfg, SensorMode};
use comrade_tool::{ToolContext, ToolRegistry};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The difference between two snapshots of a polled result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delta {
    /// Lines present in the new output but not the old one.
    pub added: Vec<String>,
    /// Lines present in the old output but not the new one.
    pub removed: Vec<String>,
}

impl Delta {
    /// True when nothing changed (no lines added and none removed).
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Compute the added/removed lines between two outputs. Comparison is over
/// trimmed, non-empty lines, preserving first-seen order, so cosmetic
/// whitespace/blank-line churn is not reported as a change.
pub fn line_delta(old: &str, new: &str) -> Delta {
    let old_lines = meaningful_lines(old);
    let new_lines = meaningful_lines(new);
    Delta {
        added: new_lines
            .iter()
            .filter(|line| !old_lines.contains(line))
            .cloned()
            .collect(),
        removed: old_lines
            .iter()
            .filter(|line| !new_lines.contains(line))
            .cloned()
            .collect(),
    }
}

fn meaningful_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// An event emitted by an enabled sensor.
#[derive(Debug, Clone)]
pub enum SensorEvent {
    /// The sensor's polled output changed since the last poll.
    Changed {
        /// The sensor's configured name.
        name: String,
        /// What the agent should do about it (queue it, or handle it).
        mode: SensorMode,
        /// Optional seed prompt for the session Comrade opens.
        prompt: Option<String>,
        /// The line diff that triggered this event.
        delta: Delta,
    },
    /// The sensor's poll failed (non-zero exit, tool error, or a missing tool).
    Error {
        /// The sensor's configured name.
        name: String,
        /// A human-readable failure description.
        message: String,
    },
}

/// What a sensor polls. Implemented for a shell command ([`CommandProbe`]) and
/// for a registered tool ([`ToolProbe`]); kept behind a trait so the polling
/// loop can be tested with a trivial fake.
#[async_trait::async_trait]
pub trait Probe: Send + Sync {
    /// Perform one poll, returning the string to watch for changes, or a
    /// human-readable description of why this poll failed.
    async fn probe(&self) -> Result<String, String>;
}

/// A sensor that runs a shell command (`bash -c`) and watches its stdout.
struct CommandProbe {
    command: String,
}

#[async_trait::async_trait]
impl Probe for CommandProbe {
    async fn probe(&self) -> Result<String, String> {
        run_command(&self.command).await
    }
}

/// A sensor that invokes a registered tool and watches its string result. The
/// tool may be a built-in, an MCP tool, or a skill.
struct ToolProbe {
    tool: String,
    args: serde_json::Value,
    tools: Arc<ToolRegistry>,
    ctx: ToolContext,
}

#[async_trait::async_trait]
impl Probe for ToolProbe {
    async fn probe(&self) -> Result<String, String> {
        let Some(tool) = self.tools.get(&self.tool) else {
            return Err(format!("no tool named {:?} is registered", self.tool));
        };
        match tool.invoke(&self.ctx, self.args.clone()).await {
            Ok(out) => Ok(out),
            Err(e) => Err(format!("tool {:?} failed: {e:#}", self.tool)),
        }
    }
}

/// A running set of sensor polling tasks. **Keep this alive** for as long as
/// the sensors should be polled; owned handles let [`Drop`] stop them.
pub struct SensorRuntime {
    tx: UnboundedSender<SensorEvent>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SensorRuntime {
    /// Start one polling task per enabled, non-blank sensor. `tools`/`ctx` back
    /// any tool-based sensor (the same registry and a `ToolContext` the caller
    /// has prepared). Returns the runtime handle (keep it alive) and the receiver
    /// of the events those tasks emit.
    pub fn start(
        sensors: &[SensorCfg],
        tools: Arc<ToolRegistry>,
        ctx: ToolContext,
    ) -> (Self, UnboundedReceiver<SensorEvent>) {
        let (tx, rx) = unbounded_channel();
        let handles = sensors
            .iter()
            .filter(|s| s.is_pollable())
            .map(|cfg| {
                let probe = build_probe(cfg, &tools, &ctx);
                let tx = tx.clone();
                let period = Duration::from_secs(cfg.effective_interval_secs());
                let name = cfg.name.clone();
                let mode = cfg.mode;
                let prompt = cfg.prompt.clone();
                tokio::spawn(poll_sensor(probe, name, mode, prompt, period, tx))
            })
            .collect();
        (Self { tx, handles }, rx)
    }

    /// A sender for the event channel. Holding one keeps the channel open even
    /// when no sensor is configured, so the UI's `recv()` never returns `None`.
    pub fn sender(&self) -> UnboundedSender<SensorEvent> {
        self.tx.clone()
    }
}

impl Drop for SensorRuntime {
    /// Stop every polling task when the runtime is dropped. The App owns the
    /// runtime for the whole session, so this runs at shutdown.
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// Build the probe a sensor polls with: a tool invocation when `tool` is set,
/// otherwise a shell command.
fn build_probe(cfg: &SensorCfg, tools: &Arc<ToolRegistry>, ctx: &ToolContext) -> Box<dyn Probe> {
    match cfg.tool_name() {
        Some(name) => Box::new(ToolProbe {
            tool: name.to_string(),
            args: cfg.args.clone(),
            tools: tools.clone(),
            ctx: ctx.clone(),
        }),
        None => Box::new(CommandProbe {
            command: cfg.command.clone(),
        }),
    }
}

/// Poll one sensor forever, emitting an event whenever its result changes.
async fn poll_sensor(
    probe: Box<dyn Probe>,
    name: String,
    mode: SensorMode,
    prompt: Option<String>,
    period: Duration,
    tx: UnboundedSender<SensorEvent>,
) {
    let mut baseline: Option<String> = None;
    loop {
        match probe.probe().await {
            Ok(output) => match baseline.as_deref() {
                // First successful poll: establish the baseline, emit nothing.
                None => baseline = Some(output),
                Some(prev) if prev != output => {
                    let delta = line_delta(prev, &output);
                    baseline = Some(output);
                    // A change that is only whitespace/blank-line churn is not
                    // worth reporting; the baseline still moves so it is not
                    // re-detected on the next poll.
                    if !delta.is_empty()
                        && tx
                            .send(SensorEvent::Changed {
                                name: name.clone(),
                                mode,
                                prompt: prompt.clone(),
                                delta,
                            })
                            .is_err()
                    {
                        return; // No one is listening any more.
                    }
                }
                Some(_) => {}
            },
            Err(message) => {
                if tx
                    .send(SensorEvent::Error {
                        name: name.clone(),
                        message,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
        tokio::time::sleep(period).await;
    }
}

/// Run `command` through `bash -c` and return its stdout, or a description of
/// why it failed. The command's own stdin is closed so it cannot hang reading
/// from Comrade's terminal.
async fn run_command(command: &str) -> Result<String, String> {
    let command = command.to_string();
    let joined = tokio::task::spawn_blocking(move || {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|e| format!("polling task failed: {e}"))?;

    match joined {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = stderr.trim();
            Err(if stderr.is_empty() {
                format!("command exited with {}", out.status)
            } else {
                format!("command exited with {}: {stderr}", out.status)
            })
        }
        Err(e) => Err(format!("could not run command: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comrade_tool::{Tool, ToolSpec};
    use serde_json::{Value, json};

    fn cfg(name: &str, command: String) -> SensorCfg {
        SensorCfg {
            name: name.into(),
            command,
            interval_secs: 0,
            mode: SensorMode::Ask,
            ..SensorCfg::default()
        }
    }

    /// A probe that returns the contents of a file, so a test can make it
    /// "change" by rewriting the file between polls.
    struct FileProbe {
        path: std::path::PathBuf,
    }
    #[async_trait::async_trait]
    impl Probe for FileProbe {
        async fn probe(&self) -> Result<String, String> {
            std::fs::read_to_string(&self.path).map_err(|e| e.to_string())
        }
    }

    /// A probe that always fails, to exercise the error path.
    struct FailingProbe;
    #[async_trait::async_trait]
    impl Probe for FailingProbe {
        async fn probe(&self) -> Result<String, String> {
            Err("boom".into())
        }
    }

    #[test]
    fn delta_reports_added_lines() {
        let d = line_delta("a\nb\n", "a\nb\nc\n");
        assert_eq!(d.added, vec!["c".to_string()]);
        assert!(d.removed.is_empty());
        assert!(!d.is_empty());
    }

    #[test]
    fn delta_reports_removed_lines() {
        let d = line_delta("a\nb\nc\n", "a\nc\n");
        assert!(d.added.is_empty());
        assert_eq!(d.removed, vec!["b".to_string()]);
    }

    #[test]
    fn delta_reports_both_and_ignores_blank_and_whitespace() {
        let d = line_delta("  a \n\nb\n", "a\n\n c \n");
        assert_eq!(d.added, vec!["c".to_string()]);
        assert_eq!(d.removed, vec!["b".to_string()]);
    }

    #[test]
    fn reordering_is_not_a_change() {
        let d = line_delta("a\nb\n", "b\na\n");
        assert!(d.is_empty(), "reordering must not report a change: {d:?}");
    }

    #[test]
    fn a_tool_sensor_and_a_command_sensor_are_both_pollable() {
        let mut tool = cfg("t", String::new());
        tool.tool = Some("mcp_jira_list".into());
        let cmd = cfg("c", "echo hi".into());
        let mut disabled = cfg("off", "echo hi".into());
        disabled.enabled = false;
        let blank = cfg("blank", "   ".into());
        let sensors = vec![tool, cmd, disabled, blank];
        let names: Vec<&str> = sensors
            .iter()
            .filter(|s| s.is_pollable())
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["t", "c"]);
    }

    #[tokio::test]
    async fn change_is_detected_after_the_baseline() {
        let dir = std::env::temp_dir().join(format!("comrade-proactive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state");
        std::fs::write(&state, "one\n").unwrap();

        let (tx, mut rx) = unbounded_channel();
        let handle = tokio::spawn(poll_sensor(
            Box::new(FileProbe {
                path: state.clone(),
            }),
            "t".into(),
            SensorMode::Ask,
            None,
            Duration::from_millis(30),
            tx,
        ));

        // Let the first poll establish the baseline (it emits nothing).
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            rx.try_recv().is_err(),
            "the baseline poll must not emit an event"
        );

        std::fs::write(&state, "one\ntwo\n").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("a change should be reported in time")
            .expect("the channel stays open");
        match ev {
            SensorEvent::Changed { delta, .. } => {
                assert_eq!(delta.added, vec!["two".to_string()]);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_poll_reports_an_error_not_a_change() {
        let (tx, mut rx) = unbounded_channel();
        let handle = tokio::spawn(poll_sensor(
            Box::new(FailingProbe),
            "bad".into(),
            SensorMode::Ask,
            None,
            Duration::from_millis(30),
            tx,
        ));
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("an error should be reported in time")
            .expect("the channel stays open");
        match ev {
            SensorEvent::Error { message, .. } => assert_eq!(message, "boom"),
            other => panic!("unexpected event: {other:?}"),
        }
        handle.abort();
    }

    /// A tool-based sensor invokes the registered tool and watches its result.
    #[tokio::test]
    async fn a_tool_sensor_invokes_the_registered_tool() {
        struct TicketTool;
        static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
            name: "mcp_jira_sprint".into(),
            description: "list the sprint's tickets".into(),
            json_schema: json!({"type": "object"}),
        });
        #[async_trait::async_trait]
        impl Tool for TicketTool {
            fn spec(&self) -> &ToolSpec {
                &SPEC
            }
            async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> anyhow::Result<String> {
                Ok("PROJ-1 open\nPROJ-2 open".into())
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(Box::new(TicketTool));
        let probe = ToolProbe {
            tool: "mcp_jira_sprint".into(),
            args: json!({}),
            tools: Arc::new(reg),
            ctx: test_ctx(),
        };
        let out = probe.probe().await.expect("the tool runs");
        assert!(out.contains("PROJ-1"), "{out}");
    }

    /// A tool-based sensor pointing at an unknown tool reports an error rather
    /// than silently watching nothing.
    #[tokio::test]
    async fn a_tool_sensor_with_an_unknown_tool_errors() {
        let probe = ToolProbe {
            tool: "does_not_exist".into(),
            args: json!({}),
            tools: Arc::new(ToolRegistry::new()),
            ctx: test_ctx(),
        };
        let err = probe.probe().await.unwrap_err();
        assert!(err.contains("no tool named"), "{err}");
    }

    /// A minimal [`ToolContext`] for the tests above.
    pub(crate) fn test_ctx() -> ToolContext {
        struct NoopIo;
        #[async_trait::async_trait]
        impl comrade_tool::UserIo for NoopIo {
            async fn ask(
                &self,
                _p: comrade_tool::UserPrompt,
            ) -> anyhow::Result<comrade_tool::UserReply> {
                Ok(comrade_tool::UserReply::Answer(String::new()))
            }
        }
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            project_root: "/tmp/x".into(),
            cwd: "/tmp/x".into(),
            session: Arc::new(comrade_core::AgentSession::new(tx)).as_control(),
            user: Arc::new(NoopIo),
            undo: Arc::new(comrade_core::MemoryUndo::new("/tmp/x".into())),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        }
    }
}
