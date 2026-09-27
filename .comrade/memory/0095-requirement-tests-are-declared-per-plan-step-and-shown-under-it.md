# 0095 - Requirement tests are declared per plan step and shown under it
status: accepted
date: 2026-09-27
tags: plan, tdd, tests, tools, ui, comrade-tool-session, comrade-tui
summary: In the TDD flow the lead declares the tests a plan step must satisfy with the `self_set_requirement_tests` tool; they are stored on `PlanStep.tests` (name + file:line) and rendered in the plan panel under that step as "requirement tests".

## Context
ADR 0080 introduced the TDD flow (write tests, validate coverage with Jev, then implement) and named a follow-up - "record the accepted test set in the session so a later step can re-check it". The human asked for the declared tests to be visible in the plan panel, with each test's name and its file:line, attached to the step the tests belong to.

## Decision
1. Model: `comrade_tool::RequirementTest { name: String, file: String, line: u32 }` (line 0 = unknown) and `#[serde(default)] pub tests: Vec<RequirementTest>` on `PlanStep` (NOT on `PlanStepDraft`). Because `PlanStep` is already serialized in every plan (core `restore`, TUI `SessionFile.plan`), tests persist for free; `set_plan` rebuilds steps with an empty list, so they are cleared with the plan.
2. Session: `SessionControl::set_step_tests(&self, target: &PlanTarget, tests: Vec<RequirementTest>) -> Result<bool, String>` (default errors). The `AgentSession` impl matches the step by `PlanTarget`, rejects an empty list or a blank name/file, refuses a step that is `InProgress` or `Done`, sets `step.tests`, emits `PlanChanged`, and does NOT touch the step's status.
3. Tool: `self_set_requirement_tests` in `crates/comrade-tool-session` (registered in `all()`), args `{index|text, tests:[{name, file, line}]}`; re-declaring replaces the step's list.
4. UI: the plan panel renders, under each non-Done step, a bold `requirement tests:` header and one row per test `- <name> (<file>:<line>)` via the pure `requirement_tests_lines(tests, width)` helper (wrapped to the panel width; a `line` of 0 omits the `:line`). A `Done` step stays a single collapsed row and does not show its tests (consistent with the note/verification it already drops).

## Rationale
Attaching the tests to the step (rather than a plan-level list or a side map keyed by step id) keeps one source of truth, serializes with the existing `PlanStep`-in-plan path, and makes the requirement visible exactly where the work is. A dedicated tool keeps `validate_tests` side-effect-free (ADR 0080) and matches the existing `self_set_step_context`/`self_set_step_model` family; the session clears them with the plan so a retired plan leaves nothing behind (ADR 0092).

## Alternatives considered
(a) A plan-level "requirement tests" section listing all tests regardless of step - rejected by the human, who chose per-step attachment. (b) A parallel `HashMap<step_id, Vec<RequirementTest>>` in `AgentSession` (like the `delegated` set) - rejected: it needs its own persistence path and clearing, while a `PlanStep` field serializes with the plan. (c) Extending `validate_tests` to also record tests - rejected: the tool takes test CODE, not locations, and ADR 0080 keeps it side-effect-free. (d) A new field on `PlanStepDraft` (declared at plan time) - deferred: kept out to avoid churn in every `set_plan` literal; tests are declared once written, after the plan exists.

## Scope
Covers how requirement tests are declared, stored, and shown. Does NOT change `validate_tests`/Jev, the delegate loop, or the plan lifecycle; it does not itself run tests or verify them.

## Impact
New `PlanStep.tests` field (all `PlanStep` literals must set it), new `set_step_tests` trait method, new `self_set_requirement_tests` session tool, and the plan-panel rendering plus the `tdd.md` prompt mention. Tests: `comrade-tool-session` tool tests (record/trim, target by text, need index-or-text, reject blank name), `comrade-core` `session::tests::set_step_tests_replaces_tests_and_guards_terminal_steps`, and `comrade-tui` `plan_step_tests::requirement_tests_*` (name+file:line, absent, no line, wrapping).

