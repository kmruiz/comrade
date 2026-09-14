# 0049 - Warm the semantic index in the background at startup (+ tool + --warm-index CLI)
status: accepted
date: 2026-09-14
tags: semantic-search, performance, warm-up, tooling, startup
summary: The app now starts building the semantic index in the background the moment it opens (non-blocking, idempotent), exposes it as the `warm_semantic_index` tool so the agent can re-warm it, and adds `comrade --warm-index` so the background-job tooling can build it as a one-shot process.

## Context
The semantic index is built lazily on the first `semantic_search`. After the batching fix that made it 2.2x faster (see #22), a fresh checkout still spent ~67s on that first call (measured: 116 memory docs + 2355 code chunks), which the user found unacceptable. They asked to run the index in the background always, and to expose a tool so the agent can also trigger it / run it as a background job.

## Decision
comrade-tool-memory gains a shared worker `build_indexes(root, &dyn Embedder)` that builds the memory and code indexes incrementally (reusing the cache), saves each only when it changed, and installs each into the resident MEM_STORE/CODE_STORE under a brief lock (never held across the build). On top of it: `warm(root) -> String` runs that worker on a detached thread and returns immediately, guarded by a process-global `WARMING` root set so a second call while one is running is a no-op; `warm_blocking(root) -> String` runs it synchronously and returns a one-line report for the CLI. Each index is best-effort, so a memory failure never stops the (slow, valuable) code index. A new `warm_semantic_index` tool calls `warm(&ctx.project_root)` and returns the status string. comrade-tui's main(): after build_deps it always calls `warm(&deps.root)` (TUI and headless alike), and a new `--warm-index` flag runs `warm_blocking` and exits before loading config, a model or the TUI.

## Rationale
Moving the one-time cost off the first search and behind app startup means the first `semantic_search` finds the index already resident. The worker is incremental, so a warm startup on an already-built index is a no-op measured at 0.04s. The tool lets the agent re-warm after large edits (the code index is only refreshed by searches otherwise), and the CLI gives `run_bg` a real command so a warm-up can be a tracked background job.

## Alternatives considered
Warming as a separate process through the background-job registry (bg.rs): rejected by the human because two processes could write the same index file and it needs more moving parts — although the `--warm-index` CLI is still provided so the agent CAN run it as a bg job if it wants. Keeping the lazy build on first search (the status quo): this is the behaviour being fixed. Building the whole index in `build_deps` synchronously: rejected, it would block startup for ~67s.

## Scope
The semantic index only (memory + code). It is deliberately NOT a general background-job framework: the warm-up is a detached in-process thread, so it is not shown in the TUI jobs panel and not killable via `bg_kill`.

## Impact
Once the background warm finishes, the first `semantic_search` in a session is instant. Measured on this repo (release): `--warm-index` cold 67.6s, incremental with a stale cache 2.0s, no-op when already warm 0.04s. Known trade-off: a cold warm uses all CPU cores for ~67s, so the very first startup of a fresh checkout may feel CPU-busy; it is one-time per checkout (subsequent starts are no-ops). Follow-up if it bites on small machines: throttle the warm's ONNX intra-op threads. Follow-up to #22.

