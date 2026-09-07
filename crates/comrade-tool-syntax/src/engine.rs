//! Tree-sitter powered code tools: `find_references`, `rename`, `list_symbols`.
//!
//! v1 is deliberately *lexical*: occurrences are identifier tokens parsed by
//! tree-sitter, so matches never appear inside strings or comments (unlike
//! grep). Cross-file semantic resolution (LSP-grade) is a later layer; tools
//! document their approximation so the model can prefer an LSP-backed tool
//! when one is available.

use std::collections::{BTreeMap, HashSet};
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
pub fn find_occurrences(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<Occurrence>> {
    let mut out = Vec::new();
    for (rel, text) in collect_files(root, path, only)? {
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
pub fn rename_edits(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<FileEdits>> {
    let mut grouped: BTreeMap<String, FileEdits> = BTreeMap::new();
    for (rel, text) in collect_files(root, path, only)? {
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

/// A resolved symbol declaration.
#[derive(Debug, Clone)]
pub struct SymbolDef {
    /// Project-root relative file.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Short kind label, e.g. `fn`, `struct`.
    pub kind: String,
    /// One-line signature / head, e.g. `fn area(p: Point) -> i32`.
    pub signature: String,
    /// Whole body text when requested (e.g. by `read_symbol`).
    pub body: Option<String>,
}

/// Find the declaration of `symbol` across the project (or `path`).
pub fn find_definition(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Option<SymbolDef>> {
    find_decl(root, symbol, path, only, false)
}

/// Find the declaration of `symbol` and return its whole body.
pub fn read_symbol(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Option<SymbolDef>> {
    find_decl(root, symbol, path, only, true)
}

fn find_decl(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
    want_body: bool,
) -> Result<Option<SymbolDef>> {
    for (rel, text) in collect_files(root, path, only)? {
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
        loop {
            let node = cursor.node();
            if is_decl_kind(node.kind())
                && let Some(name) = node.child_by_field_name("name")
                && name.utf8_text(text.as_bytes()).unwrap_or("") == symbol
            {
                let (line, _, _) = locate(&text, node.start_byte());
                let signature = signature_of(&node, &text);
                let body = if want_body {
                    Some(text[node.start_byte()..node.end_byte()].to_string())
                } else {
                    None
                };
                return Ok(Some(SymbolDef {
                    file: rel,
                    line,
                    kind: short_kind(node.kind()).to_string(),
                    signature,
                    body,
                }));
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
                    return Ok(None);
                }
                descend = false;
                break;
            }
        }
    }
    Ok(None)
}

fn is_decl_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "mod_item"
            | "type_item"
            | "static_item"
            | "const_item"
    )
}

/// One-line "head" of a declaration node: text up to the opening `{`, whitespace
/// collapsed, capped.
fn signature_of(node: &tree_sitter::Node, text: &str) -> String {
    let seg = &text[node.start_byte()..node.end_byte()];
    let head = match seg.find('{') {
        Some(i) => &seg[..i],
        None => seg,
    };
    let collapsed = head.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(300).collect()
}

/// A single declaration row used for listing.
struct DeclRow {
    /// Display label, e.g. `fn` / `impl`.
    label: String,
    /// Display text: the name, or the impl head line.
    text: String,
    line: usize,
    signature: String,
}

/// Collect declaration rows across the target files.
fn collect_decl_rows(
    root: &Path,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<DeclRow>> {
    let mut rows = Vec::new();
    for (rel, text) in collect_files(root, path, only)? {
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
        loop {
            let node = cursor.node();
            if is_decl_kind(node.kind()) || node.kind() == "impl_item" {
                if let Some(name) = node.child_by_field_name("name") {
                    let (line, _, _) = locate(&text, node.start_byte());
                    let label = short_kind(node.kind()).to_string();
                    let name = name.utf8_text(text.as_bytes()).unwrap_or("?").to_string();
                    let signature = signature_of(&node, &text);
                    rows.push(DeclRow {
                        label: label.clone(),
                        text: name,
                        line,
                        signature,
                    });
                } else if node.kind() == "impl_item" {
                    let head = text[node.start_byte()..node.end_byte()]
                        .lines()
                        .next()
                        .unwrap_or("impl")
                        .trim()
                        .to_string();
                    let (line, _, _) = locate(&text, node.start_byte());
                    let signature = signature_of(&node, &text);
                    rows.push(DeclRow {
                        label: "impl".into(),
                        text: head,
                        line,
                        signature,
                    });
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
                    return Ok(rows);
                }
                descend = false;
                break;
            }
        }
    }
    Ok(rows)
}

/// List top-level declarations (functions, structs, enums, traits, impls,
/// modules) in a file, as `kind name @ line`.
pub fn list_symbols(
    root: &Path,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<String>> {
    Ok(collect_decl_rows(root, path, only)?
        .into_iter()
        .map(|r| format!("{} {} @ {}", r.label, r.text, r.line))
        .collect())
}

/// Like [`list_symbols`] but each row carries its one-line signature:
/// `kind name | signature @ line`.
pub fn list_symbol_signatures(
    root: &Path,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<String>> {
    Ok(collect_decl_rows(root, path, only)?
        .into_iter()
        .map(|r| format!("{} {} | {} @ {}", r.label, r.text, r.signature, r.line))
        .collect())
}

/// Find declarations whose name contains `query` (case-insensitive). Returns
/// rows `kind name | signature @ line` so the caller can pick the right symbol
/// and then read only its body.
pub fn search_symbols(
    root: &Path,
    query: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<String>> {
    let q = query.to_lowercase();
    Ok(collect_decl_rows(root, path, only)?
        .into_iter()
        .filter(|r| r.text.to_lowercase().contains(&q))
        .map(|r| format!("{} {} | {} @ {}", r.label, r.text, r.signature, r.line))
        .collect())
}

/// A structural-map entry: one declaration, indented by its nesting depth in
/// modules / impls / traits.
#[derive(Debug)]
struct MapRow {
    depth: usize,
    /// Short kind label (`fn`, `struct`, `mod`, ...).
    label: String,
    /// Display text: the name, or (impl/trait) head, or `name {` for inline mods.
    text: String,
    signature: String,
    line: usize,
}

/// Recurse into the direct children of a container node (`source_file`,
/// `declaration_list` bodies) and record the declarations.
fn map_children(container: &tree_sitter::Node, text: &str, depth: usize, out: &mut Vec<MapRow>) {
    let count = container.child_count();
    for i in 0..count {
        if let Some(c) = container.child(i) {
            map_item(&c, text, depth, out);
        }
    }
}

/// Record one declaration and, when it nests others (inline `mod`, `impl`,
/// `trait`), descend one level into its body so the map shows the hierarchy.
fn map_item(node: &tree_sitter::Node, text: &str, depth: usize, out: &mut Vec<MapRow>) {
    let kind = node.kind();
    let is_decl = is_decl_kind(kind) || kind == "impl_item";
    if !is_decl {
        return;
    }
    let (label, text_col, body) = match kind {
        "impl_item" => {
            let head = text[node.start_byte()..node.end_byte()]
                .lines()
                .next()
                .unwrap_or("impl")
                .trim()
                .to_string();
            // Drop the leading `impl` keyword so the row reads "impl Foo {".
            let head = head
                .strip_prefix("impl")
                .unwrap_or(&head)
                .trim()
                .to_string();
            (
                short_kind(kind).to_string(),
                head,
                node.child_by_field_name("body"),
            )
        }
        "trait_item" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| n.utf8_text(text.as_bytes()).unwrap_or("?"))
                .unwrap_or("?")
                .to_string();
            (
                short_kind(kind).to_string(),
                name,
                node.child_by_field_name("body"),
            )
        }
        "mod_item" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| n.utf8_text(text.as_bytes()).unwrap_or("?"))
                .unwrap_or("?")
                .to_string();
            let body = node.child_by_field_name("body");
            let txt = if body.is_some() {
                format!("{name} {{")
            } else {
                format!("{name};")
            };
            (short_kind(kind).to_string(), txt, body)
        }
        _ => {
            let name = node
                .child_by_field_name("name")
                .map(|n| n.utf8_text(text.as_bytes()).unwrap_or("?"))
                .unwrap_or("?")
                .to_string();
            (short_kind(kind).to_string(), name, None)
        }
    };
    let (line, _, _) = locate(text, node.start_byte());
    out.push(MapRow {
        depth,
        label,
        text: text_col,
        signature: signature_of(node, text),
        line,
    });
    if let Some(body) = body {
        map_children(&body, text, depth + 1, out);
    }
}

/// Build a structural map of the project (or `path`): one section per file
/// listing the declarations nested under their inline `mod` / `impl` / `trait`
/// containers, so the layout of functions, modules, and types is visible
/// without grepping. `only` restricts to files that differ from HEAD when set.
/// `include` (short kinds, e.g. `fn`) restricts which declarations appear;
/// empty means every declaration kind.
pub fn structural_map(
    root: &Path,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
    with_sigs: bool,
    include: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (rel, text) in collect_files(root, path, only)? {
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
        let mut rows = Vec::new();
        let root_node = tree.root_node();
        map_children(&root_node, &text, 0, &mut rows);
        if !include.is_empty() {
            rows.retain(|r| include.contains(&r.label));
        }
        if rows.is_empty() {
            continue;
        }
        out.push(format!("== {rel} =="));
        for r in rows {
            let pad = "  ".repeat(r.depth);
            let row = if with_sigs {
                format!("{pad}{} {} | {} @ {}", r.label, r.text, r.signature, r.line)
            } else {
                format!("{pad}{} {} @ {}", r.label, r.text, r.line)
            };
            out.push(row);
        }
    }
    Ok(out)
}

/// Returns (rel_path, contents) for the target files. When `path` is provided
/// it may name a single file *or* a directory (which is walked for supported
/// sources), resolved relative to root.
fn collect_files(
    root: &Path,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    if let Some(p) = path {
        let abs = root.join(p);
        if !abs.starts_with(root) {
            anyhow::bail!("path {p:?} escapes the project root");
        }
        let is_dir = abs.is_dir();
        if is_dir {
            // A directory scope: map every supported source under it. The
            // git-modified (`only`) filter is applied per file afterwards.
            let mut paths = Vec::new();
            for ext in ["rs"] {
                walk_files(&abs, ext, &mut paths);
            }
            for sub in paths {
                if let Ok(meta) = sub.metadata() {
                    if meta.len() > MAX_FILE_BYTES {
                        continue;
                    }
                }
                let rel = sub
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| sub.to_string_lossy().into_owned());
                files.push((rel, sub));
            }
        } else {
            if let Some(set) = only {
                if !set.contains(&abs) {
                    return Ok(Vec::new());
                }
            }
            files.push((p.trim_start_matches('/').to_string(), abs));
        }
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
    if let Some(set) = only {
        files.retain(|(_, abs)| set.contains(abs));
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
