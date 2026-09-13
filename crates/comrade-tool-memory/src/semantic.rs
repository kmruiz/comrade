//! Embedded semantic search over project memory (ADRs + glossary).
//!
//! `semantic_search` finds memory by MEANING, not keywords: it embeds every
//! decision and glossary term with a small, locally-run model and ranks them by
//! cosine similarity to the query. Both halves are embedded — there is no
//! server: the model is a quantized ONNX bundle run in-process by `fastembed`
//! (downloaded once to the user cache), and the vector store is a persisted
//! flat index under the user cache dir keyed by the project root.
//!
//! Why a flat index and not HNSW: a project's memory is tens to a few hundred
//! documents, where exact cosine over all vectors is instantaneous and needs no
//! extra native dependency or index-tuning. The index only re-embeds documents
//! whose text hash changed, so steady-state calls cost one query embedding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Small, quantized retrieval model (~30 MB): strong quality-per-size for
/// short documents and English text, and fast enough to embed a whole memory
/// on the first call.
const EMBED_MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15Q;

/// Human-readable model id stored with the index so a model change invalidates
/// cached vectors.
const MODEL_ID: &str = "bge-small-en-v1.5-q";

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

/// The persisted index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Store {
    model: String,
    dim: usize,
    docs: Vec<StoredDoc>,
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

/// Rebuild `existing` against `docs`, re-embedding only new/changed documents.
fn refresh(docs: &[Doc], existing: &Store, embedder: &dyn Embedder) -> Result<Store> {
    let stale_model = existing.model != MODEL_ID;
    let old: HashMap<&str, &StoredDoc> = existing
        .docs
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();

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
    })
}

/// Rank stored documents by cosine similarity to `query`, ties broken by id,
/// optionally filtered by `kind`.
fn rank<'a>(
    store: &'a Store,
    query: &[f32],
    limit: usize,
    kind: Option<&str>,
) -> Vec<(f32, &'a StoredDoc)> {
    let mut scored: Vec<(f32, &StoredDoc)> = store
        .docs
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

/// Where the index for `root` is cached: the user cache dir, keyed by the
/// project path, so the repo stays clean.
fn index_path(root: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let key = fnv(&root.to_string_lossy());
    base.join("comrade")
        .join("semantic")
        .join(format!("{key:016x}.json"))
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

/// The in-process model, loaded (and downloaded, once) on first use.
static MODEL: OnceLock<Result<Mutex<TextEmbedding>, String>> = OnceLock::new();

/// The real embedder: a lazily-loaded local ONNX model.
struct FastEmbedder;

impl Embedder for FastEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let cell = MODEL.get_or_init(|| {
            TextEmbedding::try_new(
                InitOptions::new(EMBED_MODEL).with_show_download_progress(false),
            )
            .map(Mutex::new)
            .map_err(|e| e.to_string())
        });
        let model = cell
            .as_ref()
            .map_err(|e| anyhow::anyhow!("embedding model unavailable: {e}"))?;
        let mut guard = model.lock().unwrap_or_else(|e| e.into_inner());
        let owned: Vec<String> = texts.to_vec();
        guard.embed(owned, Some(16))
    }
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![Box::new(SemanticSearch)]
}

struct SemanticSearch;

static SEMANTIC_SEARCH_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
        name: "semantic_search".into(),
        description: "Search project memory (ADR decisions + glossary) by MEANING, not keywords: returns the closest entries with a similarity score. Use when you are unsure of the exact words used (find_adr/find_glossary are keyword search). Locally embedded model + vector index; no network for search.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What you are looking for, in natural language." },
                "kind": { "type": "string", "enum": ["adr", "glossary"], "description": "Restrict to one memory kind (default: both)." },
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
        let root = ctx.project_root.clone();
        let kind = args.kind.clone();
        let limit = args.limit;
        let rebuild = args.rebuild;

        let out = tokio::task::spawn_blocking(move || -> Result<String> {
            let docs = documents(&root)?;
            if docs.is_empty() {
                return Ok("No memory to search yet (no ADRs or glossary terms).".to_string());
            }
            let path = index_path(&root);
            let existing = if rebuild {
                Store::default()
            } else {
                load_store(&path)
            };
            let store = refresh(&docs, &existing, &FastEmbedder)?;
            save_store(&path, &store)?;
            let qvec = FastEmbedder
                .embed(std::slice::from_ref(&query))?
                .into_iter()
                .next()
                .context("no query embedding")?;
            let hits = rank(&store, &qvec, limit, kind.as_deref());
            if hits.is_empty() {
                return Ok(format!("No memory matches {query:?}."));
            }
            let mut s = format!("{} result(s) for {query:?}:\n", hits.len());
            for (score, d) in hits {
                s.push_str(&format!(
                    "  {:.3}  {}  {}\n         {}\n",
                    score, d.id, d.title, d.preview
                ));
            }
            s.push_str("\nUse read_adr/read_glossary to open a hit.");
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
        assert_eq!(emb.calls.load(Ordering::SeqCst), 1, "unchanged docs re-embedded");
        assert_eq!(again.docs.len(), 2);

        // Ranking prefers the doc whose meaning matches the query (a separate
        // embedder so the query embed does not skew the call count).
        let qemb = StubEmbedder::default();
        let q = qemb.embed(&["alpha".into()]).unwrap().remove(0);
        let hits = rank(&again, &q, 5, None);
        assert_eq!(hits[0].1.id, "adr:0001");

        // Changing one doc only re-embeds that one (not the unchanged sibling).
        let mut docs2 = docs.clone();
        docs2[0].text = "the alpha decision, revised".into();
        let _ = refresh(&docs2, &store, &emb).unwrap();
        assert_eq!(emb.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rank_filters_by_kind() {        let mut a = doc("term:X", "alpha glossary");
        a.kind = "glossary".into();
        let docs = vec![doc("adr:0001", "alpha adr"), a];
        let emb = StubEmbedder::default();
        let store = refresh(&docs, &Store::default(), &emb).unwrap();
        let q = emb.embed(&["alpha".into()]).unwrap().remove(0);
        let only_glossary = rank(&store, &q, 5, Some("glossary"));
        assert_eq!(only_glossary.len(), 1);
        assert_eq!(only_glossary[0].1.kind, "glossary");
    }

    /// End-to-end check with the real model: downloads ~30 MB on first run, so
    /// it is ignored by default. Run with `cargo test -p comrade-tool-memory -- --ignored`.
    #[test]
    #[ignore = "downloads the embedding model; run with --ignored"]
    fn real_model_embeds_and_ranks_by_meaning() {
        let emb = FastEmbedder;
        let v = emb.embed(&["hello world".into()]).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].len() >= 128, "unexpected dim {}", v[0].len());
        let cat = emb.embed(&["the cat sat on the mat".into()]).unwrap().remove(0);
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
}
