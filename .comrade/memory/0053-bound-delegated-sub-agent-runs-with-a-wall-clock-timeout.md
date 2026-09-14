# 0053 - Bound delegated sub-agent runs with a wall-clock timeout
status: accepted
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

