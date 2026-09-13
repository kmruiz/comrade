# 0037 - Tool dispatch: pre/post hooks, per-tool timeout, per-run budget
status: accepted
date: 2026-09-13
tags: agent-loop, hooks, timeouts, roadmap
summary: A shared agent::Dispatch wraps every main-agent tool call with pre/post shell hooks, an optional per-tool timeout, and output redaction; a per-run wall-clock budget ends long runs gracefully.

## Context
The loop invoked tools directly in three separate places (the ReAct path, the native path, and the deferred parallel-delegate batch), with no timeout and no way for a user to run a script before/after a tool. The roadmap called for a per-tool + per-run timeout (A3) and pre/post hooks (D1).

## Decision
Introduce a shared `agent::Dispatch { cfg, hooks, redactor }` used by all three invoke sites. `Dispatch::run` (1) runs matching pre-hooks (`Hooks::pre`), aborting the call on a non-zero exit; (2) invokes the tool under `tokio::time::timeout(cfg.agent.tool_timeout_secs)` when that is > 0; (3) redacts the output; (4) runs post-hooks (`Hooks::post`), appending any warning to the result. Hooks (`comrade_core::Hooks`, from `[[hooks.pre_tool]]`/`[[hooks.post_tool]]`) are shell commands matched by `*`/exact/`prefix*`, run via `bash -c` with COMRADE_TOOL/COMRADE_ARGS(/COMRADE_OK). A whole-run wall-clock budget (`cfg.agent.run_timeout_secs`) is checked at each loop rest point and ends the run gracefully with a FinalAnswer. The deferred parallel batch now wraps each future with the same Dispatch, keeping concurrency.

## Rationale
One code path for hooks + timeout + redaction keeps the three invoke sites consistent and testable, and reuses the existing bash-execution model.

## Alternatives considered
Per-tool timeout implemented inside each tool (duplicated, inconsistent) — rejected. Hooks as in-process Rust trait objects (faster but not user-configurable without a rebuild) — rejected: shell hooks match the existing skills/shell model. Hooks attached to the Tool trait (so delegates get them too) — deferred; kept at the dispatch layer for a first cut.

## Scope
Tool-call dispatch in the main agent loop (ReAct + native + parallel delegates). Not delegate sub-agent dispatch.

## Impact
Users can gate/observe tool use with shell hooks and cap runaway tools/runs via config. Config: `[agent] tool_timeout_secs`, `run_timeout_secs` (0 = off by default); `[[hooks.pre_tool]]`/`[[hooks.post_tool]]` with `on`/`run`. Follow-up: delegate sub-agents do not yet run hooks (their loop shares neither Dispatch nor a redactor).

