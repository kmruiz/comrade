//! Filesystem tools: `list_dir`, `read_file`, `apply_edit`, `write_file`.
//!
//! All paths are project-root relative. Mutating tools ask for human approval
//! through the [`ToolContext`] unless `auto_approve` is set, and record their
//! first write into the undo log so changes can be rolled back.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

/// Maximum characters a read/listing returns before truncation.
const MAX_OUTPUT_CHARS: usize = 6000;

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ListDir),
        Box::new(ReadFile),
        Box::new(ApplyEdit),
        Box::new(WriteFile),
    ]
}

/// Resolve a user-supplied path relative to the project root, rejecting any
/// traversal that escapes the root.
pub fn resolve(ctx: &ToolContext, user_path: &str) -> Result<PathBuf> {
    let raw = Path::new(user_path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        ctx.cwd.join(raw)
    };
    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if !normalized.starts_with(&ctx.project_root) {
        anyhow::bail!("path {user_path:?} escapes the project root");
    }
    Ok(normalized)
}

fn display_path(ctx: &ToolContext, path: &Path) -> String {
    path.strip_prefix(&ctx.project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn clamp(mut s: String) -> String {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        let mut out: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        out.push_str("\n... (output truncated)");
        s = out;
    }
    s
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

struct ListDir;

static LIST_DIR_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "list_dir".into(),
    description: "List the entries in a directory (project-root relative). Use to discover files before reading or editing.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "default": ".", "description": "Directory to list, relative to the project root." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> &ToolSpec {
        &LIST_DIR_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "default_path")]
            path: String,
        }
        fn default_path() -> String {
            ".".to_string()
        }
        let args: Args = serde_json::from_value(args)?;
        let dir = resolve(ctx, &args.path)?;
        if !dir.is_dir() {
            anyhow::bail!("{:?} is not a directory", display_path(ctx, &dir));
        }
        let mut out = String::new();
        let mut rd = tokio::fs::read_dir(&dir).await?;
        let mut entries = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            entries.push(e);
        }
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().await?.is_dir();
            if is_dir {
                out.push_str(&format!("{name}/\n"));
            } else {
                out.push_str(&format!("{name}\n"));
            }
        }
        Ok(clamp(out))
    }
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

struct ReadFile;

static READ_FILE_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "read_file".into(),
    description: "Read a text file (project-root relative). Returns raw contents, optionally windowed by line numbers.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File to read, relative to the project root." },
            "start_line": { "type": "integer", "minimum": 1, "description": "First 1-based line to return (default 1)." },
            "end_line": { "type": "integer", "minimum": 1, "description": "Last 1-based line to return (default: EOF)." }
        },
        "required": ["path"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &READ_FILE_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default)]
            start_line: Option<usize>,
            #[serde(default)]
            end_line: Option<usize>,
        }
        let args: Args = serde_json::from_value(args)?;
        let file = resolve(ctx, &args.path)?;
        let bytes = tokio::fs::read(&file)
            .await
            .with_context(|| format!("cannot read {}", display_path(ctx, &file)))?;
        if bytes.contains(&0) {
            anyhow::bail!("{:?} looks binary", display_path(ctx, &file));
        }
        let text = String::from_utf8(bytes).context("file is not valid UTF-8")?;
        let lines: Vec<&str> = text.lines().collect();
        let (lo, hi) = match (args.start_line, args.end_line) {
            (Some(s), Some(e)) => (s.saturating_sub(1), e.min(lines.len())),
            (Some(s), None) => (s.saturating_sub(1), lines.len()),
            (None, Some(e)) => (0, e.min(lines.len())),
            (None, None) => (0, lines.len()),
        };
        if lo >= lines.len() {
            return Ok(format!(
                "({} lines total; requested window is empty)",
                lines.len()
            ));
        }
        let mut out = String::new();
        for (idx, line) in lines[lo..hi].iter().enumerate() {
            out.push_str(&format!("{:>6} {}\n", lo + idx + 1, line));
        }
        out.push_str(&format!(
            "-- {}:{} of {} lines --\n",
            lo + 1,
            hi,
            lines.len()
        ));
        Ok(clamp(out))
    }
}

// ---------------------------------------------------------------------------
// apply_edit
// ---------------------------------------------------------------------------

struct ApplyEdit;

static APPLY_EDIT_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "apply_edit".into(),
    description: "Replace the first exact occurrence of `old` with `new` inside `path`. `old` must match byte-for-byte (include enough surrounding lines to be unique). Fails when not found or ambiguous; narrow the window and retry. Prefer several small precise edits over whole-file rewrites.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File to edit, relative to the project root." },
            "old": { "type": "string", "description": "Exact text to find and replace." },
            "new": { "type": "string", "description": "Replacement text." }
        },
        "required": ["path", "old", "new"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ApplyEdit {
    fn spec(&self) -> &ToolSpec {
        &APPLY_EDIT_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            old: String,
            new: String,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.old.is_empty() {
            anyhow::bail!("`old` must not be empty");
        }
        let file = resolve(ctx, &args.path)?;
        let rel = display_path(ctx, &file);
        let before = tokio::fs::read_to_string(&file)
            .await
            .with_context(|| format!("cannot read {rel}"))?;
        let count = before.matches(&args.old).count();
        if count == 0 {
            anyhow::bail!(
                "`old` block was not found in {rel}. Re-read the file and retry with an exact match."
            );
        }
        if count > 1 {
            anyhow::bail!(
                "`old` block appears {count} times in {rel}; include more context to make it unique."
            );
        }
        let after = before.replacen(&args.old, &args.new, 1);
        ctx.undo.capture(&rel, before.clone()).await?;
        ctx.confirm(
            format!("apply_edit {rel}"),
            Some(format!(
                "{rel}\n--- remove ---\n{}\n+++ insert +++\n{}",
                args.old, args.new
            )),
        )
        .await?;
        tokio::fs::write(&file, after)
            .await
            .with_context(|| format!("cannot write {rel}"))?;
        Ok(format!("Edited {rel}: replaced 1 exact block."))
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

struct WriteFile;

static WRITE_FILE_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "write_file".into(),
    description: "Overwrite (or create) a whole file (project-root relative). Parent directories are created as needed. Prefer apply_edit for targeted changes to keep diffs small.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File to write, relative to the project root." },
            "content": { "type": "string", "description": "Full new file content." }
        },
        "required": ["path", "content"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> &ToolSpec {
        &WRITE_FILE_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let file = resolve(ctx, &args.path)?;
        let rel = display_path(ctx, &file);
        let before = match tokio::fs::read_to_string(&file).await {
            Ok(text) => text,
            Err(_) => String::new(),
        };
        if before != args.content {
            ctx.undo.capture(&rel, before.clone()).await?;
            ctx.confirm(
                format!("write_file {rel}"),
                Some(format!(
                    "{rel}: {} -> {} chars",
                    before.chars().count(),
                    args.content.chars().count()
                )),
            )
            .await?;
            if let Some(parent) = file.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&file, &args.content)
                .await
                .with_context(|| format!("cannot write {rel}"))?;
            Ok(format!(
                "Wrote {rel} ({} chars).",
                args.content.chars().count()
            ))
        } else {
            Ok(format!("{rel} is unchanged; nothing written."))
        }
    }
}
