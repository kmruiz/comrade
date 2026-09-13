//! Tree-sitter powered code tools: `ts_find_references`, `ts_rename`, `ts_list_symbols`.
//!
//! v1 is deliberately *lexical*: occurrences are identifier tokens parsed by
//! tree-sitter, so matches never appear inside strings or comments (unlike
//! grep). Cross-file semantic resolution (LSP-grade) is a later layer; tools
//! document their approximation so the model can prefer an LSP-backed tool
//! when one is available.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

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

/// Recursively collect every supported source file under `root`, skipping the
/// usual build/vendor directories. A repository may mix languages, so all
/// [`SUPPORTED_EXTS`] are gathered in one pass.
fn walk_sources(root: &Path, out: &mut Vec<PathBuf>) {
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
            walk_sources(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SUPPORTED_EXTS.contains(&e))
        {
            out.push(path);
        }
    }
}

/// A source language the tree-sitter tools understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LangId {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Css,
    Html,
}

/// Every file extension treated as source, across all supported languages.
pub(crate) const SUPPORTED_EXTS: &[&str] = &[
    "rs", "js", "jsx", "mjs", "cjs", "ts", "mts", "cts", "tsx", "css", "html", "htm",
];

/// Map a file extension to its language, if Comrade supports it.
pub(crate) fn lang_of(ext: &str) -> Option<LangId> {
    Some(match ext {
        "rs" => LangId::Rust,
        "js" | "jsx" | "mjs" | "cjs" => LangId::JavaScript,
        "ts" | "mts" | "cts" => LangId::TypeScript,
        "tsx" => LangId::Tsx,
        "css" => LangId::Css,
        "html" | "htm" => LangId::Html,
        _ => return None,
    })
}

/// The tree-sitter grammar for a language.
pub(crate) fn grammar(l: LangId) -> tree_sitter::Language {
    match l {
        LangId::Rust => tree_sitter_rust::LANGUAGE.into(),
        LangId::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        LangId::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        LangId::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        LangId::Css => tree_sitter_css::LANGUAGE.into(),
        LangId::Html => tree_sitter_html::LANGUAGE.into(),
    }
}

fn language_for(ext: &str) -> Option<tree_sitter::Language> {
    lang_of(ext).map(grammar)
}

/// Node kinds that count as an identifier occurrence (`ts_find_references` /
/// `ts_rename`), per language. Rust keeps the bare `identifier`; JS/TS add
/// property/member identifiers so method and field uses are found; CSS and HTML
/// have no shared identifier node, so class/id/tag/attribute names are used.
pub(crate) fn ident_kinds(l: LangId) -> &'static [&'static str] {
    const RUST: &[&str] = &["identifier"];
    const JS: &[&str] = &[
        "identifier",
        "property_identifier",
        "shorthand_property_identifier",
        "shorthand_property_identifier_pattern",
        "private_property_identifier",
        "type_identifier",
    ];
    const CSS: &[&str] = &["class_name", "id_name", "tag_name"];
    const HTML: &[&str] = &["tag_name", "attribute_name"];
    match l {
        LangId::Rust => RUST,
        LangId::JavaScript | LangId::TypeScript | LangId::Tsx => JS,
        LangId::Css => CSS,
        LangId::Html => HTML,
    }
}

/// The short declaration label for a node kind, or `None` when the kind is not a
/// declaration Comrade indexes. Node-kind names are unique across the supported
/// grammars, so a single table suffices. Rust reuses its vocabulary
/// (`fn`/`struct`/…); JS/TS/CSS/HTML contribute
/// `class`/`method`/`interface`/`var`/`namespace`/`rule`/`el`.
pub(crate) fn decl_label(kind: &str) -> Option<&'static str> {
    Some(match kind {
        // Rust
        "function_item" => "fn",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "mod_item" => "mod",
        "type_item" => "type",
        "static_item" => "static",
        "const_item" => "const",
        // JS / TS
        "function_declaration" | "generator_function_declaration" => "fn",
        "class_declaration" | "abstract_class_declaration" => "class",
        "method_definition" => "method",
        "interface_declaration" => "interface",
        "type_alias_declaration" => "type",
        "enum_declaration" => "enum",
        "lexical_declaration" | "variable_declaration" => "var",
        "module" => "namespace",
        // CSS
        "rule_set" => "rule",
        // HTML
        "element" => "el",
        _ => return None,
    })
}

/// The body node a container declaration nests other declarations in, for the
/// structural map: Rust `impl`/`mod`/`trait`, JS/TS classes and namespaces.
pub(crate) fn container_body<'a>(node: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    match node.kind() {
        "impl_item" | "trait_item" | "mod_item" => node.child_by_field_name("body"),
        "class_declaration" | "abstract_class_declaration" | "module" => {
            node.child_by_field_name("body")
        }
        _ => None,
    }
}

/// The display name for a declaration node. Most declarations expose a `name`
/// field; a JS/TS variable declaration uses its first declarator, a CSS rule its
/// selector, and an HTML element its tag name.
pub(crate) fn decl_name(node: &tree_sitter::Node, text: &str) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        let s = n.utf8_text(text.as_bytes()).unwrap_or("?").to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    match node.kind() {
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            for c in node.children(&mut cursor) {
                if c.kind() == "variable_declarator"
                    && let Some(n) = c.child_by_field_name("name")
                {
                    return Some(n.utf8_text(text.as_bytes()).unwrap_or("?").to_string());
                }
            }
            None
        }
        "rule_set" => {
            let head = &text[node.start_byte()..node.end_byte()];
            let head = head.split('{').next().unwrap_or(head);
            let collapsed = head.split_whitespace().collect::<Vec<_>>().join(" ");
            (!collapsed.is_empty()).then(|| collapsed.chars().take(200).collect::<String>())
        }
        "element" => {
            let mut cursor = node.walk();
            for c in node.children(&mut cursor) {
                if c.kind() == "start_tag" {
                    let mut inner = c.walk();
                    for t in c.children(&mut inner) {
                        if t.kind() == "tag_name" {
                            return Some(t.utf8_text(text.as_bytes()).unwrap_or("?").to_string());
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Find every identifier occurrence of `symbol` in a single source text.
fn occurrences_in_text(l: LangId, text: &str, symbol: &str) -> Vec<(usize, usize)> {
    let lang = grammar(l);
    let kinds = ident_kinds(l);
    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(&lang);
    let Some(tree) = parser.parse(text, None) else {
        return vec![];
    };
    let mut spans = Vec::new();
    let mut cursor = tree.walk();
    let mut descend = true;
    loop {
        let node = cursor.node();
        if kinds.contains(&node.kind()) {
            let start = node.start_byte();
            let end = node.end_byte();
            if text.get(start..end) == Some(symbol) {
                spans.push((start, end));
            }
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            descend = true;
        } else if !cursor.goto_parent() {
            return spans;
        } else {
            descend = false;
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
        if let Some(l) = lang_of(
            Path::new(&rel)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        ) {
            let spans = occurrences_in_text(l, &text, symbol);
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
        let Some(l) = lang_of(ext) else {
            continue;
        };
        let spans = occurrences_in_text(l, &text, symbol);
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

/// Find the declaration of `symbol` across the project (or `path`). `kind`
/// optionally narrows to one declaration kind (short label, e.g. `"fn"`); a
/// leading kind prefix on `symbol` itself ("fn compute") is stripped and used
/// as the filter.
pub fn find_definition(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
    kind: Option<&str>,
) -> Result<Option<SymbolDef>> {
    find_decl(root, symbol, path, only, kind, false)
}

/// Find the declaration of `symbol` and return its whole body. See
/// [`find_definition`] for the `kind` semantics.
pub fn read_symbol(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
    kind: Option<&str>,
) -> Result<Option<SymbolDef>> {
    find_decl(root, symbol, path, only, kind, true)
}

fn find_decl(
    root: &Path,
    symbol: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
    kind: Option<&str>,
    want_body: bool,
) -> Result<Option<SymbolDef>> {
    // A kind keyword smuggled into the symbol ("fn compute") becomes the kind
    // filter; the bare identifier is what must equal a declaration's name.
    let (prefix_kind, symbol) = split_kind_prefix(symbol);
    let kind = match (prefix_kind, kind) {
        (Some(p), Some(k)) if !p.eq_ignore_ascii_case(k) => {
            bail!(
                "symbol {symbol:?} already names kind {p:?}, which conflicts with the kind filter {k:?}"
            )
        }
        (Some(p), _) => Some(p),
        (None, k) => k,
    };
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
            if let Some(label) = decl_label(node.kind())
                && kind.is_none_or(|k| label.eq_ignore_ascii_case(k))
                && decl_name(&node, &text).as_deref() == Some(symbol)
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
                    kind: label.to_string(),
                    signature,
                    body,
                }));
            }
            if descend && cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                descend = true;
            } else if !cursor.goto_parent() {
                break;
            } else {
                descend = false;
            }
        }
    }
    Ok(None)
}

/// Short kind labels the tree-sitter tools accept as filters. Rust source
/// keywords doubled as decl labels stay distinct because a name filter always
/// operates on the bare identifier after the keyword: "fn" is the function kind,
/// "type" the type-alias kind. JS/TS/CSS/HTML contribute `class`, `interface`,
/// `method`, `var`, `namespace`, `rule` and `el`.
pub const KIND_LABELS: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "trait",
    "impl",
    "mod",
    "type",
    "static",
    "const",
    "class",
    "interface",
    "method",
    "var",
    "namespace",
    "rule",
    "el",
];

/// When `input` starts with a kind keyword followed by whitespace ("fn
/// compute", "struct Config"), split it into the kind label and the bare
/// remainder. Returns `(None, input)` untouched otherwise. Case-insensitive on
/// the keyword; the keyword must be whitespace-delimited so names like
/// `typewriter` are never mistaken for a `type` prefix.
pub fn split_kind_prefix(input: &str) -> (Option<&'static str>, &str) {
    let head_end = input.find(char::is_whitespace).unwrap_or(input.len());
    let head = &input[..head_end];
    for &label in KIND_LABELS {
        if head.eq_ignore_ascii_case(label) {
            let rest = input[head_end..].trim_start();
            if !rest.is_empty() {
                return (Some(label), rest);
            }
            break;
        }
    }
    (None, input)
}

/// One-line "head" of a declaration node: text up to the opening `{`, whitespace
/// collapsed, capped.
pub(crate) fn signature_of(node: &tree_sitter::Node, text: &str) -> String {
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
        // Depth-first visit where every node is processed exactly once: descend
        // eagerly and only ascend when a node has no children or no siblings,
        // so a node is never re-visited after climbing back onto it. `break
        // 'walk` ends the walk for this file (its tree is exhausted) while the
        // next file is still scanned.
        'walk: loop {
            let node = cursor.node();
            if let Some(label) = decl_label(node.kind()) {
                let (line, _, _) = locate(&text, node.start_byte());
                let signature = signature_of(&node, &text);
                // A name when the node exposes one; otherwise the head line
                // (Rust `impl Foo {`, a `var` declarator, …).
                let text_col = decl_name(&node, &text).unwrap_or_else(|| {
                    text[node.start_byte()..node.end_byte()]
                        .lines()
                        .next()
                        .unwrap_or(label)
                        .trim()
                        .to_string()
                });
                rows.push(DeclRow {
                    label: label.to_string(),
                    text: text_col,
                    line,
                    signature,
                });
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'walk;
                }
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
/// and then read only its body. `kind` optionally narrows to one declaration
/// kind; a leading kind keyword in `query` itself ("fn compute") is stripped
/// and used as the filter, so a bare name search never has to carry "fn ".
pub fn search_symbols(
    root: &Path,
    query: &str,
    path: Option<&str>,
    only: Option<&HashSet<PathBuf>>,
    kind: Option<&str>,
) -> Result<Vec<String>> {
    let (prefix_kind, query) = split_kind_prefix(query);
    let kind = match (prefix_kind, kind) {
        (Some(p), Some(k)) if !p.eq_ignore_ascii_case(k) => {
            bail!(
                "query {query:?} already names kind {p:?}, which conflicts with the kind filter {k:?}"
            )
        }
        (Some(p), _) => Some(p),
        (None, k) => k,
    };
    let q = query.to_lowercase();
    Ok(collect_decl_rows(root, path, only)?
        .into_iter()
        .filter(|r| r.text.to_lowercase().contains(&q))
        .filter(|r| kind.is_none_or(|k| r.label.eq_ignore_ascii_case(k)))
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
    let Some(label) = decl_label(node.kind()) else {
        return;
    };
    let body = container_body(node);
    let mut text_col = decl_name(node, text).unwrap_or_else(|| {
        text[node.start_byte()..node.end_byte()]
            .lines()
            .next()
            .unwrap_or(label)
            .trim()
            .to_string()
    });
    match node.kind() {
        // Drop the leading `impl` keyword so the row reads "impl Foo {".
        "impl_item" => {
            text_col = text_col
                .strip_prefix("impl")
                .unwrap_or(&text_col)
                .trim()
                .to_string();
        }
        "mod_item" => {
            let has_body = node.child_by_field_name("body").is_some();
            text_col = if has_body {
                format!("{text_col} {{")
            } else {
                format!("{text_col};")
            };
        }
        _ => {}
    }
    let (line, _, _) = locate(text, node.start_byte());
    out.push(MapRow {
        depth,
        label: label.to_string(),
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
            walk_sources(&abs, &mut paths);
            for sub in paths {
                if let Ok(meta) = sub.metadata()
                    && meta.len() > MAX_FILE_BYTES
                {
                    continue;
                }
                let rel = sub
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| sub.to_string_lossy().into_owned());
                files.push((rel, sub));
            }
        } else {
            if let Some(set) = only
                && !set.contains(&abs)
            {
                return Ok(Vec::new());
            }
            files.push((p.trim_start_matches('/').to_string(), abs));
        }
    } else {
        let mut paths = Vec::new();
        walk_sources(root, &mut paths);
        for abs in paths {
            if let Ok(meta) = abs.metadata()
                && meta.len() > MAX_FILE_BYTES
            {
                continue;
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

// ---------------------------------------------------------------------------
// change-impact analysis (git diff + tree-sitter)
// ---------------------------------------------------------------------------

/// A test function (`#[test]` and friends) discovered by tree-sitter.
#[derive(Debug, Clone)]
pub struct TestFn {
    /// Project-root relative file.
    pub file: String,
    /// 1-based line of the `fn` keyword.
    pub line: usize,
    /// Function name.
    pub name: String,
    /// Whole function source text (scanned for referenced identifiers).
    pub body: String,
}

/// True when an attribute marks a test function (`#[test]`, `#[tokio::test]`,
/// `#[async_std::test]`, `#[serial_test::serial]`, `#[test_case]`).
fn attr_is_test(attr_text: &str) -> bool {
    let a = attr_text.trim();
    a.starts_with("#[test")
        || a.starts_with("#[tokio::test")
        || a.starts_with("#[async_std::test")
        || a.starts_with("#[serial_test")
        || a.starts_with("#[test_case")
}

/// Whether a `function_item` is preceded by a test attribute.
fn has_test_attr(node: tree_sitter::Node, text: &str) -> bool {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "attribute_item" => {
                if attr_is_test(&text[p.byte_range()]) {
                    return true;
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = p.prev_sibling();
    }
    false
}

/// Every test case in the project: Rust `#[test]` functions and JS/TS
/// `it(...)` / `test(...)` / `specify(...)` cases.
pub fn test_functions(root: &Path) -> Result<Vec<TestFn>> {
    let mut out = Vec::new();
    for (rel, text) in collect_files(root, None, None)? {
        let ext = Path::new(&rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let Some(l) = lang_of(ext) else {
            continue;
        };
        let lang = grammar(l);
        let mut parser = tree_sitter::Parser::new();
        let _ = parser.set_language(&lang);
        let Some(tree) = parser.parse(&text, None) else {
            continue;
        };
        match l {
            LangId::Rust => rust_tests(tree.root_node(), &text, &rel, &mut out),
            LangId::JavaScript | LangId::TypeScript | LangId::Tsx => {
                js_tests(tree.root_node(), &text, &rel, &mut out)
            }
            LangId::Css | LangId::Html => {}
        }
    }
    Ok(out)
}

/// Rust `#[test]`-annotated functions (any depth).
fn rust_tests(root: tree_sitter::Node, text: &str, rel: &str, out: &mut Vec<TestFn>) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "function_item" && has_test_attr(n, text) {
            let name = n
                .child_by_field_name("name")
                .and_then(|c| c.utf8_text(text.as_bytes()).ok())
                .unwrap_or("?")
                .to_string();
            let (line, _, _) = locate(text, n.start_byte());
            out.push(TestFn {
                file: rel.to_string(),
                line,
                name,
                body: text[n.byte_range()].to_string(),
            });
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                stack.push(c);
            }
        }
    }
}

/// JS/TS test cases: `it('name', cb)`, `test('name', cb)`, `specify(...)` and
/// their member forms (`it.only`, `test.skip`, `it.concurrent`, …). `describe`
/// blocks are containers, not cases, so they are not reported (their inner
/// `it`/`test` calls are).
fn js_tests(root: tree_sitter::Node, text: &str, rel: &str, out: &mut Vec<TestFn>) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression"
            && let Some(base) = js_test_base(n, text)
        {
            let (line, _, _) = locate(text, n.start_byte());
            let name = js_test_name(n, text).unwrap_or(base);
            let body = js_test_body(n, text);
            out.push(TestFn {
                file: rel.to_string(),
                line,
                name,
                body,
            });
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                stack.push(c);
            }
        }
    }
}

/// The base callee of a test call (`it`, `test`, `specify`, `xit`, …), or `None`
/// when the call is not a test. A member callee (`it.only`) resolves to its
/// object (`it`).
fn js_test_base(call: tree_sitter::Node, text: &str) -> Option<String> {
    const BASES: &[&str] = &["it", "test", "specify", "xit", "xtest", "fit", "ftest"];
    let callee = call.child_by_field_name("function")?;
    let base = match callee.kind() {
        "identifier" => text[callee.byte_range()].to_string(),
        "member_expression" => {
            let obj = callee.child_by_field_name("object")?;
            text[obj.byte_range()].to_string()
        }
        _ => return None,
    };
    BASES.contains(&base.as_str()).then_some(base)
}

/// The first string/template argument of a test call (its title), with the
/// surrounding quotes/backticks trimmed.
fn js_test_name(call: tree_sitter::Node, text: &str) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for a in args.children(&mut cursor) {
        if matches!(a.kind(), "string" | "template_string") {
            return Some(
                text[a.byte_range()]
                    .trim_matches(|c| c == '\'' || c == '"' || c == '`')
                    .to_string(),
            );
        }
    }
    None
}

/// The callback body of a test call (its function/arrow argument), else the
/// whole call text. Scanned for referenced identifiers by `ts_test_impact`.
fn js_test_body(call: tree_sitter::Node, text: &str) -> String {
    if let Some(args) = call.child_by_field_name("arguments") {
        let mut cursor = args.walk();
        let kids: Vec<tree_sitter::Node> = args.children(&mut cursor).collect();
        for a in kids.iter().rev() {
            if matches!(
                a.kind(),
                "arrow_function" | "function" | "function_expression"
            ) {
                return text[a.byte_range()].to_string();
            }
        }
    }
    text[call.byte_range()].to_string()
}

/// Names of declarations (any depth) defined in one source file `rel`.
pub fn decl_names_in_text(rel: &str, text: &str) -> Vec<String> {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let Some(l) = lang_of(ext) else {
        return Vec::new();
    };
    let lang = grammar(l);
    let mut parser = tree_sitter::Parser::new();
    let _ = parser.set_language(&lang);
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if decl_label(n.kind()).is_some()
            && let Some(name) = decl_name(&n, text)
        {
            out.push(name);
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                stack.push(c);
            }
        }
    }
    out
}

/// The identifier tokens in `text` (word characters only), as a set.
pub fn identifier_tokens(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-engine-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_identifiers_across_a_mixed_repo() {
        let dir = scratch("findref");
        std::fs::write(
            dir.join("a.ts"),
            "function compute() { return 1; }\nconst x = compute();\n",
        )
        .unwrap();
        std::fs::write(dir.join("b.rs"), "fn compute() {}\n").unwrap();
        let occ = find_occurrences(&dir, "compute", None, None).unwrap();
        assert!(occ.iter().any(|o| o.file == "b.rs"), "{occ:?}");
        assert_eq!(
            occ.iter().filter(|o| o.file == "a.ts").count(),
            2,
            "decl + call in a.ts: {occ:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_and_reads_ts_declarations() {
        let dir = scratch("decls");
        std::fs::write(
            dir.join("m.ts"),
            "export class Widget {\n  render(): void {}\n}\ninterface Props { x: number }\nexport function make(): Widget { return new Widget(); }\n",
        )
        .unwrap();
        let syms = list_symbols(&dir, None, None).unwrap();
        assert!(
            syms.iter().any(|s| s.starts_with("class Widget")),
            "{syms:?}"
        );
        assert!(
            syms.iter().any(|s| s.starts_with("method render")),
            "{syms:?}"
        );
        assert!(
            syms.iter().any(|s| s.starts_with("interface Props")),
            "{syms:?}"
        );
        assert!(syms.iter().any(|s| s.starts_with("fn make")), "{syms:?}");

        let hits = search_symbols(&dir, "Wid", None, None, None).unwrap();
        assert!(hits.iter().any(|s| s.contains("class Widget")), "{hits:?}");

        let def = read_symbol(&dir, "make", None, None, None)
            .unwrap()
            .expect("fn make");
        assert_eq!(def.kind, "fn");
        assert!(def.body.unwrap().contains("new Widget()"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn structural_map_nests_js_class_methods() {
        let dir = scratch("map");
        std::fs::write(dir.join("c.js"), "class Foo {\n  bar() {}\n  baz() {}\n}\n").unwrap();
        let rows = structural_map(&dir, None, None, false, &HashSet::new()).unwrap();
        let joined = rows.join("\n");
        assert!(joined.contains("class Foo"), "{joined}");
        assert!(joined.contains("  method bar"), "{joined}");
        assert!(joined.contains("  method baz"), "{joined}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_spans_cover_js_identifiers() {
        let dir = scratch("ren");
        std::fs::write(
            dir.join("x.ts"),
            "const old = 1;\nconsole.log(old + old);\n",
        )
        .unwrap();
        let edits = rename_edits(&dir, "old", None, None).unwrap();
        let e = edits.iter().find(|e| e.rel == "x.ts").expect("edits");
        assert_eq!(e.spans.len(), 3, "decl + two uses");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_css_and_html_declarations() {
        let dir = scratch("web");
        std::fs::write(
            dir.join("s.css"),
            ".alpha { color: red; }\n#beta { color: blue; }\n",
        )
        .unwrap();
        std::fs::write(dir.join("p.html"), "<div class=\"alpha\">hi</div>\n").unwrap();
        let syms = list_symbols(&dir, None, None).unwrap();
        assert!(syms.iter().any(|s| s.contains("rule .alpha")), "{syms:?}");
        assert!(syms.iter().any(|s| s.contains("rule #beta")), "{syms:?}");
        assert!(syms.iter().any(|s| s.contains("el div")), "{syms:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovers_js_ts_tests() {
        let dir = scratch("jstests");
        std::fs::write(
            dir.join("a.test.ts"),
            "describe('Widget', () => {\n  it('renders Widget', () => { expect(Widget).toBeDefined(); });\n  it.only('updates', () => {});\n});\ntest('plain', function () {});\n",
        )
        .unwrap();
        let tests = test_functions(&dir).unwrap();
        let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"renders Widget"), "{names:?}");
        assert!(names.contains(&"updates"), "{names:?}");
        assert!(names.contains(&"plain"), "{names:?}");
        // `describe` is a container, not a test case.
        assert!(!names.contains(&"Widget"), "{names:?}");
        assert_eq!(tests.len(), 3, "{names:?}");
        // The callback body carries the referenced identifier for impact analysis.
        let renders = tests.iter().find(|t| t.name == "renders Widget").unwrap();
        assert!(renders.body.contains("Widget"), "{}", renders.body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decl_names_in_text_is_language_aware() {
        let ts = "export class Widget {}\ninterface Props {}\nfunction make() {}\n";
        let names = decl_names_in_text("m.ts", ts);
        assert!(names.contains(&"Widget".to_string()), "{names:?}");
        assert!(names.contains(&"Props".to_string()), "{names:?}");
        assert!(names.contains(&"make".to_string()), "{names:?}");

        let rs = "fn compute() {}\nstruct Thing;\n";
        let names = decl_names_in_text("lib.rs", rs);
        assert!(names.contains(&"compute".to_string()), "{names:?}");
        assert!(names.contains(&"Thing".to_string()), "{names:?}");
    }
}
