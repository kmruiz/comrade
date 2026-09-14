# 0020 - User-triggered context compaction (M-c) replaces the history with a model summary
status: accepted
date: 2026-09-13
tags: core, context, tui, compaction
summary: M-c compacts the running context: the agent loop takes a take-once `CompactRequest` at its rest point, asks the model for a summary via `compact_history`, and `ContextManager::compact` replaces the history with it (keeping the system prompt).

## Context
Long sessions keep one rolling `ContextManager` across tasks; when it approaches the token budget the manager trims automatically (stubs observations, folds evicted turns into an "Earlier context (compacted)" rollup). That degradation is silent and lossy and cannot recover detail. The user wanted an explicit, user-triggered compaction: ask the model (or advisory models) for a summary of what has been done and REPLACE the running context with that summary instead of all the tool chatter.

## Decision
Add a user-triggered compaction bound to M-c. (1) `comrade_tool::CompactRequest` is a cheap clonable handle over an `Arc<AtomicBool>` with `request()`/`take()`; `ToolContext` gained a `compact: Option<CompactRequest>` field (None in headless/tests). (2) The root agent loop (`run_agent_loop`) takes a pending request at its rest point, BEFORE `enforce_budget()`, calls `comrade_core::compact::compact_history(client, ctxm)` (which asks `LlmClient::chat` — a non-streaming completion — to summarise a flattened transcript, then `ContextManager::compact(summary)` replaces the history keeping only the system message and folding the summary into the "Earlier context (compacted)" rollup), and emits `AgentEvent::ContextCompacted { before/after messages+tokens }` (or `Error` on failure). The delegate sub-loop deliberately does NOT honour compaction. (3) TUI: `MxCommand::CompactContext` ("compact-context", M-c) and `App::compact_context`; while a run is in flight it sets the shared request (performed at the next rest point), while idle it summarises now in a background task that reports through the tagged event queue. Idempotent: the flag is take-once.

## Rationale
A take-once atomic flag mirrors the existing `Steer` control pipe: it is cheap to clone into every `ToolContext`, survives the task boundary, and is naturally idempotent. Doing it at the loop's existing rest point (same place steers are drained) keeps OpenAI wire validity — we never rewrite history in the middle of a tool-call/result pair. Reusing the "Earlier context (compacted)" rollup for the summary means the automatic trimmer and the explicit compaction share one representation, and `compact_history` staying a small core function keeps it testable against a fake server.

## Alternatives considered
(a) A `flush` message on the steer pipe — rejected: the text goes to the model as a user turn rather than triggering a control action. (b) A shared `Arc<Mutex<Option<Request>>>` — rejected as heavier than an atomic flag. (c) A dedicated `AgentEvent` carrying the summary — rejected: the summary is a context detail, not chat content, and the chat should only show sizes. (d) Performing the compaction in the TUI only, never mid-run — rejected: the most valuable moment is while a long run is mid-flight.

## Scope
Covers comrade-core (context, compact, agent loop, session event), comrade-tool (CompactRequest + ToolContext field) and comrade-tui (M-c keybinding, MxCommand, idle path). Does not change the automatic budget trimming, the delegate sub-agent loop, or session persistence.

## Impact
M-c during a run compacts at the next model-iteration boundary; M-c while idle compacts immediately. The gauge updates to the post-compaction estimate. Only the root loop compacts (a nested delegate keeps its own context). Follow-up ideas (not done): let the user pick N advisory models to summarise in parallel, and a confirmation prompt.


## Merged from #0034 - Auto-compaction in the agent loop (CtxCfg.auto_compact, default on)
status: accepted
date: 2026-09-13
tags: context, agent-loop, compaction
summary: The agent loop auto-compacts an over-budget history into a model-written summary (once per over-budget episode, gated by CtxCfg.auto_compact, default on) instead of letting enforce_budget degrade it lossily.

## Context
The harness only compacted the context on an explicit user request (M-x compact-context / M-c), while `ContextManager::enforce_budget` silently degraded an over-budget history (stub large observations, evict oldest, fold thoughts into a small rollup). The doc comment on `run_agent_with_history` already claimed the manager 'compacts it automatically as it approaches the budget', which was false. Approved as the first item (A1) of the greenlit harness roadmap.

## Decision
The agent loop now compacts automatically. In `run_agent_loop`, after draining steer and before `ctxm.enforce_budget()`, if `cfg.context.auto_compact` (new `CtxCfg` field, default true), the history contains at least one assistant turn (`Role::Assistant` present), and `ContextManager::needs_auto_compaction()` (a new public predicate equal to the internal `over_budget`, i.e. total_tokens past `trim_target`), the loop calls `compact::compact_history` and emits the same `AgentEvent::ContextCompacted` as the user-triggered path. An `auto_compact_armed` flag arms compaction once per over-budget episode (disarmed on compaction, re-armed only once the history is back under the threshold) so a persistently over-budget history cannot fire a summariser call every iteration. The auto branch is an `else if`: a user CompactRequest handled this turn suppresses it.

## Rationale
Tying the trigger to `needs_auto_compaction()` reuses the exact point where the lossy trim would begin, so a summary replaces degradation at the same moment with no new tuning knobs. The 'assistant turn present' guard prevents folding a fresh prompt into a summary of itself, and the arming flag bounds the cost per episode.

## Alternatives considered
(a) Keep compaction manual only (M-c) — rejected: long autonomous runs silently degrade history via enforce_budget's lossy stub/evict, losing decisions. (b) Compact on every iteration while over budget — rejected: a history over the cap for a structural reason (system prompt > budget) would fire a summariser model call every turn. (c) Compact on a fixed turn cadence (every N turns) — rejected: decoupled from the actual budget pressure.

## Scope
Covers the agent loop's automatic context compaction and its config flag. Does NOT change `enforce_budget` (still the fallback when a summary cannot bring the history under budget), the user-triggered M-c path, or `compact_history` itself.

## Impact
Over-budget long runs are now summarised by the model instead of degraded lossily. Costs one extra (non-streaming) model call per over-budget episode. Set `[context] auto_compact = false` to restore manual-only behaviour. Files: crates/comrade-core/src/agent.rs, context.rs, config.rs; tests: agent::tests::over_budget_history_is_auto_compacted, context::tests::needs_auto_compaction_tracks_the_trim_target, config::tests::auto_compact_defaults_on_and_can_be_disabled.

## Note
Rollup of context compaction: this ADR adds user-triggered M-c compaction; #0034 adds automatic compaction in the agent loop (CtxCfg.auto_compact, default on). Both share compact_history/ContextManager::compact. Body preserved under "Merged from".
