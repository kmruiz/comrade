//! Task resolution and execution for Cargo projects.
//!
//! A "task" is either a cargo verb, a cargo alias from `.cargo/config.toml`
//! (which may itself be a `!shell` command), or scoped to a subproject via
//! `--manifest-path`.

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

/// How the resolved task is executed.
#[derive(Debug)]
pub enum CommandLine {
    Cargo { args: Vec<String> },
    Shell { script: String },
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

fn manifest_path_arg(subproject: &Option<String>) -> Option<String> {
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

    let cwd = match subproject {
        Some(dir) => {
            let dir = dir.trim_end_matches('/');
            let path = root.join(dir);
            if !path.starts_with(root) || dir.contains("..") {
                anyhow::bail!("subproject {dir:?} escapes the project root");
            }
            path
        }
        None => root.to_path_buf(),
    };

    let manifest = manifest_path_arg(subproject);

    let describe;
    let line = if let Some(alias) = find_alias(&model.aliases, task) {
        let expansion = alias.expansion.trim();
        if let Some(shell) = expansion.strip_prefix('!') {
            let mut script = shell.trim().to_string();
            if !extra.is_empty() {
                script.push(' ');
                script.push_str(&extra.join(" "));
            }
            describe = format!("bash -c {script:?}");
            CommandLine::Shell { script }
        } else {
            let mut argv = vec!["cargo".to_string()];
            argv.extend(expansion.split_whitespace().map(str::to_string));
            if let Some(m) = &manifest {
                argv.push(m.clone());
            }
            argv.extend(extra.iter().cloned());
            describe = argv.join(" ");
            CommandLine::Cargo { args: argv }
        }
    } else if let Some(mut argv) = verb_args(task) {
        if let Some(m) = &manifest {
            argv.push(m.clone());
        }
        argv.extend(extra.iter().cloned());
        let mut full = vec!["cargo".to_string()];
        full.extend(argv.iter().cloned());
        describe = full.join(" ");
        CommandLine::Cargo { args: full }
    } else {
        let available = model
            .aliases
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "unknown task {task:?}. Known verbs: {}; aliases: {available}",
            pom::CARGO_VERBS.join(", ")
        );
    };

    Ok(Resolved {
        cwd,
        line,
        describe,
    })
}

/// Run a resolved task to completion, returning the status + (capped) output.
/// Tasks that exceed `timeout_secs` are killed.
pub async fn run(resolved: &Resolved, timeout_secs: u64) -> Result<String> {
    use std::process::Stdio;

    let mut command = match &resolved.line {
        CommandLine::Cargo { args } => {
            let mut c = tokio::process::Command::new("cargo");
            c.args(args);
            c
        }
        CommandLine::Shell { script } => {
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(script);
            c
        }
    };
    command
        .current_dir(&resolved.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().context("failed to spawn task")?;
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("task timed out after {timeout_secs}s and was killed"))?
        .context("task failed to produce output")?;

    let mut body = String::new();
    body.push_str(&String::from_utf8_lossy(&output.stdout));
    body.push_str(&String::from_utf8_lossy(&output.stderr));
    let body = body.trim();
    const MAX: usize = 9000;
    let capped: String = if body.chars().count() > MAX {
        let mut s: String = body.chars().take(MAX).collect();
        s.push_str("\n... (output truncated)");
        s
    } else {
        body.to_string()
    };

    let code = output.status.code().unwrap_or(-1);
    let elapsed = started.elapsed();
    let status = if output.status.success() {
        "ok"
    } else {
        "failed"
    };
    let mut result = format!(
        "task {:?} {status} (exit {code}, {:.1}s)\n",
        resolved.describe,
        elapsed.as_secs_f32()
    );
    if !capped.is_empty() {
        result.push_str(&capped);
        result.push('\n');
    }
    Ok(result)
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
            CommandLine::Cargo { args } => assert_eq!(args, &["cargo", "test"]),
            _ => panic!("expected cargo"),
        }

        let extra = [String::from("--"), String::from("--nocapture")];
        let r = resolve(&root, "t", &none, &extra).unwrap();
        match &r.line {
            CommandLine::Cargo { args } => {
                assert_eq!(args, &["cargo", "test", "--", "--nocapture"])
            }
            _ => panic!("expected cargo alias"),
        }

        let sub = Some("crates/a".to_string());
        let r = resolve(&root, "check", &sub, &[]).unwrap();
        match &r.line {
            CommandLine::Cargo { args } => assert_eq!(
                args,
                &["cargo", "check", "--manifest-path=crates/a/Cargo.toml"]
            ),
            _ => panic!("expected cargo with manifest path"),
        }

        let r = resolve(&root, "deploy", &none, &[]).unwrap();
        match &r.line {
            CommandLine::Shell { script } => assert_eq!(script, "echo deploying"),
            _ => panic!("expected shell alias"),
        }

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
}
