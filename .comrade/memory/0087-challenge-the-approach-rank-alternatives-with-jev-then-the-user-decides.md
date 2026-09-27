# 0087 - Challenge the approach: rank alternatives with Jev, reason the top 3 with the user, then the user's choice is final
status: accepted
date: 2026-09-27
tags: requirements, alternatives, jev, web-search, prompting, tools, comrade-core
summary: Before committing to a feature the lead must challenge its soundness: look for prior art and alternatives (web_search/web_fetch and the repo), rank them with Jev via the new `rank_alternatives` tool (one choice question over the options), present the TOP 3 to the user with its reasoning via `ask_form`, and treat the user's decision as FINAL - never reopened unless the user asks.

## Context
The lead accepted the first approach - the user's or its own - without checking whether it was sound. The human wants it to challenge the feature: search online for references, find alternatives, have Jev pick which alternative is better, get the top 3, and reason with the user; after that the user's decision stands and is not challenged again. It reuses the shared `Jev` client (ADR 0079/0086) and the web tools.

## Decision
1. New tool `rank_alternatives` (crates/comrade-core/src/alternatives.rs), registered in comrade-tui only when a Jev key is configured: input `{ request, alternatives: [..] }` (2-4 options, a lone string also accepted). It sends Jev `state = { user_request, alternatives: [{id,text}] }` and ONE `choice` question - "which SINGLE alternative is best?" - judging soundness (will it work and fit the codebase/constraints), then simplicity and cost. It returns the full probability distribution as DATA: each alternative with its probability, best first (a new `jev::probabilities` helper reads the map; if no distribution is returned, Jev's single `choice` is put first).
2. New prompt section crates/comrade-core/prompts/challenge.md, included by react::build_system_prompt only when `rank_alternatives` is advertised: look for prior art/alternatives with `web_search`/`web_fetch` and in the repo, write one line per alternative (what it is + its main trade-off), rank them, then present the TOP 3 with `ask_form` - each with the lead's reasoning (why it ranks where it does, what it costs) and a `recommended` value - as a conversation, not a dump. The user's choice is FINAL: stop challenging and build it; do not reopen unless asked. Skip for trivial/unambiguous changes; one round only.
3. `## Tools` gains a routing row and `crates/comrade-core/prompts/working-style.md` step 2 a pointer, both conditional on the tool.

## Rationale
Soundness is cheap to check before building and expensive to discover after. Jev gives an objective ranking across the options - including the user's proposal - so the lead reasons with the user instead of asserting; the fully ranked distribution is what lets it present a top 3 rather than a single pick. Making the user's choice final avoids an endless design debate and matches the human's instruction.

## Alternatives considered
(a) Let the lead simply assert the best approach - REJECTED: not a challenge and not objective. (b) Ask Jev for only the top choice - REJECTED: the human wanted the top 3, so the distribution is used. (c) Have Jev generate the alternatives - REJECTED: it is an evaluator, not a generator; the lead and the web find the options. (d) Re-open the decision later if the lead disagrees - REJECTED: the user said the decision is final. (e) Fold it into the requirements section - REJECTED: it is a distinct step (soundness, not clarity) with its own tool, so it gets its own conditional section.

## Scope
A new Jev-backed tool and prompt section, advisory (no gate). Reuses the shared `Jev` client, the `[guardrails]` key and the web tools; with no key the tool and section are absent and behaviour is unchanged. It does not change the delegate loop, the plan format, or the requirements flow (it runs after requirements, before planning).

## Impact
With a Jev key the lead challenges non-obvious features and the user picks among the top 3. Tests: `alternatives::tests::ranking_lists_best_first_with_probabilities` (data-only ranking) and `alternatives_may_arrive_as_a_string_or_an_array`, and `react::dev_prompt_tests::challenge_section_is_included_only_with_rank_alternatives`. README documents the flow.
