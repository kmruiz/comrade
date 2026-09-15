# 0063 - Destructive delegate writes need the tech lead's permission (fail closed)
status: accepted
date: 2026-09-15
tags: delegation, safety, fs_write_file, ask_upwards, small-model
summary: A delegate's fs_write_file that would delete existing declarations is refused unless the parent model approves it via a new UpwardAsk::approve; with no parent to ask it fails closed, and the refusal is shown in the delegate's sub-chat.

## Context
In the delegated smoke trial a 3B delegate was asked to add one function. It made the change by rewriting src/lib.rs with fs_write_file and silently DELETED the crate's own pre-existing test (greet_works); pom_run_tests then reported "2 passed" (which included the new test) and the delegate reported success. Earlier in the same session it deleted the `greet` function and the test module outright. The parent (tech lead) never learned that code had been destroyed, and the deletion is what let the run look green.

## Decision
A delegate's write that would DELETE code is gated on the parent model's permission. Concretely: the delegate sub-agent loop calls `refuse_destructive` before dispatching a tool call; for `fs_write_file` it reads the target file, computes `comrade_tool::removed_declarations(before, after)`, and when that is non-empty it asks the parent through a new `UpwardAsk::approve(title, detail)` (the ask_upwards channel) and acts on the Verdict: Approved runs the write, Denied refuses with the parent's reason, Unavailable (no parent wired, no answer within 60s, or a reply that is not a verdict) refuses outright — destruction FAILS CLOSED. The refusal is returned as the tool result and emitted as a tool_call/tool_result event pair, so it is visible in the delegate's sub-chat. `UpwardAsk::approve` has a default implementation returning Unavailable, so a parent that does not implement permission checks fails closed rather than opening up. Verdict parsing is fail-closed too: only a reply whose first substantive line opens with APPROVE approves.

## Rationale
The human's steer: destructive actions should need the parent model's permission, and a delegate should be able to ask (ask_upwards). Enforcing it in the loop rather than in the tool means it cannot be forgotten by a delegate, it needs no prompt compliance from a 3B model, and it fails closed when there is nobody to ask - which is exactly the headless/unattended case where a silent deletion is most expensive. Reusing the existing upward channel keeps the parent's attention on the task it delegated instead of the worker's guesswork.

## Alternatives considered
(1) The old behaviour: a warning in the fs_write_file result - tried first, and measured insufficient; a 3B model reads its own rewrite as success and stops. (2) An approval-gated fs_write_file via ctx.confirm - rejected: a delegate's context is auto-approved by design (see the module docs), so it would be a no-op there, and it would need a wide ToolContext change. (3) Always refuse any declaration-deleting write - rejected by the user's steer: the lead may legitimately need to delete code (a rename, a removal) and must be able to say yes. (4) Snapshot/auto-restore the file when a rewrite loses code - rejected: silently reverts work the lead approved and hides the problem instead of surfacing it.

## Scope
Delegated sub-agents only (the delegate sub-agent loop, both the native and ReAct dispatch paths). Declarations are name-scanned (`fn`/`function`), deliberately not parsed, and an ASCII identifier scan is used. Does NOT gate the main agent (its approval is the human's, via ctx.confirm), does not gate file creation, and does not yet cover deletions made through `shell` or a rename.

## Impact
A delegate can no longer destroy the user's code silently; at worst it is forced into a smaller fs_edit. Cost: one extra parent-model call, only when a deletion is actually detected (a new file, an addition, or an unchanged rewrite never triggers it), bounded by a 60s timeout so it cannot hold a delegate's budget open. The main agent is unaffected: its fs_write_file still asks the human through ctx.confirm, or (autonomy=auto) writes and gets the tool's "this removed X" warning. Follow-ups: the same gate should cover `shell` commands that delete or overwrite (rm, git checkout --, truncation redirects) and a `ts_rename` that drops call sites; the rejection path for a DENIED write currently has no retry-once allowance.

