# 0053 - Bound delegated sub-agent runs with a wall-clock timeout
status: superseded
date: 2026-09-14
tags: delegate, timeout, config, reliability
summary: Delegated sub-agents run under a wall-clock budget ([agent].delegate_timeout_secs, default 60s); on expiry they stop and return their partial answer or a notice, so a slow/stuck delegate can no longer hang the parent run.

## Context
The human reported that agents "sometimes just loop infinitely" and asked to give delegates a timeout so a delegate "has to answer in a minute with whatever information they have". Root cause: `run_delegate_subagent` (crates/comrade-core/src/delegate.rs) awaited each model request and each tool call with no wall-clock bound. The model client timeout defaults to 600s and the delegate's tool calls do not go through the main loop's per-tool dispatch timeout, so a single slow/hung request (or a hanging tool) could hold the parent run open for minutes. The existing `max_iterations` (30) caps turns, not time.

## Decision
Give every delegated sub-agent run a wall-clock budget. `DelegateLimits` gains `timeout: Duration` (default 60s; `Duration::ZERO` = no limit), wired from a new `[agent].delegate_timeout_secs` config key (default 60) in comrade-tui's build_tools for the `delegate`, `delegate_parallel` and `ask_advise` tools. In `run_delegate_subagent`, `deadline = now + timeout`; a small `invoke_within(remaining, fut)` helper wraps every model request AND every nested tool invocation in `tokio::time::timeout`, returning `None` on expiry. When a call does not finish in time the loop returns `timeout_answer(author, timeout, last_text)` instead of erroring: it returns the most recent non-empty assistant text as a best-effort answer, or a clear notice that the delegate did not finish and should be re-delegated smaller. The cancel-token path still takes precedence (the loop checks it first).

## Rationale
Bounding the loop from inside (rather than wrapping the whole future) is the only way to return a partial answer on expiry — the user explicitly wants the delegate to "answer with whatever information they have". Wrapping both model requests and tool calls (not just model requests) closes the hanging-tool hole, since delegate tool calls bypass the main loop's per-tool timeout. A 60s default matches the user's "in a minute".

## Alternatives considered
(a) Wrap the whole `run_delegate_subagent` future in one `tokio::time::timeout` at each tool call site — rejected: the future is dropped on timeout, losing any partial answer the delegate had. (b) Rely on the existing `max_iterations` cap — rejected: it bounds turn count, not wall-clock, so a single slow/hung model request (client timeout defaults to 600s) still blocks for minutes. (c) Rely on `LlmCfg.timeout_secs` — rejected: it only bounds one request and is far too long. (d) Reserve a grace period for a final "answer now" round — rejected for now: it complicates the hard one-minute bound; can be a follow-up.

## Scope
The delegate sub-agent loop and the config/limits that feed it. Applies to every tool that runs a sub-agent via `run_delegate_subagent` (`delegate`, `delegate_parallel`, `ask_advise`). It does NOT change the main agent loop's `run_timeout_secs` (still default 0/off) nor `summarise` (a single non-tool chat call). Does not add a grace period or per-request timeout tuning.

## Impact
A stuck/slow delegate can no longer hang the parent run; it is cut off at the budget and the parent receives a usable (possibly partial) result rather than a hang. New public field `DelegateLimits::timeout` and new config key `[agent].delegate_timeout_secs`; README + glossary updated. Trade-off: a legitimately long delegate task now needs a larger `delegate_timeout_secs`. A tool that is cut off mid-flight has its future dropped (same as the main loop's `tool_timeout_secs`), which for `shell`/`run_bg` may leave a detached child process. Follow-ups: a reserved final "answer now" grace round; surfacing the timeout as an explicit event so the transcript shows it.


## Note
Observed in practice: the configured coding delegate (qwen/qwen3.6-35b-a3b) exhausts the default 60s budget and returns "stopped after 60s without a final answer" on a step that edits two files (the semantic_search `path` feature). Readiness checks (ask_advise step=N) hit the same cap. For this delegate, either raise [agent].delegate_timeout_secs or hand it a smaller step (one file / one function); a two-file implementation is faster to do directly.

## Note
Confirmed again in practice: with the default 60s budget the delegate cannot even finish a READ-ONLY readiness check (`ask_advise step=N`) on a large file — the check for a crates/comrade-tui/src/tui.rs (~12k lines) step timed out. While the budget stays at 60s, don't plan delegate steps that must read/edit/unittest that file: do them directly, or first add `[agent] delegate_timeout_secs = 600` to the project `.comrade.toml` (layered over the user config; needs Ctrl-R / M-x reload-config, or just the next start, to take effect — a reload is refused while a run is in flight).

## Note
Default raised from 60s to 300s (2026-09-14). The 60s default proved too aggressive in practice for the configured delegate (qwen/qwen3.6-35b-a3b): it consistently ran out of budget on multi-file steps and even on read-only `ask_advise step=N` readiness checks over large files (see the two notes below). The limit still exists (a delegate cannot hang the parent forever); only its default changed. `[agent].delegate_timeout_secs = 0` still disables it entirely. The Decision/Rationale above still reference the original "60s / in a minute" figure — the mechanism is unchanged, only the default value.

## Note
Mechanism superseded by #68 (2026-09-16): the budget is no longer wall-clock. `DelegateLimits::timeout` / `[agent].delegate_timeout_secs` now measure INACTIVITY - a delegate that completes nothing (no model reply, no tool result) is nudged at that many seconds and stopped at twice it, and any completed request or tool call resets the clock. The field name, the config key and the 0-disables rule are unchanged; a hung request/tool is still bounded, and the partial-answer `timeout_answer` path is kept (reworded to "stopped after Ns of inactivity"). The notes below (60s -> 300s default, delegates missing the budget on multi-file steps and on `ask_advise step=N` readiness checks) remain useful history: the inactivity change is what makes those cases survivable, since a delegate that keeps working is no longer cut off.
