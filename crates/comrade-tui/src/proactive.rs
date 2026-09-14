//! Proactive mode: sensors poll a shell command on an interval and report
//! changes so Comrade can react — notify the human, or (in `auto` mode) open a
//! session to handle the change itself.
//!
//! A sensor is a `[[sensors]]` entry in `config.toml` (see
//! [`comrade_core::SensorCfg`]). Its `command` is run via `bash -c`; the first
//! successful run only establishes a *baseline* and emits nothing. Every later
//! run whose stdout differs from the baseline emits a [`SensorEvent::Changed`]
//! carrying the line diff; a non-zero exit emits a [`SensorEvent::Error`]
//! without disturbing the baseline.
//!
//! The change detection itself ([`line_delta`]) is pure and unit-tested; only
//! the polling wrapper touches the clock and the process table.

use std::process::Stdio;
use std::time::Duration;

use comrade_core::{SensorCfg, SensorMode};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The difference between two snapshots of a command's stdout.
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
    /// The sensor's output changed since the last poll.
    Changed {
        /// The sensor's configured name.
        name: String,
        /// What the agent should do about it (notify+ask, or handle it).
        mode: SensorMode,
        /// Optional seed prompt for the session Comrade opens.
        prompt: Option<String>,
        /// The line diff that triggered this event.
        delta: Delta,
        /// The full new output, for context.
        raw: String,
    },
    /// The sensor's command failed (non-zero exit or could not be spawned).
    Error {
        /// The sensor's configured name.
        name: String,
        /// A human-readable failure description.
        message: String,
    },
    /// A confirmed `ask`-mode notification: the human said yes, so the UI should
    /// open a session for it. Never emitted by a sensor task itself — the UI
    /// feeds it back into the same channel after the human confirms.
    Confirmed {
        /// The sensor's configured name.
        name: String,
        /// Optional seed prompt for the session Comrade opens.
        prompt: Option<String>,
        /// The line diff the notification was about.
        delta: Delta,
    },
}

/// A running set of sensor polling tasks. **Keep this alive** for as long as
/// the sensors should be polled; dropping it does not abort the tasks outright
/// (they stop on their own once the event receiver is dropped), but the handles
/// are how the tasks are owned.
pub struct SensorRuntime {
    tx: UnboundedSender<SensorEvent>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SensorRuntime {
    /// Start one polling task per enabled sensor. Returns the runtime handle
    /// (keep it alive) and the receiver of the events those tasks emit.
    pub fn start(sensors: &[SensorCfg]) -> (Self, UnboundedReceiver<SensorEvent>) {
        let (tx, rx) = unbounded_channel();
        let handles = active(sensors)
            .into_iter()
            .map(|cfg| {
                let cfg = cfg.clone();
                let tx = tx.clone();
                let period = Duration::from_secs(cfg.effective_interval_secs());
                tokio::spawn(poll_sensor(cfg, period, tx))
            })
            .collect();
        (Self { tx, handles }, rx)
    }

    /// A sender for the event channel. Holding one keeps the channel open even
    /// when no sensor is configured, so the UI's `recv()` never returns `None`;
    /// the UI also uses it to feed a confirmed action back into the loop.
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

/// The sensors that should actually be polled: enabled and with a command.
fn active(sensors: &[SensorCfg]) -> Vec<&SensorCfg> {
    sensors
        .iter()
        .filter(|s| s.enabled && !s.command.trim().is_empty())
        .collect()
}

/// Poll one sensor forever, emitting an event whenever its output changes.
async fn poll_sensor(cfg: SensorCfg, period: Duration, tx: UnboundedSender<SensorEvent>) {
    let mut baseline: Option<String> = None;
    loop {
        match run_command(&cfg.command).await {
            Ok(output) => match baseline.as_deref() {
                // First successful run: establish the baseline, emit nothing.
                None => baseline = Some(output),
                Some(prev) if prev != output => {
                    let delta = line_delta(prev, &output);
                    baseline = Some(output.clone());
                    // A change that is only whitespace/blank-line churn is not
                    // worth reporting; the baseline still moves so it is not
                    // re-detected on the next poll.
                    if !delta.is_empty()
                        && tx
                            .send(SensorEvent::Changed {
                                name: cfg.name.clone(),
                                mode: cfg.mode,
                                prompt: cfg.prompt.clone(),
                                delta,
                                raw: output,
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
                        name: cfg.name.clone(),
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

    fn cfg(name: &str, command: String) -> SensorCfg {
        SensorCfg {
            name: name.into(),
            command,
            interval_secs: 0,
            mode: SensorMode::Ask,
            prompt: None,
            enabled: true,
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
    fn only_enabled_sensors_with_commands_are_active() {
        let mut disabled = cfg("off", "echo hi".into());
        disabled.enabled = false;
        let blank = cfg("blank", "   ".into());
        let on = cfg("on", "echo hi".into());
        let sensors = vec![disabled, blank, on];
        let names: Vec<&str> = active(&sensors).iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["on"]);
    }

    #[tokio::test]
    async fn change_is_detected_after_the_baseline() {
        let dir = std::env::temp_dir().join(format!("comrade-proactive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state");
        std::fs::write(&state, "one\n").unwrap();

        let (tx, mut rx) = unbounded_channel();
        let handle = tokio::spawn(poll_sensor(
            cfg("t", format!("cat '{}'", state.display())),
            Duration::from_millis(30),
            tx,
        ));

        // Let the first run establish the baseline (it emits nothing).
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            rx.try_recv().is_err(),
            "the baseline run must not emit an event"
        );

        std::fs::write(&state, "one\ntwo\n").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("a change should be reported in time")
            .expect("the channel stays open");
        match ev {
            SensorEvent::Changed { delta, raw, .. } => {
                assert_eq!(delta.added, vec!["two".to_string()]);
                assert!(raw.contains("two"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_command_reports_an_error_not_a_change() {
        let (tx, mut rx) = unbounded_channel();
        let handle = tokio::spawn(poll_sensor(
            cfg("bad", "exit 3".into()),
            Duration::from_millis(30),
            tx,
        ));
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("an error should be reported in time")
            .expect("the channel stays open");
        assert!(matches!(ev, SensorEvent::Error { .. }), "got {ev:?}");
        handle.abort();
    }
}
