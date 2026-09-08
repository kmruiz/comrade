//! Project object model (POM) and task-runner tools for Cargo projects.
//!
//! - `project_model` describes the workspace/project + subprojects (Cargo
//!   workspace members), their dependencies, and available tasks — the Comrade
//!   analogue of a Maven POM (project + modules).
//! - `run_task` executes a named task (a cargo verb, a cargo alias, or a
//!   `!shell` alias) optionally scoped to one subproject.

mod pom;
mod tasks;

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub use pom::{ProjectModel, load as load_model, render as render_model};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ProjectModelTool),
        Box::new(RunTask),
        Box::new(FormatCode),
        Box::new(RunTests),
        Box::new(Shell),
    ]
}

// ---------------------------------------------------------------------------
// project_model
// ---------------------------------------------------------------------------

struct ProjectModelTool;

static PROJECT_MODEL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "project_model".into(),
    description: "Report this Cargo project's dependencies, subprojects, and tasks (the project object model, POM). Use this INSTEAD of reading Cargo.toml files whenever the user asks about dependencies, crates/modules, the workspace layout, or what tasks can be run. Read-only.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ProjectModelTool {
    fn spec(&self) -> &ToolSpec {
        &PROJECT_MODEL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        let model = pom::load(&ctx.project_root)?;
        Ok(pom::render(&model))
    }
}

// ---------------------------------------------------------------------------
// run_task
// ---------------------------------------------------------------------------

struct RunTask;

static RUN_TASK_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "run_task".into(),
    description: "Run a named project task and return its output. Tasks come from project_model: cargo verbs (build, run, check, test, clippy, fmt, doc, bench, release) and aliases defined in .cargo/config.toml (!-prefixed aliases run as shell). Optionally scope to a subproject with its directory (relative to the root). Runs directly without approval.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "task": { "type": "string", "description": "Task name, e.g. \"test\" or a cargo alias." },
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/app\"." },
            "args": { "type": "string", "description": "Extra arguments appended to the command." },
            "timeout_secs": { "type": "integer", "minimum": 1, "default": 600, "description": "Kill the task after this many seconds." }
        },
        "required": ["task"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RunTask {
    fn spec(&self) -> &ToolSpec {
        &RUN_TASK_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            task: String,
            #[serde(default)]
            subproject: Option<String>,
            #[serde(default)]
            args: Option<String>,
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
        }
        fn default_timeout() -> u64 {
            600
        }
        let args: Args = serde_json::from_value(args)?;

        let extra: Vec<String> = args
            .args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();

        let resolved = tasks::resolve(&ctx.project_root, &args.task, &args.subproject, &extra)?;
        let output = tasks::run(&resolved, args.timeout_secs).await?;
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// format_code
// ---------------------------------------------------------------------------

struct FormatCode;

static FORMAT_CODE_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "format_code".into(),
    description: "Run the project formatter (cargo fmt --all) so code is formatted deterministically instead of hand-formatting tokens. Runs directly without approval (rustfmt only rewrites whitespace).".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for FormatCode {
    fn spec(&self) -> &ToolSpec {
        &FORMAT_CODE_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        // Rustfmt is a deterministic, whitespace-only rewrite, so it runs
        // directly (no human approval) like run_tests/run_task.
        let resolved = tasks::Resolved {
            cwd: ctx.project_root.clone(),
            line: tasks::CommandLine::Cargo {
                args: vec!["fmt".into(), "--all".into()],
            },
            describe: "cargo fmt --all".to_string(),
        };
        tasks::run(&resolved, 300).await
    }
}

// ---------------------------------------------------------------------------
// run_tests
// ---------------------------------------------------------------------------

struct RunTests;

static RUN_TESTS_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "run_tests".into(),
    description: "Run the project's tests (cargo test) and return a SIMPLIFIED summary the model can read: pass/fail totals, failing test names and key error lines - build noise is filtered out. Use to verify work instead of reading code to reason about correctness. Runs directly without approval.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/app\"." },
            "args": { "type": "string", "description": "Extra cargo test arguments, e.g. \"--lib\" or a test filter." },
            "timeout_secs": { "type": "integer", "minimum": 1, "default": 600, "description": "Kill after this many seconds." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RunTests {
    fn spec(&self) -> &ToolSpec {
        &RUN_TESTS_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            subproject: Option<String>,
            #[serde(default)]
            args: Option<String>,
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
        }
        fn default_timeout() -> u64 {
            600
        }
        let args: Args = serde_json::from_value(args)?;
        let extra: Vec<String> = args
            .args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        let resolved = tasks::resolve(&ctx.project_root, "test", &args.subproject, &extra)?;
        let raw = tasks::run(&resolved, args.timeout_secs).await?;
        Ok(simplify_test_output(&raw))
    }
}

/// Reduce raw `cargo test` output to a readable summary for the model: keep
/// totals, failing-test sections and their detail lines; drop compile/build
/// noise and the per-test "... ok" lines.
fn simplify_test_output(raw: &str) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    for line in raw.lines() {
        let t = line.trim_start();
        if t.is_empty()
            || t.starts_with("Compiling")
            || t.starts_with("Finished")
            || t.starts_with("Running")
            || t.starts_with("Doc-tests")
            || t.starts_with("warning: ")
            || t.contains("running 0 tests")
        {
            continue;
        }
        // skip individual passing tests ("test foo ... ok")
        if let Some(rest) = t.strip_prefix("test ") {
            if rest.ends_with(" ... ok") {
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
        kept += 1;
        if kept > 200 {
            out.push_str("... (output trimmed)\n");
            break;
        }
    }
    if out.trim().is_empty() {
        "test run produced no summary lines (check timeout or exit code)".to_string()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

struct Shell;

static SHELL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "shell".into(),
    description: "Run an arbitrary shell command via bash in the project (optionally in a subdirectory) and return its output. Powerful: use only when no dedicated tool fits. Approval-gated: include Justification and Risk.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Shell command to run." },
            "dir": { "type": "string", "description": "Optional directory relative to the project root to run in." },
            "timeout_secs": { "type": "integer", "minimum": 1, "default": 300, "description": "Kill after this many seconds." }
        },
        "required": ["command"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &SHELL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
            #[serde(default)]
            dir: Option<String>,
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
        }
        fn default_timeout() -> u64 {
            300
        }
        let args: Args = serde_json::from_value(args)?;
        if args.command.trim().is_empty() {
            anyhow::bail!("command must not be empty");
        }
        let cwd = match &args.dir {
            Some(dir) => {
                let dir = dir.trim_end_matches('/');
                if dir.is_empty() || dir == "." {
                    ctx.project_root.clone()
                } else {
                    let path = ctx.project_root.join(dir);
                    if !path.starts_with(&ctx.project_root) || dir.contains("..") {
                        anyhow::bail!("dir {dir:?} escapes the project root");
                    }
                    path
                }
            }
            None => ctx.project_root.clone(),
        };
        run_approved_line(
            ctx,
            cwd,
            &args.command,
            tasks::CommandLine::Shell {
                script: args.command.clone(),
            },
            args.timeout_secs,
        )
        .await
    }
}

/// Shared runner: ask for approval then execute an arbitrary command line.
async fn run_approved_line(
    ctx: &ToolContext,
    cwd: std::path::PathBuf,
    describe: &str,
    line: tasks::CommandLine,
    timeout_secs: u64,
) -> Result<String> {
    let resolved = tasks::Resolved {
        cwd,
        line,
        describe: describe.to_string(),
    };
    ctx.confirm(format!("run: {describe}"), None).await?;
    tasks::run(&resolved, timeout_secs).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_is_simplified() {
        let raw = "\
   Compiling comrade-core v0.1.0
    Finished `dev` profile [unoptimized + debuginfo]
     Running unittests src/lib.rs
running 12 tests
test foo ... ok
test bar ... FAILED

failures:
    bar

---- bar stdout ----
thread 'bar' panicked at src/lib.rs:10
note: run with `RUST_BACKTRACE=1` to see the stack trace

failures:
    bar
test result: FAILED. 11 passed; 1 failed; 0 ignored
";
        let out = simplify_test_output(raw);
        assert!(out.contains("test result: FAILED"));
        assert!(out.contains("11 passed"));
        assert!(out.contains("panicked at"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Finished"));
    }
}
