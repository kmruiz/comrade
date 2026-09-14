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
    let (store, changed) = refresh(&docs, &Store::default(), &emb).unwrap();
    assert!(changed, "a fresh index changed");
    assert_eq!(store.docs.len(), 2);
    assert_eq!(emb.calls.load(Ordering::SeqCst), 1);

    // A second refresh with the same text embeds nothing new and reports no
    // change (so no cache write is needed).
    let (again, changed) = refresh(&docs, &store, &emb).unwrap();
    assert!(!changed, "an unchanged refresh must report no change");
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
    let hits = rank(&again.docs, &q, 5, None, None);
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
    let (store, _) = refresh(&docs, &Store::default(), &emb).unwrap();
    let q = emb.embed(&["alpha".into()]).unwrap().remove(0);
    let only_glossary = rank(&store.docs, &q, 5, Some("glossary"), None);
    assert_eq!(only_glossary.len(), 1);
    assert_eq!(only_glossary[0].1.kind, "glossary");
}

#[test]
fn path_matches_file_and_directory() {
    let id = "code:crates/comrade-tool-memory/src/semantic/mod.rs:199:cosine";
    assert!(path_matches(id, "crates/comrade-tool-memory"));
    assert!(path_matches(
        id,
        "crates/comrade-tool-memory/src/semantic/mod.rs"
    ));
    assert!(path_matches(id, "mod.rs"));
    assert!(path_matches(id, "./crates/comrade-tool-memory/"));
    assert!(!path_matches(id, "comrade-core"));
    assert!(!path_matches(id, "src/main.rs"));
    // Memory documents have no file and never match a path filter.
    assert!(!path_matches("adr:0001", "crates"));
    assert!(!path_matches("term:ToolSpec", "crates"));
}

#[test]
fn rank_filters_by_path() {
    let mut a = doc("code:src/lib.rs:1:alpha_fn", "alpha thing");
    a.kind = "code".into();
    let mut b = doc("code:other/main.rs:2:beta_fn", "alpha thing");
    b.kind = "code".into();
    let docs = vec![a, b];
    let emb = StubEmbedder::default();
    let (store, _) = refresh(&docs, &Store::default(), &emb).unwrap();
    let q = emb.embed(&["alpha".into()]).unwrap().remove(0);

    let hits = rank(&store.docs, &q, 5, None, Some("src"));
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.id, "code:src/lib.rs:1:alpha_fn");
    assert_eq!(rank(&store.docs, &q, 5, None, Some("other")).len(), 1);
    assert_eq!(rank(&store.docs, &q, 5, None, Some("src")).len(), 1);
    // No filter returns both.
    assert_eq!(rank(&store.docs, &q, 5, None, None).len(), 2);
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
    let (store, _) = code_refresh(&dir, &Store::default(), &emb).unwrap();
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
    let (s1, _) = code_refresh(&dir, &Store::default(), &emb).unwrap();
    assert!(emb.calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(s1.docs.len(), 2);

    // Nothing changed: no file is re-parsed or re-embedded.
    emb.calls.store(0, Ordering::SeqCst);
    let (s2, changed2) = code_refresh(&dir, &s1, &emb).unwrap();
    assert!(!changed2, "a clean tree reports no change");
    assert_eq!(
        emb.calls.load(Ordering::SeqCst),
        0,
        "clean tree must not re-embed"
    );

    // Editing one file re-embeds only that file's chunk.
    std::fs::write(dir.join("a.rs"), "fn alpha() { let x = 1; }\n").unwrap();
    emb.calls.store(0, Ordering::SeqCst);
    let (s3, _) = code_refresh(&dir, &s2, &emb).unwrap();
    assert_eq!(emb.calls.load(Ordering::SeqCst), 1, "only a.rs changed");
    let b2 = s2.docs.iter().find(|d| d.id.contains("b.rs")).unwrap();
    let b3 = s3.docs.iter().find(|d| d.id.contains("b.rs")).unwrap();
    assert_eq!(b2.vec, b3.vec, "unchanged file's vector must be reused");

    // A deleted file drops out of the index.
    std::fs::remove_file(dir.join("b.rs")).unwrap();
    let (s4, _) = code_refresh(&dir, &s3, &emb).unwrap();
    assert!(!s4.files.contains_key("b.rs"));
    assert!(!s4.docs.iter().any(|d| d.id.contains("b.rs")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn code_refresh_is_incremental_via_stat_without_git() {
    let dir = scratch("incr-stat");
    std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
    let emb = StubEmbedder::default();
    let (s1, _) = code_refresh(&dir, &Store::default(), &emb).unwrap();
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
    let (store, _) = code_refresh(&dir, &Store::default(), &FastEmbedder).unwrap();
    let q = FastEmbedder
        .embed(&["a function that greets a person".into()])
        .unwrap()
        .remove(0);
    let hits = rank(&store.docs, &q, 2, Some("code"), None);
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
    let (store, _) = refresh(&docs, &Store::default(), &FastEmbedder).unwrap();
    assert_eq!(store.docs.len(), docs.len());
    let query = FastEmbedder
        .embed(&["finding a past decision by meaning rather than keywords".into()])
        .unwrap()
        .remove(0);
    let hits = rank(&store.docs, &query, 5, None, None);
    let ids: Vec<String> = hits.iter().map(|(_, d)| d.id.clone()).collect();
    assert!(!hits.is_empty(), "no hits");
    assert!(
        hits.iter()
            .any(|(_, d)| d.title.to_lowercase().contains("semantic")),
        "expected a semantic-search ADR among {ids:?}"
    );
}

fn stored_doc(vec: Vec<f32>) -> StoredDoc {
    StoredDoc {
        id: "code:src/lib.rs:1:f".into(),
        kind: "code".into(),
        title: "src/lib.rs:1  fn f".into(),
        preview: "fn f() {}".into(),
        hash: 0xdead_beef,
        vec,
    }
}

#[test]
fn binary_store_round_trips() {
    let mut files = BTreeMap::new();
    files.insert(
        "src/lib.rs".to_string(),
        FileStamp {
            mtime: 42,
            size: 7,
            doc_ids: vec!["code:src/lib.rs:1:f".into()],
        },
    );
    let store = Store {
        model: MODEL_ID.into(),
        dim: 3,
        docs: vec![
            stored_doc(vec![0.1, -0.25, 0.75]),
            StoredDoc {
                id: "adr:0001".into(),
                kind: "adr".into(),
                title: "#0001 [accepted] x".into(),
                preview: "p".into(),
                hash: 1,
                vec: vec![1.0, 0.0, 0.0],
            },
        ],
        files: files.clone(),
        head: Some("abc123".into()),
    };
    let bytes = encode_store(&store);
    let back = decode_store(&bytes).expect("round-trip must succeed");
    assert_eq!(back.model, store.model);
    assert_eq!(back.dim, 3);
    assert_eq!(back.head.as_deref(), Some("abc123"));
    assert_eq!(back.files, files);
    assert_eq!(back.docs.len(), 2);
    for (a, b) in back.docs.iter().zip(&store.docs) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.title, b.title);
        assert_eq!(a.preview, b.preview);
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.vec, b.vec);
    }
    // Raw f32 must be compact: ~4 bytes per float, not JSON's ~7 text bytes.
    let vec_bytes = store.docs.iter().map(|d| d.vec.len()).sum::<usize>() * 4;
    assert!(
        bytes.len() < vec_bytes + 512,
        "binary store is too large: {} bytes for {vec_bytes} vector bytes",
        bytes.len()
    );
}

#[test]
fn binary_store_rejects_garbage_and_round_trips_empty() {
    assert!(decode_store(b"").is_none());
    assert!(decode_store(b"not a store").is_none());
    // Right magic, wrong version.
    assert!(decode_store(b"CSMV\x02\x00").is_none());
    // Truncated payload.
    assert!(decode_store(b"CSMV\x01\x00").is_none());

    let empty = Store::default();
    let back = decode_store(&encode_store(&empty)).expect("empty store round-trips");
    assert!(back.docs.is_empty());
    assert!(back.files.is_empty());
    assert_eq!(back.head, None);
}

#[test]
fn load_store_normalises_vectors_and_reads_legacy_json() {
    let dir = scratch("load-norm");
    let path = dir.join("idx.bin");
    let legacy = Store {
        model: MODEL_ID.into(),
        dim: 2,
        docs: vec![stored_doc(vec![3.0, 4.0])], // deliberately not unit length
        ..Store::default()
    };
    // A pre-binary cache used the `.json` extension next to the binary path.
    std::fs::write(dir.join("idx.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();
    let loaded = load_store(&path);
    assert_eq!(loaded.docs.len(), 1, "legacy JSON cache must still load");
    let n = norm2(&loaded.docs[0].vec).sqrt();
    assert!((n - 1.0).abs() < 1e-6, "legacy vector not normalised: {n}");

    // The binary path normalises on load too.
    save_store(&path, &legacy).unwrap();
    let loaded = load_store(&path);
    let v = &loaded.docs[0].vec;
    assert!(
        (v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6,
        "{v:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refresh_stores_unit_vectors() {
    let docs = vec![doc("adr:0001", "alpha"), doc("adr:0002", "beta")];
    let emb = StubEmbedder::default();
    let (store, _) = refresh(&docs, &Store::default(), &emb).unwrap();
    for d in &store.docs {
        let n = norm2(&d.vec).sqrt();
        assert!((n - 1.0).abs() < 1e-6, "{} not unit length: {n}", d.id);
    }
}

/// A file that is dirty but re-parses to identical chunks must NOT force a cache
/// rewrite (the normal state while an agent edits): this was the main remaining
/// per-search cost — rewriting the whole index on every dirty-tree search.
#[test]
fn reparse_of_an_unchanged_dirty_file_reports_no_change() {
    let dir = scratch("dirty-reparse");
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Dirty the tree AFTER the commit, then index that dirty state.
    std::fs::write(dir.join("a.rs"), "fn alpha() { let x = 1; }\n").unwrap();
    let emb = StubEmbedder::default();
    let (s1, changed1) = code_refresh(&dir, &Store::default(), &emb).unwrap();
    assert!(changed1, "the first index of a dirty tree is a change");

    // Re-run on the still-dirty tree: a.rs is re-parsed, but its chunks hash the
    // same, so nothing is re-embedded and the index is not rewritten.
    emb.calls.store(0, Ordering::SeqCst);
    let (s2, changed2) = code_refresh(&dir, &s1, &emb).unwrap();
    assert!(
        !changed2,
        "a re-parsed file with identical chunks must not force a rewrite"
    );
    assert_eq!(emb.calls.load(Ordering::SeqCst), 0, "no chunk re-embedded");
    assert!(same_docs(&s1.docs, &s2.docs));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Length-sorted batching must not scramble the caller's order: embedding a
/// mixed-length batch must equal embedding each text on its own.
#[test]
fn real_model_preserves_order_across_batches() {
    assert!(
        FastEmbedder.embed(&["warm up".into()]).is_ok(),
        "model unavailable"
    );
    let texts: Vec<String> = (0..40)
        .map(|i| {
            let filler = "// filler about decoding buffers, headers and retrying\n".repeat(i % 7);
            format!("fn f_{i}() {{\n{filler}}}\n")
        })
        .collect();
    let batched = FastEmbedder.embed(&texts).unwrap();
    assert_eq!(batched.len(), texts.len());
    let alone: Vec<Vec<f32>> = texts
        .iter()
        .map(|t| {
            FastEmbedder
                .embed(std::slice::from_ref(t))
                .unwrap()
                .remove(0)
        })
        .collect();
    for (i, b) in batched.iter().enumerate() {
        let mut scored: Vec<(usize, f32)> = alone
            .iter()
            .enumerate()
            .map(|(j, a)| (j, cosine(b, a)))
            .collect();
        scored.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());
        let (best, top) = scored[0];
        let second = scored.get(1).map(|s| s.1).unwrap_or(0.0);
        assert_eq!(
            best, i,
            "batched[{i}] best-matches slot {best} (cos {top:.4}); order was scrambled"
        );
        assert!(
            top - second > 0.02,
            "batched[{i}] is ambiguous: {top:.4} vs {second:.4}"
        );
    }
}

/// The background warm-up builds + installs the index so a later search is a
/// no-op (the resident store already matches the tree).
#[test]
fn warm_installs_the_code_index_in_the_background() {
    let dir = scratch("warm");
    std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn beta() {}\n").unwrap();

    warm_with(&dir, std::sync::Arc::new(StubEmbedder::default()));

    // The build runs on a detached thread; wait for it to install and finish.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let installed = code_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&dir)
            .cloned();
        let done = !warming()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&dir);
        if let Some(store) = installed {
            assert_eq!(store.docs.len(), 2, "both chunks indexed");
            if done {
                // The installed store is the real, current index: rebuilding from
                // it reports no change and re-embeds nothing.
                let emb = StubEmbedder::default();
                let (_, changed) = code_refresh(&dir, &store, &emb).unwrap();
                assert!(!changed, "a warmed index must match the tree");
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "warm-up did not finish in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Clean the process-global statics so other tests are unaffected.
    code_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&dir);
    mem_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Timing benchmark (NOT a correctness test). Run explicitly with:
/// `cargo test -p comrade-tool-memory --release -- --ignored --nocapture bench_semantic`
///
/// It measures, on this repo's real memory+code index, the costs that make up a
/// `semantic_search` call: loading the store from disk (per-process cold start),
/// initialising the embedded model (first embed), a code-index refresh (tree
/// walk + parse + embed), the memory index build, and steady-state query
/// latency (embed + dot scan) repeated a few times.
#[test]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
fn bench_semantic() {
    use std::time::Instant;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    // 1. Per-process cold start: every fresh search process loads the store once.
    let t = Instant::now();
    let cached = load_store(&code_index_path(&root));
    eprintln!(
        "load code store from disk  : {:>9.2?} ({} docs)",
        t.elapsed(),
        cached.docs.len()
    );

    // 2. Model init: inflate assets + build the ONNX session, on the first embed.
    let t = Instant::now();
    let _ = FastEmbedder
        .embed(&["warm up the model".to_string()])
        .unwrap();
    eprintln!("model init (first embed)   : {:>9.2?}", t.elapsed());

    // 3. Code-index rebuild from scratch: walk + parse the tree and embed every
    //    chunk (the cold path that makes the first search slow).
    let t = Instant::now();
    let (cstore, _) = code_refresh(&root, &Store::default(), &FastEmbedder).unwrap();
    eprintln!(
        "code rebuild (cold)        : {:>9.2?} ({} docs)",
        t.elapsed(),
        cstore.docs.len()
    );

    // 4. Memory-index build (ADRs + glossary).
    let docs = documents(&root).unwrap();
    let t = Instant::now();
    let (mstore, _) = refresh(&docs, &Store::default(), &FastEmbedder).unwrap();
    eprintln!(
        "memory index build         : {:>9.2?} ({} docs)",
        t.elapsed(),
        mstore.docs.len()
    );

    // 5. Steady state: warm model, resident stores, one embed + two dot scans.
    let query = "finding a past decision by meaning rather than keywords";
    for i in 1..=5 {
        let t = Instant::now();
        let mut qvec = FastEmbedder.embed(&[query.to_string()]).unwrap().remove(0);
        normalize(&mut qvec);
        let code = rank(&cstore.docs, &qvec, 5, None, None);
        let mem = rank(&mstore.docs, &qvec, 5, None, None);
        eprintln!(
            "warm search #{i}              : {:>9.2?} ({} code + {} mem hits)",
            t.elapsed(),
            code.len(),
            mem.len()
        );
    }
}

/// Thread-scaling check (NOT a correctness test): is one ONNX session actually
/// using the cores? Run: `cargo test -p comrade-tool-memory --release --
/// --ignored --nocapture bench_threads`
#[test]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
fn bench_threads() {
    use std::time::Instant;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let filler = "// decode the buffer, walk the records, validate each header and \
                  accumulate the counters for the running session ";
    let mut texts = Vec::new();
    for i in 0..128 {
        let mut s = format!("fn worker_{i}() {{\n");
        while s.len() < 800 {
            s.push_str(filler);
        }
        s.push_str("}\n");
        texts.push(s);
    }
    eprintln!("cores={cores} texts={} (≈800 chars each)", texts.len());
    for (label, threads) in [
        ("intra=1", Some(1usize)),
        ("intra=2", Some(2)),
        ("intra=4", Some(4)),
        ("intra=default", None),
    ] {
        let mut m = build_embedding(threads).unwrap();
        let t = Instant::now();
        let _ = m.embed(&texts, Some(EMBED_BATCH)).unwrap();
        eprintln!("{label:<14}: {:>9.2?}", t.elapsed());
    }
}

/// Chunk-length distribution + batching-padding simulation (NOT a correctness
/// test). Run: `cargo test -p comrade-tool-memory --release -- --ignored
/// --nocapture bench_chunk_stats`
#[test]
#[ignore = "diagnostic benchmark; run with --ignored --nocapture"]
fn bench_chunk_stats() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut lens: Vec<usize> = Vec::new();
    for (rel, abs, _, _) in code_files(&root) {
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        if bytes.len() as u64 > MAX_CODE_BYTES || bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for c in comrade_tool_syntax::chunks_of_file(&rel, &text) {
            lens.push(c.text.chars().count());
        }
    }
    let n = lens.len();
    if n == 0 {
        return;
    }
    let mut sorted = lens.clone();
    sorted.sort_unstable();
    let sum: usize = lens.iter().sum();
    let pct = |p: f64| sorted[((n as f64 - 1.0) * p).round() as usize];
    eprintln!(
        "chunks={n} total_chars={sum} avg={:.0} p50={} p90={} p99={} max={}",
        sum as f64 / n as f64,
        pct(0.5),
        pct(0.9),
        pct(0.99),
        sorted[n - 1]
    );
    let padded = |order: &[usize], batch: usize| -> usize {
        order
            .chunks(batch)
            .map(|c| c.iter().copied().max().unwrap_or(0) * c.len())
            .sum()
    };
    for batch in [8usize, 16] {
        eprintln!(
            "batch {batch}: actual_chars={sum} padded_in_order={} padded_sorted={}",
            padded(&lens, batch),
            padded(&sorted, batch)
        );
    }
    for cap in [2000usize, 1500, 1000] {
        let capped: usize = lens.iter().map(|c| (*c).min(cap)).sum();
        eprintln!("cap {cap} chars: total={capped} (was {sum})");
    }
}
