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
    description: "Read a text file (project-root relative). Returns raw contents, optionally windowed by line numbers.".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            fn set_plan(&self, _steps: Vec<String>) {}
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
}
