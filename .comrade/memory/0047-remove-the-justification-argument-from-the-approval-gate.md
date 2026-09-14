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

