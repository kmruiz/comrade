//! Git tools: `git_status`, `git_diff`, `git_show`, `git_log`, `git_commit`.
//!
//! Shells out to the user's `git` so credentials, hooks, and config are reused.
//! `git_commit` runs without human approval.

use std::process::Stdio;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_OUTPUT_CHARS: usize = 6000;

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(GitStatus),
        Box::new(GitDiff),
        Box::new(GitShow),
        Box::new(GitLog),
        Box::new(GitCommit),
    ]
}

fn clamp(mut s: String) -> String {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        let mut out: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        out.push_str("\n... (output truncated)");
        s = out;
    }
    s
}

async fn git(ctx: &ToolContext, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(&ctx.project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to spawn git")?;
    if !out.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn current_branch(ctx: &ToolContext) -> String {
    git(ctx, &["branch", "--show-current"])
        .await
        .unwrap_or_default()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// git_status
// ---------------------------------------------------------------------------

struct GitStatus;

static GIT_STATUS_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_status".into(),
    description: "Show the git working tree status (branch, staged/modified/untracked files). Use before committing or when you need the lay of the land.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitStatus {
    fn spec(&self) -> &ToolSpec {
        &GIT_STATUS_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        let branch = current_branch(ctx).await;
        let body = git(ctx, &["status", "--short"]).await?;
        Ok(clamp(format!(
            "branch: {branch}\n{}",
            if body.is_empty() {
                "(clean)".to_string()
            } else {
                body
            }
        )))
    }
}

// ---------------------------------------------------------------------------
// git_diff
// ---------------------------------------------------------------------------

struct GitDiff;

static GIT_DIFF_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_diff".into(),
    description: "Show a unified diff. By default shows all changes vs HEAD; pass `staged: true` for the index diff, or `path` to limit to one file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Limit diff to this file (project-root relative)." },
            "staged": { "type": "boolean", "default": false, "description": "Show only staged changes (git diff --cached)." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitDiff {
    fn spec(&self) -> &ToolSpec {
        &GIT_DIFF_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            staged: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let mut git_args: Vec<String> = vec!["diff".to_string()];
        if args.staged {
            git_args.push("--cached".to_string());
        } else {
            git_args.push("HEAD".to_string());
        }
        if let Some(p) = args.path {
            git_args.push("--".to_string());
            git_args.push(p);
        }
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let body = git(ctx, &refs).await?;
        Ok(clamp(if body.is_empty() {
            "(no diff)".into()
        } else {
            body
        }))
    }
}

// ---------------------------------------------------------------------------
// git_show
// ---------------------------------------------------------------------------

struct GitShow;

static GIT_SHOW_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_show".into(),
    description: "Show a git object: a commit (metadata + diff), tag, or a file at a revision. Pass `rev` (default HEAD) and optionally `path` for a file inside that revision. Use instead of the shell.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "rev": { "type": "string", "description": "Revision to show, e.g. a hash, branch, tag, or HEAD~2 (default: HEAD)." },
            "path": { "type": "string", "description": "Optional file within `rev`; shows its content at that revision." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitShow {
    fn spec(&self) -> &ToolSpec {
        &GIT_SHOW_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "default_rev")]
            rev: String,
            #[serde(default)]
            path: Option<String>,
        }
        fn default_rev() -> String {
            "HEAD".to_string()
        }
        let args: Args = serde_json::from_value(args)?;
        let spec: Vec<String> = match args.path {
            Some(p) => vec![format!("{}:{}", args.rev, p)],
            None => vec![args.rev],
        };
        let mut git_args: Vec<String> = vec!["show".to_string()];
        git_args.extend(spec);
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let body = git(ctx, &refs).await?;
        Ok(clamp(if body.is_empty() {
            "(nothing to show)".into()
        } else {
            body
        }))
    }
}

// ---------------------------------------------------------------------------
// git_log
// ---------------------------------------------------------------------------

struct GitLog;

static GIT_LOG_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_log".into(),
    description: "Show recent commit history (one-line style). Useful for matching the repo's commit message conventions.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "n": { "type": "integer", "default": 10, "minimum": 1, "maximum": 50, "description": "Number of commits to show." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitLog {
    fn spec(&self) -> &ToolSpec {
        &GIT_LOG_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "default_n")]
            n: usize,
        }
        fn default_n() -> usize {
            10
        }
        let args: Args = serde_json::from_value(args)?;
        let nstr = args.n.to_string();
        let body = git(ctx, &["log", "--oneline", "-n", &nstr]).await?;
        Ok(clamp(body))
    }
}

// ---------------------------------------------------------------------------
// git_commit
// ---------------------------------------------------------------------------

struct GitCommit;

static GIT_COMMIT_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_commit".into(),
    description: "Stage and commit. Pass `paths` to commit only those files; omit it to stage and commit everything. Call git_diff/git_status first to verify what is being committed.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "Commit message. Match the repo's existing style (see git_log)." },
            "paths": { "type": "array", "items": { "type": "string" }, "description": "Files to stage (project-root relative). Omit to stage all changes." }
        },
        "required": ["message"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitCommit {
    fn spec(&self) -> &ToolSpec {
        &GIT_COMMIT_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            message: String,
            #[serde(default)]
            paths: Vec<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let stage = stage_args(&args.paths);
        let refs: Vec<&str> = stage.iter().map(String::as_str).collect();
        git(ctx, &refs).await?;
        let message_file = write_message_file(&args.message).await?;
        let msg_arg = message_file.to_str().unwrap_or("/dev/null").to_string();
        let result = git(ctx, &["commit", "-F", &msg_arg])
            .await
            .inspect_err(|_e| {
                let _ = std::fs::remove_file(&message_file);
            })?;
        let _ = std::fs::remove_file(&message_file);
        Ok(result)
    }
}

/// Shape the `git add` invocation: no paths stages everything (`-A`); given
/// paths, stage exactly those (the `--` stops git parsing a path as an option).
fn stage_args(paths: &[String]) -> Vec<String> {
    let paths: Vec<&str> = paths
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if paths.is_empty() {
        vec!["add".into(), "-A".into()]
    } else {
        let mut args = vec!["add".to_string(), "--".to_string()];
        args.extend(paths.into_iter().map(str::to_string));
        args
    }
}

async fn write_message_file(message: &str) -> Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("comrade-commit-{}.txt", std::process::id()));
    tokio::fs::write(&path, message).await?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_args_stages_all_without_paths() {
        assert_eq!(stage_args(&[]), vec!["add", "-A"]);
        // blank paths behave like no paths
        assert_eq!(stage_args(&["  ".into()]), vec!["add", "-A"]);
    }

    #[test]
    fn stage_args_stages_exactly_the_given_paths() {
        assert_eq!(
            stage_args(&["crates/foo/src/lib.rs".into(), "Cargo.toml".into()]),
            vec!["add", "--", "crates/foo/src/lib.rs", "Cargo.toml"]
        );
        // blank entries are dropped, the `--` guards dash-prefixed names
        assert_eq!(
            stage_args(&["-x".into(), " ".into(), "a.rs".into()]),
            vec!["add", "--", "-x", "a.rs"]
        );
    }
}
