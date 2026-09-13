//! Tree-sitter code chunking for semantic (embedding) code search.
//!
//! [`code_chunks`] turns a project's sources into indexable chunks. For Rust the
//! unit is a declaration — one chunk per `fn`/`struct`/`enum`/… item, recursing
//! through `impl`/`mod`/`trait` containers so a method becomes its own chunk
//! labelled with its container. Everything else (and any Rust file that parses
//! to no declaration) falls back to fixed line windows. Every chunk carries the
//! file and the 1-based line of the declaration, so an embedding hit is directly
//! a location (`path:line`).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::engine::{short_kind, signature_of};

/// Extensions treated as source: Rust is parsed with tree-sitter, the rest are
/// chunked as line windows.
const CODE_EXT: &[&str] = &[
    "rs", "toml", "md", "py", "js", "ts", "tsx", "go", "c", "h", "cpp", "hpp", "java", "rb", "sh",
];

/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 1_000_000;
/// Lines per window in the fallback chunker and for oversized declarations.
const WINDOW_LINES: usize = 40;
/// Cap on a single chunk's embedded text (characters).
const MAX_CHUNK_CHARS: usize = 8_000;

/// One indexable chunk of source: a declaration or a line window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeChunk {
    /// Project-root relative path.
    pub file: String,
    /// 1-based line of the declaration (or of the window's first line).
    pub line: usize,
    /// Short kind label (`fn`, `struct`, …, or `lines` for a window).
    pub kind: String,
    /// Declaration name; empty for `impl`/`mod` headers and line windows.
    pub name: String,
    /// Text embedded for this chunk.
    pub text: String,
}

/// Chunk every source file under `root`, skipping vendor/build directories.
pub fn code_chunks(root: &Path) -> Result<Vec<CodeChunk>> {
    let mut files = Vec::new();
    walk(root, &mut files);
    files.sort();

    let mut chunks = Vec::new();
    for abs in files {
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        if bytes.len() as u64 > MAX_FILE_BYTES || bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let rel = abs
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| abs.to_string_lossy().into_owned());
        chunks.extend(chunks_of_file(&rel, &text));
    }
    Ok(chunks)
}

/// Chunk one source file's text: Rust by declaration, everything else (or Rust
/// that parses to no declaration) as line windows. `rel` is the project-root
/// relative path recorded on each chunk.
pub fn chunks_of_file(rel: &str, text: &str) -> Vec<CodeChunk> {
    let is_rust = Path::new(rel).extension().and_then(|e| e.to_str()) == Some("rs");
    let mut out = if is_rust {
        symbol_chunks(rel, text)
    } else {
        Vec::new()
    };
    if out.is_empty() {
        out = window_chunks(rel, text, 1);
    }
    out
}

/// Recursively collect source files, skipping the usual build/vendor dirs.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | "target" | "node_modules" | "vendor" | ".idea" | ".vscode"
            ) {
                continue;
            }
            walk(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CODE_EXT.contains(&e))
        {
            out.push(path);
        }
    }
}

/// One chunk per declaration in a Rust source, recursing through containers.
fn symbol_chunks(rel: &str, text: &str) -> Vec<CodeChunk> {
    let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(&lang);
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(tree.root_node(), text, rel, "", &mut out);
    out
}

fn collect(scope: tree_sitter::Node, text: &str, rel: &str, ctx: &str, out: &mut Vec<CodeChunk>) {
    let mut cursor = scope.walk();
    for child in scope.children(&mut cursor) {
        let kind = child.kind();
        if matches!(kind, "impl_item" | "mod_item" | "trait_item") {
            let head = signature_of(&child, text);
            let sub = if ctx.is_empty() {
                head
            } else {
                format!("{ctx} / {head}")
            };
            let before = out.len();
            // Items live inside the container's `declaration_list` body, not as
            // direct children, so recurse into that body.
            if let Some(body) = child.child_by_field_name("body") {
                collect(body, text, rel, &sub, out);
            }
            if out.len() == before {
                // A container with no nested declaration (e.g. `mod foo;`).
                out.push(CodeChunk {
                    file: rel.to_string(),
                    line: child.start_position().row + 1,
                    kind: short_kind(kind).to_string(),
                    name: name_of(&child, text),
                    text: cap(join(ctx, &signature_of(&child, text))),
                });
            }
        } else if is_leaf_decl(kind) {
            emit_leaf(rel, &child, text, ctx, out);
        }
    }
}

/// Emit one chunk for a leaf declaration, or line windows when it is oversized.
fn emit_leaf(rel: &str, node: &tree_sitter::Node, text: &str, ctx: &str, out: &mut Vec<CodeChunk>) {
    let start = with_doc_start(node);
    let body = &text[start..node.end_byte()];
    let line = node.start_position().row + 1;
    if body.chars().count() > MAX_CHUNK_CHARS {
        out.extend(window_chunks(rel, body, line));
        return;
    }
    out.push(CodeChunk {
        file: rel.to_string(),
        line,
        kind: short_kind(node.kind()).to_string(),
        name: name_of(node, text),
        text: cap(join(ctx, body)),
    });
}

/// `true` for declaration kinds that become a single chunk (containers such as
/// `impl`/`mod`/`trait` are handled by [`collect`] instead).
fn is_leaf_decl(kind: &str) -> bool {
    matches!(
        kind,
        "function_item" | "struct_item" | "enum_item" | "type_item" | "static_item" | "const_item"
    )
}

/// The declaration's name, or `""` when it has none (e.g. an `impl`).
fn name_of(node: &tree_sitter::Node, text: &str) -> String {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(text.as_bytes()).ok())
        .unwrap_or("")
        .to_string()
}

/// Byte offset where the declaration's doc comment / attributes start (contiguous
/// `///`, `/* */` and `#[...]` siblings immediately above it).
fn with_doc_start(node: &tree_sitter::Node) -> usize {
    let mut start = node.start_byte();
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if matches!(
            p.kind(),
            "line_comment" | "block_comment" | "attribute_item"
        ) {
            start = p.start_byte();
            prev = p.prev_sibling();
        } else {
            break;
        }
    }
    start
}

/// Fixed line windows over `text`, numbering from `first_line`.
fn window_chunks(rel: &str, text: &str, first_line: usize) -> Vec<CodeChunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let end = (i + WINDOW_LINES).min(lines.len());
        out.push(CodeChunk {
            file: rel.to_string(),
            line: first_line + i,
            kind: "lines".to_string(),
            name: String::new(),
            text: cap(lines[i..end].join("\n")),
        });
        i = end;
    }
    out
}

fn join(ctx: &str, body: &str) -> String {
    if ctx.is_empty() {
        body.to_string()
    } else {
        format!("{ctx}\n{body}")
    }
}

fn cap(s: String) -> String {
    if s.chars().count() <= MAX_CHUNK_CHARS {
        s
    } else {
        s.chars().take(MAX_CHUNK_CHARS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-chunks-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn chunks_rust_declaration_with_location_and_doc() {
        let dir = scratch("decl");
        std::fs::write(
            dir.join("a.rs"),
            "/// Adds one.\n#[inline]\nfn compute(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .unwrap();
        let chunks = code_chunks(&dir).unwrap();
        let c = chunks
            .iter()
            .find(|c| c.name == "compute")
            .expect("compute chunk");
        assert_eq!(c.kind, "fn");
        assert_eq!(c.file, "a.rs");
        assert_eq!(c.line, 3, "line must point at the fn, not the doc comment");
        assert!(c.text.contains("Adds one."), "doc comment kept: {}", c.text);
        assert!(c.text.contains("fn compute"), "body kept: {}", c.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunks_recurse_into_impl_and_label_with_container() {
        let dir = scratch("impl");
        std::fs::write(
            dir.join("a.rs"),
            "struct Foo;\n\nimpl Foo {\n    fn bar(&self) {}\n}\n",
        )
        .unwrap();
        let chunks = code_chunks(&dir).unwrap();
        let bar = chunks.iter().find(|c| c.name == "bar").expect("bar chunk");
        assert_eq!(bar.kind, "fn");
        assert!(
            bar.text.contains("impl Foo"),
            "container context missing: {}",
            bar.text
        );
        assert!(
            !chunks.iter().any(|c| c.kind == "impl"),
            "the impl header must not be its own chunk"
        );
        assert!(chunks.iter().any(|c| c.name == "Foo" && c.kind == "struct"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunks_fall_back_to_line_windows_for_non_rust() {
        let dir = scratch("window");
        let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("notes.md"), text).unwrap();
        let chunks = code_chunks(&dir).unwrap();
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|c| c.kind == "lines" && c.file == "notes.md")
        );
        assert_eq!(chunks[0].line, 1);
        assert_eq!(chunks[1].line, 41);
        assert_eq!(chunks[2].line, 81);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunks_empty_dir_has_none() {
        let dir = scratch("empty");
        assert!(code_chunks(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
