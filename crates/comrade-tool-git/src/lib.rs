//! Git tools: `git_status`, `git_diff`, `git_show`, `git_log`, `git_blame`,
//! `git_commit`, `git_stash`, `git_branch`, `git_checkout`.
//!
//! Shells out to the user's `git` so credentials, hooks, and config are reused.
//! `git_commit` runs without human approval; the destructive tree operations
//! (`git_stash`, `git_checkout`) ask first.

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
        Box::new(GitBlame),
        Box::new(GitCommit),
        Box::new(GitStash),
        Box::new(GitBranch),
        Box::new(GitCheckout),
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
    description: "Show a unified diff. Defaults to changes vs HEAD; pass `staged: true` for the index (vs HEAD) or a `rev` (a ref, or `A..B`/`A...B` range) to diff against it. `path` limits to one file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Limit diff to this file (project-root relative)." },
            "staged": { "type": "boolean", "default": false, "description": "Show only staged changes (git diff --cached)." },
            "rev": { "type": "string", "description": "Diff against this revision, e.g. `main`, `HEAD~2`, `main...HEAD`, or `A..B`. When omitted and not `staged`, diffs against HEAD." }
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
            #[serde(default)]
            rev: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let git_args = diff_args(args.path, args.staged, args.rev);
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
    description: "Show recent commit history (one-line style). Use to match the repo's commit conventions, or with `search` to find when a string was added/removed (pickaxe) and `path` to limit to a file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "n": { "type": "integer", "default": 10, "minimum": 1, "maximum": 50, "description": "Number of commits to show." },
            "path": { "type": "string", "description": "Limit history to this file (project-root relative)." },
            "search": { "type": "string", "description": "Pickaxe: only commits that change the number of occurrences of this string." }
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
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            search: Option<String>,
        }
        fn default_n() -> usize {
            10
        }
        let args: Args = serde_json::from_value(args)?;
        let nstr = args.n.to_string();
        let mut git_args: Vec<String> = vec!["log".to_string(), "--oneline".to_string(), "-n".to_string(), nstr];
        if let Some(s) = &args.search {
            git_args.push(format!("-S{s}"));
        }
        if let Some(p) = &args.path {
            git_args.push("--".to_string());
            git_args.push(p.clone());
        }
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let body = git(ctx, &refs).await?;
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

// ---------------------------------------------------------------------------
// git_blame
// ---------------------------------------------------------------------------

struct GitBlame;

static GIT_BLAME_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_blame".into(),
    description: "Show who last changed each line of a file (git blame), optionally over a line range or at a revision. Use to find when and why a line is the way it is.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File to blame (project-root relative)." },
            "start_line": { "type": "integer", "minimum": 1, "description": "First line of the range to blame." },
            "end_line": { "type": "integer", "minimum": 1, "description": "Last line of the range (defaults to start_line)." },
            "rev": { "type": "string", "description": "Revision to blame at (default: the working tree)." }
        },
        "required": ["path"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitBlame {
    fn spec(&self) -> &ToolSpec {
        &GIT_BLAME_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default)]
            start_line: Option<usize>,
            #[serde(default)]
            end_line: Option<usize>,
            #[serde(default)]
            rev: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let git_args = blame_args(&args.path, args.start_line, args.end_line, args.rev);
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let body = git(ctx, &refs).await?;
        Ok(clamp(body))
    }
}

// ---------------------------------------------------------------------------
// git_stash
// ---------------------------------------------------------------------------

struct GitStash;

static GIT_STASH_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_stash".into(),
    description: "Manage the git stash: `list` (default), `push` (optionally with a message), or `pop`/`apply`/`drop` an entry. Use `push` to checkpoint uncommitted work before a risky change. Mutating actions ask first.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["list", "push", "pop", "apply", "drop"], "default": "list", "description": "What to do (default: list)." },
            "message": { "type": "string", "description": "Message for `push`." },
            "index": { "type": "integer", "minimum": 0, "default": 0, "description": "Stash entry index for pop/apply/drop (default 0)." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitStash {
    fn spec(&self) -> &ToolSpec {
        &GIT_STASH_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "lowercase")]
        enum Action {
            #[default]
            List,
            Push,
            Pop,
            Apply,
            Drop,
        }
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            action: Action,
            #[serde(default)]
            message: Option<String>,
            #[serde(default)]
            index: usize,
        }
        let args: Args = serde_json::from_value(args)?;
        let body = match args.action {
            Action::List => git(ctx, &["stash", "list"]).await?,
            Action::Push => {
                ctx.confirm("Stash current changes", None).await?;
                match &args.message {
                    Some(m) => git(ctx, &["stash", "push", "-m", m]).await?,
                    None => git(ctx, &["stash", "push"]).await?,
                }
            }
            Action::Pop => {
                ctx.confirm(format!("Pop stash@{{{}}}", args.index), None)
                    .await?;
                let idx = format!("stash@{{{}}}", args.index);
                git(ctx, &["stash", "pop", &idx]).await?
            }
            Action::Apply => {
                let idx = format!("stash@{{{}}}", args.index);
                git(ctx, &["stash", "apply", &idx]).await?
            }
            Action::Drop => {
                ctx.confirm(format!("Drop stash@{{{}}}", args.index), None)
                    .await?;
                let idx = format!("stash@{{{}}}", args.index);
                git(ctx, &["stash", "drop", &idx]).await?
            }
        };
        Ok(clamp(if body.trim().is_empty() {
            "(no output)".into()
        } else {
            body
        }))
    }
}

// ---------------------------------------------------------------------------
// git_branch
// ---------------------------------------------------------------------------

struct GitBranch;

static GIT_BRANCH_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_branch".into(),
    description: "List branches (default) or create and switch to a new branch with `name`. Use before starting a feature so work lands on its own branch.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Create and switch to this new branch. Omit to list branches." },
            "all": { "type": "boolean", "default": false, "description": "When listing, include remote-tracking branches." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitBranch {
    fn spec(&self) -> &ToolSpec {
        &GIT_BRANCH_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            all: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let body = match &args.name {
            Some(name) => {
                let name = name.trim();
                if name.is_empty() {
                    bail!("`name` must not be blank");
                }
                git(ctx, &["checkout", "-b", name]).await?
            }
            None => {
                let mut git_args: Vec<&str> = vec!["branch", "--verbose"];
                if args.all {
                    git_args.push("--all");
                }
                git(ctx, &git_args).await?
            }
        };
        Ok(clamp(if body.trim().is_empty() {
            "(no output)".into()
        } else {
            body
        }))
    }
}

// ---------------------------------------------------------------------------
// git_checkout
// ---------------------------------------------------------------------------

struct GitCheckout;

static GIT_CHECKOUT_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_checkout".into(),
    description: "Switch to a branch or commit (`ref`, optionally `create: true` to make it) or restore working-tree files from HEAD (`paths`). Destructive to uncommitted changes; asks first.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "ref": { "type": "string", "description": "Branch/tag/commit to check out." },
            "create": { "type": "boolean", "default": false, "description": "Create the branch if it does not exist (git checkout -b)." },
            "paths": { "type": "array", "items": { "type": "string" }, "description": "Files to restore from HEAD (project-root relative)." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitCheckout {
    fn spec(&self) -> &ToolSpec {
        &GIT_CHECKOUT_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        // `ref` is a Rust keyword; deserialize it from the JSON key via rename.
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, rename = "ref")]
            target: Option<String>,
            #[serde(default)]
            create: bool,
            #[serde(default)]
            paths: Vec<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let paths: Vec<&str> = args
            .paths
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect();
        match (args.target.as_deref(), paths.is_empty()) {
            (Some(_), false) => bail!("pass either `ref` or `paths`, not both"),
            (None, true) => bail!("pass `ref` or `paths`"),
            (Some(target), true) => {
                let what = if args.create {
                    format!("Create and check out branch {target}")
                } else {
                    format!("Check out {target}")
                };
                ctx.confirm(what, None).await?;
                let body = if args.create {
                    git(ctx, &["checkout", "-b", target]).await?
                } else {
                    git(ctx, &["checkout", target]).await?
                };
                Ok(clamp(body))
            }
            (None, false) => {
                ctx.confirm(
                    format!("Restore {} file(s) from HEAD (discards local changes)", paths.len()),
                    Some(paths.join("\n")),
                )
                .await?;
                let mut git_args: Vec<&str> = vec!["checkout", "--"];
                git_args.extend(paths.iter().copied());
                let body = git(ctx, &git_args).await?;
                Ok(clamp(if body.trim().is_empty() {
                    "(restored)".into()
                } else {
                    body
                }))
            }
        }
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

/// Build the `git diff` argument vector from the tool args.
fn diff_args(path: Option<String>, staged: bool, rev: Option<String>) -> Vec<String> {
    let mut a = vec!["diff".to_string()];
    if staged {
        a.push("--cached".to_string());
        a.push("HEAD".to_string());
    } else {
        a.push(rev.unwrap_or_else(|| "HEAD".to_string()));
    }
    if let Some(p) = path {
        a.push("--".to_string());
        a.push(p);
    }
    a
}

/// Build the `git blame` argument vector from the tool args.
fn blame_args(
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    rev: Option<String>,
) -> Vec<String> {
    let mut a = vec!["blame".to_string(), "--date=short".to_string()];
    if let Some(rev) = rev {
        a.push(rev);
    }
    if let Some(start) = start_line {
        let end = end_line.unwrap_or(start).max(start);
        a.push(format!("-L{start},{end}"));
    }
    a.push("--".to_string());
    a.push(path.to_string());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_args_defaults_to_head_and_honours_rev_and_path() {
        assert_eq!(diff_args(None, false, None), vec!["diff", "HEAD"]);
        assert_eq!(diff_args(None, false, Some("main".into())), vec!["diff", "main"]);
        assert_eq!(
            diff_args(Some("a.rs".into()), true, None),
            vec!["diff", "--cached", "HEAD", "--", "a.rs"]
        );
        assert_eq!(
            diff_args(Some("a.rs".into()), false, Some("main...HEAD".into())),
            vec!["diff", "main...HEAD", "--", "a.rs"]
        );
    }

    #[test]
    fn blame_args_carry_rev_and_line_range() {
        assert_eq!(blame_args("a.rs", None, None, None), vec!["blame", "--date=short", "--", "a.rs"]);
        assert_eq!(
            blame_args("a.rs", Some(10), Some(20), Some("HEAD~1".into())),
            vec!["blame", "--date=short", "HEAD~1", "-L10,20", "--", "a.rs"]
        );
        // end defaults to start
        assert_eq!(
            blame_args("a.rs", Some(5), None, None),
            vec!["blame", "--date=short", "-L5,5", "--", "a.rs"]
        );
    }

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
