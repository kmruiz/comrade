//! Tree-sitter powered code tools.

mod chunks;
mod engine;

pub use chunks::{CodeChunk, chunks_of_file, code_chunks};

use engine::KIND_LABELS;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(TsFindReferences),
        Box::new(TsRename),
        Box::new(TsListSymbols),
        Box::new(TsStructuralMap),
        Box::new(TsReadSymbol),
        Box::new(TsFindSymbol),
        Box::new(TsTestImpact),
    ]
}

/// When `enabled`, the set of files that differ from HEAD.
fn changed_scope(ctx: &ToolContext, enabled: bool) -> Result<Option<HashSet<PathBuf>>> {
    if enabled {
        Ok(Some(comrade_tool::changed_files_abs(&ctx.project_root)?))
    } else {
        Ok(None)
    }
}

/// When a scope is set and an explicit path was given, require that a *file*
/// path is part of the change set. Directory paths are always allowed: the
/// per-file scope check happens inside the engine.
fn guard_path_scope(
    ctx: &ToolContext,
    path: &Option<String>,
    scope: &Option<HashSet<PathBuf>>,
) -> Result<()> {
    if let (Some(p), Some(set)) = (path, scope) {
        let abs = ctx.project_root.join(p);
        if !abs.starts_with(&ctx.project_root) {
            anyhow::bail!("path {p:?} escapes the project root");
        }
        if abs.is_dir() {
            return Ok(());
        }
        if !set.contains(&abs) {
            anyhow::bail!("{p} is not modified (git_modified_only)");
        }
    }
    Ok(())
}

fn clamp(mut s: String) -> String {
    const MAX: usize = 6000;
    if s.chars().count() > MAX {
        let mut out: String = s.chars().take(MAX).collect();
        out.push_str("\n... (output truncated)");
        s = out;
    }
    s
}

fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Validate an optional declaration-kind filter. Accepts the short labels
/// (`fn`, `struct`, ...) case-insensitively and returns the canonical label.
fn normalize_kind(kind: Option<&str>) -> Result<Option<&str>> {
    let Some(k) = kind.map(str::trim).filter(|k| !k.is_empty()) else {
        return Ok(None);
    };
    KIND_LABELS
        .iter()
        .find(|l| l.eq_ignore_ascii_case(k))
        .copied()
        .map(Some)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown kind {k:?}; expected one of {}",
                KIND_LABELS.join(", ")
            )
        })
}

// ---------------------------------------------------------------------------
// ts_find_references
// ---------------------------------------------------------------------------

struct TsFindReferences;

static TS_FIND_REFERENCES_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_find_references".into(),
    description: "Find uses of a symbol (fn/struct/field/variable name) across the project via tree-sitter. Matches identifier tokens only, never inside strings or comments. Lexical, not semantic.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Identifier to search for." },
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD (staged, unstaged, untracked)." }
        },
        "required": ["symbol"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsFindReferences {
    fn spec(&self) -> &ToolSpec {
        &TS_FIND_REFERENCES_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            symbol: String,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git_modified_only: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        // A "fn foo" style kind keyword is not an identifier: strip it so the
        // token search looks for the bare name.
        let symbol = engine::split_kind_prefix(&args.symbol).1;
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let occ = engine::find_occurrences(
            &ctx.project_root,
            symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )?;
        if occ.is_empty() {
            return Ok(format!("No occurrences of {:?} found.", symbol));
        }
        let mut out = format!("Occurrences of {symbol}:\n");
        let mut seen = 0usize;
        for o in &occ {
            out.push_str(&format!(
                "{}:{}:{}  {}\n",
                o.file,
                o.line,
                o.column,
                o.context_line.trim()
            ));
            seen += 1;
            if seen >= 200 {
                out.push_str(&format!("... and {} more\n", occ.len() - seen));
                break;
            }
        }
        Ok(clamp(out))
    }
}

// ---------------------------------------------------------------------------
// ts_rename
// ---------------------------------------------------------------------------

struct TsRename;

static TS_RENAME_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_rename".into(),
    description: "Rename a symbol across the project by rewriting every tree-sitter identifier token equal to `symbol`. Approximate but safe (never matches inside strings/comments). Interactive: approve the preview first.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Current identifier." },
            "new": { "type": "string", "description": "New identifier." },
            "path": { "type": "string", "description": "Optional file to restrict the rename to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only rename occurrences in files that differ from HEAD (staged, unstaged, untracked)." }
        },
        "required": ["symbol", "new"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsRename {
    fn spec(&self) -> &ToolSpec {
        &TS_RENAME_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            symbol: String,
            #[serde(rename = "new")]
            new_name: String,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git_modified_only: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        // "fn helper" means the helper: kind keywords are not part of the name.
        let symbol = engine::split_kind_prefix(&args.symbol).1;
        if !is_valid_identifier(symbol) {
            anyhow::bail!("{symbol:?} is not a valid identifier");
        }
        if !is_valid_identifier(&args.new_name) {
            anyhow::bail!("{:?} is not a valid identifier", args.new_name);
        }
        if symbol == args.new_name {
            anyhow::bail!("new name equals old name");
        }
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let edits = engine::rename_edits(
            &ctx.project_root,
            symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )?;
        let total: usize = edits.iter().map(|e| e.spans.len()).sum();
        if total == 0 {
            return Ok(format!(
                "No occurrences of {symbol:?} found; nothing renamed."
            ));
        }

        let mut preview = String::new();
        let mut shown = 0usize;
        for e in &edits {
            for (start, _end) in &e.spans {
                let line_start = e.text[..*start].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let context_line = e.text[line_start..].split('\n').next().unwrap_or("");
                preview.push_str(&format!("{}: {}\n", e.rel, context_line.trim()));
                shown += 1;
                if shown >= 40 {
                    preview.push_str(&format!("... and {} more\n", total - shown));
                    break;
                }
            }
            if shown >= 40 {
                break;
            }
        }

        for e in &edits {
            ctx.undo.capture(&e.rel, e.text.clone()).await?;
        }
        ctx.confirm(
            format!(
                "rename {symbol} -> {new_name} ({total} occurrences)",
                new_name = args.new_name
            ),
            Some(preview),
        )
        .await?;

        engine::apply_edits(&ctx.project_root, &edits, &args.new_name)?;
        Ok(format!(
            "Renamed {symbol} -> {new_name} in {files} file(s), {total} occurrence(s).",
            symbol = args.symbol,
            new_name = args.new_name,
            files = edits.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// ts_list_symbols
// ---------------------------------------------------------------------------

struct TsListSymbols;

static TS_LIST_SYMBOLS_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_list_symbols".into(),
    description: "List top-level declarations (fn, struct, enum, trait, impl, mod) in a file or the whole project, with line numbers. Use to orient before finding references or editing.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Optional file to inspect (project-root relative). Defaults to the whole project." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only inspect files that differ from HEAD (staged, unstaged, untracked)." },
            "with_signatures": { "type": "boolean", "default": false, "description": "Include each declaration's one-line signature." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsListSymbols {
    fn spec(&self) -> &ToolSpec {
        &TS_LIST_SYMBOLS_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git_modified_only: bool,
            #[serde(default)]
            with_signatures: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let symbols = if args.with_signatures {
            engine::list_symbol_signatures(&ctx.project_root, args.path.as_deref(), scope.as_ref())?
        } else {
            engine::list_symbols(&ctx.project_root, args.path.as_deref(), scope.as_ref())?
        };
        if symbols.is_empty() {
            return Ok("No symbols found.".to_string());
        }
        Ok(clamp(symbols.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// ts_structural_map
// ---------------------------------------------------------------------------

struct TsStructuralMap;

static TS_STRUCTURAL_MAP_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_structural_map".into(),
    description: "Build a tree-sitter structural map of the project (or one file): declarations nested under their containers (Rust mod/impl/trait, JS/TS classes and namespaces), with line numbers. Filter by file or declaration kind. Orient fast.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Optional file or subdir to map (project-root relative). Defaults to the whole project." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only map files that differ from HEAD (staged, unstaged, untracked)." },
            "with_signatures": { "type": "boolean", "default": false, "description": "Include each declaration's one-line signature." },
            "kinds": { "type": "array", "items": { "type": "string", "enum": KIND_LABELS }, "description": "Restrict to these declaration kinds (short labels). Default: all kinds." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsStructuralMap {
    fn spec(&self) -> &ToolSpec {
        &TS_STRUCTURAL_MAP_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git_modified_only: bool,
            #[serde(default)]
            with_signatures: bool,
            #[serde(default)]
            kinds: Vec<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let include: HashSet<String> = args.kinds.iter().map(|k| k.to_lowercase()).collect();
        for k in &include {
            if !KIND_LABELS.contains(&k.as_str()) {
                anyhow::bail!(
                    "unknown kind {k:?}; expected one of {}",
                    KIND_LABELS.join(", ")
                );
            }
        }
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let lines = engine::structural_map(
            &ctx.project_root,
            args.path.as_deref(),
            scope.as_ref(),
            args.with_signatures,
            &include,
        )?;
        if lines.is_empty() {
            return Ok("No declarations matched the given filters.".to_string());
        }
        Ok(clamp(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// ts_read_symbol
// ---------------------------------------------------------------------------

struct TsReadSymbol;

static TS_READ_SYMBOL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_read_symbol".into(),
    description: "Locate a symbol's declaration and read its kind, one-line signature and file:line. With body:true, also returns the full declaration body (fn, struct, enum, const, ...). Use before editing a specific item instead of reading the whole file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Identifier whose declaration to locate (bare name, not \"fn name\")." },
            "body": { "type": "boolean", "default": false, "description": "When true, return the full declaration body too. Default false: kind + signature + location only." },
            "type": { "type": "string", "enum": KIND_LABELS, "description": "Optional declaration kind to narrow to (fn, struct, enum, trait, impl, mod, type, static, const)." },
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD." }
        },
        "required": ["symbol"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsReadSymbol {
    fn spec(&self) -> &ToolSpec {
        &TS_READ_SYMBOL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            symbol: String,
            #[serde(default)]
            body: bool,
            #[serde(default, rename = "type")]
            kind: Option<String>,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git_modified_only: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let kind = normalize_kind(args.kind.as_deref())?;
        // Kind keywords in the input are a filter, not part of the identifier:
        // report the bare name and hand the engine the stripped identifier.
        let (pfx, bare) = engine::split_kind_prefix(&args.symbol);
        if let (Some(p), Some(k)) = (pfx, kind)
            && !p.eq_ignore_ascii_case(k)
        {
            anyhow::bail!(
                "symbol {:?} already names kind {p:?}, which conflicts with the type filter {k:?}",
                bare
            );
        }
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        // body=true -> engine::read_symbol (full declaration); false ->
        // engine::find_definition (kind + signature + location only).
        let def = if args.body {
            engine::read_symbol(
                &ctx.project_root,
                bare,
                args.path.as_deref(),
                scope.as_ref(),
                pfx.or(kind),
            )?
        } else {
            engine::find_definition(
                &ctx.project_root,
                bare,
                args.path.as_deref(),
                scope.as_ref(),
                pfx.or(kind),
            )?
        };
        match def {
            Some(def) if !args.body => Ok(format!(
                "`{symbol}` defined at {file}:{line}\nkind: {kind}\nsignature: {signature}",
                symbol = bare,
                file = def.file,
                line = def.line,
                kind = def.kind,
                signature = def.signature
            )),
            Some(def) => {
                let body = def.body.unwrap_or_default();
                let lines = body.lines().count();
                let mut out = format!(
                    "`{symbol}` ({kind}) at {file}:{line}\n{signature}\n---- ({lines} lines)\n",
                    symbol = bare,
                    kind = def.kind,
                    file = def.file,
                    line = def.line,
                    signature = def.signature
                );
                out.push_str(&body);
                Ok(clamp(out))
            }
            None => Ok(format!("No definition found for {:?}.", bare)),
        }
    }
}

// ---------------------------------------------------------------------------
// ts_find_symbol
// ---------------------------------------------------------------------------

struct TsFindSymbol;

static TS_FIND_SYMBOL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_find_symbol".into(),
    description: "Find declarations whose NAME contains the query (case-insensitive), across the project or a file. Pass the bare name only, e.g. query=build - not fn build.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Substring of a symbol name, e.g. \"build\", \"Error\". Do NOT prefix it with a kind keyword (\"fn build\")." },
            "type": { "type": "string", "enum": KIND_LABELS, "description": "Optional declaration kind to filter by (fn, struct, enum, trait, impl, mod, type, static, const)." },
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 25, "description": "Max matches to return." }
        },
        "required": ["query"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsFindSymbol {
    fn spec(&self) -> &ToolSpec {
        &TS_FIND_SYMBOL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default, rename = "type")]
            kind: Option<String>,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git_modified_only: bool,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_limit() -> usize {
            25
        }
        let args: Args = serde_json::from_value(args)?;
        if args.query.trim().is_empty() {
            anyhow::bail!("query must not be empty");
        }
        let kind = normalize_kind(args.kind.as_deref())?;
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let mut rows = engine::search_symbols(
            &ctx.project_root,
            &args.query,
            args.path.as_deref(),
            scope.as_ref(),
            kind,
        )?;
        let total = rows.len();
        rows.truncate(args.limit);
        if rows.is_empty() {
            return Ok(format!("No symbols matching {:?} found.", args.query));
        }
        let mut out = format!(
            "{total} symbol(s) matching {:?}:
",
            args.query
        );
        out.push_str(&rows.join("\n"));
        if total > args.limit {
            out.push_str(&format!("\n... and {} more", total - args.limit));
        }
        Ok(clamp(out))
    }
}

// ---------------------------------------------------------------------------
// ts_test_impact
// ---------------------------------------------------------------------------

struct TsTestImpact;

static TS_TEST_IMPACT_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ts_test_impact".into(),
    description: "Map the files changed since a revision (git diff) to the tests likely to cover them, using tree-sitter: a test is affected when it references a symbol declared in a changed file, or lives in the same crate/directory. Discovers Rust `#[test]` functions and JS/TS `it`/`test`/`specify` cases. Use to run the smallest useful test set before the full suite.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "rev": { "type": "string", "default": "HEAD", "description": "Compare against this revision (e.g. HEAD, main, HEAD~3). Untracked files are always included." },
            "max": { "type": "integer", "default": 100, "minimum": 1, "maximum": 500, "description": "Cap on reported tests." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for TsTestImpact {
    fn spec(&self) -> &ToolSpec {
        &TS_TEST_IMPACT_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "default_rev")]
            rev: String,
            #[serde(default = "default_max")]
            max: usize,
        }
        fn default_rev() -> String {
            "HEAD".to_string()
        }
        fn default_max() -> usize {
            100
        }
        let args: Args = serde_json::from_value(args)?;
        let root = &ctx.project_root;
        let changed = changed_files(root, &args.rev)?;
        if changed.is_empty() {
            return Ok(format!("No changed files vs {}.", args.rev));
        }

        // Symbols declared in the changed source files -> the file declaring them.
        let mut symbols: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut changed_src: Vec<String> = Vec::new();
        for f in &changed {
            if engine::lang_of(file_ext(f)).is_none() {
                continue;
            }
            changed_src.push(f.clone());
            let Ok(text) = std::fs::read_to_string(root.join(f)) else {
                continue;
            };
            for name in engine::decl_names_in_text(f, &text) {
                if ignored_symbol(&name) {
                    continue;
                }
                symbols.entry(name).or_default().push(f.clone());
            }
        }
        let changed_crates: HashSet<String> = changed_src.iter().map(|f| crate_of(f)).collect();
        let changed_dirs: HashSet<String> = changed_src.iter().map(|f| parent_dir(f)).collect();

        let tests = engine::test_functions(root)?;
        let mut affected: Vec<(bool, String, usize, String, String)> = Vec::new();
        for t in &tests {
            let toks = engine::identifier_tokens(&t.body);
            let mut hits: Vec<String> = symbols
                .keys()
                .filter(|s| toks.contains(*s))
                .cloned()
                .collect();
            hits.sort();
            hits.truncate(6);
            let same_only = hits.is_empty()
                && !t.file.is_empty()
                && (changed_crates.contains(&crate_of(&t.file))
                    || changed_dirs.contains(&parent_dir(&t.file)));
            let reason = if !hits.is_empty() {
                format!("references {}", hits.join(", "))
            } else if same_only {
                "same crate/dir as a changed file".to_string()
            } else {
                continue;
            };
            // Symbol-matched tests first, then same-crate ones.
            affected.push((
                hits.is_empty(),
                t.file.clone(),
                t.line,
                t.name.clone(),
                reason,
            ));
        }
        affected.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

        let total = affected.len();
        let mut out = format!(
            "{} changed file(s) vs {}; {} test(s) likely affected:\n",
            changed.len(),
            args.rev,
            total
        );
        for c in &changed {
            out.push_str(&format!("  changed: {c}\n"));
        }
        if total == 0 {
            out.push_str(
                "(no tests reference the changed symbols; run the changed module's suite)\n",
            );
            return Ok(clamp(out));
        }
        out.push('\n');
        for (_, file, line, name, reason) in affected.iter().take(args.max) {
            out.push_str(&format!("{file}:{line} {name}  ({reason})\n"));
        }
        if total > args.max {
            out.push_str(&format!("... and {} more\n", total - args.max));
        }
        out.push_str("\nsuggested test runs:\n");
        // Rust crates: `cargo test -p <pkg>`.
        let mut crates: Vec<String> = affected
            .iter()
            .filter(|(_, f, _, _, _)| f.ends_with(".rs"))
            .map(|(_, f, _, _, _)| crate_of(f))
            .filter(|c| !c.is_empty())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        crates.sort();
        for c in crates {
            match crate_package(root, &c) {
                Some(pkg) => out.push_str(&format!("  cargo test -p {pkg}\n")),
                None => out.push_str(&format!("  (cargo test in {c})\n")),
            }
        }
        // JS/TS (and any other non-Rust) tests: name each file to run.
        let mut js_files: Vec<String> = affected
            .iter()
            .filter(|(_, f, _, _, _)| !f.ends_with(".rs"))
            .map(|(_, f, _, _, _)| f.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        js_files.sort();
        for f in js_files {
            out.push_str(&format!("  {f}\n"));
        }
        Ok(clamp(out))
    }
}

/// `git diff --name-only <rev>` plus untracked files, as root-relative paths.
fn changed_files(root: &std::path::Path, rev: &str) -> Result<Vec<String>> {
    let run = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run git {}: {e}", args.join(" ")))?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let mut files = Vec::new();
    for l in run(&["diff", "--name-only", rev])?.lines() {
        let t = l.trim();
        if !t.is_empty() {
            files.push(t.to_string());
        }
    }
    for l in run(&["ls-files", "--others", "--exclude-standard"])?.lines() {
        let t = l.trim();
        if !t.is_empty() {
            files.push(t.to_string());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// The crate directory of a root-relative path (`crates/foo/src/x.rs` -> `crates/foo`).
fn crate_of(rel: &str) -> String {
    let mut parts = rel.split('/');
    match (parts.next(), parts.next()) {
        (Some("crates"), Some(name)) => format!("crates/{name}"),
        (Some(first), _) => first.to_string(),
        _ => String::new(),
    }
}

/// The lowercase extension of a root-relative path, without the dot.
fn file_ext(rel: &str) -> &str {
    rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

/// The parent directory of a root-relative path (`src/a/b.ts` -> `src/a`).
fn parent_dir(rel: &str) -> String {
    rel.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default()
}

/// Read the `name = "..."` from a crate dir's Cargo.toml (falls back to the dir name).
fn crate_package(root: &std::path::Path, crate_dir: &str) -> Option<String> {
    let manifest = root.join(crate_dir).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(val) = rest.strip_prefix('=') {
                let name = val.trim().trim_matches('"').trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    crate_dir.rsplit('/').next().map(str::to_string)
}

/// Symbols too generic to be a useful impact signal.
fn ignored_symbol(name: &str) -> bool {
    name.len() < 3
        || matches!(
            name,
            "self" | "new" | "main" | "fmt" | "from" | "into" | "default" | "test"
        )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-syntax-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_and_renames_rust_identifiers() {
        let root = scratch();
        let a = r#"
fn helper() -> u32 { 1 }

fn main() {
    let x = helper();
    println!("{}", x);   // helper() inside a comment must NOT match
}
"#;
        std::fs::write(root.join("main.rs"), a).unwrap();
        std::fs::write(root.join("notes.txt"), "helper appears here as plain text").unwrap();

        let occ = crate::engine::find_occurrences(&root, "helper", None, None).unwrap();
        // function definition + call site, but NOT the comment/string or .txt file
        assert_eq!(occ.len(), 2, "occurrences: {occ:#?}");

        let edits = crate::engine::rename_edits(&root, "helper", None, None).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].spans.len(), 2);
        assert_eq!(
            crate::engine::apply_edits(&root, &edits, "assist").unwrap(),
            2
        );
        let after = std::fs::read_to_string(root.join("main.rs")).unwrap();
        assert!(after.contains("fn assist()"));
        assert!(after.contains("assist();"));
        // comment and other file preserved
        assert!(after.contains("// helper() inside a comment must NOT match"));
        assert!(
            std::fs::read_to_string(root.join("notes.txt"))
                .unwrap()
                .contains("helper appears")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lists_top_level_symbols() {
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            r#"
pub struct Point { x: i32 }
enum Color { Red }
trait Draw { fn draw(&self); }
impl Draw for Point { fn draw(&self) {} }
fn area(p: Point) -> i32 { p.x }
"#,
        )
        .unwrap();
        let syms = crate::engine::list_symbols(&root, None, None).unwrap();
        assert!(syms.iter().any(|s| s.contains("fn area")));
        assert!(syms.iter().any(|s| s.contains("struct Point")));
        assert!(syms.iter().any(|s| s.contains("trait Draw")));
        assert!(syms.iter().any(|s| s.contains("impl")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_definition_signature_and_body() {
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            r#"
pub fn compute(x: i32) -> i32 {
    let y = x * 2;
    y + 1
}
struct Thing { a: i32 }
"#,
        )
        .unwrap();

        let def = crate::engine::find_definition(&root, "compute", None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(def.file, "lib.rs");
        assert_eq!(def.kind, "fn");
        assert!(
            def.signature.contains("fn compute(x: i32) -> i32"),
            "{}",
            def.signature
        );
        assert!(def.body.is_none());

        let full = crate::engine::read_symbol(&root, "compute", None, None, None)
            .unwrap()
            .unwrap();
        let body = full.body.unwrap();
        assert!(body.contains("let y = x * 2;"), "{body}");
        assert!(body.contains("y + 1"));

        // struct found too
        let s = crate::engine::find_definition(&root, "Thing", None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(s.kind, "struct");

        assert!(
            crate::engine::find_definition(&root, "nope", None, None, None)
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn counts_references_across_files() {
        let root = scratch();
        std::fs::write(
            root.join("a.rs"),
            "fn helper() {}\nfn main() { helper(); helper(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("b.rs"), "fn other() { helper(); }\n").unwrap();
        let occ = crate::engine::find_occurrences(&root, "helper", None, None).unwrap();
        // definition in a + 2 calls in a + 1 call in b
        assert_eq!(occ.len(), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lists_symbols_with_signatures() {
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        let sigs = crate::engine::list_symbol_signatures(&root, None, None).unwrap();
        assert!(
            sigs.iter()
                .any(|s| s.contains("fn add | fn add(a: i32, b: i32) -> i32 @")),
            "{sigs:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn builds_nested_structural_map() {
        use std::collections::HashSet;
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            r#"
pub mod api {
    pub struct Client { url: String }
    impl Client {
        pub fn new(url: &str) -> Self { todo!() }
        pub fn get(&self) -> u32 { 0 }
    }
}
fn main() {}
"#,
        )
        .unwrap();
        let all: HashSet<String> = HashSet::new();
        let lines = crate::engine::structural_map(&root, None, None, true, &all).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("== lib.rs =="), "{joined}");
        assert!(joined.contains("mod api {"), "{joined}");
        assert!(joined.contains("  struct Client |"), "{joined}");
        assert!(joined.contains("impl Client {"), "{joined}");
        assert!(joined.contains("    fn new |"), "{joined}");
        assert!(joined.contains("    fn get |"), "{joined}");
        assert!(joined.contains("fn main"), "{joined}");

        // kinds filter: only functions (and methods nested under impls).
        let only_fns: HashSet<String> = ["fn".to_string()].into();
        let lines = crate::engine::structural_map(&root, None, None, false, &only_fns).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("fn new"), "{joined}");
        assert!(joined.contains("fn main"), "{joined}");
        assert!(!joined.contains("struct Client"), "{joined}");
        assert!(!joined.contains("mod api"), "{joined}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maps_a_directory_path() {
        use std::collections::HashSet;
        let root = scratch();
        std::fs::create_dir_all(root.join("crates/foo/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/bar/src")).unwrap();
        std::fs::write(
            root.join("crates/foo/src/lib.rs"),
            "pub fn foo() {}\nstruct Foo {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/bar/src/lib.rs"),
            "pub fn bar() {}\nstruct Bar {}\n",
        )
        .unwrap();
        let all: HashSet<String> = HashSet::new();
        let lines =
            crate::engine::structural_map(&root, Some("crates/foo"), None, false, &all).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("== crates/foo/src/lib.rs =="), "{joined}");
        assert!(joined.contains("fn foo"), "{joined}");
        assert!(!joined.contains("bar"), "{joined}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod find_symbol_tests {
    use std::path::PathBuf;

    use super::{crate_of, engine, ignored_symbol, normalize_kind};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-findsym-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn searches_declarations_by_name() {
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn build_config() {}\nfn run_build() {}\nstruct Config {}\n",
        )
        .unwrap();
        let hits = crate::engine::search_symbols(&root, "build", None, None, None).unwrap();
        assert!(
            hits.iter().any(|h| h.contains("fn build_config")),
            "{hits:?}"
        );
        assert!(hits.iter().any(|h| h.contains("fn run_build")), "{hits:?}");
        assert!(!hits.iter().any(|h| h.contains("struct Config")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_kind_prefix_only_accepts_whitespace_delimited_keywords() {
        assert_eq!(engine::split_kind_prefix("fn build"), (Some("fn"), "build"));
        assert_eq!(engine::split_kind_prefix("Fn build"), (Some("fn"), "build"));
        assert_eq!(
            engine::split_kind_prefix("struct Config"),
            (Some("struct"), "Config")
        );
        // A bare identifier (even one starting with a keyword) is untouched.
        assert_eq!(
            engine::split_kind_prefix("typewriter"),
            (None, "typewriter")
        );
        assert_eq!(engine::split_kind_prefix("fnfoo"), (None, "fnfoo"));
        assert_eq!(engine::split_kind_prefix("build"), (None, "build"));
        // Keyword alone or with only whitespace after it: no prefix to split.
        assert_eq!(engine::split_kind_prefix("fn"), (None, "fn"));
        assert_eq!(engine::split_kind_prefix("fn "), (None, "fn "));
    }

    #[test]
    fn kind_prefix_query_still_finds_the_fn() {
        // The agent's sloppy habit: "fn build" should search names, not fail.
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn build_config() {}\nfn run_build() {}\nstruct Config {}\n",
        )
        .unwrap();
        let hits = engine::search_symbols(&root, "fn build", None, None, None).unwrap();
        assert!(
            hits.iter().any(|h| h.contains("fn build_config")),
            "{hits:?}"
        );
        assert!(hits.iter().any(|h| h.contains("fn run_build")), "{hits:?}");
        assert!(
            !hits.iter().any(|h| h.contains("struct Config")),
            "{hits:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn type_filter_narrows_by_declaration_kind() {
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            "fn config() {}\nstruct Config {}\nenum ConfigKind {}\n",
        )
        .unwrap();
        let fns = engine::search_symbols(&root, "config", None, None, Some("fn")).unwrap();
        assert_eq!(fns.len(), 1, "{fns:?}");
        assert!(fns[0].starts_with("fn config"), "{fns:?}");
        let structs = engine::search_symbols(&root, "config", None, None, Some("Struct")).unwrap();
        assert_eq!(structs.len(), 1, "{structs:?}");
        assert!(structs[0].starts_with("struct Config"), "{structs:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_definition_strips_prefix_and_conflict_bails() {
        let root = scratch();
        std::fs::write(root.join("lib.rs"), "pub fn area() {}\nstruct Thing {}\n").unwrap();
        let def = engine::find_definition(&root, "fn area", None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(def.kind, "fn");
        assert_eq!(def.signature, "pub fn area()");
        // A prefix that contradicts an explicit type filter is an error.
        assert!(engine::find_definition(&root, "fn area", None, None, Some("struct")).is_err());
        // Kind filter alone finds the declaration too.
        let t = engine::find_definition(&root, "Thing", None, None, Some("struct"))
            .unwrap()
            .unwrap();
        assert_eq!(t.kind, "struct");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_kind_validates_labels() {
        assert_eq!(normalize_kind(None).unwrap(), None);
        assert_eq!(normalize_kind(Some("fn")).unwrap(), Some("fn"));
        assert_eq!(normalize_kind(Some("Struct")).unwrap(), Some("struct"));
        assert_eq!(normalize_kind(Some("  fn  ")).unwrap(), Some("fn"));
        assert!(normalize_kind(Some("bogus")).is_err());
        assert_eq!(normalize_kind(Some("Method")).unwrap(), Some("method"));
        assert_eq!(normalize_kind(Some("  ")).unwrap(), None);
    }

    #[test]
    fn test_functions_finds_only_annotated_fns() {
        let root = scratch();
        std::fs::write(
            root.join("lib.rs"),
            "\
pub fn helper() {}
#[test]
fn a() { assert_eq!(thing(), 1); }
#[tokio::test]
async fn b() {}
#[cfg(test)]
mod tests {
    #[test]
    fn inner() {}
}
",
        )
        .unwrap();
        let fns = engine::test_functions(&root).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a"), "{names:?}");
        assert!(names.contains(&"b"), "{names:?}");
        assert!(names.contains(&"inner"), "{names:?}");
        assert!(!names.contains(&"helper"), "{names:?}");
        let a = fns.iter().find(|f| f.name == "a").unwrap();
        assert!(a.body.contains("thing()"), "{}", a.body);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decl_names_and_tokens() {
        let text = "pub struct Store {}\nfn compute(x: usize) -> usize { x }\nenum Kind { A }\n";
        let names = engine::decl_names_in_text("lib.rs", text);
        assert!(names.contains(&"Store".to_string()), "{names:?}");
        assert!(names.contains(&"compute".to_string()), "{names:?}");
        assert!(names.contains(&"Kind".to_string()), "{names:?}");
        let toks = engine::identifier_tokens("let x = Store::open(compute);");
        assert!(toks.contains("Store"));
        assert!(toks.contains("compute"));
        assert!(!toks.contains("="));
    }

    #[test]
    fn path_helpers_for_impact_mapping() {
        assert_eq!(crate::file_ext("src/App.tsx"), "tsx");
        assert_eq!(crate::file_ext("crates/foo/src/lib.rs"), "rs");
        assert_eq!(crate::file_ext("Makefile"), "");
        assert_eq!(crate::parent_dir("src/a/b.ts"), "src/a");
        assert_eq!(crate::parent_dir("main.rs"), "");
        assert_eq!(crate::crate_of("crates/foo/src/lib.rs"), "crates/foo");
        assert_eq!(crate::crate_of("src/App.tsx"), "src");
    }

    #[test]
    fn crate_paths_and_ignored_symbols() {
        assert_eq!(
            crate_of("crates/comrade-core/src/agent.rs"),
            "crates/comrade-core"
        );
        assert_eq!(crate_of("README.md"), "README.md");
        assert!(ignored_symbol("new"));
        assert!(ignored_symbol("id"));
        assert!(!ignored_symbol("Store"));
        assert!(!ignored_symbol("compute"));
    }
}
