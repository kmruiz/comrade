//! Embedded semantic search over project memory (ADRs + glossary) and source
//! code.
//!
//! `semantic_search` finds things by MEANING, not keywords. Memory is embedded
//! from the ADR/glossary store; code is embedded from tree-sitter symbol chunks
//! (see `comrade-tool-syntax`) that carry their `file:line`. Everything is
//! embedded — there is no server and no download: the model is an int8-quantized
//! ONNX bundle compiled into the binary (deflated at build time by `build.rs`,
//! inflated in memory on first use) and run in-process by `fastembed`, and the
//! vector store is a persisted flat index under the user cache dir keyed by the
//! project root.
//!
//! Why a flat index and not HNSW: a project's memory is tens to a few hundred
//! documents, where exact cosine over all vectors is instantaneous and needs no
//! extra native dependency or index-tuning. The index only re-embeds documents
//! whose text hash changed, so steady-state calls cost one query embedding.
//!
//! The code index is incremental and git-driven: it records the HEAD it was
//! built at plus a per-file stamp, and `git::dirty_files` says what changed
//! (committed diff + working tree). Unchanged files are neither re-parsed nor
//! re-embedded; projects without a repo fall back to mtime/size stamps.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The int8-quantized BGE-small-en-v1.5 ONNX graph (~34 MB raw, ~24 MB deflated),
/// compiled into the binary so there is no download, no network and no model
/// cache to manage. `build.rs` deflates the raw asset from `assets/` into
/// `OUT_DIR`; `inflate` restores it in memory on first use.
const MODEL_ONNX: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/assets/model_quantized.onnx.deflate"
));
/// The tokenizer and its config files, deflated alongside the graph.
const MODEL_TOKENIZER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/assets/tokenizer.json.deflate"));
const MODEL_CONFIG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/assets/config.json.deflate"));
const MODEL_SPECIAL_TOKENS: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/assets/special_tokens_map.json.deflate"
));
const MODEL_TOKENIZER_CONFIG: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/assets/tokenizer_config.json.deflate"
));

/// Human-readable model id stored with the index so a model change invalidates
/// cached vectors.
const MODEL_ID: &str = "bge-small-en-v1.5-int8";

/// A document to index: one ADR or one glossary term.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Doc {
    /// Stable key, e.g. `adr:0003` or `term:ToolSpec`.
    pub id: String,
    /// `adr` or `glossary`.
    pub kind: String,
    /// One-line title shown in results.
    pub title: String,
    /// Text that is embedded (title + body).
    pub text: String,
}

/// One indexed document: its key, its text hash (for incremental rebuild) and
/// its embedding vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDoc {
    id: String,
    kind: String,
    title: String,
    preview: String,
    hash: u64,
    vec: Vec<f32>,
}

/// Per-file stat stamp + the ids of the documents it produced, so an unchanged
/// file can be reused without reading or parsing it again.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileStamp {
    /// Modification time in nanoseconds since the UNIX epoch (0 if unavailable).
    mtime: u64,
    size: u64,
    /// Ids of the `StoredDoc`s produced by this file.
    doc_ids: Vec<String>,
}

/// The persisted index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Store {
    model: String,
    dim: usize,
    docs: Vec<StoredDoc>,
    /// Per-file stamps for the code index (empty for the memory index).
    #[serde(default)]
    files: BTreeMap<String, FileStamp>,
    /// The git HEAD this code index was built at, for git-driven reindexing.
    #[serde(default)]
    head: Option<String>,
}

/// Turns text into embedding vectors. Abstracted so the index logic can be
/// tested without downloading a model.
trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// FNV-1a hash of a document's text, used to skip re-embedding unchanged docs.
fn fnv(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Cosine similarity in [-1, 1]; 0 when either vector is zero.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// A single-line preview of a document body, capped.
fn preview_of(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");
    line.chars().take(160).collect()
}

/// Build the documents to index from the memory store and the glossary.
fn documents(root: &Path) -> Result<Vec<Doc>> {
    let mut docs = Vec::new();
    for meta in crate::store::list(root)? {
        let entry = crate::store::read(root, meta.id)?;
        docs.push(Doc {
            id: format!("adr:{:04}", meta.id),
            kind: "adr".into(),
            title: format!("#{:04} [{}] {}", meta.id, meta.status, meta.excerpt()),
            text: entry.body,
        });
    }
    for t in crate::glossary::terms(root)? {
        docs.push(Doc {
            id: format!("term:{}", t.term),
            kind: "glossary".into(),
            title: t.term.clone(),
            text: format!("{}\n\n{}", t.term, t.body),
        });
    }
    Ok(docs)
}

/// Extensions treated as source for the code index (mirrors comrade-tool-syntax).
const CODE_EXT: &[&str] = &[
    "rs", "toml", "md", "py", "js", "ts", "tsx", "go", "c", "h", "cpp", "hpp", "java", "rb", "sh",
];
/// Source files larger than this are not indexed.
const MAX_CODE_BYTES: u64 = 1_000_000;

/// Source files under `root` as `(rel, abs, mtime_nanos, size)`, skipping the
/// usual build/vendor directories.
fn code_files(root: &Path) -> Vec<(String, PathBuf, u64, u64)> {
    let mut out = Vec::new();
    walk_code(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk_code(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf, u64, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                ".git" | "target" | "node_modules" | "vendor" | ".idea" | ".vscode"
            ) {
                continue;
            }
            walk_code(root, &path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CODE_EXT.contains(&e))
        {
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            out.push((rel, path, mtime, size));
        }
    }
}

/// A source chunk as an indexable document. Its id/title carry `file:line` so a
/// hit is directly a location.
fn code_doc(rel: &str, line: usize, kind: &str, name: &str, text: String) -> Doc {
    let title = if name.is_empty() {
        format!("{rel}:{line}  {kind}")
    } else {
        format!("{rel}:{line}  {kind} {name}")
    };
    Doc {
        id: format!("code:{rel}:{line}:{name}"),
        kind: "code".into(),
        title,
        text,
    }
}

/// Rebuild `existing` against `docs`, re-embedding only new/changed documents.
fn refresh(docs: &[Doc], existing: &Store, embedder: &dyn Embedder) -> Result<Store> {
    let stale_model = existing.model != MODEL_ID;
    let old: HashMap<&str, &StoredDoc> = existing.docs.iter().map(|d| (d.id.as_str(), d)).collect();

    let mut slots: Vec<Option<StoredDoc>> = Vec::with_capacity(docs.len());
    let mut todo: Vec<(usize, &Doc)> = Vec::new();
    for (i, doc) in docs.iter().enumerate() {
        let h = fnv(&doc.text);
        let reusable = !stale_model
            && old
                .get(doc.id.as_str())
                .is_some_and(|prev| prev.hash == h && !prev.vec.is_empty());
        if reusable {
            let prev = old[doc.id.as_str()];
            slots.push(Some(StoredDoc {
                id: doc.id.clone(),
                kind: doc.kind.clone(),
                title: doc.title.clone(),
                preview: preview_of(&doc.text),
                hash: h,
                vec: prev.vec.clone(),
            }));
        } else {
            slots.push(None);
            todo.push((i, doc));
        }
    }

    if !todo.is_empty() {
        let texts: Vec<String> = todo.iter().map(|(_, d)| d.text.clone()).collect();
        let vectors = embedder.embed(&texts)?;
        if vectors.len() != todo.len() {
            anyhow::bail!(
                "embedder returned {} vectors for {} documents",
                vectors.len(),
                todo.len()
            );
        }
        for ((i, doc), vec) in todo.iter().zip(vectors) {
            slots[*i] = Some(StoredDoc {
                id: doc.id.clone(),
                kind: doc.kind.clone(),
                title: doc.title.clone(),
                preview: preview_of(&doc.text),
                hash: fnv(&doc.text),
                vec,
            });
        }
    }

    let docs: Vec<StoredDoc> = slots.into_iter().flatten().collect();
    let dim = docs
        .iter()
        .map(|d| d.vec.len())
        .max()
        .unwrap_or(existing.dim);
    Ok(Store {
        model: MODEL_ID.into(),
        dim,
        docs,
        ..Store::default()
    })
}

/// Rank documents by cosine similarity to `query`, ties broken by id,
/// optionally filtered by `kind`.
fn rank<'a>(
    docs: &'a [StoredDoc],
    query: &[f32],
    limit: usize,
    kind: Option<&str>,
) -> Vec<(f32, &'a StoredDoc)> {
    let mut scored: Vec<(f32, &StoredDoc)> = docs
        .iter()
        .filter(|d| kind.is_none_or(|k| d.kind.eq_ignore_ascii_case(k)))
        .map(|d| (cosine(query, &d.vec), d))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    scored.truncate(limit.max(1));
    scored
}

/// The user cache dir Comrade owns: `$XDG_CACHE_HOME/comrade` (fallback
/// `~/.cache/comrade`, else the temp dir). The vector index is cached here so
/// machine-specific data stays out of the repo.
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("comrade")
}

/// Where the index for `root` is cached: the user cache dir, keyed by the
/// project path, so the repo stays clean.
fn index_path(root: &Path) -> PathBuf {
    let key = fnv(&root.to_string_lossy());
    cache_dir()
        .join("semantic")
        .join(format!("{key:016x}.json"))
}

/// Where the code index for `root` is cached (separate from the memory index so
/// memory-only searches stay small and fast).
fn code_index_path(root: &Path) -> PathBuf {
    let key = fnv(&root.to_string_lossy());
    cache_dir()
        .join("semantic")
        .join(format!("{key:016x}-code.json"))
}

fn load_store(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = serde_json::to_string(store)?;
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Rebuild the code index against `existing`, re-parsing and re-embedding only
/// what changed. Change detection is git-driven when the project is a repo (the
/// recorded HEAD + `git::dirty_files`), falling back to per-file mtime/size;
/// unchanged files are reused outright, and within a changed file a chunk whose
/// text is unchanged keeps its vector.
fn code_refresh(root: &Path, existing: &Store, embedder: &dyn Embedder) -> Result<Store> {
    let stale_model = existing.model != MODEL_ID;
    let old: HashMap<&str, &StoredDoc> = existing.docs.iter().map(|d| (d.id.as_str(), d)).collect();
    let dirty = crate::git::dirty_files(root, existing.head.as_deref());

    let mut files: BTreeMap<String, FileStamp> = BTreeMap::new();
    let mut docs: Vec<StoredDoc> = Vec::new();
    let mut todo: Vec<(String, String, String)> = Vec::new(); // (id, title, text)

    for (rel, abs, mtime, size) in code_files(root) {
        let reusable = !stale_model
            && existing.files.get(&rel).is_some_and(|stamp| match &dirty {
                // Git says this file did not change since the last index.
                Some(set) => !set.contains(&rel),
                // No repo: fall back to the stat stamp.
                None => stamp.mtime == mtime && stamp.size == size,
            });
        if reusable {
            let stamp = &existing.files[&rel];
            let mut doc_ids = Vec::new();
            for id in &stamp.doc_ids {
                if let Some(d) = old.get(id.as_str()) {
                    docs.push((*d).clone());
                }
                doc_ids.push(id.clone());
            }
            files.insert(
                rel,
                FileStamp {
                    mtime,
                    size,
                    doc_ids,
                },
            );
            continue;
        }

        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        if bytes.len() as u64 > MAX_CODE_BYTES || bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let mut doc_ids = Vec::new();
        for chunk in comrade_tool_syntax::chunks_of_file(&rel, &text) {
            let doc = code_doc(&rel, chunk.line, &chunk.kind, &chunk.name, chunk.text);
            doc_ids.push(doc.id.clone());
            let h = fnv(&doc.text);
            match old.get(doc.id.as_str()) {
                Some(prev) if prev.hash == h && !prev.vec.is_empty() => docs.push((*prev).clone()),
                _ => todo.push((doc.id, doc.title, doc.text)),
            }
        }
        files.insert(
            rel,
            FileStamp {
                mtime,
                size,
                doc_ids,
            },
        );
    }

    if !todo.is_empty() {
        let texts: Vec<String> = todo.iter().map(|(_, _, t)| t.clone()).collect();
        let vectors = embedder.embed(&texts)?;
        if vectors.len() != todo.len() {
            anyhow::bail!(
                "embedder returned {} vectors for {} chunks",
                vectors.len(),
                todo.len()
            );
        }
        for ((id, title, text), vec) in todo.into_iter().zip(vectors) {
            docs.push(StoredDoc {
                id,
                kind: "code".into(),
                title,
                preview: preview_of(&text),
                hash: fnv(&text),
                vec,
            });
        }
    }

    let dim = docs
        .iter()
        .map(|d| d.vec.len())
        .max()
        .unwrap_or(existing.dim);
    Ok(Store {
        model: MODEL_ID.into(),
        dim,
        docs,
        files,
        head: crate::git::head_sha(root),
    })
}

/// Force a full rebuild of both the memory and code indexes, discarding the
/// caches. Used by the `reindex-semantic-search` command.
pub fn reindex(root: &Path) -> Result<String> {
    let mem = refresh(&documents(root)?, &Store::default(), &FastEmbedder)?;
    save_store(&index_path(root), &mem)?;
    let code = code_refresh(root, &Store::default(), &FastEmbedder)?;
    save_store(&code_index_path(root), &code)?;
    Ok(format!(
        "Reindexed semantic search: {} memory doc(s), {} code chunk(s).",
        mem.docs.len(),
        code.docs.len()
    ))
}

/// The in-process model, built from the embedded assets on first use.
static MODEL: OnceLock<Result<Mutex<TextEmbedding>, String>> = OnceLock::new();

/// Inflate one of the assets `build.rs` deflated into `OUT_DIR`.
fn inflate(name: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .with_context(|| format!("cannot inflate embedded asset {name}"))?;
    Ok(out)
}

/// The real embedder: the embedded int8 model run in-process via ONNX Runtime.
struct FastEmbedder;

impl Embedder for FastEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let cell = MODEL.get_or_init(|| {
            let onnx = inflate("model_quantized.onnx", MODEL_ONNX).map_err(|e| e.to_string())?;
            let tokenizer = TokenizerFiles {
                tokenizer_file: inflate("tokenizer.json", MODEL_TOKENIZER)
                    .map_err(|e| e.to_string())?,
                config_file: inflate("config.json", MODEL_CONFIG).map_err(|e| e.to_string())?,
                special_tokens_map_file: inflate("special_tokens_map.json", MODEL_SPECIAL_TOKENS)
                    .map_err(|e| e.to_string())?,
                tokenizer_config_file: inflate("tokenizer_config.json", MODEL_TOKENIZER_CONFIG)
                    .map_err(|e| e.to_string())?,
            };
            // BGE-small uses CLS pooling (matches fastembed's own model config).
            let model = UserDefinedEmbeddingModel::new(onnx, tokenizer).with_pooling(Pooling::Cls);
            TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::new())
                .map(Mutex::new)
                .map_err(|e| e.to_string())
        });
        let model = cell
            .as_ref()
            .map_err(|e| anyhow::anyhow!("embedding model unavailable: {e}"))?;
        let mut guard = model.lock().unwrap_or_else(|e| e.into_inner());
        guard.embed(texts, Some(16))
    }
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![Box::new(SemanticSearch)]
}

struct SemanticSearch;

static SEMANTIC_SEARCH_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
        name: "semantic_search".into(),
        description: "Search project memory (ADR decisions + glossary) AND source code by MEANING, not keywords: returns the closest hits with a similarity score. Code hits are tree-sitter symbols/line windows carrying file:line. Use when you only know the meaning and not the exact word (find_adr/ts_find_symbol/fs_rgrep need a word you do not have). `scope` picks memory (default), code, or all. Locally embedded model + vector index; no network for search; the index is rebuilt incrementally and only changed files are re-parsed.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What you are looking for, in natural language." },
                "scope": { "type": "string", "enum": ["memory", "code", "all"], "default": "memory", "description": "Search project memory, source code, or both." },
                "kind": { "type": "string", "enum": ["adr", "glossary", "code"], "description": "Restrict to one hit kind (default: any in scope)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 30, "default": 5, "description": "Max results." },
                "rebuild": { "type": "boolean", "default": false, "description": "Force a full re-embed of the index (rarely needed; it rebuilds incrementally)." }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
});

#[async_trait]
impl Tool for SemanticSearch {
    fn spec(&self) -> &ToolSpec {
        &SEMANTIC_SEARCH_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default)]
            scope: Option<String>,
            #[serde(default)]
            kind: Option<String>,
            #[serde(default = "default_limit")]
            limit: usize,
            #[serde(default)]
            rebuild: bool,
        }
        fn default_limit() -> usize {
            5
        }
        let args: Args = serde_json::from_value(args)?;
        let query = args.query.trim().to_string();
        if query.is_empty() {
            anyhow::bail!("`query` must not be empty");
        }
        let scope = args.scope.unwrap_or_else(|| "memory".to_string());
        if !matches!(scope.as_str(), "memory" | "code" | "all") {
            anyhow::bail!("`scope` must be one of memory|code|all (got {scope:?})");
        }
        let root = ctx.project_root.clone();
        let kind = args.kind.clone();
        let limit = args.limit;
        let rebuild = args.rebuild;

        let out = tokio::task::spawn_blocking(move || -> Result<String> {
            let want_memory = scope != "code";
            let want_code = scope != "memory";
            let mut docs: Vec<StoredDoc> = Vec::new();
            let mut any_source = false;

            if want_memory {
                let memory_docs = documents(&root)?;
                any_source |= !memory_docs.is_empty();
                let path = index_path(&root);
                let existing = if rebuild {
                    Store::default()
                } else {
                    load_store(&path)
                };
                let store = refresh(&memory_docs, &existing, &FastEmbedder)?;
                save_store(&path, &store)?;
                docs.extend(store.docs);
            }
            if want_code {
                let path = code_index_path(&root);
                let existing = if rebuild {
                    Store::default()
                } else {
                    load_store(&path)
                };
                let store = code_refresh(&root, &existing, &FastEmbedder)?;
                save_store(&path, &store)?;
                any_source |= !store.docs.is_empty();
                docs.extend(store.docs);
            }

            if !any_source {
                return Ok(match scope.as_str() {
                    "code" => "No source chunks to search yet.".to_string(),
                    "all" => "Nothing to search yet (no ADRs, glossary terms or source chunks)."
                        .to_string(),
                    _ => "No memory to search yet (no ADRs or glossary terms).".to_string(),
                });
            }

            let qvec = FastEmbedder
                .embed(std::slice::from_ref(&query))?
                .into_iter()
                .next()
                .context("no query embedding")?;
            let hits = rank(&docs, &qvec, limit, kind.as_deref());
            if hits.is_empty() {
                return Ok(format!("No results for {query:?} in scope {scope}."));
            }
            let has_code = hits.iter().any(|(_, d)| d.kind == "code");
            let has_memory = hits.iter().any(|(_, d)| d.kind != "code");
            let mut s = format!("{} result(s) for {query:?}:\n", hits.len());
            for (score, d) in hits {
                s.push_str(&format!(
                    "  {:.3}  {}  {}\n         {}\n",
                    score, d.id, d.title, d.preview
                ));
            }
            let mut hints = Vec::new();
            if has_memory {
                hints.push("read_adr/read_glossary to open a memory hit");
            }
            if has_code {
                hints.push(
                    "ts_read_symbol (by name) or fs_read_file at path:line to open a code hit",
                );
            }
            s.push_str(&format!("\nUse {}.", hints.join("; ")));
            Ok(s)
        })
        .await
        .context("semantic search task panicked")??;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deterministic embedder: a 2-D one-hot on whether the text mentions
    /// "alpha", so ranking is predictable without a model.
    #[derive(Default)]
    struct StubEmbedder {
        calls: AtomicUsize,
    }

    impl Embedder for StubEmbedder {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| {
                    if t.to_lowercase().contains("alpha") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    fn doc(id: &str, text: &str) -> Doc {
        Doc {
            id: id.into(),
            kind: "adr".into(),
            title: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn cosine_is_bounded() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn refresh_reuses_unchanged_docs_and_ranks_by_meaning() {
        let docs = vec![
            doc("adr:0001", "the alpha decision"),
            doc("adr:0002", "the beta decision"),
        ];
        let emb = StubEmbedder::default();
        let store = refresh(&docs, &Store::default(), &emb).unwrap();
        assert_eq!(store.docs.len(), 2);
        assert_eq!(emb.calls.load(Ordering::SeqCst), 1);

        // A second refresh with the same text embeds nothing new.
        let again = refresh(&docs, &store, &emb).unwrap();
        assert_eq!(
            emb.calls.load(Ordering::SeqCst),
            1,
            "unchanged docs re-embedded"
        );
        assert_eq!(again.docs.len(), 2);

        // Ranking prefers the doc whose meaning matches the query (a separate
        // embedder so the query embed does not skew the call count).
        let qemb = StubEmbedder::default();
        let q = qemb.embed(&["alpha".into()]).unwrap().remove(0);
        let hits = rank(&again.docs, &q, 5, None);
        assert_eq!(hits[0].1.id, "adr:0001");

        // Changing one doc only re-embeds that one (not the unchanged sibling).
        let mut docs2 = docs.clone();
        docs2[0].text = "the alpha decision, revised".into();
        let _ = refresh(&docs2, &store, &emb).unwrap();
        assert_eq!(emb.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rank_filters_by_kind() {
        let mut a = doc("term:X", "alpha glossary");
        a.kind = "glossary".into();
        let docs = vec![doc("adr:0001", "alpha adr"), a];
        let emb = StubEmbedder::default();
        let store = refresh(&docs, &Store::default(), &emb).unwrap();
        let q = emb.embed(&["alpha".into()]).unwrap().remove(0);
        let only_glossary = rank(&store.docs, &q, 5, Some("glossary"));
        assert_eq!(only_glossary.len(), 1);
        assert_eq!(only_glossary[0].1.kind, "glossary");
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-sem-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn code_refresh_indexes_chunks_with_file_and_line() {
        let dir = scratch("chunks");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "fn alpha_greeter() { println!(\"hi\") }\n\nfn beta_counter() {}\n",
        )
        .unwrap();
        let emb = StubEmbedder::default();
        let store = code_refresh(&dir, &Store::default(), &emb).unwrap();
        let greet = store
            .docs
            .iter()
            .find(|d| d.title.contains("alpha_greeter"))
            .expect("alpha_greeter chunk");
        assert_eq!(greet.kind, "code");
        assert!(
            greet.title.contains("src/lib.rs:"),
            "title must carry file:line: {}",
            greet.title
        );
        assert!(greet.id.starts_with("code:src/lib.rs:1:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_refresh_is_incremental_via_git() {
        let dir = scratch("incr-git");
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn beta() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);

        let emb = StubEmbedder::default();
        let s1 = code_refresh(&dir, &Store::default(), &emb).unwrap();
        assert!(emb.calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(s1.docs.len(), 2);

        // Nothing changed: no file is re-parsed or re-embedded.
        emb.calls.store(0, Ordering::SeqCst);
        let s2 = code_refresh(&dir, &s1, &emb).unwrap();
        assert_eq!(
            emb.calls.load(Ordering::SeqCst),
            0,
            "clean tree must not re-embed"
        );

        // Editing one file re-embeds only that file's chunk.
        std::fs::write(dir.join("a.rs"), "fn alpha() { let x = 1; }\n").unwrap();
        emb.calls.store(0, Ordering::SeqCst);
        let s3 = code_refresh(&dir, &s2, &emb).unwrap();
        assert_eq!(emb.calls.load(Ordering::SeqCst), 1, "only a.rs changed");
        let b2 = s2.docs.iter().find(|d| d.id.contains("b.rs")).unwrap();
        let b3 = s3.docs.iter().find(|d| d.id.contains("b.rs")).unwrap();
        assert_eq!(b2.vec, b3.vec, "unchanged file's vector must be reused");

        // A deleted file drops out of the index.
        std::fs::remove_file(dir.join("b.rs")).unwrap();
        let s4 = code_refresh(&dir, &s3, &emb).unwrap();
        assert!(!s4.files.contains_key("b.rs"));
        assert!(!s4.docs.iter().any(|d| d.id.contains("b.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_refresh_is_incremental_via_stat_without_git() {
        let dir = scratch("incr-stat");
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
        let emb = StubEmbedder::default();
        let s1 = code_refresh(&dir, &Store::default(), &emb).unwrap();
        assert!(emb.calls.load(Ordering::SeqCst) >= 1);

        // Same size and mtime: reused.
        emb.calls.store(0, Ordering::SeqCst);
        let _ = code_refresh(&dir, &s1, &emb).unwrap();
        assert_eq!(emb.calls.load(Ordering::SeqCst), 0);

        // Different size: re-embedded.
        std::fs::write(dir.join("a.rs"), "fn alpha() { let x = 1; }\n").unwrap();
        emb.calls.store(0, Ordering::SeqCst);
        let _ = code_refresh(&dir, &s1, &emb).unwrap();
        assert_eq!(emb.calls.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The embedded model ranks a code chunk by meaning and reports its location.
    #[test]
    fn real_model_ranks_code_by_meaning() {
        let dir = scratch("real-code");
        std::fs::write(
            dir.join("greet.rs"),
            "fn greet(name: &str) { println!(\"Hello, {name}!\"); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("math.rs"),
            "fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        let store = code_refresh(&dir, &Store::default(), &FastEmbedder).unwrap();
        let q = FastEmbedder
            .embed(&["a function that greets a person".into()])
            .unwrap()
            .remove(0);
        let hits = rank(&store.docs, &q, 2, Some("code"));
        assert!(
            hits[0].1.title.contains("greet.rs:"),
            "greeting code should rank first: {:?}",
            hits.iter().map(|h| &h.1.title).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sanity check of the real (embedded) model: related sentences must be
    /// closer than unrelated ones.
    #[test]
    fn real_model_embeds_and_ranks_by_meaning() {
        let emb = FastEmbedder;
        let v = emb.embed(&["hello world".into()]).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].len() >= 128, "unexpected dim {}", v[0].len());
        let cat = emb
            .embed(&["the cat sat on the mat".into()])
            .unwrap()
            .remove(0);
        let kitten = emb
            .embed(&["a small kitten rests on a rug".into()])
            .unwrap()
            .remove(0);
        let physics = emb
            .embed(&["the quantum chromodynamics lagrangian".into()])
            .unwrap()
            .remove(0);
        assert!(
            cosine(&cat, &kitten) > cosine(&cat, &physics),
            "semantic similarity did not beat the unrelated sentence"
        );
    }

    /// End-to-end check on REAL data: build the index from this repo's own
    /// `.comrade/memory` and confirm a natural-language query surfaces the
    /// matching ADR. Skips itself when there is no memory to read.
    #[test]
    fn embedded_model_ranks_this_repos_memory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let docs = match documents(&root) {
            Ok(d) => d,
            Err(_) => return, // no .comrade/memory in this checkout
        };
        if docs.iter().filter(|d| d.kind == "adr").count() < 5 {
            return; // not enough memory to be meaningful
        }
        let store = refresh(&docs, &Store::default(), &FastEmbedder).unwrap();
        assert_eq!(store.docs.len(), docs.len());
        let query = FastEmbedder
            .embed(&["finding a past decision by meaning rather than keywords".into()])
            .unwrap()
            .remove(0);
        let hits = rank(&store.docs, &query, 5, None);
        let ids: Vec<String> = hits.iter().map(|(_, d)| d.id.clone()).collect();
        assert!(!hits.is_empty(), "no hits");
        assert!(
            hits.iter()
                .any(|(_, d)| d.title.to_lowercase().contains("semantic")),
            "expected a semantic-search ADR among {ids:?}"
        );
    }
}
