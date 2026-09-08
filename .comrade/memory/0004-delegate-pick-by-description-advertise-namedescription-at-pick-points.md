# 0004 - Delegate pick-by-description: advertise name+description at pick points
status: accepted
tags: delegate, tool-advertisement, description, selection
summary: DelegateCfg.description is now shown to the tech lead wherever a delegate is picked (delegate tool doc, model-arg schema, unknown-model error, TUI model panel); selection stays exact-name.

## Context
DelegateCfg.description ("when to use this model" blurb) was dead config: the delegate tool advertised only bare names (`- mistral`), so the parent LLM could not choose a developer by what its description said. Feature request + user decision: advertise, do NOT add description matching.

## Decision
1. crates/comrade-core/src/delegate.rs: private fn delegate_line(name, description) renders `  - name` or `  - name: description` (blank blurbs degrade to bare name). Use it for (a) the "Configured delegates — pick the one whose description best fits the task:" section of the delegate tool description, (b) the schema `model` property description (repeats the pick list), (c) the unknown-delegate-model bail message. Selection stays an exact-name enum.
2. crates/comrade-tui/src/tui.rs model panel: append ` — <description>` (dim) to each delegate line; clipped by the panel.
3. Tests in delegate.rs: schema_advertises_models_and_required_args asserts spec description + model-arg description contain "name: name test delegate"; invoke_rejects_unknown_model asserts the error lists "cheap: cheap test delegate"; new blank_blurbs_degrade_to_bare_name_lines covers empty blurbs.

## Consequences
set_plan's `model` field (comrade-tool-session, no delegate config) still cannot advertise delegates; the parent learns names from the delegate tool listing, which is unchanged behaviour. NOT implemented (user chose advertise over match): no fuzzy description matching in the model arg. Verify with: cargo test -p comrade-core delegate, then full cargo test (all green as of commit).

