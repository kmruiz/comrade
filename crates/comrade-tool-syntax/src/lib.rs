//! Tree-sitter powered code tools.

mod engine;

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
    ]
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
            "path": { "type": "string", "description": "Optional file to restrict the search to (project-root relative)." }
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
        }
        let args: Args = serde_json::from_value(args)?;
        let occ = engine::find_occurrences(&ctx.project_root, &args.symbol, args.path.as_deref())?;
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
            "path": { "type": "string", "description": "Optional file to restrict the rename to (project-root relative)." }
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
        let edits = engine::rename_edits(&ctx.project_root, &args.symbol, args.path.as_deref())?;
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
            "path": { "type": "string", "description": "Optional file to inspect (project-root relative). Defaults to the whole project." }
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
        }
        let args: Args = serde_json::from_value(args)?;
        let symbols = engine::list_symbols(&ctx.project_root, args.path.as_deref())?;
        if symbols.is_empty() {
            return Ok("No symbols found.".to_string());
        }
        Ok(clamp(symbols.join("\n")))
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

        let occ = crate::engine::find_occurrences(&root, "helper", None).unwrap();
        // function definition + call site, but NOT the comment/string or .txt file
        assert_eq!(occ.len(), 2, "occurrences: {occ:#?}");

        let edits = crate::engine::rename_edits(&root, "helper", None).unwrap();
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
        let syms = crate::engine::list_symbols(&root, None).unwrap();
        assert!(syms.iter().any(|s| s.contains("fn area")));
        assert!(syms.iter().any(|s| s.contains("struct Point")));
        assert!(syms.iter().any(|s| s.contains("trait Draw")));
        assert!(syms.iter().any(|s| s.contains("impl")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
