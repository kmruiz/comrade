# 0068 - Delegate budget is an inactivity clock: nudge at N, stop at 2N
status: accepted
date: 2026-09-15
tags: delegate, timeout, config, reliability
summary: A delegated run's budget now measures INACTIVITY: after `[agent].delegate_timeout_secs` (300s) with nothing completed the delegate is nudged to act, and at twice that (600s) it is stopped; any completed model reply or tool result resets the clock.

## Context
ADR #53 gave every delegated run a wall-clock budget (`[agent].delegate_timeout_secs`, default since raised to 300s) and cut the delegate off when it expired. In practice that is too aggressive in the wrong direction: a delegate working steadily but slowly is killed mid-task, while the failure it was meant to catch is a delegate that is doing NOTHING (a hung model request, a frozen turn). The human asked for the budget to measure idleness instead: "300s is fine in terms of time, but it should be time without doing anything - if the delegate spends 300s without doing anything, nudge it to do some action; if after 600s it didn't do anything, then stop it."

## Decision
In `run_delegate_subagent` (crates/comrade-core/src/delegate.rs) the absolute `deadline = now + timeout` is replaced by an INACTIVITY clock. `last_progress: Instant` is reset by every completed model request and every tool call that returned; `idle_nudge_at = limits.timeout` and `idle_stop_at = limits.timeout * 2`. At the top of each turn the loop checks the clock: at/after `idle_stop_at` it returns `timeout_answer`; between the two gates it injects the new `IDLE_NUDGE` text once per idle stretch (`ctxm.push_user_merged`). `invoke_within` is bounded by the time left until the NEXT gate (`idle_left(last_progress, idle_nudged)`), not by a fixed deadline, so a hung request is interruptible exactly at the nudge point. A cut-off tool call is now ANSWERED with an error tool result (history stays API-valid) and the loop continues, so the delegate gets another turn, instead of aborting the run. `[agent].delegate_timeout_secs` keeps its name and 300 default, now meaning the nudge threshold; `0` still disables the limit entirely.

## Rationale
What must be bounded is idleness, not effort. The nudge can only be seen on the delegate's next request, so the in-flight request must be interruptible at the nudge point: bounding every call by the time left until the next gate is what makes the nudge land at 300s instead of only after the call returns. Progress resetting the clock is what lets a slow-but-working delegate finish. Doubling the configured value for the stop keeps a single knob and lands exactly on the requested 300s nudge / 600s stop for the shipped default (300).

## Alternatives considered
(a) A second config key `delegate_stop_secs` — rejected: one knob is enough and stop = 2x nudge is exactly the ratio asked for; it can be added later if a deployment needs a different ratio. (b) Keep the wall-clock budget and merely add a nudge — rejected: the complaint was the cut-off itself, not the lack of a nudge. (c) Count only state-changing progress (edits) as activity — rejected: reads/tests are work too, and the call-count read guard (`DELEGATE_READ_NUDGE`) already covers "reads forever without acting"; a time guard should not double up on it. (d) Reset the clock only when a tool returns, not on a model reply — rejected: a model answering a prompt but handed a hung tool each turn would be stopped despite working (and the repeat guard already aborts that case).

## Scope
The delegate sub-agent loop and the config/limits that feed it; applies to `delegate`, `delegate_parallel` and `ask_advise` (all run through `run_delegate_subagent`). It does not change the main loop's `run_timeout_secs` (still off) or `tool_timeout_secs`, nor `summarise`. The nudge is not surfaced as a chat event (the delegate's own transcript carries it).

## Impact
A delegate that keeps completing requests or tools is never cut off for taking its time; only a frozen or hung run is. Cost: a genuinely hung request now takes up to 2x the configured value (600s at the default) before the run stops, with a "stopped after Ns of inactivity" notice; and a hung tool yields a recoverable error observation rather than ending the run. Tests: `a_frozen_delegate_is_nudged_to_act`, `a_progressing_delegate_is_never_cut_off`, `a_hanging_tool_is_cut_off_and_the_delegate_recovers`, `a_delegate_that_never_answers_times_out_within_its_budget`. Follow-up if needed: a separate stop key, and surfacing the nudge as a chat event so the transcript shows the delegate was nudged.

