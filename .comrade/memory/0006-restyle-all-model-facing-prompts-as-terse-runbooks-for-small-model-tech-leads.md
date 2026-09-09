# 0006 - Restyle all model-facing prompts as terse runbooks for small-model tech leads
status: accepted
date: 2026-09-09
tags: prompts, small-model, runbook, delegation, tool-descriptions
summary: Restyled every model-facing prompt (tech-lead sections, sub-agent bodies, ToolSpec descriptions) as terse runbooks for small-model support; merged duplicate delegation sections and kept all wording-pinning tests green.

## Context
Goal: let any small-ish model act as the Comrade tech lead. Audited every model-facing prompt: the tech-lead system prompt assembled by react::build_system_prompt from markdown sections in crates/comrade-core/prompts/, the sub-agent bodies for delegate/ask_advise (prompts/delegate-system.md and advise-system.md, substituted by delegate::render_subagent_system), and every ToolSpec.description rendered per-tool into the prompt (ReAct caps the first line at ~140-170 chars; native function calling sends the full description plus json_schema property descriptions). Findings: two delegation prompt sections (delegate-by-default.md and delegation-lead.md) were both appended and overlapped heavily; working-style/memory were long narrative; many ToolSpec descriptions were 200-600 char walls. Delegating the tightening to the configured small delegate (ministral-3-3b) failed server-side with 'Context size has been exceeded' — evidence that verbose prompts/schemas exceed small-model windows, so the per-crate work had to be done by the main agent.

## Decision
Merge the two delegation sections into one lean '## Delegate by default' runbook (delegation-lead.md deleted; DELEGATION_LEAD const and its inclusion removed from react.rs). Rewrite working-style.md and memory.md as numbered short runbooks. Rewrite delegate-system.md and advise-system.md as short runbooks. Compress ToolSpec descriptions in comrade-tool-fs/memory/project/session/syntax/web to one or two short lines while preserving routing guidance and semantics. Keep all wording-pinning tests (react.rs dev_prompt_tests/trust_tests, delegate.rs prompt tests) unchanged and green.

## Rationale
Short, structured, imperative text with one idea per line is more reliably followed by small models than dense multi-clause prose; fewer tokens also fit small context windows. Test-pinned semantic contracts were preserved unchanged so behaviour did not change.

## Alternatives considered
(1) Full rewrite from scratch of every section without keeping pinned phrases — rejected: would silently drop the semantic contracts the tests encode. (2) Blind sentence truncation of descriptions by script — tried first; dropped key routing hints (e.g. project_model 'instead of reading Cargo.toml'), replaced with hand-written compressed text. (3) Delegating per-crate tightening to ministral-3-3b — rejected after all three parallel runs failed with model context overflow.

## Scope
Model-facing prompt text only: markdown prompt sections, sub-agent system bodies, and ToolSpec.description strings. Does NOT cover json_schema per-property 'description' strings (still verbose in native mode) nor the delegate/ask_advise ToolSpec descriptions in comrade-core.

## Impact
Tech-lead prompt is shorter and the duplicated delegation guidance is gone. Tool descriptions in the tool crates are now compact. Follow-ups: trim the comrade-core delegate/ask_advise ToolSpec descriptions (still long, duplicate the delegation runbook) and the long json_schema per-property description strings; re-check whether the delegate sub-agent tool listing still exceeds small-model context (it previously made ministral-3-3b overflow).


## Note
Follow-ups done 2026-09-10. (1) delegate and ask_advise ToolSpec descriptions trimmed from ~3k to ~1k chars (short runbooks ending in the configured-delegate listing); the listing is no longer duplicated in the model-arg schema description. (2) Longest json_schema per-property descriptions compressed (set_plan model/verification, update_plan status, delegate/advise args); no per-property description over ~160 chars remains in the tool crates. (3) Native-mode delegate sub-agent prompts list tools by name only (the native schemas carry the descriptions), shrinking the delegate system prompt. New decisions in the same change: added a read-only git_show tool (show a commit/tag/file-at-revision without the shell; wired into the read-only classifier and TUI icon) and stopped run_task from running tests — tests route to run_tests, which returns a compact failure summary (RUN_TASK_VERBS in pom.rs no longer advertises `test`; the internal test verb stays for run_tests). Also: git_commit now accepts an optional `paths` array to stage only those files; omitted or empty stages everything.
