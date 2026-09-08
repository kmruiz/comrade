# 0026 - Risk dropped from the approval gate — Justification only
status: accepted
tags: approval, gate, justification, refactor, ux
summary: Approval gate now requires Justification only (risk field removed end-to-end: ApprovalNotes, ParsedTurn, ToolCard, events, prompts, gating, schema, descriptions).

## Context
User directed change (supersedes the Justification+Risk gate): "Let's remove the risk field on the approval gate." Previously, approval-gated tools (write_file, rename, shell, remember, amend_decision, delegate, apply_edit/patch, ...) required the model to supply BOTH a `Justification` and a `Risk` line/arg before the human-confirm dialog showed. Rationale: the risk text was noise; the human decides anyway.

## Decision
Approval gate now requires ONLY `Justification`. 1. comrade-tool ApprovalNotes lost `risk`; render() emits just the Justification line. 2. react.rs ParsedTurn lost `risk`; parse_turn no longer extracts a Risk section. ReAct prompt text: write Justification between Thought and Tool; native calls pass only `justification`. 3. agent.rs: both gates (ReAct loop + run_native_calls) refuse a gated call only when justification is missing; augmented_spec advertises only `justification` in the schema; Prepared/event no longer carry risk. 4. AgentEvent::ToolCall (session.rs) and tui.rs ToolCard lost `risk`; tool-card render/search/dialog no longer show a risk line. 5. Tool descriptions (remember, amend_decision, shell) say "include Justification". Tests updated. VERIFICATION: cargo test -p comrade-tool, -p comrade-core (92), -p comrade-tui (85) all green.

## Consequences
Stray "Risk:" text is now tolerated, not required, in three places (kept deliberately): react.rs SECTION_STOPS (so a stray Risk line still terminates justification extraction), tui.rs strip_react_scaffolding list (old model output won't leak into the transcript), and react.rs/tui.rs tests. When adding any new approval-gated tool or editing prompts/UI text, do NOT reintroduce Risk.

