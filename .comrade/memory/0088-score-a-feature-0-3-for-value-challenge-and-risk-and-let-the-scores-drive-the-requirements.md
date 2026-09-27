# 0088 - Score a feature 0-3 for value, challenge and risk; the scores drive how much to gather and when to raise
status: accepted
date: 2026-09-27
tags: requirements, scoring, jev, prompting, tools, comrade-core
summary: Before gathering requirements the lead scores the feature with Jev via the new `score_feature` tool - customer value, technical challenge and UX challenge (each 0-3) plus the risk of a negative architecture or product impact (0-1) - and the `## Requirements` prompt uses those scores to decide: raise a high architecture/product risk with the user at once, question a low customer value, and gather more information when the technical/UX challenge is high.

## Context
The lead treated every request as worth building and equally hard, with no judgement of a feature's value, difficulty or risk. The human wants it to score a feature 0-3 on customer value, technical challenge and UX challenge, gather more information based on that, and RAISE it when a feature would negatively impact the architecture or the product. It reuses the shared `Jev` client (ADR 0079/0086/0087).

## Decision
1. New tool `score_feature` (crates/comrade-core/src/scoring.rs), registered in comrade-tui only when a Jev key is configured: input `{ feature }` (the request verbatim). It asks Jev three `score` questions over four levels (0-3) - `customer_value`, `technical_challenge`, `ux_challenge` - and two `noul` risks - `architecture_risk`, `product_risk` (0-1). It returns DATA: each score with its level label (from the response legend, else the nearest level) and the two risk probabilities. No imperative.
2. The `## Requirements` prompt (crates/comrade-core/prompts/requirements.md) gains the scoring as its first step, with thresholds that turn the numbers into actions: a high architecture or product risk (`>= 0.6`) is RAISED with the user at once, before more work; a low customer value (`<= 1`) questions whether the feature is worth building; a high technical or UX challenge (`>= 2`) means gather MORE information before planning.
3. The tool is registered alongside the other Jev tools (`evaluate_questions`, `rank_alternatives`, `validate_tests`); README and glossary document it.

## Rationale
A cheap objective score keeps the lead from building low-value features or walking into an architectural/product regression, and it sizes the requirements effort (a hard or risky feature earns more questions and alternatives). Jev gives the numbers; the thresholds live in the prompt so the policy stays visible and tunable, and the result is data-only (## Trust boundaries). Raising rather than blocking matches the human's wording: the user is warned, the decision stays theirs.

## Alternatives considered
(a) Let the lead judge value/difficulty in prose - REJECTED: subjective and easy to skip. (b) A single 0-3 "priority" - REJECTED: the human named three separate dimensions plus the risk. (c) Auto-stop a feature with a high risk - REJECTED: raise it with the user, do not block. (d) Put the thresholds in the tool result as instructions - REJECTED: the tool returns data; the prompt decides. (e) Fold it into `evaluate_questions` - REJECTED: scoring the feature and filtering the questions are different judgements.

## Scope
A new Jev-backed tool and a step in the requirements prompt, advisory. Reuses the `[guardrails]` key and the shared `Jev` client; with no key the tool and step are absent and behaviour is unchanged. Does not change the delegate loop, the plan format, the challenge step, or the TDD flow.

## Impact
With a Jev key the lead scores every feature and lets the scores drive the requirements and the raise. Tests: `scoring::tests::scores_render_as_data` (data-only) and `nearest_level_maps_the_weighted_score`. README documents the scoring.
