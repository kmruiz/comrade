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
        let (store, _) = refresh(&docs, &Store::default(), &emb).unwrap();
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
        let (store, _) = refresh(&docs, &Store::default(), &FastEmbedder).unwrap();
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
