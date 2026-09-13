# 0040 - Make the semantic index resident: load once, write only on change, skip clean-repo walks
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

