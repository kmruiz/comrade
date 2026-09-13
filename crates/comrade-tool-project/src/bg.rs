//! Background process tools: `run_bg`, `bg_status`, `bg_tail`, `bg_kill`.
//!
//! Long-running commands (slow builds, test suites, dev servers) can be started
//! detached so the agent keeps working while they run. Each job's combined
//! stdout+stderr is captured into a bounded buffer that `bg_status`/`bg_tail`
//! read and `bg_kill` stops. Jobs die with the app (the child is `kill_on_drop`).

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

/// Keep at most this many chars of a job's combined output; older output is
/// dropped from the front so a chatty server cannot exhaust memory.
const MAX_JOB_OUTPUT: usize = 200_000;

/// Lifecycle state of a background job.
#[derive(Debug, Clone)]
enum BgStatus {
    Running,
    Exited(i32),
    Failed(String),
}

impl BgStatus {
    fn label(&self) -> String {
        match self {
            BgStatus::Running => "running".into(),
            BgStatus::Exited(0) => "done".into(),
            BgStatus::Exited(c) => format!("failed (exit {c})"),
            BgStatus::Failed(e) => format!("failed to start ({e})"),
        }
    }
}

struct Job {
    id: String,
    command: String,
    started: Instant,
    status: BgStatus,
    output: String,
    cancel: CancellationToken,
}

/// Shared registry of background jobs, cloned into every bg tool so the four
/// tools see one another's jobs.
struct BgHub {
    jobs: Mutex<HashMap<String, Job>>,
    next: AtomicU64,
}

impl BgHub {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Job>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append `text` to a job's output, trimming the front past the cap.
    fn append(&self, id: &str, text: &str) {
        let mut jobs = self.lock();
        if let Some(job) = jobs.get_mut(id) {
            job.output.push_str(text);
            if job.output.len() > MAX_JOB_OUTPUT {
                let mut cut = job.output.len() - MAX_JOB_OUTPUT;
                while cut < job.output.len() && !job.output.is_char_boundary(cut) {
                    cut += 1;
                }
                job.output = format!("... (earlier output trimmed)\n{}", &job.output[cut..]);
            }
        }
    }

    fn set_status(&self, id: &str, status: BgStatus) {
        if let Some(job) = self.lock().get_mut(id) {
            job.status = status;
        }
    }

    fn start(&self, command: &str) -> String {
        let id = format!("bg-{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.lock().insert(
            id.clone(),
            Job {
                id: id.clone(),
                command: command.to_string(),
                started: Instant::now(),
                status: BgStatus::Running,
                output: String::new(),
                cancel: CancellationToken::new(),
            },
        );
        id
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort_by_key(|s| {
            s.trim_start_matches("bg-")
                .parse::<u64>()
                .unwrap_or(u64::MAX)
        });
        ids
    }

    /// One job rendered as `id | command | status | elapsed`, plus its tail.
    fn render_job(&self, id: &str, lines: usize) -> Option<String> {
        let jobs = self.lock();
        let job = jobs.get(id)?;
        let mut out = format!(
            "{} | {} | {} | {:.1}s\n",
            job.id,
            job.command,
            job.status.label(),
            job.started.elapsed().as_secs_f32()
        );
        let body = tail(&job.output, lines);
        if body.is_empty() {
            out.push_str("(no output yet)\n");
        } else {
            out.push_str(&body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        }
        Some(out)
    }

    fn render_list(&self) -> String {
        let ids = self.ids();
        if ids.is_empty() {
            return "no background jobs".to_string();
        }
        let jobs = self.lock();
        let mut out = format!("{} background job(s):\n", ids.len());
        for id in ids {
            if let Some(job) = jobs.get(&id) {
                out.push_str(&format!(
                    "{} | {} | {} | {:.1}s\n",
                    job.id,
                    job.command,
                    job.status.label(),
                    job.started.elapsed().as_secs_f32()
                ));
            }
        }
        out
    }
}

/// The last `lines` lines of `output`.
fn tail(output: &str, lines: usize) -> String {
    let all: Vec<&str> = output.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// One background job as seen by an outside observer (the TUI's job panel and
/// stop command): a snapshot, not a live handle.
#[derive(Debug, Clone)]
pub struct BgJobInfo {
    pub id: String,
    pub command: String,
    /// Human status label, e.g. "running", "done", "failed (exit 1)".
    pub status: String,
    pub running: bool,
    pub elapsed_secs: f32,
}

/// A cheap, cloneable handle to the shared background-job registry, for
/// observers outside the tools (e.g. the TUI). Cloning shares the same jobs.
#[derive(Clone)]
pub struct BgJobs {
    hub: Arc<BgHub>,
}

impl Default for BgJobs {
    fn default() -> Self {
        Self::new()
    }
}

impl BgJobs {
    /// A fresh, empty registry.
    pub fn new() -> Self {
        Self { hub: BgHub::new() }
    }

    /// Every job, oldest first.
    pub fn list(&self) -> Vec<BgJobInfo> {
        // Take the ids (which locks/unlocks) BEFORE the read lock below: `ids()`
        // locks the same mutex, so nesting the two would deadlock.
        let ids = self.hub.ids();
        let jobs = self.hub.lock();
        ids.into_iter()
            .filter_map(|id| {
                jobs.get(&id).map(|j| BgJobInfo {
                    id: j.id.clone(),
                    command: j.command.clone(),
                    status: j.status.label(),
                    running: matches!(j.status, BgStatus::Running),
                    elapsed_secs: j.started.elapsed().as_secs_f32(),
                })
            })
            .collect()
    }

    /// Only the jobs still running, oldest first.
    pub fn running(&self) -> Vec<BgJobInfo> {
        self.list().into_iter().filter(|j| j.running).collect()
    }

    /// Request that `id` be killed. Returns `false` when no such job exists.
    pub fn kill(&self, id: &str) -> bool {
        match self.hub.lock().get(id) {
            Some(job) => {
                job.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/// All four background-process tools sharing one job registry.
pub fn tools(jobs: &BgJobs) -> Vec<Box<dyn Tool>> {
    let hub = jobs.hub.clone();
    vec![
        Box::new(RunBg { hub: hub.clone() }),
        Box::new(BgStatusTool { hub: hub.clone() }),
        Box::new(BgTail { hub: hub.clone() }),
        Box::new(BgKill { hub }),
    ]
}

/// Start the job process in `cwd` and wire up its output readers + monitor.
async fn spawn_job(hub: &Arc<BgHub>, cwd: &Path, id: &str, command: &str) -> Result<()> {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            hub.set_status(id, BgStatus::Failed(e.to_string()));
            anyhow::bail!("failed to start job: {e}");
        }
    };
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, hub.clone(), id.to_string());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, hub.clone(), id.to_string());
    }
    let cancel = hub
        .lock()
        .get(id)
        .map(|j| j.cancel.clone())
        .context("job vanished")?;
    spawn_monitor(child, hub.clone(), id.to_string(), cancel);
    Ok(())
}

/// Drain an async reader into the job's output buffer.
fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(mut reader: R, hub: Arc<BgHub>, id: String) {
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    hub.append(&id, &text);
                }
            }
        }
    });
}

/// Wait for the child to exit (or for a kill request) and record the outcome.
fn spawn_monitor(
    mut child: tokio::process::Child,
    hub: Arc<BgHub>,
    id: String,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let outcome = tokio::select! {
            _ = cancel.cancelled() => None,
            res = child.wait() => Some(res),
        };
        let status = match outcome {
            Some(Ok(s)) => BgStatus::Exited(s.code().unwrap_or(-1)),
            Some(Err(e)) => BgStatus::Failed(e.to_string()),
            None => {
                let _ = child.start_kill();
                match child.wait().await {
                    Ok(s) => BgStatus::Exited(s.code().unwrap_or(-1)),
                    Err(e) => BgStatus::Failed(e.to_string()),
                }
            }
        };
        hub.set_status(&id, status);
    });
}

// ---------------------------------------------------------------------------
// run_bg
// ---------------------------------------------------------------------------

struct RunBg {
    hub: Arc<BgHub>,
}

static RUN_BG_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
        name: "run_bg".into(),
        description: "Start a long-running shell command in the background and return a job id. Use for slow builds/tests/servers so you can keep working; check it with bg_status/bg_tail and stop it with bg_kill. Approval-gated.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run (via bash -c) in the project root." }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }
});

#[async_trait]
impl Tool for RunBg {
    fn spec(&self) -> &ToolSpec {
        &RUN_BG_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let command = args.command.trim();
        if command.is_empty() {
            anyhow::bail!("`command` must not be empty");
        }
        comrade_tool::check_command(command, &comrade_tool::policy())?;
        ctx.confirm(format!("Start background job: {command}"), None)
            .await?;
        let id = self.hub.start(command);
        spawn_job(&self.hub, &ctx.project_root, &id, command).await?;
        Ok(format!(
            "started {id}: {command}\nUse bg_status id={id} (or bg_tail) to check it, bg_kill to stop it."
        ))
    }
}

// ---------------------------------------------------------------------------
// bg_status
// ---------------------------------------------------------------------------

struct BgStatusTool {
    hub: Arc<BgHub>,
}

static BG_STATUS_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
        name: "bg_status".into(),
        description: "Show one background job's status, elapsed time and the tail of its output, or list every job. Poll a job started with run_bg to see if it finished.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Job id, e.g. bg-1. Omit to list all jobs." },
                "lines": { "type": "integer", "default": 40, "minimum": 1, "maximum": 1000, "description": "Lines of output to show for one job." }
            },
            "additionalProperties": false
        }),
    }
});

#[async_trait]
impl Tool for BgStatusTool {
    fn spec(&self) -> &ToolSpec {
        &BG_STATUS_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            id: Option<String>,
            #[serde(default = "default_lines")]
            lines: usize,
        }
        fn default_lines() -> usize {
            40
        }
        let args: Args = serde_json::from_value(args)?;
        match args.id {
            Some(id) => self
                .hub
                .render_job(&id, args.lines)
                .ok_or_else(|| anyhow::anyhow!("unknown job {id:?}")),
            None => Ok(self.hub.render_list()),
        }
    }
}

// ---------------------------------------------------------------------------
// bg_tail
// ---------------------------------------------------------------------------

struct BgTail {
    hub: Arc<BgHub>,
}

static BG_TAIL_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
        name: "bg_tail".into(),
        description: "Show the last N lines of a background job's output. Use to read a job's latest progress without re-listing.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Job id, e.g. bg-1." },
                "lines": { "type": "integer", "default": 100, "minimum": 1, "maximum": 2000, "description": "Number of trailing lines to show." }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
    }
});

#[async_trait]
impl Tool for BgTail {
    fn spec(&self) -> &ToolSpec {
        &BG_TAIL_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
            #[serde(default = "default_lines")]
            lines: usize,
        }
        fn default_lines() -> usize {
            100
        }
        let args: Args = serde_json::from_value(args)?;
        let jobs = self.hub.lock();
        let job = jobs
            .get(&args.id)
            .ok_or_else(|| anyhow::anyhow!("unknown job {:?}", args.id))?;
        Ok(format!(
            "{} ({})\n{}",
            job.id,
            job.status.label(),
            tail(&job.output, args.lines)
        ))
    }
}

// ---------------------------------------------------------------------------
// bg_kill
// ---------------------------------------------------------------------------

struct BgKill {
    hub: Arc<BgHub>,
}

static BG_KILL_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
    name: "bg_kill".into(),
    description: "Stop a running background job started with run_bg (request its child be killed)."
        .into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Job id, e.g. bg-1." }
        },
        "required": ["id"],
        "additionalProperties": false
    }),
});

#[async_trait]
impl Tool for BgKill {
    fn spec(&self) -> &ToolSpec {
        &BG_KILL_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let jobs = self.hub.lock();
        let job = jobs
            .get(&args.id)
            .ok_or_else(|| anyhow::anyhow!("unknown job {:?}", args.id))?;
        job.cancel.cancel();
        Ok(format!("kill requested for {}", args.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-bg-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn wait_over(hub: &Arc<BgHub>, id: &str) {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if !matches!(hub.lock().get(id).unwrap().status, BgStatus::Running) {
                return;
            }
        }
        panic!("job {id} never finished");
    }

    #[test]
    fn tail_returns_last_lines() {
        assert_eq!(tail("a\nb\nc\nd\n", 2), "c\nd");
        assert_eq!(tail("a\nb\nc\nd\n", 10), "a\nb\nc\nd");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn hub_trims_oversized_output() {
        let hub = BgHub::new();
        let id = hub.start("echo hi");
        hub.append(&id, &"x".repeat(MAX_JOB_OUTPUT + 1000));
        let len = hub.lock().get(&id).unwrap().output.len();
        assert!(len <= MAX_JOB_OUTPUT + 64, "len={len}");
        assert!(
            hub.lock()
                .get(&id)
                .unwrap()
                .output
                .starts_with("... (earlier output trimmed)")
        );
    }

    #[tokio::test]
    async fn job_captures_output_and_completes() {
        let dir = scratch();
        let hub = BgHub::new();
        let id = hub.start("echo hello-bg; echo oops 1>&2");
        spawn_job(&hub, &dir, &id, "echo hello-bg; echo oops 1>&2")
            .await
            .unwrap();
        wait_over(&hub, &id).await;
        let out = hub.render_job(&id, 100).unwrap();
        assert!(out.contains("hello-bg"), "{out}");
        assert!(out.contains("oops"), "{out}");
        assert!(out.contains("done"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn kill_stops_a_running_job() {
        let dir = scratch();
        let hub = BgHub::new();
        let id = hub.start("sleep 30");
        spawn_job(&hub, &dir, &id, "sleep 30").await.unwrap();
        hub.lock().get(&id).unwrap().cancel.cancel();
        wait_over(&hub, &id).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bgjobs_lists_and_kills() {
        let dir = scratch();
        let jobs = BgJobs::new();
        let id = jobs.hub.start("sleep 30");
        spawn_job(&jobs.hub, &dir, &id, "sleep 30").await.unwrap();

        // The observer sees the running job.
        let running = jobs.running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, id);
        assert!(running[0].running);

        // Kill through the handle, then the job leaves the running set.
        assert!(jobs.kill(&id));
        assert!(!jobs.kill("bg-does-not-exist"));
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if jobs.running().is_empty() {
                break;
            }
        }
        assert!(jobs.running().is_empty(), "job did not stop");
        // The job is still listed (with a terminal status), just not running.
        assert_eq!(jobs.list().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
