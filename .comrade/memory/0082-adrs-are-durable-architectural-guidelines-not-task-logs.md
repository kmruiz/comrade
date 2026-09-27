# 0082 - ADRs are durable architectural guidelines, not task logs; the lead hands delegates the ADRs that apply
status: accepted
date: 2026-09-27
tags: memory, adr, prompts, delegation, guidelines, comrade-core
summary: The ADR bar is tightened in the prompts and the record_adr description - an ADR is a durable architectural/design guideline that the agent, other developers and delegates must follow, written to be read by someone implementing a feature, never a task log or a one-off choice - and the lead is told to read the relevant ADRs before planning and cite their ids in a delegate's step context, which the delegate must then read and follow.

## Context
The memory had grown to 80+ ADRs, many of them documenting a session's change ("we added/changed X") rather than a durable rule another developer or delegate must follow. The human's point: ADRs must only be architectural guidelines for the agent itself, other developers and delegates; and, conversely, the agent can and should tell a delegate to read the relevant ADRs to implement a feature. Delegates already carry `find_adr`/`read_adr`/`find_glossary`/`read_glossary` read-only (crates/comrade-tui/src/main.rs `delegate_registry`), so the channel exists - it just was not being used or framed that way.

## Decision
1. Tighten the bar in the model-facing prompts and the tool description:
   - crates/comrade-core/prompts/memory.md: ADRs are "ARCHITECTURAL GUIDELINES, nothing else" - a durable RULE or DESIGN others must follow, written to be read by someone implementing a feature (decision, rationale, alternatives, scope, impact). Explicitly exclude task notes, session logs, bugfixes, refactors, "we added X", how-tos and one-off choices; the test is "would another developer or delegate need this as a RULE?".
   - crates/comrade-core/prompts/tools-intro.md: the memory row now frames ADRs as guidelines read before planning and citeable in a delegate's context.
   - crates/comrade-core/prompts/working-style.md step 6: record only rules a future agent needs.
   - The `record_adr` ToolSpec description (crates/comrade-tool-memory/src/lib.rs) states the same bar.
2. The lead hands delegates the ADRs that apply: crates/comrade-core/prompts/delegate-by-default.md gains a step-writing rule - "if the step must follow a project rule, name the ADR id in the step's `context` (e.g. 'follow ADR 0042'); the delegate can read it with `read_adr`".
3. crates/comrade-core/prompts/delegate-system.md step 1: if the Context cites an ADR by number, read it with `read_adr` first - it is the project's rule for this change, and the delegate must follow it.
4. The glossary gained an "ADR (when to record)" entry documenting the same policy.

## Rationale
Overuse dilutes the signal: when every change is an ADR, none is a guideline. The operational test "would another developer or delegate need this as a RULE?" is what separates a guideline from a log. Delegates cannot see the lead's reasoning or its memory search, so the lead - which did the reconnaissance - must pass the ADR ids in the step context; the delegate then reads exactly those and no more, consistent with its "trust the lead, do not re-explore" contract.

## Alternatives considered
(a) Keep the old broad bar ("important long-term decision") - REJECTED: it is what produced 80+ ADRs. (b) Delete or bulk-trim the existing ADRs - OUT OF SCOPE: this records the policy; pruning is a separate cleanup. (c) Let the delegate search ADRs itself (`find_adr`) instead of being handed ids - REJECTED: the lead's reconnaissance is the trusted channel, and a small delegate should not re-explore. (d) Put the policy only in the prompts, no ADR - REJECTED: it is itself a durable guideline other sessions/delegates must follow, so it belongs in memory.

## Scope
Model-facing prompt wording, the `record_adr` description, the `merge_adr` description and behaviour, and the glossary. The `merge` implementation changed so it stops producing silt; the store format and the delegate loop are unchanged.

## Impact
The agent records fewer, higher-signal ADRs and delegates receive the ids of the ADRs they must follow. Tests: `react::dev_prompt_tests::prompt_encodes_adr_decisions_and_glossary` updated to the new wording, and `store::tests::merge_appends_and_removes_the_source` (the merged source file must be gone, not a superseded stub). The cleanup ran in two passes. Pass 1 removed 30 ADRs - 13 pure UI/backlog records and 17 superseded fragments whose bodies already lived in a kept rollup. Pass 2 folded 11 feature-defining ADRs into rollups and dropped the fragments: new rollups 0083 (proactive sensors, from the sensors cluster), 0084 (LLM client provider/prompt-caching, from the client cluster) and 0085 (harness tool additions: background jobs, search/web/git/check tools, summarise, show_diagram); and 0049 folded into 0022 and 0074 into 0079. The provenance headings and the few inline references were de-numbered so nothing dangles; the store parses all 44 and there are no references to a removed id. As a code change, `merge_adr` now CONSUMES its sources (crates/comrade-tool-memory/src/store.rs `merge`): each source body is appended under a `## Merged: <title>` heading and the source file is deleted, so future merges cannot reintroduce superseded stubs - update the tool/glossary wording to match (done). The deleted files are tracked by git, so they remain recoverable until commit.
