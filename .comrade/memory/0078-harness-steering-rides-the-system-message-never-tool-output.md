# 0078 - Harness steering rides the system message, never tool output, to avoid prompt-injection false positives
status: accepted
date: 2026-09-27
tags: agent-loop, context, steering, prompt-injection, nudges, prompts, tools, comrade-core
summary: All harness-authored steering (loop guards, plan/read/loop-refusal nudges, idle and supervision corrections, human steers) is queued with ContextManager::push_note and rendered into the SYSTEM message under a "## Harness notes" header; tool results stay factual data and the UI shows the steering as AgentEvent::Notice, so a model no longer reads it as a prompt-injection planted in tool output.

## Context
A model run against Comrade complained that some tools were trying to inject prompts. The mechanism was real and documented: `ContextManager::push_user_merged` (ADR 0059) appended a harness nudge with `\n\n` onto the LAST message when it was a `Role::Tool` result or a ReAct user observation, so text like "STOP investigating…" appeared WELDED TO THE TAIL OF THE PRECEDING TOOL OUTPUT (a `semantic_search`/`fs_rgrep` result). To a model, instructions inside tool data are exactly the shape of a prompt-injection attack, so a benign loop guard was indistinguishable from an attack. Several harness directives were also emitted AS tool results/observations: the read guard returned its instruction in place of the read's output, the no-progress refusal carried "Do NOT call it again", and the "Every task starts with a plan" nudge was pushed as a fabricated user observation. `push_user_merged` existed because strict local templates (LM Studio's Mistral) reject two user turns in a row or a user turn straight after tool results, so the fix could not simply be "push a user message".

## Decision
Introduce a trusted steering channel in `ContextManager` and route every harness directive through it:
1. `push_note(text)` queues a note; `request_messages(&self)` returns the message list for the next request with queued notes appended to the SYSTEM message under a `## Harness notes` header (one-shot; `clear_notes()` clears after the model answers). The stored history is never mutated, so notes do not replay.
2. Both model-request sites (`crates/comrade-core/src/agent.rs` and `crates/comrade-core/src/delegate.rs`) call `request_messages()` and `clear_notes()` after a successful turn.
3. All harness steering uses it: `STALL_NUDGE`/`VERIFY_NUDGE`/`PLAN_FIRST_NUDGE`, `read_guard_message`, `LOOP_REFUSAL_NUDGE`, the delegate `IDLE_NUDGE`/`VERIFY_NUDGE`/`STALL_NUDGE` and `SUPERVISE_PREFIX` corrections, and messages drained from the human `Steer` bus (labelled "The user sent this…").
4. Tool results stay factual: the read guard answers with a plain refusal ("ERROR: read limit reached…; `<tool>` was not run.") and the no-progress refusal is factual ("ERROR: `<tool>` was already called…; it was not run again."); the steering goes out of band.
5. The UI is told via `AgentEvent::Notice("loop guard: …")` (rendered as a meta line), never a fake `ToolResult`, so the transcript no longer shows directives as tool output either.
`push_user_merged` is kept for genuine user text but is no longer used for harness steering.

## Rationale
The system message is the one channel a model is trained to treat as authoritative instruction; delivering steering there removes the ambiguity by construction, needs no role gymnastics (system is always legal), and leaves tool results as pure data. Notes are transient, so they never accumulate or replay. This supersedes ADR 0059's "any new nudge must use push_user_merged" for HARNESS steering while preserving its goal — valid role alternation — because the system channel never inserts a turn at all.

## Alternatives considered
(a) Keep `push_user_merged` but wrap the nudge in a loud "[comrade system note]" delimiter — REJECTED: it is still text inside a tool result; a model that flags injected instructions does not parse our labels. (b) Push a fresh `Role::User` turn for every nudge — REJECTED: strict templates reject a user turn straight after tool results (the reason 0059 existed). (c) Insert a mid-conversation `Role::System` message — REJECTED: strict templates commonly allow only one leading system message; appending to the existing system message is the compatible form. (d) Remove harness steering entirely and rely only on prompts — REJECTED: the loop guards demonstrably help small models finish and stop editing. (e) Leave instructions in tool results but mark them non-executable in the system prompt — REJECTED as weaker and still injection-shaped.

## Scope
Covers the delivery channel for harness-authored steering and the wording of the few tool results that used to carry directives. Does NOT change when the nudges fire (the `LoopTracker` thresholds, read-guard count, idle clock, supervision interval) or what they say, and does not touch `AgentEvent`'s tool-call/result variants used for real tools. Prompt sources (tools-intro.md, working-style.md, delegate-system.md) were touched only for the unrelated semantic_search async note.

## Impact
A model no longer sees instructions welded onto tool output; the steering is still delivered on the very next request. `AgentEvent::Notice` gives the human a visible "loop guard:" line without faking a tool result. New harness steering must call `push_note` (not `push_user_merged`, not a fabricated tool result). Tests: `context::tests::harness_notes_ride_the_system_message_and_are_transient` plus the existing delegate read-guard, idle-nudge and supervision tests, which now assert behaviour through the note channel. Glossary term "harness note (steering channel)" replaces the old "loop-guard nudge" presentation trap.
