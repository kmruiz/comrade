# 0027 - Semantic code search: semantic_search scope=code over tree-sitter chunks, git-incremental index, M-x reindex
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
