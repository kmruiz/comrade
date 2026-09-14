# 0047 - Remove the justification argument from the approval gate
status: accepted
date: 2026-09-14
tags: approval, agent, tui, protocol
summary: Approval-gated tools no longer require a model-supplied justification: the ReAct `Justification:` line, the native `justification` argument and the mandatory-field refusal were removed, together with ApprovalNotes, ToolContext::approval and AgentEvent::ToolCall.justification.

## Context
Approval-gated tools (fs_write_file, ts_rename, shell, run_bg) demanded the model supply a reason: in ReAct a `Justification:` line above the Tool line, in native function calls a `justification` argument injected into the tool schema by `augmented_spec`, stripped before dispatch. A missing/empty value was refused with a "repeat the call" error, and a present value was forwarded through `ctx.set_approval(ApprovalNotes{..})` to `ToolContext::confirm`, which rendered it above the confirm dialog's diff. The user asked to remove the justification argument from the approval gate.

## Decision
The justification requirement is gone end-to-end. Deleted in comrade-core/src/agent.rs: the APPROVAL_GATED_TOOLS const, is_approval_gated(), augmented_spec() (and its call in the native tool-spec build), the ReAct-path refusal block, the run_native_calls justification extraction/refusal and the ctx.clear_approval() call. In react.rs: ParsedTurn.justification, the extract_section(..,"Justification:") call and the now-unused extract_section+SECTION_STOPS. In session.rs: AgentEvent::ToolCall.justification. In comrade-tool: ApprovalNotes, ToolContext.approval, set_approval/clear_approval/take_approval; confirm() now passes its `diff` straight to UserPrompt::Confirm. In comrade-tui: ToolCard.justification and its two render sites/msg_searchable. protocol.md no longer documents the `Justification:` line.

## Rationale
The user asked for it: the justification line was friction the model had to satisfy without adding real signal (the confirm dialog already shows the tool's own summary and, for edits, a diff). Purely additive data-flow removal: no gate behaviour changes — the human still confirms via ctx.confirm and auto_approve still short-circuits.

## Alternatives considered
1) Keep validating but stop requiring — rejected as half-measures that leave dead plumbing. 2) Keep the field display-only — rejected, the user asked for full removal. The test-only legacy handling that strips `Justification:` from ReAct scaffolding text (tui.rs strip_react_scaffolding/preview_lines) was left in place since it still guards rendering of older transcripts.

## Scope
Covers the approval gate's justification requirement across agent loop, ReAct parser, session event, TUI card and protocol.md, plus the (now-unused) ApprovalNotes plumbing it fed. Does NOT touch the per-delegate `approval = "ask"/"deny"` policy (ADR #3), security.autonomy/auto_approve, or the confirm dialog itself.

## Impact
The model no longer emits or is refused for a Justification; approval-gated tools are advertised with their plain schema. Public API narrowed: ApprovalNotes and AgentEvent::ToolCall.justification are gone. Bundled with the v0.2.0 minor release.


## Merged from #0008 - Ungate ADR/glossary memory writes (record_adr, amend_adr, record_glossary)
status: accepted
date: 2026-09-09
tags: approval, memory, tools, autonomy
summary: record_adr, amend_adr and record_glossary are fully ungated: removed from APPROVAL_GATED_TOOLS and their ctx.confirm dialogs deleted, since they only write the agent's own .comrade/memory.

## Context
The ADR/glossary memory tools (record_adr, amend_adr, record_glossary) were double-gated like delegate once was (ADR #2): listed in APPROVAL_GATED_TOOLS so the loop demanded a Justification, and each tool called ctx.confirm before writing. Their writes touch only .comrade/memory/, the agent's own durable memory, so a human checkpoint was deemed unnecessary. The user asked to remove the approval gate for ADR and glossary tasks.

## Decision
record_adr, amend_adr and record_glossary are no longer approval-gated: removed from APPROVAL_GATED_TOOLS (agent.rs) so no Justification is required, and the ctx.confirm prompts inside each tool's invoke were deleted so they write .comrade/memory directly. They remain in MUTATING_TOOLS (still count as state changes for the loop tracker and read guard) and remain non-browsable to advisors (advise.rs). Tool descriptions, the memory crate module doc, and prompts/protocol.md were reworded from 'Approval-gated' to 'runs directly without approval'; protocol.md's approval-gated list is now just fs_write_file, ts_rename, shell.

## Rationale
Consistency with the delegate ungating (ADR #2): where the tool's side effects are confined to state the agent owns (.comrade/memory/), the human gate adds friction without protecting user files. Memory writes are also fully undoable via the same ctx.undo capture retained before each write.

## Alternatives considered
1) Keep the Justification gate and only delete the internal ctx.confirm dialogs - rejected: the user asked for a fully ungated write, and the ctx.confirm was the actual human pause, so removing only the classification would not stop the dialogs. 2) Gate on an autonomy config knob - rejected: overkill for writes confined to the agent's own .comrade/memory directory. 3) Leave approval-gated - rejected by request.

## Scope
Covers the three memory tools' approval classification in agent.rs (APPROVAL_GATED_TOOLS), their internal ctx.confirm prompts in comrade-tool-memory/src/lib.rs, the tool descriptions/module doc, and prompts/protocol.md wording. Does not change MUTATING_TOOLS membership, the advisor deny-list (advise.rs), delegate access to these tools, or the other approval-gated tools.

## Impact
The agent can now record and amend ADRs and glossary terms with no human checkpoint, matching git_commit/pom_run_tests/delegate. The remaining approval-gated tools are fs_write_file, ts_rename and shell. If unattended memory writes ever need a checkpoint, a config knob (like the delegate approval policy in ADR #3) is the follow-up.

## Merged from #0032 - Remove the verify-then-commit gate on git_commit
status: accepted
date: 2026-09-13
tags: agent-loop, git, guard
summary: Removed the agent-loop gate that refused `git_commit` until the last change was backed by a green test run, since it misfired on formatter-originated edits; the prompts still advise verifying before committing.

## Context
The agent loop had a 'verify-then-commit monitor': it set a `verified_after_change` flag to false on any code-changing tool (fs_edit, fs_write_file, ts_rename, pom_format_code, shell, delegate) and only back to true on a `pom_run_tests`/`pom_run_task` output containing `test result: ok.`, then hard-refused any `git_commit` while the flag was false (both the ReAct and native dispatch paths, in crates/comrade-core/src/agent.rs). In practice it misfired: a formatter-originated edit (pom_format_code) or any output whose green signal did not match the exact string left the flag false and blocked commits the user considered legitimate.

## Decision
Remove the gate entirely from the agent loop (crates/comrade-core/src/agent.rs): the `git_commit && !verified_after_change` refusals on both dispatch paths, the `verified_after_change` state threaded through `run_calls`/`run_native_calls`, and the `CODE_CHANGES` / `update_verify_state` / `verify_guard_message` helpers with their tests. `git_commit` no longer inspects whether tests were run.

## Rationale
The gate was a heuristic over tool names and output strings that produced false positives (notably on formatter-driven edits) without reliably catching unverified commits; the prompts already carry the verification guidance, so the hard refusal added friction for no dependable safety.

## Alternatives considered
(a) Keep the gate but also treat `pom_format_code` (and other whitespace-only actions) as non-invalidating — rejected: whack-a-mole on a heuristic, and it was still wrong about which outputs count as green. (b) Keep the gate only for non-formatting changes — rejected: extra complexity for little value. (c) Make the gate advisory (a hint) instead of a hard refusal — possible, but the working-style/protocol prompts already instruct running tests to green before committing, so the reminder is redundant.

## Scope
Covers the verify-then-commit enforcement inside the agent loop only. Does NOT change the `git_commit` tool itself, the approval gating of other tools, the read guard, or the loop tracker.

## Impact
The model can commit without a preceding green test run in the same turn; it must judge for itself when the suite is green. The false refusals disappear. The guidance to verify before committing remains in the prompts (crates/comrade-core/prompts/*), so the behaviour is unchanged when the model follows its instructions. `git_commit`'s own argument validation (git_diff/git_status advice, path scoping) is untouched.

## Note
Rollup of approval-gate simplifications: this ADR removed the model-supplied justification argument; #0008 ungated the memory writes (record_adr/amend_adr/record_glossary); #0032 removed the verify-then-commit gate on git_commit. Bodies preserved under "Merged from".
