# 0001 - ask_advise tool: read-only advisory consult of a delegate
status: accepted
date: 2026-09-09
tags: tools, delegate, advice
summary: ask_advise: consult a configured delegate for advice via a read-only sub-agent, no handoff/plan/approval

## Context
The main model kept using the `delegate` hand-off (or its own judgement) even when it only wanted a second opinion, e.g. how to plan/split a task before committing to the shape of the work. `delegate` marks plan steps working, grants an auto-approved full tool registry and a human handoff approval — heavyweight for a consult. A new `ask_advise` tool was requested that lets the lead ask a configured delegate for advice without handing off any work.

## Decision
Added an `ask_advise` tool (comrade-core/src/advise.rs): the lead passes `model` + `question` (+optional `context`) and one configured delegate runs a READ-ONLY sub-agent (reusing delegate.rs's run_delegate_subagent) then replies with advice text. Its tool registry is exactly the main loop's read-only classification (`agent::is_read_only`, exposed pub; whitelisted in comrade-tui's advise_registry()) so advisors can browse/search/git-log/consult memory+web but can never write/edit/run/commit/plan/ask. It is not approval-gated, never touches plan steps, and costs one extra model conversation. `delegate` and `ask_advise` share one source of truth for [[delegates]] validation (`delegate::build_targets`) and the delegate target struct; delegates are denied `ask_advise` (DENIED_FOR_DELEGATES) so a sub-agent cannot spawn extra model chats.

## Rationale
Advice should be grounded (user chose read-only tools) but must never mutate state; reusing the delegate sub-agent loop gives protocol parity (native + react delegates) with a different system prompt and nudge wording, and reusing agent.rs's is_read_only avoids a third tool-classification list drifting from the main loop.

## Alternatives considered
1) Text-only advice (no tools at all): cheapest, but an advisor cannot ground advice in the repo; user explicitly chose read-only tools. 2) Reuse the delegate sub-agent with the full tool registry minus DENIED_FOR_DELEGATES: would let advisors run tasks/mutate, defeating 'advice not execution'. 3) A separate hand-maintained whitelist of read-only tools: rejected because it would drift from the main loop's read-only classification (agent.rs READ_ONLY_TOOLS).

## Scope
covers comrade-core AskAdviseTool, its prompt (prompts/advise-system.md), advise_registry() in comrade-tui, tool deny/approval classification. Does not cover giving advisors non-read-only tools or plan-step integration.

## Impact
The lead can consult cheaper/faster configured delegates as advisors (prompt delegate-by-default.md now points advisory reviews at ask_advise). Registry building in comrade-tui is duplicated across delegate_registry()/advise_registry() but both are thin filters over shared name-level truth. Tool only exists when [[delegates]] are configured. UI: ask_advise shares the delegate tool icon/headline/reply-card parsing.

