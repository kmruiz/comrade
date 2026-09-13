# 0022 - Semantic memory search: fastembed model + persisted flat cosine index
status: accepted
date: 2026-09-13
tags: memory, search, dependencies, embeddings
summary: Add a `semantic_search` memory tool backed by fastembed's quantized BGE-small-en-v1.5 (in-process ONNX) and a flat cosine index cached in the user cache dir, rebuilt incrementally by text hash.

## Context
The user asked for semantic search over project memory "using an embedded vector db and an embedded small embeddings model". `find_adr`/`find_glossary` were keyword-only, so a decision could be missed when the query used different words. A stack had to be chosen that stays local (no server) and small enough to ship.

## Decision
`comrade-tool-memory` gains `semantic_search`. Embeddings are produced by `fastembed` (fastembed 5.x) running the quantized ONNX model `BGESmallENV15Q` (~30 MB, downloaded once to the user cache, loaded lazily into a process-wide `OnceLock<Result<Mutex<TextEmbedding>>>`). The vector store is a persisted FLAT index (exact cosine over all vectors) written as JSON to `$XDG_CACHE_HOME/comrade/semantic/<fnv(project_root)>.json` (fallback `~/.cache`, else temp). Documents are every ADR (body) and every glossary term; each is re-embedded only when its FNV-1a text hash changes (or the model id changes), so a steady-state call costs one query embedding. The `Embedder` trait isolates the model so index/ranking logic is unit-tested with a stub, and a real-model test is `#[ignore]`d. Embedding runs on `spawn_blocking`.

## Rationale
fastembed is the established local-embeddings crate in Rust and BGE-small is a strong quality-per-size retrieval model; the quantized variant keeps the download small. A flat index is the right engineering choice for a corpus of tens-to-hundreds of memory documents: exact, instant, dependency-free, and trivially correct, with no ANN tuning to get wrong.

## Alternatives considered
HNSW crates (`hnsw_rs`, `instant-distance`, `usearch`) were rejected: extra (sometimes native) dependency and index-tuning for no benefit at this corpus size. `candle`/`rust-bert` (pure-Rust or libtorch) were rejected as far more code/build weight. Storing the index inside `.comrade/memory/` was rejected so machine-specific binary data never lands in the repo.

## Scope
Covers how memory is embedded, indexed and persisted. Does not add semantic search over source code, and does not replace the keyword tools (`find_adr`/`find_glossary` remain the cheap default).

## Impact
`comrade-tool-memory` now pulls the fastembed/ort/tokenizers dependency tree, so the TUI binary is larger and the first `semantic_search` call downloads ~30 MB (then caches). `semantic_search` is classified read-only (advisors and delegates may call it). A future ANN swap only needs a different `Store` implementation behind the same `refresh`/`rank`/`Embedder` seams.

