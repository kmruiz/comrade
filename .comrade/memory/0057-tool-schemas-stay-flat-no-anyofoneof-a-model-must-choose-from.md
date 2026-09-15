# 0057 - Tool schemas stay flat: no anyOf/oneOf a model must choose from
status: accepted
date: 2026-09-15
tags: tools, schema, small-model, lm-studio, prompts
summary: Model-facing ToolSpec schemas must be a single flat shape with an explicit top-level `required`; `anyOf` breaks a local OpenAI-compatible server's tool-call grammar (the model emits empty/garbage calls) and `oneOf` silently drops the alternative-required field.

## Context
Making Comrade work end to end with mistralai/ministral-3-3b served by LM Studio (OpenAI-compatible, http://localhost:1234/v1). Smoke runs kept derailing: the delegate emitted dozens of bogus tool calls and an `fs_edit` call carrying only `path` (or only `old`), then flailed into random shell commands (echo/tr/toupper/!), and a 60-tool-call run hit max_iterations. Direct curl A/B against the server with the real schema isolated the cause: `fs_edit` advertised two call shapes via `anyOf` ((path+old+new) | diff) with additionalProperties:false. With that schema the model produced ~60 EMPTY-argument calls per request; with a flat `required: ["path","old","new"]` and no `diff` property it produced the correct three keys 3/3. A second probe showed a top-level `oneOf` (index | text) on self_update_plan makes the model omit `index` entirely (args = {"status":"done"}), which the tool then rejects as "no step identified". The same server compiles the JSON schema into a generation grammar, so schema shape - not prompt wording - decides whether a small model can even form a valid call.

## Decision
Model-facing ToolSpec.json_schema must be FLAT: one shape, every field the model must supply listed in the top-level `required`, `additionalProperties: false`. Alternatives are expressed as optional properties plus runtime validation, never as `anyOf`/`oneOf`/`allOf` over required-subset branches. Concretely: fs_edit advertises literal mode only (path+old+new required, NO `diff` property; the unified-diff patch mode stays an accepted RUNTIME argument for internal callers), and self_update_plan / self_set_step_model / self_set_step_context require `index` (the `text` selector stays a runtime fallback for a caller that cannot name the index).

## Rationale
Measured, not theoretical: the flat schema made the model emit well-formed calls every time, the anyOf schema made it emit none. The tool description is still readable by bigger models, and nothing in our own code validated args against the schema, so reshaping it costs nothing at runtime. Keeping `diff` accepted (but unadvertised) preserves every internal caller and the existing patch-mode tests.

## Alternatives considered
(1) Keep the anyOf and rely on a better error message - rejected: the model retried the malformed call three times and ignored the error. (2) Split fs_edit into fs_edit + fs_patch so each has a flat schema - rejected: more tools to choose from, and the probe showed the model handles the single flat fs_edit correctly. (3) Keep `diff` as an optional property of fs_edit - rejected: probed, the model then ALSO emits a bogus `diff`, which would flip the tool into patch mode and fail. (4) Make the fields optional and let the runtime error - rejected: probed, the model then omits them.

## Scope
ToolSpec.json_schema of model-facing tools. Pinned by tests: comrade-tool-fs `fs_edit_schema_is_flat_literal_only` and comrade-tool-session `plan_step_tools_advertise_a_flat_step_selector`. Deliberately NOT changed: amend_adr's `anyOf` and ask_form's nested `options` `anyOf` - both probed harmless (amend_adr produced id+status+note; ask_form is interactive-only and off the small-model path) and flattening them would either force a wrong field or lose a call shape. Not covered: ReAct (text) protocol schemas beyond the descriptions.

## Impact
A small local model can now form valid fs_edit and plan-update calls, removing the single largest source of derailment (empty/garbage tool calls). Trade-off: a model can no longer discover fs_edit's patch mode or the `text` step selector from the schema - both remain available to code and to a caller that passes them explicitly. Follow-up: if amend_adr/ask_form ever show the same degeneration, flatten them the same way (ask_form would need its `options` union split into two properties).

