//! Project object model (POM) and task-runner tools.
//!
//! - `pom_model` describes the workspace/project + subprojects (workspace
//!   members), their dependencies, and available tasks — the Comrade
//!   analogue of a Maven POM (project + modules).
//! - `pom_run_task` executes a named task (a standard build-system task, a
//!   configured alias, or a `!shell` alias) optionally scoped to one subproject.

mod bg;
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
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(PomModelTool),
        Box::new(PomRunTask),
        Box::new(PomFormatCode),
        Box::new(PomRunTests),
        Box::new(PomCheck),
        Box::new(Shell),
    ];
    tools.extend(bg::tools());
    tools
}

// ---------------------------------------------------------------------------
// pom_model
// ---------------------------------------------------------------------------

struct PomModelTool;

static POM_MODEL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_model".into(),
    description: "Report this project's dependencies, subprojects, layout and tasks (the POM). Use INSTEAD of reading the project manifest (e.g. Cargo.toml) when asked about deps/modules/tasks. Read-only.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for PomModelTool {
    fn spec(&self) -> &ToolSpec {
        &POM_MODEL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        let model = pom::load(&ctx.project_root)?;
        Ok(pom::render(&model))
    }
}

// ---------------------------------------------------------------------------
// pom_run_task
// ---------------------------------------------------------------------------

struct PomRunTask;

static POM_RUN_TASK_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_run_task".into(),
    description: "Run a named project task and return its output: standard project tasks (build, run, check, clippy, fmt, doc, bench, release) and configured aliases; optionally scope to a subproject. Runs directly without approval. Run tests with pom_run_tests, not here - it returns only the failure summary and costs far less context.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "task": { "type": "string", "description": "Task name, e.g. \"build\" or a configured alias. Use pom_run_tests to run tests." },
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
impl Tool for PomRunTask {
    fn spec(&self) -> &ToolSpec {
        &POM_RUN_TASK_SPEC
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
        if args.task.trim() == "test" {
            anyhow::bail!(
                "running tests through pom_run_task is disabled - use the pom_run_tests tool instead \
                 (it returns only failing tests and costs far less context)"
            );
        }
        let extra: Vec<String> = args
            .args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();

        let resolved = tasks::resolve(&ctx.project_root, &args.task, &args.subproject, &extra)?;
        ensure_not_test_run(&resolved.line)?;
        let output = tasks::run(&resolved, args.timeout_secs).await?;
        Ok(output)
    }
}

/// Refuse a resolved pom_run_task command that would execute `cargo test` (covers
/// the literal `test` verb and aliases that expand to it). Tests belong to the
/// dedicated `pom_run_tests` tool, whose output is a compact failure summary.
fn ensure_not_test_run(line: &tasks::CommandLine) -> anyhow::Result<()> {
    if let tasks::CommandLine::Cargo { args } = line
        && args.first().map(String::as_str) == Some("test")
    {
        anyhow::bail!(
            "running tests through pom_run_task is disabled - use the pom_run_tests tool instead \
             (it returns only failing tests and costs far less context)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pom_format_code
// ---------------------------------------------------------------------------

struct PomFormatCode;

static POM_FORMAT_CODE_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_format_code".into(),
    description: "Run the project's formatter so code is formatted deterministically instead of hand-formatting tokens. Runs directly without approval (whitespace-only rewrites).".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for PomFormatCode {
    fn spec(&self) -> &ToolSpec {
        &POM_FORMAT_CODE_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        // A deterministic, whitespace-only rewrite, so it runs
        // directly (no human approval) like pom_run_tests/pom_run_task.
        let resolved = tasks::Resolved {
            cwd: ctx.project_root.clone(),
            line: tasks::CommandLine::Cargo {
                args: vec!["fmt".into(), "--all".into()],
            },
            describe: "format all code".to_string(),
        };
        tasks::run(&resolved, 300).await
    }
}

// ---------------------------------------------------------------------------
// pom_run_tests
// ---------------------------------------------------------------------------

struct PomRunTests;

static POM_RUN_TESTS_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_run_tests".into(),
    description: "Run the project's tests and return a SIMPLIFIED summary the model can read: pass/fail totals, failing test names, key error lines. Use to verify work instead of reasoning about code.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/app\"." },
            "args": { "type": "string", "description": "Extra test-runner arguments, e.g. \"--lib\" or a test filter." },
            "timeout_secs": { "type": "integer", "minimum": 1, "default": 600, "description": "Kill after this many seconds." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for PomRunTests {
    fn spec(&self) -> &ToolSpec {
        &POM_RUN_TESTS_SPEC
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
        if let Some(rest) = t.strip_prefix("test ")
            && rest.ends_with(" ... ok")
        {
            continue;
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
// pom_check
// ---------------------------------------------------------------------------

struct PomCheck;

static POM_CHECK_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_check".into(),
    description: "Type-check the project (cargo check) and return the first `max_errors` compiler errors with their file:line:col plus the total count. Much cheaper than pom_run_tests for iterating on compile errors; pass `all_targets: true` to also check tests/examples.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/comrade-core\"." },
            "all_targets": { "type": "boolean", "default": false, "description": "Also check tests, examples and benches (cargo check --all-targets)." },
            "args": { "type": "string", "description": "Extra cargo-check arguments (e.g. \"--features foo\")." },
            "max_errors": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20, "description": "Show at most this many errors." },
            "timeout_secs": { "type": "integer", "minimum": 1, "default": 600, "description": "Kill after this many seconds." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for PomCheck {
    fn spec(&self) -> &ToolSpec {
        &POM_CHECK_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            subproject: Option<String>,
            #[serde(default)]
            all_targets: bool,
            #[serde(default)]
            args: Option<String>,
            #[serde(default = "default_max_errors")]
            max_errors: usize,
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
        }
        fn default_max_errors() -> usize {
            20
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
        let mut resolved = tasks::resolve(&ctx.project_root, "check", &args.subproject, &[])?;
        if let tasks::CommandLine::Cargo { args: a } = &mut resolved.line {
            if args.all_targets {
                a.push("--all-targets".to_string());
            }
            a.extend(extra);
            a.push("--message-format=json".to_string());
        }
        let out = tasks::exec(&resolved, args.timeout_secs).await?;
        let (errors, total) = parse_check_json(&out.body, args.max_errors);
        let mut s = format!(
            "cargo check {} (exit {}, {:.1}s)\n",
            if out.success { "ok" } else { "failed" },
            out.code,
            out.elapsed.as_secs_f32()
        );
        if total == 0 && out.success {
            s.push_str("no errors");
            return Ok(s);
        }
        if total == 0 {
            // No JSON diagnostics parsed: surface the human-readable error lines.
            let mut shown = 0usize;
            for line in out.body.lines() {
                if line.starts_with('{') || line.trim().is_empty() {
                    continue;
                }
                s.push_str(line);
                s.push('\n');
                shown += 1;
                if shown >= 40 {
                    break;
                }
            }
            return Ok(s);
        }
        s.push_str(&format!(
            "{total} error(s), showing first {}:\n",
            errors.len()
        ));
        for e in &errors {
            s.push_str(e);
            s.push('\n');
        }
        Ok(s)
    }
}

/// Parse `cargo check --message-format=json` output: return up to `max` formatted
/// error diagnostics and the total number of errors seen.
fn parse_check_json(raw: &str, max: usize) -> (Vec<String>, usize) {
    let mut errors = Vec::new();
    let mut total = 0usize;
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        if msg.get("level").and_then(Value::as_str) != Some("error") {
            continue;
        }
        total += 1;
        if errors.len() < max {
            errors.push(format_diagnostic(msg));
        }
    }
    (errors, total)
}

/// Format one JSON diagnostic as `file:line:col: error[CODE]: message`.
fn format_diagnostic(msg: &Value) -> String {
    let text = msg.get("message").and_then(Value::as_str).unwrap_or("");
    let code = msg
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .map(|c| format!("[{c}]"))
        .unwrap_or_default();
    let loc = msg
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| {
            spans
                .iter()
                .find(|s| s.get("is_primary").and_then(Value::as_bool).unwrap_or(false))
                .or_else(|| spans.first())
        })
        .map(|s| {
            let f = s.get("file_name").and_then(Value::as_str).unwrap_or("");
            let l = s.get("line_start").and_then(Value::as_u64).unwrap_or(0);
            let c = s.get("column_start").and_then(Value::as_u64).unwrap_or(0);
            format!("{f}:{l}:{c}")
        })
        .unwrap_or_default();
    if loc.is_empty() {
        format!("error{code}: {text}")
    } else {
        format!("{loc}: error{code}: {text}")
    }
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

struct Shell;

static SHELL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "shell".into(),
    description: "LAST RESORT - prefer a dedicated tool (fs_read_file/fs_list_dir, fs_rgrep, pom_run_task/pom_run_tests, git_*, pom_model) over the shell. Runs an arbitrary bash command and returns raw output. Approval-gated.".into(),
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

    #[test]
    fn run_task_rejects_test_runs() {
        let cargo = tasks::CommandLine::Cargo {
            args: vec!["test".into(), "--lib".into()],
        };
        let err = ensure_not_test_run(&cargo).unwrap_err().to_string();
        assert!(err.contains("pom_run_tests"), "{err}");
        // a cargo alias expanding to `test` is caught the same way
        let alias = tasks::CommandLine::Cargo {
            args: vec!["test".into()],
        };
        assert!(ensure_not_test_run(&alias).is_err());
        // non-test cargo commands and shell aliases still pass
        let build = tasks::CommandLine::Cargo {
            args: vec!["build".into()],
        };
        assert!(ensure_not_test_run(&build).is_ok());
        let shell = tasks::CommandLine::Shell {
            script: "echo hi".into(),
        };
        assert!(ensure_not_test_run(&shell).is_ok());
    }

    #[test]
    fn parses_and_caps_json_diagnostics() {
        // Two compiler-message errors + noise; cap at 1.
        let raw = r#"{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `a`","code":{"code":"E0425"},"spans":[{"file_name":"src/main.rs","line_start":3,"column_start":5,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"warning","message":"unused","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":null,"spans":[{"file_name":"src/lib.rs","line_start":9,"column_start":1,"is_primary":true}]}}
Compiling foo
"#;
        let (errors, total) = parse_check_json(raw, 1);
        assert_eq!(total, 2);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0], "src/main.rs:3:5: error[E0425]: cannot find value `a`");

        let (all, total) = parse_check_json(raw, 10);
        assert_eq!(total, 2);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], "src/lib.rs:9:1: error: mismatched types");
    }
}
