//! Tree-sitter powered code tools: `find_references`, `rename`, `list_symbols`.
//!
//! v1 is deliberately *lexical*: occurrences are identifier tokens parsed by
//! tree-sitter, so matches never appear inside strings or comments (unlike
//! grep). Cross-file semantic resolution (LSP-grade) is a later layer; tools
//! document their approximation so the model can prefer an LSP-backed tool
//! when one is available.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

const MAX_FILE_BYTES: u64 = 1_000_000;

/// One identifier occurrence.
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// Project-root relative path.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    /// Full text of the source line, for context.
    pub context_line: String,
}

/// Byte-span matches grouped per file, ready to be rewritten.
#[derive(Debug)]
pub struct FileEdits {
    pub rel: String,
    pub text: String,
    /// Byte ranges (start, end), sorted ascending.
    pub spans: Vec<(usize, usize)>,
}

fn walk_files(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
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
            walk_files(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

fn language_for(ext: &str) -> Option<tree_sitter::Language> {
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        _ => None,
    }
}

/// Find every identifier occurrence of `symbol` in a single source text.
fn occurrences_in_text<'t>(
    lang: &tree_sitter::Language,
    text: &'t str,
    symbol: &str,
) -> Vec<(usize, usize)> {
    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(lang);
    let Some(tree) = parser.parse(text, None) else {
        return vec![];
    };
    let mut spans = Vec::new();
    let mut cursor = tree.walk();
    let mut descend = true;
    loop {
        let node = cursor.node();
        if node.kind() == "identifier" {
            let start = node.start_byte();
            let end = node.end_byte();
            if text.get(start..end) == Some(symbol) {
                spans.push((start, end));
            }
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                descend = true;
                break;
            }
            if !cursor.goto_parent() {
                return spans;
            }
            descend = false;
            break;
        }
    }
}

/// Collect all occurrences of `symbol` across a project. When `path` is given,
/// restrict the search to that file.
pub fn find_occurrences(root: &Path, symbol: &str, path: Option<&str>) -> Result<Vec<Occurrence>> {
    let mut out = Vec::new();
    for (rel, text) in collect_files(root, path)? {
        if let Some(lang) = language_for(
            Path::new(&rel)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        ) {
            let spans = occurrences_in_text(&lang, &text, symbol);
            for (start, _end) in spans {
                let (line, col, context_line) = locate(&text, start);
                out.push(Occurrence {
                    file: rel.clone(),
                    line,
                    column: col,
                    context_line,
                });
            }
        }
    }
    Ok(out)
}

/// Grouped byte-span edits for a symbol rename across the project.
pub fn rename_edits(root: &Path, symbol: &str, path: Option<&str>) -> Result<Vec<FileEdits>> {
    let mut grouped: BTreeMap<String, FileEdits> = BTreeMap::new();
    for (rel, text) in collect_files(root, path)? {
        let ext = Path::new(&rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let Some(lang) = language_for(ext) else {
            continue;
        };
        let spans = occurrences_in_text(&lang, &text, symbol);
        if !spans.is_empty() {
            grouped.insert(rel.clone(), FileEdits { rel, text, spans });
        }
    }
    Ok(grouped.into_values().collect())
}

/// List top-level declarations (functions, structs, enums, traits, impls,
/// modules) in a file.
pub fn list_symbols(root: &Path, path: Option<&str>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (rel, text) in collect_files(root, path)? {
        let ext = Path::new(&rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let Some(lang) = language_for(ext) else {
            continue;
        };
        let mut parser = tree_sitter::Parser::new();
        let _ = parser.set_language(&lang);
        let Some(tree) = parser.parse(&text, None) else {
            continue;
        };
        let mut cursor = tree.walk();
        let mut descend = true;
        let decls = [
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "mod_item",
            "type_item",
            "static_item",
            "const_item",
        ];
        loop {
            let node = cursor.node();
            if decls.contains(&node.kind()) {
                if node.kind() == "impl_item" {
                    let head = text[node.start_byte()..node.end_byte()]
                        .lines()
                        .next()
                        .unwrap_or("impl")
                        .trim();
                    let (line, _, _) = locate(&text, node.start_byte());
                    out.push(format!("impl {head} @ {line}"));
                } else if let Some(name) = node.child_by_field_name("name") {
                    let (line, _, _) = locate(&text, node.start_byte());
                    let label = short_kind(node.kind());
                    out.push(format!(
                        "{label} {} @ {line}",
                        name.utf8_text(text.as_bytes()).unwrap_or("?")
                    ));
                }
            }
            if descend && cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    descend = true;
                    break;
                }
                if !cursor.goto_parent() {
                    return Ok(out);
                }
                descend = false;
                break;
            }
        }
    }
    Ok(out)
}

/// Returns (rel_path, contents) for the target files. When `path` is provided
/// only that file (resolved relative to root) is returned.
fn collect_files(root: &Path, path: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    if let Some(p) = path {
        let abs = root.join(p);
        if !abs.starts_with(root) {
            anyhow::bail!("path {p:?} escapes the project root");
        }
        files.push((p.trim_start_matches('/').to_string(), abs));
    } else {
        let mut paths = Vec::new();
        for ext in ["rs"] {
            walk_files(root, ext, &mut paths);
        }
        for abs in paths {
            if let Ok(meta) = abs.metadata() {
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
            }
            let rel = abs
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| abs.to_string_lossy().into_owned());
            files.push((rel, abs));
        }
    }
    let mut out = Vec::new();
    for (rel, abs) in files {
        let bytes = std::fs::read(&abs).with_context(|| format!("cannot read {rel}"))?;
        if bytes.contains(&0) {
            continue;
        }
        if let Ok(text) = String::from_utf8(bytes) {
            out.push((rel, text));
        }
    }
    Ok(out)
}

fn short_kind(kind: &str) -> &'static str {
    match kind {
        "function_item" => "fn",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "mod_item" => "mod",
        "type_item" => "type",
        "static_item" => "static",
        "const_item" => "const",
        _ => "item",
    }
}

/// Compute the 1-based line/column and the source line for a byte offset.
fn locate(text: &str, byte: usize) -> (usize, usize, String) {
    let prefix = &text[..byte.min(text.len())];
    let line = prefix.matches('\n').count() + 1;
    let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = prefix[line_start..].chars().count() + 1;
    let context_line = text[line_start..]
        .split('\n')
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_string();
    (line, column, context_line)
}

pub fn apply_edits(root: &Path, edits: &[FileEdits], replacement: &str) -> Result<usize> {
    let mut total = 0usize;
    for fe in edits {
        let mut result = fe.text.clone();
        for (start, end) in fe.spans.iter().rev() {
            result.replace_range(*start..*end, replacement);
            total += 1;
        }
        let abs = root.join(&fe.rel);
        std::fs::write(&abs, result).with_context(|| format!("cannot write {}", fe.rel))?;
    }
    Ok(total)
}
