# 0034 - Auto-compaction in the agent loop (CtxCfg.auto_compact, default on)
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

