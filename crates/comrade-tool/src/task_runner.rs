//! The `TaskRunner` contract: run a named project task (a standard build-system
//! verb or a configured alias) and hand back its **uncapped** output.
//!
//! This lives in `comrade-tool` so `comrade-core` never has to depend on the
//! concrete tool crates: the `summarise` tool accepts an
//! `Option<Arc<dyn TaskRunner>>` and the project tool crate (`comrade-tool-project`)
//! supplies the real implementation. The `pom_*` tools cap their own output;
//! `summarise` needs the whole body to summarise, hence the dedicated contract.

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;

/// The result of running a task to completion, with the UNCAPPED combined
/// stdout+stderr.
#[derive(Debug, Clone)]
pub struct TaskRun {
    /// Human-readable command line that was run (the ecosystem's `describe`).
    pub describe: String,
    /// Whether the process exited successfully.
    pub success: bool,
    /// Process exit code (`-1` when terminated by a signal).
    pub code: i32,
    /// Combined stdout+stderr, uncapped.
    pub body: String,
    /// Wall-clock time the task took.
    pub elapsed: Duration,
}

/// Runs a named project task and returns its uncapped output. Implemented by
/// `comrade-tool-project` and injected into tools in `comrade-core` so the core
/// stays agnostic to concrete tool crates.
#[async_trait]
pub trait TaskRunner: Send + Sync {
    /// Resolve `task` (a standard verb or a configured alias) in `root`'s build
    /// ecosystem, optionally scoped to `subproject`, and run it to completion.
    /// `ecosystem` selects a backend in a polyglot repo; `extra` are extra args
    /// appended to the command (already split into argv tokens).
    async fn run_task(
        &self,
        root: &Path,
        task: &str,
        subproject: Option<&str>,
        ecosystem: Option<&str>,
        extra: &[String],
        timeout_secs: u64,
    ) -> Result<TaskRun>;
}
