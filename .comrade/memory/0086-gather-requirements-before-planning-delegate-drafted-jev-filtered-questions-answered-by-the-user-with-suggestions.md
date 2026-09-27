# 0086 - Gather requirements before planning: delegate-drafted, Jev-filtered questions answered by the user with suggestions
status: accepted
date: 2026-09-27
tags: requirements, prompting, jev, ask_advise, ask_form, tools, comrade-core
summary: For a feature or bug the lead must clarify first: it drafts clarifying questions (with a delegate via ask_advise), filters them through Jev with the new `evaluate_questions` tool (the user request as state, one noul per question), and asks the user only the accepted FEATURE-level ones via `ask_form`, always with a suggested answer; technical questions pass only for big architectural changes.

## Context
The lead tended to plan and implement straight from a terse request, guessing intent, instead of pushing back for requirements. The human wants it pushier: consult the delegates on whether the feature is clear and for clarifying questions, have Jev judge whether those questions are good, then ask the user - from a FEATURE standpoint, technical only for big architectural changes, and always with suggestions. The pieces already existed (a read-only delegate consult `ask_advise`, an interactive `ask_form` with `recommended` values, and the shared Jev client from ADR 0079/0080); what was missing was the question filter and the prompting.

## Decision
1. New tool `evaluate_questions` (crates/comrade-core/src/requirements.rs): input `{ request, questions }`. It sets Jev's `state` to the user request verbatim and asks ONE `noul` per candidate question - "is this a GOOD clarifying question to ask the user?" - whose criteria demand a FEATURE-level question, allow a technical one only when the request implies a big architectural change (a new service/dependency, a data-model or protocol change, a security/performance trade-off), and reject implementation-detail or internally-answerable questions. It returns DATA: each question with its probability and an `ACCEPTED`/`REJECTED` label (accept at `>= 0.60`), never an imperative (the ## Trust boundaries rule).
2. New prompt section crates/comrade-core/prompts/requirements.md, included by react::build_system_prompt only when `evaluate_questions` is advertised. For a feature or bug it tells the lead to: restate the request and name what is ambiguous; consult a delegate with `ask_advise` ("is it clear enough to build? list the clarifying questions and your suggested answer to each"); draft questions FROM A FEATURE STANDPOINT (technical only for a big architectural change); filter them with `evaluate_questions`; ask the user the accepted ones with `ask_form`, giving every field a `recommended` value and a one-line rationale; only then plan (## Test-first still applies). It is skipped when the request is unambiguous or trivial.
3. `## Tools` gains a routing row and `crates/comrade-core/prompts/working-style.md` step 2 a pointer, both conditional on the tool.
4. The tool is registered in comrade-tui only when a Jev key is configured, sharing the `Jev` client with the guardrail and `validate_tests`; it is lead-only.

## Rationale
Clarifying up front is far cheaper than building the wrong thing, and the user asked for it to be the default for features and bugs. Jev keeps the user from being interrogated: it drops over-technical, implementation-detail, and internally-answerable questions, so only a few high-value feature questions reach `ask_form`. Requiring a `recommended` value makes answering one click and forces the lead to have an opinion. Keeping the tool result data-only preserves the trust-boundary rule the guardrail work established (directives live in the trusted prompt, not in tool output).

## Alternatives considered
(a) Ask the user directly with no filter - REJECTED: the lead would ask too many or too technical questions. (b) Hard-gate planning on answered questions - REJECTED: the human chose advisory (be pushy in the prompt, do not block). (c) Have a delegate ask the user - REJECTED: delegates are non-interactive by design (no user IO). (d) One Jev `choice` question for the whole set - REJECTED: the human specified one `noul` per question so each is scored independently. (e) Put the questions in the tool result as instructions - REJECTED: directives belong in the prompt (## Trust boundaries).

## Scope
A new Jev-backed tool and prompt section, advisory. Does not change the delegate loop, the guardrail, or the plan format; reuses the shared `Jev` client and the `[guardrails]` key. With no key the tool and section are absent and behaviour is unchanged.

## Impact
With a Jev key, the lead clarifies features/bugs through `ask_advise` -> `evaluate_questions` -> `ask_form`, with suggestions. Tests: `requirements::tests::review_labels_questions_by_threshold` (data-only labels) and `questions_may_arrive_as_a_string_or_an_array`, and `react::dev_prompt_tests::requirements_section_is_included_only_with_evaluate_questions`. README documents the flow.
