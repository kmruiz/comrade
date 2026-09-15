//! Project object model (POM) and task-runner tools.
//!
//! - `pom_model` describes the workspace/project + subprojects (workspace
//!   members), their dependencies, and available tasks — the Comrade
//!   analogue of a Maven POM (project + modules).
//! - `pom_run_task` executes a named task (a standard build-system task, a
//!   configured alias, or a `!shell` alias) optionally scoped to one subproject.

mod bg;
mod ecosystem;
mod node;
mod pom;
mod tasks;

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub use bg::{BgJobInfo, BgJobs};
pub use pom::{ProjectModel, load as load_model, render as render_model};

pub fn all() -> Vec<Box<dyn Tool>> {
    all_with_jobs().0
}

/// Like [`all`] but also hands back the shared background-job registry, so an
/// observer outside the tools (the TUI's job panel and stop command) can list
/// and stop jobs. Only the main registry needs this; the delegate registry
/// filters the bg tools out entirely.
pub fn all_with_jobs() -> (Vec<Box<dyn Tool>>, BgJobs) {
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(PomModelTool),
        Box::new(PomRunTask),
        Box::new(PomFormatCode),
        Box::new(PomRunTests),
        Box::new(PomCheck),
        Box::new(Shell),
    ];
    let jobs = BgJobs::new();
    tools.extend(bg::tools(&jobs));
    (tools, jobs)
}

/// The concrete [`comrade_tool::TaskRunner`] backed by the detected build
/// ecosystem(s). Injected into `comrade-core`'s `summarise` tool so it can run a
/// named project task (verb or alias) and summarise its uncapped output, while
/// the core stays agnostic to this crate.
pub struct ProjectTaskRunner;

#[async_trait]
impl comrade_tool::TaskRunner for ProjectTaskRunner {
    async fn run_task(
        &self,
        root: &std::path::Path,
        task: &str,
        subproject: Option<&str>,
        ecosystem: Option<&str>,
        extra: &[String],
        timeout_secs: u64,
    ) -> Result<comrade_tool::TaskRun> {
        let eco = ecosystem::pick(root, ecosystem, task.trim())?;
        if !eco.supports(root, task) {
            anyhow::bail!(
                "unknown task {:?} for the {} ecosystem; known verbs: {} (plus configured aliases)",
                task,
                eco.name(),
                ecosystem::VERBS.join(", ")
            );
        }
        let subproject = subproject.map(str::to_string);
        let resolved = eco.resolve(root, task, &subproject, extra)?;
        let out = tasks::exec(&resolved, timeout_secs).await?;
        Ok(comrade_tool::TaskRun {
            describe: resolved.describe,
            success: out.success,
            code: out.code,
            body: out.body,
            elapsed: out.elapsed,
        })
    }
}

// ---------------------------------------------------------------------------
// pom_model
// ---------------------------------------------------------------------------

struct PomModelTool;

static POM_MODEL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_model".into(),
    description: "Report this project's dependencies, subprojects, layout and tasks (the POM). Use INSTEAD of reading the project manifest (e.g. Cargo.toml, package.json) when asked about deps/modules/tasks. Read-only.".into(),
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
        let ecos = ecosystem::detect_all(&ctx.project_root);
        if ecos.is_empty() {
            // Reuse the canonical "unsupported project" error.
            return ecosystem::detect(&ctx.project_root).map(|_| String::new());
        }
        // A repository may host several ecosystems (Cargo + npm); show them all.
        let mut sections = Vec::new();
        for eco in ecos {
            sections.push(format!(
                "Ecosystem: {}\n{}",
                eco.name(),
                eco.model(&ctx.project_root)?
            ));
        }
        Ok(sections.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// pom_run_task
// ---------------------------------------------------------------------------

struct PomRunTask;

static POM_RUN_TASK_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_run_task".into(),
    description: "Run a named project task and return its output: standard project tasks (e.g. build, run, check, fmt, doc, test, bench, release) and configured aliases; optionally scope to a subproject. Runs directly without approval. Works for Cargo and npm projects; in a repo with several build ecosystems pass `ecosystem` to choose one. Run tests with pom_run_tests, not here - it returns only the failure summary and costs far less context.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "task": { "type": "string", "description": "Task name, e.g. \"build\" or a configured alias. Use pom_run_tests to run tests." },
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/app\"." },
            "ecosystem": { "type": "string", "description": "Which build ecosystem to use in a polyglot repo, \"cargo\" or \"npm\". Defaults to the only one present, or the one that supports the task." },
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
            ecosystem: Option<String>,
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

        let eco = ecosystem::pick(
            &ctx.project_root,
            args.ecosystem.as_deref(),
            args.task.trim(),
        )?;
        if !eco.supports(&ctx.project_root, &args.task) {
            anyhow::bail!(
                "unknown task {:?} for the {} ecosystem; known verbs: {} (plus configured aliases)",
                args.task,
                eco.name(),
                ecosystem::VERBS.join(", ")
            );
        }
        let resolved = eco.resolve(&ctx.project_root, &args.task, &args.subproject, &extra)?;
        // Refuse a command that would run tests (covers an alias expanding to
        // `test`): tests belong to the dedicated pom_run_tests tool, whose
        // output is a compact failure summary.
        if eco.is_test_command(&resolved.line) {
            anyhow::bail!(
                "running tests through pom_run_task is disabled - use the pom_run_tests tool instead \
                 (it returns only failing tests and costs far less context)"
            );
        }
        let output = tasks::run(&resolved, args.timeout_secs).await?;
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// pom_format_code
// ---------------------------------------------------------------------------

struct PomFormatCode;

static POM_FORMAT_CODE_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_format_code".into(),
    description: "Run the project's formatter so code is formatted deterministically instead of hand-formatting tokens. Runs directly without approval (whitespace-only rewrites). Cargo projects use `cargo fmt`, npm projects use prettier.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "ecosystem": { "type": "string", "description": "Which build ecosystem to use in a polyglot repo, \"cargo\" or \"npm\". Defaults to the only one present." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for PomFormatCode {
    fn spec(&self) -> &ToolSpec {
        &POM_FORMAT_CODE_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            ecosystem: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        // A deterministic, whitespace-only rewrite, so it runs
        // directly (no human approval) like pom_run_tests/pom_run_task.
        let eco = ecosystem::pick(&ctx.project_root, args.ecosystem.as_deref(), "fmt")?;
        let resolved = eco.format_command(&ctx.project_root)?;
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
    description: "Run the project's tests and return a SIMPLIFIED summary the model can read: pass/fail totals, failing test names, key error lines. Use to verify work instead of reasoning about code. Runs the WHOLE suite - there is no test-filter argument. When it reports all tests passing, the change is verified: stop re-running it. Works for Cargo and npm projects; pass `ecosystem` in a polyglot repo. In a Cargo workspace, a root-level run tests the whole workspace (every member).".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/app\"." },
            "ecosystem": { "type": "string", "description": "Which build ecosystem to use in a polyglot repo, \"cargo\" or \"npm\". Defaults to the only one present, or the one that supports the test task." },
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
            ecosystem: Option<String>,
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
        let eco = ecosystem::pick(&ctx.project_root, args.ecosystem.as_deref(), "test")?;
        // A small model happily invents a `subproject` (e.g. "src", "tests").
        // Any supported backend counts, so a polyglot repo is judged by the union
        // of manifests. Rather than spend an iteration on an error, ignore the
        // bogus value, test the whole project, and say so.
        let mut ignored_subproject: Option<String> = None;
        let subproject = match args.subproject.as_deref() {
            Some(sub)
                if !sub.trim().is_empty()
                    && !ecosystem::detect_all(&ctx.project_root)
                        .iter()
                        .any(|e| e.is_project_dir(&ctx.project_root.join(sub))) =>
            {
                ignored_subproject = Some(sub.to_string());
                None
            }
            other => other.map(str::to_string),
        };
        let resolved = eco.resolve(&ctx.project_root, "test", &subproject, &extra)?;
        let raw = tasks::run(&resolved, args.timeout_secs).await?;
        let summary = eco.simplify_tests(&raw);
        Ok(match ignored_subproject {
            Some(sub) => format!(
                "(no build project at {sub:?}; tested the whole project instead)\n{summary}"
            ),
            None => summary,
        })
    }
}

// ---------------------------------------------------------------------------
// pom_check
// ---------------------------------------------------------------------------

struct PomCheck;

static POM_CHECK_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "pom_check".into(),
    description: "Type-check the project (e.g. cargo check, or tsc --noEmit for a TypeScript npm project) and return the first `max_errors` compiler errors with their file:line:col plus the total count. Much cheaper than pom_run_tests for iterating on compile errors; pass `all_targets: true` to also check tests/examples. Uses the project's build ecosystem; pass `ecosystem` in a polyglot repo.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/comrade-core\"." },
            "ecosystem": { "type": "string", "description": "Which build ecosystem to use in a polyglot repo, \"cargo\" or \"npm\". Defaults to the only one present, or the one that has a check step." },
            "all_targets": { "type": "boolean", "default": false, "description": "Also check tests, examples and benches (cargo check --all-targets)." },
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
            ecosystem: Option<String>,
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
        let eco = ecosystem::pick(&ctx.project_root, args.ecosystem.as_deref(), "check")?;
        let Some(line) = eco.check_command(
            &ctx.project_root,
            &args.subproject,
            args.all_targets,
            &extra,
        )?
        else {
            anyhow::bail!(
                "the {} ecosystem has no separate check step; use pom_run_tests or pom_run_task",
                eco.name()
            );
        };
        let resolved = tasks::Resolved {
            cwd: ctx.project_root.clone(),
            describe: line.describe(),
            line,
        };
        let out = tasks::exec(&resolved, args.timeout_secs).await?;
        let (errors, total) = eco.parse_diagnostics(&out.body, args.max_errors);
        let mut s = format!(
            "{} check {} (exit {}, {:.1}s)\n",
            eco.name(),
            if out.success { "ok" } else { "failed" },
            out.code,
            out.elapsed.as_secs_f32()
        );
        if total == 0 && out.success {
            s.push_str("no errors");
            return Ok(s);
        }
        if total == 0 {
            // Could not parse structured diagnostics: surface the raw output.
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
        // Policy gate BEFORE approval: a denied command never even prompts.
        comrade_tool::check_command(&args.command, &comrade_tool::policy())?;
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
    use crate::ecosystem::Ecosystem;

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
        let out = ecosystem::Cargo.simplify_tests(raw);
        assert!(out.contains("test result: FAILED"));
        assert!(out.contains("11 passed"));
        assert!(out.contains("panicked at"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Finished"));
    }

    #[test]
    fn is_test_command_guards_pom_run_task() {
        let eco = ecosystem::Cargo;
        // a `test` invocation (or an alias expanding to one) is refused
        assert!(eco.is_test_command(&tasks::CommandLine::Program {
            program: "cargo".into(),
            args: vec!["test".into(), "--lib".into()],
        }));
        // other cargo commands and shell aliases pass
        assert!(!eco.is_test_command(&tasks::CommandLine::Program {
            program: "cargo".into(),
            args: vec!["build".into()],
        }));
        assert!(!eco.is_test_command(&tasks::CommandLine::Shell {
            script: "echo hi".into(),
        }));
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
        let (errors, total) = ecosystem::Cargo.parse_diagnostics(raw, 1);
        assert_eq!(total, 2);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0],
            "src/main.rs:3:5: error[E0425]: cannot find value `a`"
        );

        let (all, total) = ecosystem::Cargo.parse_diagnostics(raw, 10);
        assert_eq!(total, 2);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], "src/lib.rs:9:1: error: mismatched types");
    }

    fn scratch_ctx(root: &std::path::Path) -> ToolContext {
        use comrade_tool::{
            PlanStatus, PlanStep, PlanTarget, SessionControl, UndoLog, UserIo, UserPrompt,
            UserReply,
        };
        use std::sync::Arc;
        struct S;
        impl SessionControl for S {
            fn set_title(&self, _t: &str) {}
            fn title(&self) -> String {
                "t".into()
            }
            fn set_plan(&self, _s: Vec<comrade_tool::PlanStepDraft>) {}
            fn plan(&self) -> Vec<PlanStep> {
                Vec::new()
            }
            fn update_plan(&self, _t: PlanTarget, _s: PlanStatus, _n: Option<String>) -> bool {
                false
            }
            fn finish_plan(&self, _s: Option<String>) {}
            fn set_status(&self, _s: &str) {}
            fn status(&self) -> String {
                String::new()
            }
        }
        struct U;
        #[async_trait]
        impl UserIo for U {
            async fn ask(&self, _p: UserPrompt) -> anyhow::Result<UserReply> {
                Ok(UserReply::Answer("yes".into()))
            }
        }
        struct L;
        #[async_trait]
        impl UndoLog for L {
            async fn capture(&self, _p: &str, _b: String) -> anyhow::Result<()> {
                Ok(())
            }
            async fn undo_last(&self) -> anyhow::Result<usize> {
                Ok(0)
            }
            async fn is_empty(&self) -> bool {
                true
            }
            async fn len(&self) -> usize {
                0
            }
        }
        ToolContext {
            project_root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            session: Arc::new(S),
            user: Arc::new(U),
            undo: Arc::new(L),
            auto_approve: true,
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        }
    }

    #[test]
    fn shell_refuses_a_command_on_the_policy_deny_list() {
        let dir = std::env::temp_dir().join(format!("comrade-shellpolicy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = scratch_ctx(&dir);
        comrade_tool::set_policy(comrade_tool::SecurityPolicy {
            shell_deny: vec!["curl ".into()],
            ..Default::default()
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(Shell.invoke(&ctx, json!({ "command": "curl https://evil" })))
            .unwrap_err();
        assert!(err.to_string().contains("deny list"), "{err}");

        // Reset the global policy so other tests are unaffected.
        comrade_tool::set_policy(comrade_tool::SecurityPolicy::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
