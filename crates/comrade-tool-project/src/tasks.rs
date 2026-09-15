//! Cargo task resolution + the generic process runner.
//!
//! [`resolve`] is the Cargo-specific resolver (verb -> cargo args, aliases from
//! `.cargo/config.toml`, subproject -> `--manifest-path`); the `pom_*` tools
//! reach it through the [`crate::ecosystem::Ecosystem`] seam so other build
//! ecosystems (npm, Maven, Go, ...) can slot in behind the same tools.
//! [`exec`]/[`run`] are ecosystem-neutral: they run a [`CommandLine`] (a program
//! invocation or a shell script) and capture its output.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::pom::{self, Alias};

/// Standard verb -> (cargo args). `release` is a build variant, not a subcommand.
fn verb_args(task: &str) -> Option<Vec<String>> {
    let argv = match task {
        "release" => vec!["build".to_string(), "--release".to_string()],
        "build" => vec!["build".to_string()],
        "run" => vec!["run".to_string()],
        "check" => vec!["check".to_string()],
        "test" => vec!["test".to_string()],
        "clippy" => vec!["clippy".to_string()],
        "fmt" => vec!["fmt".to_string()],
        "doc" => vec!["doc".to_string()],
        "bench" => vec!["bench".to_string()],
        _ => return None,
    };
    Some(argv)
}

/// How a resolved task is executed: a build-tool invocation (`program` + args,
/// e.g. `cargo check`) or a shell script run with `bash -c`. Program is generic
/// so a backend for another ecosystem (npm/maven/go) emits its own tool.
#[derive(Debug)]
pub enum CommandLine {
    Program { program: String, args: Vec<String> },
    Shell { script: String },
}

impl CommandLine {
    /// One-line description used in the task result header.
    pub fn describe(&self) -> String {
        match self {
            CommandLine::Program { program, args } => {
                let mut parts = vec![program.clone()];
                parts.extend(args.iter().cloned());
                parts.join(" ")
            }
            CommandLine::Shell { script } => format!("bash -c {script:?}"),
        }
    }
}

#[derive(Debug)]
pub struct Resolved {
    /// Working directory for the command.
    pub cwd: PathBuf,
    pub line: CommandLine,
    /// Short human description, e.g. `cargo test --manifest-path crates/app/Cargo.toml`.
    pub describe: String,
}

fn find_alias<'a>(aliases: &'a [Alias], task: &str) -> Option<&'a Alias> {
    aliases.iter().find(|a| a.name == task)
}

pub fn manifest_path_arg(subproject: &Option<String>) -> Option<String> {
    subproject
        .as_ref()
        .map(|s| format!("--manifest-path={}/Cargo.toml", s.trim_end_matches('/')))
}

/// Resolve `task` (+optional `extra` args) into an executable command.
///
/// `root` is the workspace root. `subproject`, when given, is a crate directory
/// relative to `root`; cargo tasks get `--manifest-path`, `!shell` aliases run
/// inside that directory.
pub fn resolve(
    root: &Path,
    task: &str,
    subproject: &Option<String>,
    extra: &[String],
) -> Result<Resolved> {
    if task.is_empty() {
        anyhow::bail!("task name must not be empty");
    }
    let model = pom::load(root)?;

    // Working directory used by `!shell` aliases and bare shell commands when a
    // subproject is selected. `cargo` commands always run from the workspace
    // root, because `--manifest-path` is resolved relative to it.
    let sub_cwd = match subproject {
        Some(dir) => {
            let dir = dir.trim_end_matches('/');
            let path = root.join(dir);
            if !path.starts_with(root) || dir.contains("..") {
                anyhow::bail!("subproject {dir:?} escapes the project root");
            }
            Some(path)
        }
        None => None,
    };

    let manifest = manifest_path_arg(subproject);

    let line = if let Some(alias) = find_alias(&model.aliases, task) {
        let expansion = alias.expansion.trim();
        if let Some(shell) = expansion.strip_prefix('!') {
            let mut script = shell.trim().to_string();
            if !extra.is_empty() {
                script.push(' ');
                script.push_str(&extra.join(" "));
            }
            CommandLine::Shell { script }
        } else {
            let mut args = expansion
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            if let Some(m) = &manifest {
                args.push(m.clone());
            }
            args.extend(extra.iter().cloned());
            CommandLine::Program {
                program: "cargo".to_string(),
                args,
            }
        }
    } else if let Some(mut args) = verb_args(task) {
        // A root-level `cargo test` (the verb `pom_run_tests` uses) covers the
        // whole workspace when one is declared, so the summary lists every
        // member's tests in a single pass instead of only the root package.
        if task == "test" && subproject.is_none() && model.is_workspace {
            args.push("--workspace".to_string());
        }
        if let Some(m) = &manifest {
            args.push(m.clone());
        }
        args.extend(extra.iter().cloned());
        CommandLine::Program {
            program: "cargo".to_string(),
            args,
        }
    } else {
        let available = model
            .aliases
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "unknown task {task:?}. Known verbs: {}; aliases: {available}",
            pom::RUN_TASK_VERBS.join(", ")
        );
    };

    let describe = line.describe();

    // cargo commands run from the workspace root (manifest-path is root
    // relative); shell commands run inside the subproject when one is given.
    let cwd = match (&line, &sub_cwd) {
        (CommandLine::Program { .. }, _) => root.to_path_buf(),
        (CommandLine::Shell { .. }, Some(sub)) => sub.clone(),
        (CommandLine::Shell { .. }, None) => root.to_path_buf(),
    };

    Ok(Resolved {
        cwd,
        line,
        describe,
    })
}

/// Result of running a task to completion.
#[derive(Debug)]
pub struct TaskOutput {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Process exit code (`-1` when terminated by a signal).
    pub code: i32,
    /// Combined stdout+stderr, uncapped.
    pub body: String,
    /// Wall-clock time the task took.
    pub elapsed: Duration,
}

/// Run a resolved task to completion, returning status + UNCAPPED output. Tasks
/// that exceed `timeout_secs` are killed.
pub async fn exec(resolved: &Resolved, timeout_secs: u64) -> Result<TaskOutput> {
    use std::process::Stdio;

    let configure = |program: &str, resolved: &Resolved| {
        let mut command = match &resolved.line {
            CommandLine::Program { args, .. } => {
                let mut c = tokio::process::Command::new(program);
                c.args(args);
                c
            }
            CommandLine::Shell { script } => {
                let mut c = tokio::process::Command::new(program);
                c.arg("-c").arg(script);
                c
            }
        };
        command
            .current_dir(&resolved.cwd)
            // Never inherit stdin: a command that reads it (`cat` with no
            // operand is the classic small-model output) would block on the
            // terminal forever and freeze the run. With an empty stdin such a
            // command sees EOF and exits immediately.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    };

    let mut command = match &resolved.line {
        CommandLine::Program { program, .. } => configure(program, resolved),
        CommandLine::Shell { .. } => configure("bash", resolved),
    };

    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `cargo` may not be on the PATH inherited by this process even
            // though it is on the login shell's PATH. Locate it and retry.
            if let CommandLine::Program { program, .. } = &resolved.line
                && program == "cargo"
            {
                if let Some(path) = find_cargo().await {
                    let mut retry = configure(path.to_string_lossy().as_ref(), resolved);
                    match retry.spawn() {
                        Ok(child) => child,
                        Err(e) => return Err(anyhow::anyhow!("failed to spawn {path:?}: {e}")),
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "cargo was not found on PATH ({e}). PATH={}",
                        std::env::var("PATH").unwrap_or_default()
                    ));
                }
            } else {
                return Err(anyhow::anyhow!("failed to spawn bash: {e}"));
            }
        }
        Err(e) => return Err(anyhow::anyhow!("failed to spawn task: {e}")),
    };

    let started = std::time::Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("task timed out after {timeout_secs}s and was killed"))?
        .context("task failed to produce output")?;

    let mut body = String::new();
    body.push_str(&String::from_utf8_lossy(&output.stdout));
    body.push_str(&String::from_utf8_lossy(&output.stderr));

    Ok(TaskOutput {
        success: output.status.success(),
        code: output.status.code().unwrap_or(-1),
        body: body.trim().to_string(),
        elapsed: started.elapsed(),
    })
}

/// Run a resolved task to completion, returning the status header + (capped)
/// output — the shape the model reads.
pub async fn run(resolved: &Resolved, timeout_secs: u64) -> Result<String> {
    let out = exec(resolved, timeout_secs).await?;
    let status = if out.success { "ok" } else { "failed" };
    let capped = cap(&out.body, 9000);
    let mut result = format!(
        "task {:?} {status} (exit {}, {:.1}s)\n",
        resolved.describe,
        out.code,
        out.elapsed.as_secs_f32()
    );
    if !capped.is_empty() {
        result.push_str(&capped);
        result.push('\n');
    }
    Ok(result)
}

/// Cap `body` at `max` characters, eliding the middle when it is longer.
///
/// BOTH ends are kept because they carry different information and the caller
/// only ever wants one of them per verb: the HEAD holds the compile errors
/// (`pom_run_tests`/`pom_check`) and the first built target, while the TAIL
/// holds the `test result: ...` totals that [`Ecosystem::simplify_tests`]
/// parses. Keeping only the head dropped every `test result:` line on a
/// workspace-wide `cargo test` (whose head is thousands of `test NAME ... ok`
/// lines), so the simplifier saw no results at all and the model was told a
/// green suite had failed to build.
fn cap(body: &str, max: usize) -> String {
    const MARKER: &str = "\n... (output truncated) ...\n";
    let total = body.chars().count();
    if total <= max {
        return body.to_string();
    }
    // Give the tail a third of the budget, so the trailing totals survive, and
    // charge the elision marker to the head.
    let tail_len = max / 3;
    let head_len = max.saturating_sub(tail_len + MARKER.chars().count());
    let head: String = body.chars().take(head_len).collect();
    let tail: String = body.chars().skip(total - tail_len).collect::<String>();
    format!("{head}{MARKER}{tail}")
}

/// Try to locate `cargo` when it is missing from this process's PATH: first via
/// a login shell (`bash -lc 'command -v cargo'`), then the rustup default.
async fn find_cargo() -> Option<PathBuf> {
    if let Ok(out) = tokio::process::Command::new("bash")
        .args(["-lc", "command -v cargo"])
        .output()
        .await
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            let p = PathBuf::from(path);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".cargo/bin/cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pom;

    #[test]
    fn cap_keeps_the_tail_so_test_totals_survive() {
        // A workspace-wide `cargo test`: the head is thousands of per-test
        // "... ok" lines and the `test result:` totals sit at the very end.
        // Keeping only the head dropped every total, so `simplify_tests` saw
        // no results and the model was told a green suite had failed to build.
        let mut body = String::from("running 2 tests\n");
        for i in 0..2000 {
            body.push_str(&format!(
                "test crate::very::long::test_name_number_{i} ... ok\n"
            ));
        }
        body.push_str("test result: ok. 2000 passed; 0 failed; 0 ignored\n");

        let capped = cap(&body, 9000);
        assert!(
            capped.contains("test result: ok. 2000 passed"),
            "the tail totals must survive the cap"
        );
        assert!(
            capped.contains("output truncated"),
            "the elision must be marked"
        );
        assert!(
            capped.chars().count() <= 9000,
            "the cap must still bound the output, got {}",
            capped.chars().count()
        );

        // Short output is returned untouched, and the head is still kept so
        // compile errors survive.
        assert_eq!(cap("short\n", 9000), "short\n");
        let mut errs = String::from("error[E0425]: cannot find value `a`\n");
        errs.push_str(&"filler\n".repeat(4000));
        let capped = cap(&errs, 9000);
        assert!(capped.starts_with("error[E0425]"), "{capped}");
    }

    #[test]
    fn a_capped_workspace_test_run_still_reports_its_totals() {
        use crate::ecosystem::{Cargo, Ecosystem};

        // The shape of a real `cargo test --workspace` in this repo: compile
        // noise and thousands of per-test "... ok" lines first, the totals last
        // - plus a PASSING test whose name contains "error". `pom_run_tests`
        // caps the raw output before simplifying it, so both the cap (keep the
        // tail) and the error scan (only real diagnostics) are exercised.
        let mut raw = String::from(
            "   Compiling comrade-core v0.1.0\n    Finished `dev` profile\nrunning 2000 tests\n",
        );
        raw.push_str("test delegate::tests::context_overflow_errors_are_recognised ... ok\n");
        for i in 0..2000 {
            raw.push_str(&format!("test crate::t{i} ... ok\n"));
        }
        raw.push_str("\ntest result: ok. 2000 passed; 0 failed; 0 ignored\n");

        let summary = Cargo.simplify_tests(&cap(&raw, 9000));
        assert!(
            summary.contains("test result: ok. 2000 passed"),
            "a green suite must report its totals, got: {summary}"
        );
        assert!(
            !summary.contains("could not build"),
            "a green suite must never be reported as a build failure: {summary}"
        );
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-task-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolves_verbs_and_aliases() {
        let root = scratch();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/a/src")).unwrap();
        std::fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[alias]\nt = \"test\"\ndeploy = \"!echo deploying\"\n",
        )
        .unwrap();

        let none = None;
        let r = resolve(&root, "test", &none, &[]).unwrap();
        match &r.line {
            CommandLine::Program { program, args } => {
                assert_eq!(program, "cargo");
                // at a workspace root the whole workspace is tested
                assert_eq!(args, &["test", "--workspace"]);
            }
            _ => panic!("expected cargo"),
        }

        // a subproject-scoped run targets only that crate (no --workspace)
        let sub_a = Some("crates/a".to_string());
        let r = resolve(&root, "test", &sub_a, &[]).unwrap();
        match &r.line {
            CommandLine::Program { program, args } => {
                assert_eq!(program, "cargo");
                assert_eq!(args, &["test", "--manifest-path=crates/a/Cargo.toml"]);
            }
            _ => panic!("expected cargo"),
        }

        let extra = [String::from("--"), String::from("--nocapture")];
        let r = resolve(&root, "t", &none, &extra).unwrap();
        match &r.line {
            CommandLine::Program { program, args } => {
                assert_eq!(program, "cargo");
                assert_eq!(args, &["test", "--", "--nocapture"])
            }
            _ => panic!("expected cargo alias"),
        }

        let sub = Some("crates/a".to_string());
        let r = resolve(&root, "check", &sub, &[]).unwrap();
        match &r.line {
            CommandLine::Program { program, args } => {
                assert_eq!(program, "cargo");
                assert_eq!(args, &["check", "--manifest-path=crates/a/Cargo.toml"])
            }
            _ => panic!("expected cargo with manifest path"),
        }
        // cargo runs from the workspace root, so --manifest-path resolves
        assert_eq!(r.cwd, root, "cargo must run from the workspace root");

        let r = resolve(&root, "deploy", &none, &[]).unwrap();
        match &r.line {
            CommandLine::Shell { script } => assert_eq!(script, "echo deploying"),
            _ => panic!("expected shell alias"),
        }
        // shell aliases run inside the chosen subproject
        let r = resolve(&root, "deploy", &sub, &[]).unwrap();
        assert_eq!(r.cwd, root.join("crates/a"));

        assert!(resolve(&root, "nope", &none, &[]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_model_ok() {
        let root = scratch();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let m = pom::load(&root).unwrap();
        assert!(pom::render(&m).contains("Project: x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cargo_check_executes_without_duplicate_arg() {
        let root = scratch();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let none = None;
        let resolved = resolve(&root, "check", &none, &[]).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt
            .block_on(run(&resolved, 120))
            .expect("cargo check should run");
        // If the leading "cargo" was duplicated, cargo would say this:
        assert!(
            !out.contains("'cargo' is not a cargo command"),
            "duplicate cargo program arg: {out}"
        );
        assert!(out.contains("exit 0"), "cargo check failed: {out}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod hang_tests {
    use super::*;

    fn shell(script: &str) -> Resolved {
        Resolved {
            describe: script.into(),
            cwd: std::env::temp_dir(),
            line: CommandLine::Shell {
                script: script.into(),
            },
        }
    }

    /// A command that reads stdin (`cat` with no operand - a garbage shell
    /// command a small model loves to emit) used to block the runner forever,
    /// freezing the whole agent run. It must get an empty stdin and exit.
    #[tokio::test]
    async fn a_command_reading_stdin_does_not_freeze_the_runner() {
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(std::time::Duration::from_secs(20), exec(&shell("cat"), 5))
            .await
            .expect("exec must not block on stdin")
            .expect("cat sees EOF and exits ok");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert!(out.success, "cat should exit 0 on an empty stdin");
    }

    /// A command that ignores its stdin and truly hangs is still killed by the
    /// timeout, and the call returns.
    #[tokio::test]
    async fn a_hanging_command_is_killed_by_the_timeout() {
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            exec(&shell("sleep 60"), 1),
        )
        .await
        .expect("exec must return once its own timeout fires");
        let err = out.expect_err("sleep 60 must time out").to_string();
        assert!(err.contains("timed out"), "{err}");
    }
}
