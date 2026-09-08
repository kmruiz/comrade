//! Filesystem tools: `list_dir`, `read_file`, `apply_edit`, `write_file`.
//!
//! All paths are project-root relative. Mutating tools ask for human approval
//! through the [`ToolContext`] unless `auto_approve` is set, and record their
//! first write into the undo log so changes can be rolled back.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

/// Maximum characters a read/listing returns before truncation.
const MAX_OUTPUT_CHARS: usize = 6000;

/// A bare `read_file` (no window) on a file longer than this returns only the
/// head of the file, not the whole thing, so reading one big file cannot burn
/// the whole context budget. Pass `start_line`/`end_line` (or use `read_ranges`)
/// to read past the default window; the footer reports the total line count.
const MAX_UNWINDOWED_LINES: usize = 150;

/// When `enabled`, returns the set of files that differ from HEAD; otherwise
/// `None` (no restriction). Propagates the "not a git repository" error.
fn changed_scope(ctx: &ToolContext, enabled: bool) -> Result<Option<HashSet<PathBuf>>> {
    if enabled {
        Ok(Some(comrade_tool::changed_files_abs(&ctx.project_root)?))
    } else {
        Ok(None)
    }
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ListDir),
        Box::new(ListFiles),
        Box::new(RGrep),
        Box::new(ReadFile),
        Box::new(ReadRanges),
        Box::new(ApplyEdit),
        Box::new(ApplyPatch),
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

/// 1-based file line containing byte offset `byte` in `text`.
fn line_of(text: &str, byte: usize) -> usize {
    1 + text[..byte.min(text.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
}

/// Unified-diff hunk header for an apply_edit: the old block starts at the
/// line containing its first byte; the replacement lands at the same line.
/// Returns e.g. `@@ -12,3 +12,4 @@` (new side count is 0 for a pure delete).
fn edit_location(before: &str, old: &str, new: &str) -> String {
    let start = before.find(old).expect("caller verified exactly one match");
    let old_end = line_of(before, start + old.len().saturating_sub(1));
    let old_span = old_end - line_of(before, start) + 1;
    let new_span = if new.is_empty() {
        0
    } else {
        let after = before.replacen(old, new, 1);
        let new_end = line_of(&after, start + new.len().saturating_sub(1));
        new_end - line_of(&after, start) + 1
    };
    let start = line_of(before, start);
    format!("@@ -{start},{old_span} +{start},{new_span} @@")
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

struct ListDir;

static LIST_DIR_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "list_dir".into(),
    description: "List the entries in a directory (project-root relative). Use to discover files before reading or editing. With git_modified_only, only entries containing changes vs HEAD are shown.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "default": ".", "description": "Directory to list, relative to the project root." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only show entries that differ from HEAD (staged, unstaged, untracked). Requires a git repository." }
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
            #[serde(default)]
            git_modified_only: bool,
        }
        fn default_path() -> String {
            ".".to_string()
        }
        let args: Args = serde_json::from_value(args)?;
        let changed = changed_scope(ctx, args.git_modified_only)?;
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
            let path = e.path();
            let include = match &changed {
                None => true,
                Some(set) => set.contains(&path) || set.iter().any(|c| c.starts_with(&path)),
            };
            if !include {
                continue;
            }
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
    description: "Read a text file (project-root relative). Returns raw contents, optionally windowed by line numbers. Reading a whole file costs context, so a bare read (no start_line/end_line) of a file longer than 150 lines returns only the head window and the total line count: to edit a small part, pass start_line/end_line or use read_ranges, and prefer read_symbol/structural_map to jump straight at one declaration instead of reading the whole file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File to read, relative to the project root." },
            "start_line": { "type": "integer", "minimum": 1, "description": "First 1-based line to return (default 1)." },
            "end_line": { "type": "integer", "minimum": 1, "description": "Last 1-based line to return (default: EOF)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Error unless the file differs from HEAD (staged, unstaged, untracked)." }
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
            #[serde(default)]
            git_modified_only: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let changed = changed_scope(ctx, args.git_modified_only)?;
        let file = resolve(ctx, &args.path)?;
        if let Some(set) = &changed {
            if !set.contains(&file) {
                anyhow::bail!(
                    "{rel} is not modified (git_modified_only)",
                    rel = display_path(ctx, &file)
                );
            }
        }
        let bytes = tokio::fs::read(&file)
            .await
            .with_context(|| format!("cannot read {}", display_path(ctx, &file)))?;
        if bytes.contains(&0) {
            anyhow::bail!("{:?} looks binary", display_path(ctx, &file));
        }
        let text = String::from_utf8(bytes).context("file is not valid UTF-8")?;
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        // Explicit windows are honoured as requested; a bare read (no window at
        // all) is capped to the head of the file so whole big files don't eat
        // the context budget. The footer always reports the total line count
        // and how to read the rest.
        let (lo, hi) = match (args.start_line, args.end_line) {
            (Some(s), Some(e)) => (s.saturating_sub(1), e.min(total)),
            (Some(s), None) => (s.saturating_sub(1), total),
            (None, Some(e)) => (0, e.min(total)),
            (None, None) => (0, MAX_UNWINDOWED_LINES.min(total)),
        };
        if lo >= total {
            return Ok(format!("({total} lines total; requested window is empty)"));
        }
        let unwindowed_cap =
            args.start_line.is_none() && args.end_line.is_none() && total > MAX_UNWINDOWED_LINES;
        // Clamp the content first so the informative footer below always
        // survives (a plain whole-output clamp would cut it off).
        let mut content = String::new();
        for (idx, line) in lines[lo..hi].iter().enumerate() {
            content.push_str(&format!("{:>6} {}\n", lo + idx + 1, line));
        }
        let mut out = clamp(content);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("-- {}..{} of {total} lines", lo + 1, hi));
        if unwindowed_cap {
            out.push_str(&format!(
                " (file longer than the {MAX_UNWINDOWED_LINES}-line default window: \
                 pass start_line/end_line or use read_ranges to read the rest)"
            ));
        }
        out.push_str(" --\n");
        Ok(out)
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
        let loc = edit_location(&before, &args.old, &args.new);
        ctx.undo.capture(&rel, before.clone()).await?;
        ctx.confirm(
            format!("apply_edit {rel}"),
            Some(format!(
                "{rel}\n{loc}\n--- remove ---\n{}\n+++ insert +++\n{}",
                args.old, args.new
            )),
        )
        .await?;
        tokio::fs::write(&file, after)
            .await
            .with_context(|| format!("cannot write {rel}"))?;
        Ok(format!("Edited {rel}: replaced 1 exact block ({loc})."))
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

// ---------------------------------------------------------------------------
// list_files
// ---------------------------------------------------------------------------

struct ListFiles;

static LIST_FILES_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "list_files".into(),
    description: "List project files matching a glob pattern (project-root relative), one per line. Prefer this over walking with list_dir. `*` matches within a path segment, `**` matches across directories. Examples: \"**/*.rs\", \"src/**/*.rs\", \"Cargo.toml\", \"*.md\".".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Glob pattern matched against paths relative to the project root." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only list files that differ from HEAD (staged, unstaged, untracked)." }
        },
        "required": ["pattern"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> &ToolSpec {
        &LIST_FILES_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
            #[serde(default)]
            git_modified_only: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let changed = changed_scope(ctx, args.git_modified_only)?;
        let pattern = args.pattern.trim().trim_start_matches("./");
        if pattern.is_empty() || pattern.contains('\\') || pattern.contains("..") {
            anyhow::bail!("invalid glob pattern {:?}", args.pattern);
        }

        let mut files = Vec::new();
        walk(&ctx.project_root, &mut files);
        files.sort();

        let mut matches = Vec::new();
        for file in files {
            if let Some(set) = &changed {
                if !set.contains(&file) {
                    continue;
                }
            }
            let rel = file
                .strip_prefix(&ctx.project_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if glob_matches(pattern, &rel) {
                matches.push(rel);
            }
        }

        let total = matches.len();
        const MAX_LINES: usize = 400;
        let mut out = format!("{total} file(s) matching {pattern:?}:\n");
        for rel in matches.iter().take(MAX_LINES) {
            out.push_str(&format!("{rel}\n"));
        }
        if total > MAX_LINES {
            out.push_str(&format!("... and {} more\n", total - MAX_LINES));
        }
        Ok(clamp(out))
    }
}

// ---------------------------------------------------------------------------
// rgrep
// ---------------------------------------------------------------------------

struct RGrep;

static RGREP_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "rgrep".into(),
    description: "Search for literal text in project files, like a filtered grep. Returns matching lines as file:line: text. Use when you need to find every place a string, identifier, or phrase appears. `glob` restricts which files are searched (default \"**/*\"). Matching is substring-based; use ignore_case for case-insensitive search.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Literal text to search for (not a regex)." },
            "glob": { "type": "string", "default": "**/*", "description": "Glob restricting files to search." },
            "ignore_case": { "type": "boolean", "default": false, "description": "Case-insensitive match." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD (staged, unstaged, untracked)." }
        },
        "required": ["pattern"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RGrep {
    fn spec(&self) -> &ToolSpec {
        &RGREP_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
            #[serde(default = "default_glob")]
            glob: String,
            #[serde(default)]
            ignore_case: bool,
            #[serde(default)]
            git_modified_only: bool,
        }
        fn default_glob() -> String {
            "**/*".to_string()
        }
        let args: Args = serde_json::from_value(args)?;
        if args.pattern.is_empty() {
            anyhow::bail!("`pattern` must not be empty");
        }
        let changed = changed_scope(ctx, args.git_modified_only)?;

        let matches = search_files(
            &ctx.project_root,
            &args.glob,
            &args.pattern,
            args.ignore_case,
            changed.as_ref(),
        )?;

        let total = matches.len();
        const MAX_LINES: usize = 300;
        let mut out = format!("{total} match(es) for {:?}:\n", args.pattern);
        for (file, line, text) in matches.iter().take(MAX_LINES) {
            out.push_str(&format!("{file}:{line}: {text}\n"));
        }
        if total > MAX_LINES {
            out.push_str(&format!("... and {} more\n", total - MAX_LINES));
        }
        Ok(clamp(out))
    }
}

/// Search matching lines across files under `root` (relative `glob`), returning
/// (file, 1-based line, trimmed line text) tuples. When `only` is `Some`, only
/// files in that set are searched.
fn search_files(
    root: &Path,
    glob: &str,
    needle: &str,
    ignore_case: bool,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<(String, usize, String)>> {
    let glob = glob.trim().trim_start_matches("./");
    if glob.is_empty() || glob.contains('\\') || glob.contains("..") {
        anyhow::bail!("invalid glob {:?}", glob);
    }
    let needle_owned;
    let needle: &str = if ignore_case {
        needle_owned = needle.to_lowercase();
        &needle_owned
    } else {
        needle
    };

    let mut files = Vec::new();
    walk(root, &mut files);
    files.sort();

    let mut out = Vec::new();
    for file in files {
        if let Some(set) = only {
            if !set.contains(&file) {
                continue;
            }
        }
        let rel = file
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !glob_matches(glob, &rel) {
            continue;
        }
        if file
            .metadata()
            .map(|m| m.len() > 4 * 1024 * 1024)
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let hay = if ignore_case {
            text.to_lowercase()
        } else {
            text.clone()
        };
        let src_lines: Vec<&str> = text.split('\n').collect();
        for (idx, line) in hay.split('\n').enumerate() {
            if line.contains(needle) {
                let src = src_lines[idx].trim();
                let trimmed: String = src.chars().take(160).collect();
                out.push((rel.clone(), idx + 1, trimmed));
            }
        }
    }
    Ok(out)
}

/// Directories never walked by file listings, regardless of ignore files.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "vendor",
    ".idea",
    ".vscode",
    "dist",
];

/// Collect regular files under `dir`, honouring gitignore-style rules
/// (`.gitignore`, `.ignore`, git excludes/global) and the hardcoded
/// [`IGNORED_DIRS`]. Backed by the `ignore` crate (ripgrep's walker).
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .standard_filters(true)
        .hidden(true)
        .require_git(false)
        .follow_links(false);
    for result in builder.build() {
        let Ok(entry) = result else { continue };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        let skipped = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .any(|seg| IGNORED_DIRS.contains(&seg));
        if skipped {
            continue;
        }
        out.push(entry.into_path());
    }
}

/// Minimal glob: `*` matches any chars within a segment, `?` one char, `**`
/// matches across directory separators (and a leading `**/` also matches zero
/// directories). Nothing else is special-cased.
fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_at(pattern.as_bytes(), 0, text.as_bytes(), 0)
}

fn glob_at(p: &[u8], pi: usize, t: &[u8], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        b'*' => {
            // Collapse consecutive '*'s; more than one means `**`.
            let mut j = pi;
            while j < p.len() && p[j] == b'*' {
                j += 1;
            }
            let is_double = j - pi >= 2;
            // A `**/` run may match zero directories (skip the slash too).
            if is_double && j < p.len() && p[j] == b'/' && glob_at(p, j + 1, t, ti) {
                return true;
            }
            // `*` must not cross a '/'; `**` may. Try each suffix position.
            for k in ti..=t.len() {
                if is_double || no_sep(&t[ti..k]) {
                    if glob_at(p, j, t, k) {
                        return true;
                    }
                }
            }
            false
        }
        b'?' => ti < t.len() && t[ti] != b'/' && glob_at(p, pi + 1, t, ti + 1),
        c => ti < t.len() && t[ti] == c && glob_at(p, pi + 1, t, ti + 1),
    }
}

fn no_sep(slice: &[u8]) -> bool {
    !slice.contains(&b'/')
}

// ---------------------------------------------------------------------------
// read_ranges
// ---------------------------------------------------------------------------

struct ReadRanges;

static READ_RANGES_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "read_ranges".into(),
    description: "Read several non-contiguous 1-based line ranges of one file in a single call. Each range is [start, end] inclusive. Use instead of repeated read_file calls when you need a few windows of the same file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File to read, relative to the project root." },
            "ranges": { "type": "array", "items": { "type": "array", "items": { "type": "integer", "minimum": 1 }, "minItems": 2, "maxItems": 2 }, "minItems": 1, "description": "Inclusive [start, end] line ranges." }
        },
        "required": ["path", "ranges"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ReadRanges {
    fn spec(&self) -> &ToolSpec {
        &READ_RANGES_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            ranges: Vec<(usize, usize)>,
        }
        let args: Args = serde_json::from_value(args)?;
        let file = resolve(ctx, &args.path)?;
        let rel = display_path(ctx, &file);
        let bytes = tokio::fs::read(&file)
            .await
            .with_context(|| format!("cannot read {rel}"))?;
        if bytes.contains(&0) {
            anyhow::bail!("{rel} looks binary");
        }
        let text = String::from_utf8(bytes).context("file is not valid UTF-8")?;
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let mut out = String::new();
        for (start, end) in args.ranges {
            let lo = start.saturating_sub(1);
            let hi = end.min(total);
            if lo >= total {
                out.push_str(&format!("# {start}..{end} (out of range, {total} lines)\n"));
                continue;
            }
            out.push_str(&format!("# {start}..{end} of {total}\n"));
            for (i, line) in lines[lo..hi].iter().enumerate() {
                out.push_str(&format!("{:>6} {}\n", lo + i + 1, line));
            }
        }
        Ok(clamp(out))
    }
}

// ---------------------------------------------------------------------------
// apply_patch (unified diff)
// ---------------------------------------------------------------------------

struct ApplyPatch;

static APPLY_PATCH_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "apply_patch".into(),
    description: "Apply a unified diff to files (project-root relative), much more compact than apply_edit: send only +/- hunks with a little surrounding context. Format:\n  --- a/<path>\n  +++ b/<path>\n  @@ ... @@ (ignored)\n    context line\n  - removed line\n  + added line\nEach hunk's old block must appear exactly once in the file. The change is approved by the human before being written.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "diff": { "type": "string", "description": "Unified diff text." }
        },
        "required": ["diff"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> &ToolSpec {
        &APPLY_PATCH_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            diff: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let patches = parse_unified(&args.diff)?;
        if patches.is_empty() {
            anyhow::bail!("no hunks found in the diff");
        }

        // Compute the intended changes first, then ask for approval, then write.
        let mut pending: Vec<(String, String, String, usize)> = Vec::new();
        let mut preview = String::new();
        for patch in &patches {
            let abs = resolve(ctx, &patch.path)?;
            let rel = display_path(ctx, &abs);
            let before = tokio::fs::read_to_string(&abs)
                .await
                .with_context(|| format!("cannot read {rel}"))?;
            let after = apply_hunks(&before, &patch.hunks)
                .with_context(|| format!("failed to apply diff to {rel}"))?;
            if before == after {
                continue;
            }
            preview.push_str(&format!("{rel}: {} hunk(s)\n", patch.hunks.len()));
            pending.push((rel, before, after, patch.hunks.len()));
        }
        if pending.is_empty() {
            return Ok("Nothing to apply: diff matches current content.".to_string());
        }
        ctx.confirm(
            format!("apply_patch ({pending} file(s))", pending = pending.len()),
            Some(preview),
        )
        .await?;

        let mut applied = 0usize;
        for (rel, before, after, hunks) in pending {
            ctx.undo.capture(&rel, before).await?;
            let abs = resolve(ctx, &rel)?;
            tokio::fs::write(&abs, after)
                .await
                .with_context(|| format!("cannot write {rel}"))?;
            applied += hunks;
        }
        Ok(format!(
            "Applied {applied} hunk(s) across {} file(s).",
            patches.len()
        ))
    }
}

/// A parsed file diff: one or more hunks to apply in order.
struct FilePatch {
    path: String,
    hunks: Vec<Hunk>,
}

struct Hunk {
    /// Old lines to find (context + removals, in order).
    old: Vec<String>,
    /// Replacement lines (context + additions).
    new: Vec<String>,
}

/// Parse a simplified unified diff: `--- a/x` / `+++ b/x` headers, `@@`
/// lines ignored, hunk body = context/`-`/`+` lines.
fn parse_unified(diff: &str) -> Result<Vec<FilePatch>> {
    let mut patches = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut body: Vec<String> = Vec::new();

    let finish = |cur_path: &Option<String>,
                  body: &mut Vec<String>,
                  patches: &mut Vec<FilePatch>|
     -> Result<()> {
        if let Some(path) = cur_path {
            let hunks = hunks_from_body(body)?;
            if !hunks.is_empty() {
                patches.push(FilePatch {
                    path: path.clone(),
                    hunks,
                });
            }
        }
        *body = Vec::new();
        Ok(())
    };

    for raw in diff.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("+++ ") {
            finish(&cur_path, &mut body, &mut patches)?;
            cur_path = Some(rest.trim_start_matches("b/").trim().to_string());
            body.clear();
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("@@") || line.starts_with("\\ No newline") {
            continue;
        }
        body.push(line.to_string());
    }
    finish(&cur_path, &mut body, &mut patches)?;
    Ok(patches)
}

fn hunks_from_body(body: &[String]) -> Result<Vec<Hunk>> {
    // Group contiguous body lines into hunks (each is one edit block).
    let mut hunks = Vec::new();
    let mut old = Vec::new();
    let mut new = Vec::new();
    for line in body {
        match line.chars().next() {
            Some(' ') => {
                old.push(line[1..].to_string());
                new.push(line[1..].to_string());
            }
            Some('-') => old.push(line[1..].to_string()),
            Some('+') => new.push(line[1..].to_string()),
            _ => {}
        }
    }
    if !old.is_empty() {
        hunks.push(Hunk { old, new });
    }
    Ok(hunks)
}

/// Apply parsed hunks to `before`, returning the new content. Each hunk's old
/// block must occur exactly once.
fn apply_hunks(before: &str, hunks: &[Hunk]) -> Result<String> {
    let mut content: Vec<String> = before.lines().map(str::to_string).collect();
    for hunk in hunks {
        let window = content.len();
        let old_len = hunk.old.len();
        if old_len == 0 {
            continue;
        }
        let mut matches = Vec::new();
        if window >= old_len {
            for i in 0..=(window - old_len) {
                if content[i..i + old_len] == hunk.old[..] {
                    matches.push(i);
                }
            }
        }
        if matches.is_empty() {
            anyhow::bail!("hunk block not found");
        }
        if matches.len() > 1 {
            anyhow::bail!(
                "hunk block is ambiguous ({} occurrences); add more context lines",
                matches.len()
            );
        }
        let at = matches[0];
        let mut next = Vec::with_capacity(content.len() - old_len + hunk.new.len());
        next.extend(content[..at].iter().cloned());
        next.extend(hunk.new.iter().cloned());
        next.extend(content[at + old_len..].iter().cloned());
        content = next;
    }
    // Reconstruct with a trailing newline only if the original had one.
    let joined = content.join("\n");
    if before.ends_with('\n') {
        Ok(format!("{joined}\n"))
    } else {
        Ok(joined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_of_counts_newlines_before_byte() {
        assert_eq!(line_of("a\nb\nc", 0), 1);
        assert_eq!(line_of("a\nb\nc", 1), 1); // the '\n' itself is line 1's
        assert_eq!(line_of("a\nb\nc", 2), 2);
        assert_eq!(line_of("a\nb\nc", 4), 3);
        assert_eq!(line_of("", 0), 1);
    }

    #[test]
    fn edit_location_reports_line_range() {
        let before = "line one\nline two\nline three\nline four\n";
        // Replace whole line 2 with two lines.
        let loc = edit_location(before, "line two\n", "line 2a\nline 2b\n");
        assert_eq!(loc, "@@ -2,1 +2,2 @@");
        // Replace line 3 with nothing (delete).
        let loc = edit_location(before, "line three\n", "");
        assert_eq!(loc, "@@ -3,1 +3,0 @@");
        // A single-line replacement elsewhere keeps line 1 untouched.
        let loc = edit_location(before, "line one\n", "first line\n");
        assert_eq!(loc, "@@ -1,1 +1,1 @@");
    }

    #[test]
    fn glob_rules() {
        // exact
        assert!(glob_matches("Cargo.toml", "Cargo.toml"));
        assert!(!glob_matches("Cargo.toml", "cargo.toml"));
        // * within segment only
        assert!(glob_matches("*.rs", "main.rs"));
        assert!(!glob_matches("*.rs", "src/main.rs"));
        // ** across segments
        assert!(glob_matches("**/*.rs", "src/main.rs"));
        assert!(glob_matches("**/*.rs", "main.rs"));
        assert!(glob_matches("**/*.rs", "a/b/c/lib.rs"));
        assert!(!glob_matches("**/*.rs", "a/b/lib.txt"));
        // ** zero-dir prefix handled
        assert!(glob_matches("src/**/*.rs", "src/main.rs"));
        assert!(glob_matches("src/**/*.rs", "src/a/b.rs"));
        assert!(!glob_matches("src/**/*.rs", "lib.rs"));
    }

    #[test]
    fn rgrep_filters_by_glob_and_case() {
        let root = std::env::temp_dir().join(format!(
            "comrade-rgrep-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "fn main() { hello(); }\nfn hello() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/b.txt"), "just hello text\n").unwrap();
        std::fs::write(root.join("README.md"), "Hello world\n").unwrap();

        let m = super::search_files(&root, "**/*.rs", "hello", false, None).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m.iter().all(|(f, _, _)| f.ends_with(".rs")));
        // glob restricts to md
        let m2 = super::search_files(&root, "*.md", "hello", true, None).unwrap();
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].0, "README.md");
        assert_eq!(m2[0].2, "Hello world");
        // case-sensitive finds nothing in md
        assert!(
            super::search_files(&root, "*.md", "hello", false, None)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_honours_gitignore_and_ignore() {
        let root = std::env::temp_dir().join(format!(
            "comrade-ignore-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::create_dir_all(root.join("also-ignored")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n**/*.gen.rs\n").unwrap();
        std::fs::write(root.join(".ignore"), "also-ignored/\n").unwrap();
        std::fs::write(root.join("ignored/x.rs"), "fn x() {}\n").unwrap();
        std::fs::write(root.join("also-ignored/y.rs"), "fn y() {}\n").unwrap();
        std::fs::write(root.join("keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(root.join("junk.gen.rs"), "fn junk() {}\n").unwrap();
        std::fs::write(root.join("plain.txt"), "hello\n").unwrap();

        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut rels: Vec<String> = files
            .iter()
            .filter_map(|p| p.strip_prefix(&root).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        rels.sort();
        // ignored/, also-ignored/ and the generated file must be excluded;
        // .gitignore/.ignore themselves are hidden files, so also absent.
        assert_eq!(
            rels,
            vec!["keep.rs".to_string(), "plain.txt".to_string()],
            "{rels:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_files_git_modified_only_scopes_to_changes() {
        use std::sync::Arc;

        use async_trait::async_trait;
        use comrade_tool::{
            PlanStatus, PlanStep, PlanTarget, SessionControl, UndoLog, UserIo, UserPrompt,
            UserReply,
        };

        struct StubSession;
        impl SessionControl for StubSession {
            fn set_title(&self, _t: &str) {}
            fn title(&self) -> String {
                "test".into()
            }
            fn set_plan(&self, _steps: Vec<comrade_tool::PlanStepDraft>) {}
            fn plan(&self) -> Vec<PlanStep> {
                vec![]
            }
            fn update_plan(&self, _t: PlanTarget, _s: PlanStatus, _n: Option<String>) -> bool {
                true
            }
            fn finish_plan(&self, _s: Option<String>) {}
            fn set_status(&self, _s: &str) {}
            fn status(&self) -> String {
                String::new()
            }
        }
        struct StubUser;
        #[async_trait]
        impl UserIo for StubUser {
            async fn ask(&self, _p: UserPrompt) -> anyhow::Result<UserReply> {
                Ok(UserReply::Answer("yes".into()))
            }
        }
        struct StubUndo;
        #[async_trait]
        impl UndoLog for StubUndo {
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

        fn git(root: &std::path::Path, args: &[&str]) {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let root = std::env::temp_dir().join(format!(
            "comrade-gitmodified-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("untouched.rs"), "fn u() {}\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "init"]);

        // now a modified + an untracked change
        std::fs::write(root.join("untouched.rs"), "fn u() { changed }\n").unwrap();
        std::fs::write(root.join("brand_new.rs"), "fn n() {}\n").unwrap();
        std::fs::write(root.join("notes.txt"), "not rust\n").unwrap();

        let session: Arc<dyn SessionControl> = Arc::new(StubSession);
        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session,
            user: Arc::new(StubUser),
            undo: Arc::new(StubUndo),
            auto_approve: true,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            stop: None,
        };
        let list = ListFiles;
        let args = json!({ "pattern": "**/*.rs", "git_modified_only": true });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(list.invoke(&ctx, args)).unwrap();
        assert!(out.contains("untouched.rs"), "{out}");
        assert!(out.contains("brand_new.rs"), "{out}");
        assert!(out.contains("2 file(s)"), "{out}");
        assert!(!out.contains("notes.txt"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_file_default_window_caps_bare_reads_of_long_files() {
        use std::sync::Arc;

        use async_trait::async_trait;
        use comrade_tool::{
            PlanStatus, PlanStep, PlanTarget, SessionControl, UndoLog, UserIo, UserPrompt,
            UserReply,
        };

        struct StubSession;
        impl SessionControl for StubSession {
            fn set_title(&self, _t: &str) {}
            fn title(&self) -> String {
                "test".into()
            }
            fn set_plan(&self, _steps: Vec<comrade_tool::PlanStepDraft>) {}
            fn plan(&self) -> Vec<PlanStep> {
                vec![]
            }
            fn update_plan(&self, _t: PlanTarget, _s: PlanStatus, _n: Option<String>) -> bool {
                true
            }
            fn finish_plan(&self, _s: Option<String>) {}
            fn set_status(&self, _s: &str) {}
            fn status(&self) -> String {
                String::new()
            }
        }
        struct StubUser;
        #[async_trait]
        impl UserIo for StubUser {
            async fn ask(&self, _p: UserPrompt) -> anyhow::Result<UserReply> {
                Ok(UserReply::Answer("yes".into()))
            }
        }
        struct StubUndo;
        #[async_trait]
        impl UndoLog for StubUndo {
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

        let root = std::env::temp_dir().join(format!(
            "comrade-readfile-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let body: String = (1..=300).map(|i| format!("body line {i}\n")).collect();
        std::fs::write(root.join("big.rs"), &body).unwrap();

        let ctx = ToolContext {
            project_root: root.clone(),
            cwd: root.clone(),
            session: Arc::new(StubSession),
            user: Arc::new(StubUser),
            undo: Arc::new(StubUndo),
            auto_approve: true,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            stop: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // A bare read of a 300-line file returns only the head window, still
        // states the total, and tells the caller how to read the rest.
        let out = rt
            .block_on(ReadFile.invoke(&ctx, json!({ "path": "big.rs" })))
            .unwrap();
        assert!(out.contains("body line 1"), "{out}");
        assert!(!out.contains("body line 200"), "{out}");
        assert!(out.contains("-- 1..150 of 300 lines"), "{out}");
        assert!(
            out.contains("pass start_line/end_line or use read_ranges to read the rest"),
            "{out}"
        );

        // An explicit window is honoured exactly, even past the default cap.
        let out = rt
            .block_on(ReadFile.invoke(
                &ctx,
                json!({ "path": "big.rs", "start_line": 250, "end_line": 260 }),
            ))
            .unwrap();
        assert!(out.contains("body line 250"), "{out}");
        assert!(out.contains("body line 260"), "{out}");
        assert!(!out.contains("body line 1"), "{out}");
        assert!(out.contains("-- 250..260 of 300 lines"), "{out}");

        // A file small enough to fit the default window is returned whole.
        std::fs::write(root.join("small.rs"), "a\nb\nc\n").unwrap();
        let out = rt
            .block_on(ReadFile.invoke(&ctx, json!({ "path": "small.rs" })))
            .unwrap();
        assert!(out.contains("a\n") && out.contains("c"), "{out}");
        assert!(out.contains("-- 1..3 of 3 lines"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn parses_and_applies_removal_and_addition() {
        let before = "line one\nline two\nline three\n";
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n line one\n-line two\n+line TWO\n line three\n";
        let patches = parse_unified(diff).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "a.txt");
        let after = apply_hunks(before, &patches[0].hunks).unwrap();
        assert_eq!(after, "line one\nline TWO\nline three\n");
    }

    #[test]
    fn appends_only_with_context() {
        let before = "fn main() {}\n";
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,3 @@\n fn main() {}\n+// done\n";
        let patches = parse_unified(diff).unwrap();
        let after = apply_hunks(before, &patches[0].hunks).unwrap();
        assert_eq!(after, "fn main() {}\n// done\n");
    }

    #[test]
    fn ambiguous_hunk_is_rejected() {
        let before = "a\nb\na\n";
        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let patches = parse_unified(diff).unwrap();
        assert!(apply_hunks(before, &patches[0].hunks).is_err());
    }

    #[test]
    fn multi_file_patch_parses_paths() {
        let diff = "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-x\n+y\n--- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-x\n+z\n";
        let patches = parse_unified(diff).unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].path, "one.txt");
        assert_eq!(patches[1].path, "two.txt");
    }
}
