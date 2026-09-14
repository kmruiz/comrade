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
//! extra native dependency or index-tuning. The index is loaded once and kept
//! resident per project root (so repeated searches neither re-read nor re-parse
//! it), and is written back only when its CONTENT actually changed — a dirty
//! working tree whose files re-parse to identical chunks is not a change, so an
//! editing agent no longer rewrites the whole cache on every search. It also
//! skips the file walk entirely when the repo is clean at the recorded HEAD and
//! only re-embeds documents whose text hash changed, so a steady-state call
//! costs one query embedding. Vectors are stored pre-normalised (L2), so ranking
//! is a plain dot product with no per-document norm or `sqrt`. (An ANN structure
//! remains a future option if a project ever grows large enough that the linear
//! scan matters.)
//!
//! The cache is a compact, self-describing binary blob (`CSMV` magic, raw
//! little-endian `f32` vectors) rather than JSON: raw f32 is ~4 bytes per
//! component where JSON's text floats cost ~7, which cut the code index for this
//! repo from ~11.6 MB to ~4.1 MB. A legacy `.json` cache is still read once and
//! re-saved in the binary form.
//!
//! The code index is incremental and git-driven: it records the HEAD it was
//! built at plus a per-file stamp, and `git::dirty_files` says what changed
//! (committed diff + working tree). Unchanged files are neither re-parsed nor
//! re-embedded; projects without a repo fall back to mtime/size stamps.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// A scored hit detached from the store (no vector), so a result list can be
/// built without holding a borrow on the resident index.
struct Hit {
    score: f32,
    id: String,
    kind: String,
    title: String,
    preview: String,
}

impl Hit {
    fn of(score: f32, d: &StoredDoc) -> Self {
        Self {
            score,
            id: d.id.clone(),
            kind: d.kind.clone(),
            title: d.title.clone(),
            preview: d.preview.clone(),
        }
    }
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

/// Squared L2 length of a vector.
fn norm2(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>()
}

/// Scale `v` to unit length in place (a no-op for a zero or non-finite vector),
/// so ranking against it becomes a plain dot product.
fn normalize(v: &mut [f32]) {
    let n = norm2(v).sqrt();
    if n > 0.0 && n.is_finite() {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// Normalise every vector in a store; applied on load so caches written before
/// vectors were stored pre-normalised still rank correctly.
fn normalize_store(store: &mut Store) {
    for doc in &mut store.docs {
        normalize(&mut doc.vec);
    }
}

/// Dot product: on unit vectors this is exactly the cosine similarity, which is
/// why the index stores normalised vectors (it removes a norm + two `sqrt` per
/// document from every query).
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Cosine similarity in [-1, 1]; 0 when either vector is zero. Kept for tests
/// that check raw model output; ranking uses [`dot`] on pre-normalised vectors.
#[cfg(test)]
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

/// Whether two stores hold the same documents, compared order-independently by
/// `(id, hash)`. The hash captures the embedded text, so equal signatures mean
/// equal vectors — this is how a re-parsed file that produced identical chunks
/// is recognised as "no change" instead of forcing a full cache rewrite.
fn same_docs(a: &[StoredDoc], b: &[StoredDoc]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut x: Vec<(&str, u64)> = a.iter().map(|d| (d.id.as_str(), d.hash)).collect();
    let mut y: Vec<(&str, u64)> = b.iter().map(|d| (d.id.as_str(), d.hash)).collect();
    x.sort_unstable();
    y.sort_unstable();
    x == y
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
/// Returns the new store and whether it differs from `existing` (so the caller
/// only writes the cache when something actually changed).
fn refresh(docs: &[Doc], existing: &Store, embedder: &dyn Embedder) -> Result<(Store, bool)> {
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
            let mut vec = vec;
            normalize(&mut vec);
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
    let changed = stale_model || !same_docs(&docs, &existing.docs);
    Ok((
        Store {
            model: MODEL_ID.into(),
            dim,
            docs,
            ..Store::default()
        },
        changed,
    ))
}

/// Rank documents by similarity to `query` (a dot product: the query and every
/// stored vector are unit-length), ties broken by id, optionally filtered by
/// `kind`.
fn rank<'a>(
    docs: &'a [StoredDoc],
    query: &[f32],
    limit: usize,
    kind: Option<&str>,
) -> Vec<(f32, &'a StoredDoc)> {
    let mut scored: Vec<(f32, &StoredDoc)> = docs
        .iter()
        .filter(|d| kind.is_none_or(|k| d.kind.eq_ignore_ascii_case(k)))
        .map(|d| (dot(query, &d.vec), d))
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
    cache_dir().join("semantic").join(format!("{key:016x}.bin"))
}

/// Where the code index for `root` is cached (separate from the memory index so
/// memory-only searches stay small and fast).
fn code_index_path(root: &Path) -> PathBuf {
    let key = fnv(&root.to_string_lossy());
    cache_dir()
        .join("semantic")
        .join(format!("{key:016x}-code.bin"))
}

/// Magic + version marking the compact binary index format (see [`encode_store`]).
const STORE_MAGIC: &[u8; 4] = b"CSMV";
const STORE_VERSION: u16 = 1;

/// Append a length-prefixed UTF-8 string to the buffer.
fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Serialise a store to a compact, dependency-free binary blob: length-prefixed
/// strings and vectors of raw little-endian `f32`, instead of the ~3.5x-bloated
/// text-f32 JSON that dominated both cache size and cold-start parse time.
fn encode_store(store: &Store) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(STORE_MAGIC);
    out.extend_from_slice(&STORE_VERSION.to_le_bytes());
    put_str(&mut out, &store.model);
    out.extend_from_slice(&(store.dim as u32).to_le_bytes());
    out.extend_from_slice(&(store.docs.len() as u32).to_le_bytes());
    for d in &store.docs {
        put_str(&mut out, &d.id);
        put_str(&mut out, &d.kind);
        put_str(&mut out, &d.title);
        put_str(&mut out, &d.preview);
        out.extend_from_slice(&d.hash.to_le_bytes());
        out.extend_from_slice(&(d.vec.len() as u32).to_le_bytes());
        for v in &d.vec {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(&(store.files.len() as u32).to_le_bytes());
    for (key, stamp) in &store.files {
        put_str(&mut out, key);
        out.extend_from_slice(&stamp.mtime.to_le_bytes());
        out.extend_from_slice(&stamp.size.to_le_bytes());
        out.extend_from_slice(&(stamp.doc_ids.len() as u32).to_le_bytes());
        for id in &stamp.doc_ids {
            put_str(&mut out, id);
        }
    }
    match &store.head {
        Some(h) => {
            out.push(1);
            put_str(&mut out, h);
        }
        None => out.push(0),
    }
    out
}

/// A bounds-checked little-endian reader over a byte slice.
struct Rd<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        Some(std::str::from_utf8(self.take(n)?).ok()?.to_string())
    }
}

/// Parse a blob produced by [`encode_store`], or `None` if it is not a valid
/// index of this version (the caller then falls back to a JSON cache or a
/// rebuild).
fn decode_store(bytes: &[u8]) -> Option<Store> {
    let mut r = Rd::new(bytes);
    if r.take(4)? != STORE_MAGIC {
        return None;
    }
    if u16::from_le_bytes(r.take(2)?.try_into().ok()?) != STORE_VERSION {
        return None;
    }
    let model = r.string()?;
    let dim = r.u32()? as usize;
    let ndocs = r.u32()? as usize;
    let mut docs = Vec::with_capacity(ndocs.min(1 << 20));
    for _ in 0..ndocs {
        let id = r.string()?;
        let kind = r.string()?;
        let title = r.string()?;
        let preview = r.string()?;
        let hash = r.u64()?;
        let n = r.u32()? as usize;
        let mut vec = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            vec.push(r.f32()?);
        }
        docs.push(StoredDoc {
            id,
            kind,
            title,
            preview,
            hash,
            vec,
        });
    }
    let nfiles = r.u32()? as usize;
    let mut files = BTreeMap::new();
    for _ in 0..nfiles {
        let key = r.string()?;
        let mtime = r.u64()?;
        let size = r.u64()?;
        let nids = r.u32()? as usize;
        let mut doc_ids = Vec::with_capacity(nids.min(1 << 20));
        for _ in 0..nids {
            doc_ids.push(r.string()?);
        }
        files.insert(
            key,
            FileStamp {
                mtime,
                size,
                doc_ids,
            },
        );
    }
    let head = match r.u8()? {
        0 => None,
        1 => Some(r.string()?),
        _ => return None,
    };
    Some(Store {
        model,
        dim,
        docs,
        files,
        head,
    })
}

/// Load the resident store from disk: the compact binary format first, then a
/// legacy JSON cache (so an existing index is migrated, not discarded), else an
/// empty store the caller will rebuild.
fn load_store(path: &Path) -> Store {
    let mut store = read_store(path);
    // Caches written before vectors were stored pre-normalised still rank
    // correctly once their vectors are made unit-length here.
    normalize_store(&mut store);
    store
}

fn read_store(path: &Path) -> Store {
    let current = std::fs::read(path).ok().and_then(|bytes| {
        if bytes.starts_with(STORE_MAGIC) {
            decode_store(&bytes)
        } else {
            serde_json::from_slice::<Store>(&bytes).ok()
        }
    });
    if let Some(store) = current {
        return store;
    }
    // Migrate a legacy `.json` cache written before the binary format.
    let legacy = std::fs::read_to_string(path.with_extension("json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Store>(&text).ok());
    if let Some(store) = legacy {
        return store;
    }
    Store::default()
}

/// The resident memory index per project root: loaded once and kept in memory,
/// so repeated searches never re-read or re-parse the store, and only a changed
/// index is written back.
static MEM_STORE: OnceLock<Mutex<HashMap<PathBuf, Store>>> = OnceLock::new();
/// The resident code index per project root (see [`MEM_STORE`]).
static CODE_STORE: OnceLock<Mutex<HashMap<PathBuf, Store>>> = OnceLock::new();

fn mem_store() -> &'static Mutex<HashMap<PathBuf, Store>> {
    MEM_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn code_store() -> &'static Mutex<HashMap<PathBuf, Store>> {
    CODE_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Project roots with a background warm-up in flight, so [`warm`] is idempotent.
static WARMING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn warming() -> &'static Mutex<HashSet<PathBuf>> {
    WARMING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn save_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let bytes = encode_store(store);
    std::fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Rebuild the code index against `existing`, re-parsing and re-embedding only
/// what changed. Change detection is git-driven when the project is a repo (the
/// recorded HEAD + `git::dirty_files`), falling back to per-file mtime/size;
/// unchanged files are reused outright, and within a changed file a chunk whose
/// text is unchanged keeps its vector.
fn code_refresh(root: &Path, existing: &Store, embedder: &dyn Embedder) -> Result<(Store, bool)> {
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
            let mut vec = vec;
            normalize(&mut vec);
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
    // Changed only when the resulting content actually differs from the cached
    // index: the document set, the per-file stamps or the recorded HEAD. A file
    // re-parsed to identical chunks (the common dirty-tree case) is not a change,
    // so a search never rewrites the whole cache just because the tree is dirty.
    let head = crate::git::head_sha(root);
    let changed = stale_model
        || !same_docs(&docs, &existing.docs)
        || files != existing.files
        || head != existing.head;
    Ok((
        Store {
            model: MODEL_ID.into(),
            dim,
            docs,
            files,
            head,
        },
        changed,
    ))
}

/// Force a full rebuild of both the memory and code indexes, discarding the
/// caches. Used by the `reindex-semantic-search` command.
pub fn reindex(root: &Path) -> Result<String> {
    let (mem, _) = refresh(&documents(root)?, &Store::default(), &FastEmbedder)?;
    save_store(&index_path(root), &mem)?;
    let (code, _) = code_refresh(root, &Store::default(), &FastEmbedder)?;
    save_store(&code_index_path(root), &code)?;
    // Drop any resident copies so the next search reloads the fresh indexes.
    mem_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    code_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    Ok(format!(
        "Reindexed semantic search: {} memory doc(s), {} code chunk(s).",
        mem.docs.len(),
        code.docs.len()
    ))
}

/// Warm the semantic index in the BACKGROUND so the first `semantic_search` is
/// instant: build the memory and code indexes (incrementally, reusing whatever
/// is already cached), save each and install it into the resident store, then
/// return. The work runs on a detached thread, so this never blocks the caller
/// (the TUI at startup, or the `warm_semantic_index` tool). Idempotent: while a
/// warm-up for the same root is still running, a second call is a no-op.
pub fn warm(root: &Path) -> String {
    warm_with(root, Arc::new(FastEmbedder))
}

/// [`warm`] with an explicit embedder (the seam tests use).
fn warm_with(root: &Path, embedder: Arc<dyn Embedder>) -> String {
    {
        let mut set = warming().lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(root.to_path_buf()) {
            return "Semantic index warm-up is already running in the background.".to_string();
        }
    }
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        build_indexes(&root, embedder.as_ref());
        warming()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&root);
    });
    "Warming the semantic index in the background (memory + code chunks). The next semantic_search will be fast once it finishes.".to_string()
}

/// Build the index synchronously and return a one-line report. Used by the
/// `--warm-index` CLI, where the process is short-lived and wants to block until
/// the index exists (and to be a no-op release-fast when it is already warm).
pub fn warm_blocking(root: &Path) -> String {
    let (memory, code) = build_indexes(root, &FastEmbedder);
    format!("Semantic index ready: {memory} memory doc(s), {code} code chunk(s).")
}

/// Build the memory and code indexes for `root` incrementally (reusing whatever
/// is cached), save each and install it into the resident maps. Each index is
/// best-effort and independent: a failure in one is logged and does not stop the
/// other (the code index is the slow, valuable one). Returns `(memory docs, code
/// docs)`; a failed index reports `0`.
fn build_indexes(root: &Path, embedder: &dyn Embedder) -> (usize, usize) {
    let memory = match documents(root)
        .and_then(|docs| refresh(&docs, &load_store(&index_path(root)), embedder))
    {
        Ok((mem, changed)) => {
            if changed && let Err(e) = save_store(&index_path(root), &mem) {
                eprintln!("[comrade] cannot save the memory index: {e:#}");
            }
            let n = mem.docs.len();
            // Install under a brief lock (never hold one across the build).
            mem_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(root.to_path_buf(), mem);
            n
        }
        Err(e) => {
            eprintln!("[comrade] memory index warm-up failed: {e:#}");
            0
        }
    };

    let code = match code_refresh(root, &load_store(&code_index_path(root)), embedder) {
        Ok((code, changed)) => {
            if changed && let Err(e) = save_store(&code_index_path(root), &code) {
                eprintln!("[comrade] cannot save the code index: {e:#}");
            }
            let n = code.docs.len();
            code_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(root.to_path_buf(), code);
            n
        }
        Err(e) => {
            eprintln!("[comrade] code index warm-up failed: {e:#}");
            0
        }
    };

    (memory, code)
}

/// The in-process model, built from the embedded assets on first use.
static MODEL: OnceLock<Result<Mutex<TextEmbedding>, String>> = OnceLock::new();

/// Batch size for a single ONNX `run`.
const EMBED_BATCH: usize = 16;

/// Inflate one of the assets `build.rs` deflated into `OUT_DIR`.
fn inflate(name: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .with_context(|| format!("cannot inflate embedded asset {name}"))?;
    Ok(out)
}

/// Build one in-process embedding session from the embedded assets. `intra_threads`
/// is handed to ONNX Runtime (`None` = one thread per available core).
fn build_embedding(intra_threads: Option<usize>) -> Result<TextEmbedding, String> {
    let onnx = inflate("model_quantized.onnx", MODEL_ONNX).map_err(|e| e.to_string())?;
    let tokenizer = TokenizerFiles {
        tokenizer_file: inflate("tokenizer.json", MODEL_TOKENIZER).map_err(|e| e.to_string())?,
        config_file: inflate("config.json", MODEL_CONFIG).map_err(|e| e.to_string())?,
        special_tokens_map_file: inflate("special_tokens_map.json", MODEL_SPECIAL_TOKENS)
            .map_err(|e| e.to_string())?,
        tokenizer_config_file: inflate("tokenizer_config.json", MODEL_TOKENIZER_CONFIG)
            .map_err(|e| e.to_string())?,
    };
    // BGE-small uses CLS pooling (matches fastembed's own model config).
    let model = UserDefinedEmbeddingModel::new(onnx, tokenizer).with_pooling(Pooling::Cls);
    let mut opts = InitOptionsUserDefined::new();
    if let Some(n) = intra_threads {
        opts = opts.with_intra_threads(n);
    }
    TextEmbedding::try_new_from_user_defined(model, opts).map_err(|e| e.to_string())
}

/// The real embedder: the embedded int8 model run in-process via ONNX Runtime.
struct FastEmbedder;

/// Embed `texts` on the single resident session, batching similar lengths
/// together. ONNX pads every sequence in a batch to the longest one, so feeding
/// chunks in ascending length order and restoring the caller's order afterwards
/// removes most of the wasted compute: for this repo's very uneven chunk lengths
/// the padding was ~3x the real content, i.e. two thirds of a cold build's work.
///
/// A single session with one thread per core already saturates the CPU here —
/// measured, running several single-threaded sessions in parallel was *slower*
/// (and a larger batch was slower too, for the same padding reason).
fn embed_ordered(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let cell = MODEL.get_or_init(|| build_embedding(None).map(Mutex::new));
    let model = cell
        .as_ref()
        .map_err(|e| anyhow::anyhow!("embedding model unavailable: {e}"))?;
    let mut guard = model.lock().unwrap_or_else(|e| e.into_inner());

    // Short inputs already fit one batch; order cannot matter.
    if texts.len() < EMBED_BATCH {
        return guard.embed(texts, Some(EMBED_BATCH));
    }
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| texts[i].len());
    let sorted: Vec<&str> = order.iter().map(|&i| texts[i].as_str()).collect();
    let vectors = guard.embed(sorted.as_slice(), Some(EMBED_BATCH))?;
    if vectors.len() != texts.len() {
        anyhow::bail!(
            "embedder returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        );
    }
    let mut out: Vec<Option<Vec<f32>>> = (0..texts.len()).map(|_| None).collect();
    for (slot, v) in order.into_iter().zip(vectors) {
        out[slot] = Some(v);
    }
    out.into_iter()
        .map(|o| o.context("embedder returned fewer vectors than texts"))
        .collect()
}

impl Embedder for FastEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        embed_ordered(texts)
    }
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![Box::new(SemanticSearch), Box::new(WarmSemanticIndex)]
}

struct SemanticSearch;

static SEMANTIC_SEARCH_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
        name: "semantic_search".into(),
        description: "Search project memory (ADR decisions + glossary) AND source code by MEANING, not keywords: returns the closest hits with a similarity score. Code hits are tree-sitter symbols/line windows carrying file:line. Reach for it EARLY to locate things when you know the domain but not yet the exact symbol, file or string - it is the first choice whenever you know WHAT you want but not WHERE it lives, NOT a fallback; find_adr/ts_find_symbol/fs_rgrep only help once you already hold a name. Searches memory and code by default; pass `scope` to narrow to memory or code. Locally embedded model + vector index; no network for search; the index is rebuilt incrementally and only changed files are re-parsed.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What you are looking for, in natural language." },
                "scope": { "type": "string", "enum": ["memory", "code", "all"], "default": "all", "description": "Search project memory, source code, or both (default: both)." },
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
        let scope = args.scope.unwrap_or_else(|| "all".to_string());
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
            let mut any_source = false;

            // Operate on the resident indexes: load once, reuse across calls,
            // and only write the cache back when something actually changed.
            let mut mem_guard = mem_store().lock().unwrap_or_else(|e| e.into_inner());
            let mut code_guard = code_store().lock().unwrap_or_else(|e| e.into_inner());

            if want_memory {
                let memory_docs = documents(&root)?;
                any_source |= !memory_docs.is_empty();
                let path = index_path(&root);
                let entry = mem_guard
                    .entry(root.clone())
                    .or_insert_with(|| load_store(&path));
                if rebuild {
                    *entry = Store::default();
                }
                let (store, changed) = refresh(&memory_docs, entry, &FastEmbedder)?;
                if rebuild || changed {
                    save_store(&path, &store)?;
                }
                *entry = store;
            }
            if want_code {
                let path = code_index_path(&root);
                let entry = code_guard
                    .entry(root.clone())
                    .or_insert_with(|| load_store(&path));
                if rebuild {
                    *entry = Store::default();
                }
                // Fast path: a clean repo whose HEAD still matches the index
                // needs no file walk and no rewrite.
                let clean = !rebuild
                    && entry.model == MODEL_ID
                    && !entry.docs.is_empty()
                    && entry.head.is_some()
                    && entry.head == crate::git::head_sha(&root)
                    && matches!(
                        crate::git::dirty_files(&root, entry.head.as_deref()),
                        Some(files) if files.is_empty()
                    );
                if !clean {
                    let (store, changed) = code_refresh(&root, entry, &FastEmbedder)?;
                    if rebuild || changed {
                        save_store(&path, &store)?;
                    }
                    *entry = store;
                }
                any_source |= !entry.docs.is_empty();
            }

            if !any_source {
                return Ok(match scope.as_str() {
                    "code" => "No source chunks to search yet.".to_string(),
                    "all" => "Nothing to search yet (no ADRs, glossary terms or source chunks)."
                        .to_string(),
                    _ => "No memory to search yet (no ADRs or glossary terms).".to_string(),
                });
            }

            let mut qvec = FastEmbedder
                .embed(std::slice::from_ref(&query))?
                .into_iter()
                .next()
                .context("no query embedding")?;
            normalize(&mut qvec);

            // Rank each resident index, then merge — no vectors are cloned.
            let mut hits: Vec<Hit> = Vec::new();
            if want_memory {
                for (score, d) in rank(&mem_guard[&root].docs, &qvec, limit, kind.as_deref()) {
                    hits.push(Hit::of(score, d));
                }
            }
            if want_code {
                for (score, d) in rank(&code_guard[&root].docs, &qvec, limit, kind.as_deref()) {
                    hits.push(Hit::of(score, d));
                }
            }
            hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.id.cmp(&b.id))
            });
            hits.truncate(limit.max(1));
            if hits.is_empty() {
                return Ok(format!("No results for {query:?} in scope {scope}."));
            }
            let has_code = hits.iter().any(|h| h.kind == "code");
            let has_memory = hits.iter().any(|h| h.kind != "code");
            let mut s = format!("{} result(s) for {query:?}:\n", hits.len());
            for h in &hits {
                s.push_str(&format!(
                    "  {:.3}  {}  {}\n         {}\n",
                    h.score, h.id, h.title, h.preview
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

/// The tool the agent calls to kick off a background index warm-up.
struct WarmSemanticIndex;

static WARM_SEMANTIC_INDEX_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "warm_semantic_index".into(),
    description: "Start building the semantic-search index (project memory + code chunks) in the BACKGROUND and return immediately. Building the index in a fresh checkout is CPU-heavy and takes a minute or two, so the app also does this automatically at startup; call this after a large change or when semantic_search feels slow, then carry on - it never blocks. Idempotent: a call while a warm-up is already running is a no-op.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for WarmSemanticIndex {
    fn spec(&self) -> &ToolSpec {
        &WARM_SEMANTIC_INDEX_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        Ok(warm(&ctx.project_root))
    }
}

#[cfg(test)]
mod tests;
