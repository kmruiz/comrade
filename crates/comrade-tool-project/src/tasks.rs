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

/// Cap `body` at `max` characters, appending a truncation marker.
fn cap(body: &str, max: usize) -> String {
    if body.chars().count() > max {
        let mut s: String = body.chars().take(max).collect();
        s.push_str("\n... (output truncated)");
        s
    } else {
        body.to_string()
    }
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
