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


## Note
2026-09-14 update: the model is no longer DOWNLOADED. The int8-quantized BGE-small-en-v1.5 ONNX bundle (~35 MB: model_quantized.onnx + tokenizer.json + 3 config files) now lives at `crates/comrade-tool-memory/assets/bge-small-en-v1.5-int8/` and is compiled into the binary with `include_bytes!`. `FastEmbedder` builds it via fastembed's `TextEmbedding::try_new_from_user_defined(UserDefinedEmbeddingModel::new(...).with_pooling(Pooling::Cls), InitOptionsUserDefined::new())`; there is no cache dir, no network and no first-call download. The vector index still lives under the user cache dir. `MODEL_ID` is now `bge-small-en-v1.5-int8` (was `bge-small-en-v1.5-q`), so old indexes rebuild. `Cargo.toml` pins `fastembed = { version = "5", default-features = false, features = ["ort-download-binaries-native-tls"] }` (keeps ort/tokenizers, drops the model-download extras). The `real_model_embeds_and_ranks_by_meaning` test is no longer `#[ignore]`d: it proves the embedded model loads offline.

## Note
2026-09-14 update: binary-size reduction. The embedded model was making the release binary 96.2 MB. Two cheap levers were applied, both measured:\n1. `crates/comrade-tool-memory/build.rs` deflates each raw asset in `crates/comrade-tool-memory/assets/bge-small-en-v1.5-int8/` with flate2 (pure-Rust miniz_oxide, already in the dep tree) into `OUT_DIR/assets/<name>.deflate`; `crates/comrade-tool-memory/src/semantic/mod.rs` now `include_bytes!`s those compressed copies and `inflate()`s them in memory on first model use. The ~35 MB of raw assets become ~24.7 MB in the binary (-9.5 MB model, -0.5 MB tokenizer). Raw files stay the source of truth.\n2. `[profile.release] strip = true` in the workspace Cargo.toml removes ~13.7 MB of `.symtab`/`.strtab`.\nResult: target/release/comrade 96,191,032 -> 72,518,632 bytes (-24.6%), verified `stripped` with no `.symtab`. The `real_model_embeds_and_ranks_by_meaning` test (no longer #[ignore]d) exercises the inflate path offline.\nRemaining cost is dominated by statically linked ONNX Runtime (~30 MB): fastembed exposes `ort-load-dynamic` (dlopen libonnxruntime at runtime) if a smaller binary is needed later, at the price of shipping/loading a shared library.

## Merged from #0027 - Semantic code search: semantic_search scope=code over tree-sitter chunks, git-incremental index, M-x reindex
status: accepted
date: 2026-09-13
tags: memory, search, syntax, tools, index
summary: Extend `semantic_search` with `scope: memory|code|all`; code hits come from tree-sitter symbol chunks carrying file:line, indexed in a separate per-project flat index that is reindexed incrementally from git changes, plus a palette-only M-x `reindex-semantic-search` command.

## Context
ADR #22 shipped `semantic_search` over project memory only (ADRs + glossary). With the embedding model now compiled into the binary (no download), it made sense to use embeddings more: the request was to let `semantic_search` find code too, so the model sees a `file:line` for a hit. Constraints added along the way: don't reindex the whole project every call (index once, then follow changes), and prefer git as the change signal (it survives rebases/checkouts and covers uncommitted edits) over mtime.

## Decision
(1) Extend `semantic_search` rather than add a new tool: a `scope` arg (`memory` default | `code` | `all`) and `kind` extended to `adr|glossary|code`. Code hits carry `file:line` in title/id and point to ts_read_symbol / fs_read_file. (2) The chunker is a public `code_chunks`/`chunks_of_file`/`CodeChunk` in `comrade-tool-syntax`; `comrade-tool-memory` depends on it (no cycle: syntax depends only on comrade-tool). (3) Chunking is symbol-based via tree-sitter: one chunk per declaration, recursing through `impl`/`mod`/`trait` (the container head is a text-context prefix on the chunk text, the reported line is the declaration's own line even when the embedded text includes its doc comment/attributes); a 40-line window fallback handles non-Rust, oversized (>8k chars) and non-parsing files. (4) A separate per-project code index file (`<hash>-code.json`) sits beside the memory index so memory-only searches stay small/fast. (5) The code index is INCREMENTAL and git-driven: the store records the indexed HEAD plus a per-file stamp (mtime/size/doc-ids); on refresh `git diff --name-only <indexed_head> HEAD` together with `git status --porcelain` (working tree/untracked) give the dirty set, so unchanged files are neither re-parsed nor re-embedded and a dirty file reuses any chunk whose text hash is unchanged; non-git projects fall back to the mtime/size stamp. (6) A palette-only M-x command `reindex-semantic-search` calls a public `comrade_tool_memory::reindex(root)` to force a full rebuild of both indexes, reporting via a new `AgentEvent::Notice`.

## Rationale
Extending the existing tool keeps one place the model learns ("search by meaning") and one embedder/index implementation. Symbol-level chunks match how navigation works (ts_read_symbol) and make a hit a location. A separate index file keeps the common memory search cheap. Git is the right change signal for a coding agent: the edits the model makes (and commits) are exactly what git sees, so "index once, then follow the diff" replaces repeated full re-embeds without any file-watching.

## Alternatives considered
A separate `code_search` tool (rejected: two tools to teach, shared index logic). One combined index file (rejected: memory-only searches would carry the whole code base). Fixed line windows instead of symbols (rejected: loses the file:line of a named item). mtime-only change detection (rejected: noisy and wrong across checkouts/rebases). Reindexing synchronously inside every search call (rejected: unbounded latency on a large tree).

## Scope
Covers how memory and code are chunked, embedded and indexed, and how the code index is kept fresh. Does not add semantic search over non-Rust languages beyond the line-window fallback, does not replace keyword navigation (ts_find_symbol/fs_rgrep stay the cheap default), and does not change the embedded model or the flat-cosine store from #22.

## Impact
Embedding cost now scales with project code size on the first code search; the index is a cache under the user cache dir (nothing lands in the repo) and is refreshed incrementally after that. `semantic_search` stays read-only (already in READ_ONLY_TOOLS). `AgentEvent` gained a `Notice` variant for background-command results. The M-x command has no default key.


## Note
Default scope changed from `memory` to `all`: an omitted `scope` now searches BOTH memory and source code. Agents narrow with `scope=memory` or `scope=code` when they only want one side. Updated the spec `default`, the tool description and the runtime fallback in crates/comrade-tool-memory/src/semantic.rs.

## Note
Follow-up (deferred, requested by the human): `semantic_search` is too slow in practice — the flat per-project cosine index is scanned linearly and memory+code indexes are rebuilt/loaded per query. A proper index is needed (e.g. an ANN structure such as HNSW/usearch, an embedding cache keyed by content hash, and a persisted+memory-mapped index that is loaded once rather than per call). Do this as a dedicated task, not part of the current safety/hooks/timeouts batch.

## Note
Addressed in #40: the index is now resident (loaded once per project root), written only when it changed, and the code file walk is skipped on a clean repo — the per-query disk I/O and tree walk that made semantic_search slow are gone. A binary/mmap vector blob and an ANN structure remain deferred (memmap2 is not currently a dependency).

## Merged from #0040 - Make the semantic index resident: load once, write only on change, skip clean-repo walks
status: accepted
date: 2026-09-13
tags: semantic-search, performance, memory, index
summary: semantic_search now keeps its memory+code index resident per project (loaded once), writes the cache only when something changed, and skips the file walk on a clean repo — removing the per-query re-parse/re-write that made it slow; exact cosine search is retained and ANN/mmap are deferred.

## Context
`semantic_search` was reported as slow. Reading semantic.rs showed the real cost is not the cosine scan but the per-query I/O: every call read the whole JSON store from disk, re-ran the refresh (cheap: reuse by text hash), and WROTE the entire store back — plus, for the code index, re-walked and stat'd the whole tree even when nothing had changed.

## Decision
Keep exact cosine search but make the index resident and write-on-change. `MEM_STORE`/`CODE_STORE` hold one `Store` per project root in a process-global map, loaded from disk at most once and reused across searches; the refresh result is merged back in place and `save_store` runs only when the refresh reports a change (`refresh`/`code_refresh` now return `(Store, bool)`). The code index has a fast path: when the recorded HEAD still matches `head_sha` and `dirty_files` reports a clean tree, the whole file walk, re-parse and rewrite are skipped. Ranking no longer clones vectors: hits are merged as a small `Hit` (score + id/kind/title/preview) from the two resident stores.

## Rationale
The cheapest correct fix targets the actual bottleneck (per-query disk I/O and a redundant tree walk) with no new dependency, and keeps the human-readable JSON plus exact search that the existing tests and incremental logic rely on.

## Alternatives considered
A true mmap'd vector blob (memmap2) — deferred: memmap2 is not in Cargo.lock and adding it needs a network fetch; a resident in-memory index gives the same query-speed win (mmap would only save RSS). A binary/bincode store instead of JSON — deferred: the resident cache already removes the per-query parse cost, so the on-disk format can stay human-readable for now. HNSW/usearch (ANN) — deferred per the human's choice: exact cosine over a few hundred vectors is instant; ANN only matters at much larger scale.

## Scope
The comrade-tool-memory semantic index: load-once resident stores, write-only-on-change, clean-repo fast path, and non-cloning result merge. Not an ANN index and not a change of on-disk format.

## Impact
Steady-state searches no longer re-parse or rewrite the on-disk index and, on a clean repo, no longer walk the tree — the only per-query work left is the query embedding plus a linear cosine scan. `reindex` clears the resident caches so the next search reloads fresh. Follow-up: a binary (or mmap'd) vector blob and an ANN structure remain available if a project ever grows enough to need them.


## Note
2026-09-15 re-analysis of the remaining slowness (reported slow again on a small project). Measurements: the code index cache is 11.6 MB JSON for 2293 chunks across 145 files, dim 384 — avg 5376 JSON bytes/doc vs 1536 raw f32 bytes (~3.5x bloat from text-f32 serialization); memory index 520 KB. git subprocesses (rev-parse/diff/status) cost ~3 ms here, not the bottleneck. Root causes, ranked: (1) `code_refresh` sets `changed = true` whenever ANY file was re-parsed, and invoke then `save_store`s the whole store — so while the working tree is dirty (the normal state for an editing agent) EVERY code search rewrites 11.6 MB of JSON; (2) cold start parses 11.6 MB JSON + inflates the ~25 MB model + creates the ORT session, once per process; (3) the query ONNX forward pass per call, serialized under a global `Mutex<TextEmbedding>`; (4) `cosine()` recomputes both vector norms and two sqrt per doc on every query instead of using pre-normalized vectors; (5) `documents(root)` re-reads every ADR + the glossary on each memory search. The linear scan itself (2293 x 384) is sub-millisecond and is NOT the problem, which confirms #22/#40's reasoning that an ANN/embedded vector engine buys nothing here. Cheapest high-value fixes, in order: binary (bincode) or raw-f32-blob (+ optional memmap2) store instead of JSON; stop rewriting on no-op re-parses (compare content hashes / debounce); pre-normalize vectors so rank is a plain dot product; cache the memory `documents()` list. ANN (usearch/hnsw_rs) and an embedded vector DB remain unjustified at this corpus size.

## Note
2026-09-15 implemented fixes (1)-(4) from the note above. (1) Change detection is now content-based: `refresh`/`code_refresh` report `changed` from a comparison of the resulting store (order-independent `(id, hash)` signatures via `same_docs`, the per-file `FileStamp` map, and the recorded HEAD) instead of "any file was re-parsed". A dirty working tree whose files re-parse to identical chunks no longer rewrites the whole cache — measured on the real index, deflate aside, this removes the per-search 11.6 MB write. (2) The on-disk store is now a compact dependency-free binary blob (`encode_store`/`decode_store`: magic `CSMV`, u16 version, length-prefixed strings, raw little-endian f32 vectors) written to `<key>.bin`; `load_store` still reads a legacy `<key>.json` once and re-saves it in binary form. Measured on this repo's code index: JSON 11,587,403 -> binary 4,129,692 bytes (3.5 MB of that is the raw vectors). (3) Vectors are stored pre-normalised (`normalize` at creation, `normalize_store` on load for old caches) so ranking is a plain dot product (`dot`) — no per-document norm or two `sqrt` per query; the query vector is normalised too. (4) The `documents()` cache is NOT yet done (still re-reads ADRs + glossary per memory search) — only the memory index is small (520 KB) so it was left out of this pass. Explicitly NOT taken, per the human's decision: bson (not in Cargo.lock, needs a network fetch; BSON has no f32 scalar so serde doubles every component, and it repeats field names per doc — no size win) and flate2 compression on top of the binary blob (flate2 is already a dependency and the model assets use raw deflate, but measured it only shaves 4.13 -> 3.47 MB, ~16 %, because the payload is dominated by incompressible f32; it would not speed up the resident query path and would make the cache opaque). memmap2 and ANN remain deferred.

## Merged from #0041 - Position semantic_search as a first-class orientation tool, not a fallback
status: accepted
date: 2026-09-13
tags: prompt, tools, semantic_search, agent-behaviour
summary: semantic_search is deliberately a first-class EARLY orientation tool (domain known, symbol/file not), not a last-resort fallback.

## Context
The agent's own tool guidance (crates/comrade-core/prompts/tools-intro.md, working-style.md, delegate-system.md) described semantic_search as a last-resort fallback, gated on "you do not know the exact word", and listed it last. In practice it was called only ~3 times per session, because that framing conditions it on a rarely-noticed state ("I don't know the word") while the common orientation case is "I know the domain but not the symbol/file". The human flagged the underuse.

## Decision
semantic_search is positioned as a first-class, EARLY orientation tool, not a fallback. It is the first choice whenever you know WHAT you want but not WHERE it lives (you can name the domain, not the exact symbol, file or string). tools-intro.md's "Only the meaning" bullet and working-style.md's default loop (step 1, and step 3's leading tool) now say so explicitly; the word "fallback" is gone from its description.

## Rationale
The prompt is the durable lever: sessions are ephemeral, so a behavioural promise changes nothing. Rewording the tool's positioning (WHAT you want known, WHERE it lives unknown) matches the actual orientation situation the agent hits at the start of most tasks and removes the narrow trigger that caused underuse.

## Alternatives considered
(a) Just use it more this session - no durable effect, rejected. (b) Restrict or re-rank the tool's results - unnecessary; the tool works, the guidance was the problem. (c) Also mirror the wording into delegate-system.md - deferred (out of scope of the chosen change); the delegate prompt still lists semantic_search last and is a candidate follow-up.

## Scope
Covers the model-facing prompt wording for semantic_search in tools-intro.md and working-style.md. Does not change the tool's behaviour, its ToolSpec description in crates/comrade-tool-memory/src/semantic.rs, or the delegate system prompt.

## Impact
The agent should reach for semantic_search at the start of exploratory tasks. Follow-up: align the same framing in crates/comrade-core/prompts/delegate-system.md so delegates orient the same way.


## Note
Follow-up done: the same framing was aligned in the two remaining spots. (1) crates/comrade-core/prompts/delegate-system.md step 2 now leads with semantic_search EARLY (domain known, exact symbol/file unknown) and puts the name-based tools after it. (2) The SEMANTIC_SEARCH_SPEC.description in crates/comrade-tool-memory/src/semantic.rs (the text the model sees for the tool itself) now says it is the first choice when you know WHAT you want but not WHERE it lives, NOT a fallback. All three model-facing mentions (tools-intro.md, working-style.md, delegate-system.md) plus the ToolSpec now agree.

## Note
Rollup of the semantic_search feature: this ADR is the spine (embedded flat index), #0027 adds code search (scope memory|code|all over tree-sitter chunks, git-incremental), #0040 makes the index resident/write-on-change with the binary CSMV store and dot-product ranking, #0041 positions semantic_search as a first-class early orientation tool. Bodies preserved under "Merged from".
