# 0020 - User-triggered context compaction (M-c) replaces the history with a model summary
status: accepted
date: 2026-09-13
tags: core, context, tui, compaction
summary: M-c compacts the running context: the agent loop takes a take-once `CompactRequest` at its rest point, asks the model for a summary via `compact_history`, and `ContextManager::compact` replaces the history with it (keeping the system prompt).

## Decision
Add a user-triggered compaction bound to M-c. (1) `comrade_tool::CompactRequest` is a cheap clonable handle over an `Arc<AtomicBool>` with `request()`/`take()`; `ToolContext` gained a `compact: Option<CompactRequest>` field (None in headless/tests). (2) The root agent loop (`run_agent_loop`) takes a pending request at its rest point, BEFORE `enforce_budget()`, calls `comrade_core::compact::compact_history(client, ctxm)` (which asks `LlmClient::chat` — a non-streaming completion — to summarise a flattened transcript, then `ContextManager::compact(summary)` replaces the history keeping only the system message and folding the summary into the "Earlier context (compacted)" rollup), and emits `AgentEvent::ContextCompacted { before/after messages+tokens }` (or `Error` on failure). The delegate sub-loop deliberately does NOT honour compaction. (3) TUI: `MxCommand::CompactContext` ("compact-context", M-c) and `App::compact_context`; while a run is in flight it sets the shared request (performed at the next rest point), while idle it summarises now in a background task that reports through the tagged event queue. Idempotent: the flag is take-once.

## Rationale
A take-once atomic flag mirrors the existing `Steer` control pipe: it is cheap to clone into every `ToolContext`, survives the task boundary, and is naturally idempotent. Doing it at the loop's existing rest point (same place steers are drained) keeps OpenAI wire validity — we never rewrite history in the middle of a tool-call/result pair. Reusing the "Earlier context (compacted)" rollup for the summary means the automatic trimmer and the explicit compaction share one representation, and `compact_history` staying a small core function keeps it testable against a fake server.

## Alternatives considered
(a) A `flush` message on the steer pipe — rejected: the text goes to the model as a user turn rather than triggering a control action. (b) A shared `Arc<Mutex<Option<Request>>>` — rejected as heavier than an atomic flag. (c) A dedicated `AgentEvent` carrying the summary — rejected: the summary is a context detail, not chat content, and the chat should only show sizes. (d) Performing the compaction in the TUI only, never mid-run — rejected: the most valuable moment is while a long run is mid-flight.</alternatives>
<parameter name="context">Long sessions keep one rolling `ContextManager` across tasks; when it approaches the token budget the manager trims automatically (stubs observations, folds evicted turns into an "Earlier context (compacted)" rollup). That degradation is silent and lossy and cannot recover detail. The user wanted an explicit, user-triggered compaction: ask the model (or advisory models) for a summary of what has been done and REPLACE the running context with that summary instead of all the tool chatter.

## Scope
Covers comrade-core (context, compact, agent loop, session event), comrade-tool (CompactRequest + ToolContext field) and comrade-tui (M-c keybinding, MxCommand, idle path). Does not change the automatic budget trimming, the delegate sub-agent loop, or session persistence.

## Impact
M-c during a run compacts at the next model-iteration boundary; M-c while idle compacts immediately. The gauge updates to the post-compaction estimate. Only the root loop compacts (a nested delegate keeps its own context). Follow-up ideas (not done): let the user pick N advisory models to summarise in parallel, and a confirmation prompt.

