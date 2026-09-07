//! Tree-sitter powered code tools.

mod engine;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(FindReferences),
        Box::new(Rename),
        Box::new(ListSymbols),
        Box::new(FindDefinition),
        Box::new(ReadSymbol),
        Box::new(ReferencesCount),
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

/// When a scope is set and an explicit path was given, require that the path is
/// part of the change set.
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

// ---------------------------------------------------------------------------
// find_references
// ---------------------------------------------------------------------------

struct FindReferences;

static FIND_REFERENCES_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "find_references".into(),
    description: "Find uses of a symbol (function/struct/field/variable name) across the project using tree-sitter parsing. Matches identifier tokens only (never inside strings/comments) but is lexical, not semantic. Returns file:line:col with source context.".into(),
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
impl Tool for FindReferences {
    fn spec(&self) -> &ToolSpec {
        &FIND_REFERENCES_SPEC
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
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let occ = engine::find_occurrences(
            &ctx.project_root,
            &args.symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )?;
        if occ.is_empty() {
            return Ok(format!("No occurrences of {:?} found.", args.symbol));
        }
        let mut out = format!("Occurrences of {}:\n", args.symbol);
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
// rename
// ---------------------------------------------------------------------------

struct Rename;

static RENAME_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "rename".into(),
    description: "Rename a symbol across the project by rewriting every tree-sitter identifier token that equals `symbol`. Approximate but safe (never matches inside strings/comments). Interactive: you approve the change preview before files are written. Prefer an LSP rename tool when available for semantic accuracy.".into(),
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
impl Tool for Rename {
    fn spec(&self) -> &ToolSpec {
        &RENAME_SPEC
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
        if !is_valid_identifier(&args.symbol) {
            anyhow::bail!("{:?} is not a valid identifier", args.symbol);
        }
        if !is_valid_identifier(&args.new_name) {
            anyhow::bail!("{:?} is not a valid identifier", args.new_name);
        }
        if args.symbol == args.new_name {
            anyhow::bail!("new name equals old name");
        }
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let edits = engine::rename_edits(
            &ctx.project_root,
            &args.symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )?;
        let total: usize = edits.iter().map(|e| e.spans.len()).sum();
        if total == 0 {
            return Ok(format!(
                "No occurrences of {:?} found; nothing renamed.",
                args.symbol
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
                symbol = args.symbol,
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
// list_symbols
// ---------------------------------------------------------------------------

struct ListSymbols;

static LIST_SYMBOLS_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "list_symbols".into(),
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
impl Tool for ListSymbols {
    fn spec(&self) -> &ToolSpec {
        &LIST_SYMBOLS_SPEC
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
// find_definition
// ---------------------------------------------------------------------------

struct FindDefinition;

static FIND_DEFINITION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "find_definition".into(),
    description: "Locate where a symbol is defined and return its kind, one-line signature, and file:line — never the whole body. Cheaper than reading the file when you only need to know a declaration.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Identifier to locate." },
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD." }
        },
        "required": ["symbol"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for FindDefinition {
    fn spec(&self) -> &ToolSpec {
        &FIND_DEFINITION_SPEC
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
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        match engine::find_definition(
            &ctx.project_root,
            &args.symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )? {
            Some(def) => Ok(format!(
                "`{symbol}` defined at {file}:{line}\nkind: {kind}\nsignature: {signature}",
                symbol = args.symbol,
                file = def.file,
                line = def.line,
                kind = def.kind,
                signature = def.signature
            )),
            None => Ok(format!("No definition found for {:?}.", args.symbol)),
        }
    }
}

// ---------------------------------------------------------------------------
// read_symbol
// ---------------------------------------------------------------------------

struct ReadSymbol;

static READ_SYMBOL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "read_symbol".into(),
    description: "Read just one symbol's declaration body (function, struct, enum, trait, const, ...) with its file:line and signature. Use before editing a specific item instead of reading the whole file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Identifier whose declaration body to read." },
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD." }
        },
        "required": ["symbol"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ReadSymbol {
    fn spec(&self) -> &ToolSpec {
        &READ_SYMBOL_SPEC
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
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        match engine::read_symbol(
            &ctx.project_root,
            &args.symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )? {
            Some(def) => {
                let body = def.body.unwrap_or_default();
                let lines = body.lines().count();
                let mut out = format!(
                    "`{symbol}` ({kind}) at {file}:{line}\n{signature}\n---- ({lines} lines)\n",
                    symbol = args.symbol,
                    kind = def.kind,
                    file = def.file,
                    line = def.line,
                    signature = def.signature
                );
                out.push_str(&body);
                Ok(clamp(out))
            }
            None => Ok(format!("No definition found for {:?}.", args.symbol)),
        }
    }
}

// ---------------------------------------------------------------------------
// references_count
// ---------------------------------------------------------------------------

struct ReferencesCount;

static REFERENCES_COUNT_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "references_count".into(),
    description: "Count references to a symbol across the project (lexical tree-sitter identifiers). Returns a total and per-file breakdown, not context lines — use it to gauge blast radius (e.g. before a rename) cheaply.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Identifier to count." },
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." },
            "git_modified_only": { "type": "boolean", "default": false, "description": "Only search files that differ from HEAD." }
        },
        "required": ["symbol"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ReferencesCount {
    fn spec(&self) -> &ToolSpec {
        &REFERENCES_COUNT_SPEC
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
        let scope = changed_scope(ctx, args.git_modified_only)?;
        guard_path_scope(ctx, &args.path, &scope)?;
        let occ = engine::find_occurrences(
            &ctx.project_root,
            &args.symbol,
            args.path.as_deref(),
            scope.as_ref(),
        )?;
        if occ.is_empty() {
            return Ok(format!("`{}` has no references.", args.symbol));
        }
        let mut per_file: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for o in &occ {
            *per_file.entry(o.file.as_str()).or_insert(0) += 1;
        }
        let total = occ.len();
        let files = per_file.len();
        let mut out = format!(
            "`{}`: {total} reference(s) across {files} file(s)\n",
            args.symbol
        );
        for (file, count) in per_file.iter().take(8) {
            out.push_str(&format!("  {count:>4}  {file}\n"));
        }
        if per_file.len() > 8 {
            out.push_str(&format!("  … and {} more file(s)\n", per_file.len() - 8));
        }
        Ok(out)
    }
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

        let def = crate::engine::find_definition(&root, "compute", None, None)
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

        let full = crate::engine::read_symbol(&root, "compute", None, None)
            .unwrap()
            .unwrap();
        let body = full.body.unwrap();
        assert!(body.contains("let y = x * 2;"), "{body}");
        assert!(body.contains("y + 1"));

        // struct found too
        let s = crate::engine::find_definition(&root, "Thing", None, None)
            .unwrap()
            .unwrap();
        assert_eq!(s.kind, "struct");

        assert!(
            crate::engine::find_definition(&root, "nope", None, None)
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
}
