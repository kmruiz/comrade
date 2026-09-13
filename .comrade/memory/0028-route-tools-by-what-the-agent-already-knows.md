# 0028 - Route tools by what the agent already knows
status: accepted
date: 2026-09-13
tags: prompts, tool-routing, semantic-search
summary: Model-facing tool guidance picks a tool by what the agent already knows (project facts/memory/exact text/symbol/meaning/nothing), with fs_rgrep a first choice for literal text and semantic_search for meaning-only lookups.

## Context
semantic_search shipped, so the prompt routing guidance had to say when to use it vs the keyword tools. Three places carried overlapping/partly-contradicting routing text: crates/comrade-core/prompts/tools-intro.md (the 'Choose the most specific tool' list), working-style.md step 3 ('Orient with the CHEAPEST tool'), and each ToolSpec.description first line (capped at 170 chars by react::render_tool in text mode, full in native mode). A past ADR (#6) established terse imperative runbook prompts for small models.

## Decision
Adopt one routing rule stated consistently: pick the tool by what you already know. Project facts (deps/layout/tasks) -> pom_model (do NOT read Cargo.toml). Past decision / project term -> find_adr/read_adr, find_glossary/read_glossary. Exact text (config key, error string, comment, name inside a string) -> fs_rgrep, which is a FIRST choice for literal text, not a last resort. A symbol name (fn/struct/enum/field) -> ts_find_symbol/ts_read_symbol/ts_find_references/ts_structural_map (prefer over fs_read_file for code). Only the meaning, not the word -> semantic_search (searches ADR/glossary AND code). Nothing known yet -> fs_list_files/fs_list_dir first. shell stays the documented LAST RESORT.

## Rationale
Routing by 'what you know' is a single mental test a small model can apply every turn, and it removes the ambiguity between the four search-ish tools (find_adr/fs_rgrep/ts_*/semantic_search). fs_rgrep is the cheapest correct tool for literal strings, so calling it a last resort pushed agents into a semantics engine for a literal job; only the shell earns 'last resort'.

## Alternatives considered
(1) Original four buckets (pom tools=understand project, semantic_search=abstract, ts=specific symbols, rgrep=last resort) - rejected: 'understand the project' overstates pom_model (pom_run_task only runs things), 'abstract' is vague, and demoting fs_rgrep contradicts its strong literal-search fit. (2) Edit only one of the three locations - rejected: the others would contradict it. (3) Rewrite the tool descriptions from scratch - rejected: most already carried correct routing; only fs_rgrep/semantic_search first lines needed the new emphasis.

## Scope
Model-facing routing prose only: tools-intro.md (rewritten as the what-you-know table), working-style.md step 3, delegate-system.md step 2 (delegates get per-tool lines only, not the ## Tools section), and the first lines of the fs_rgrep and semantic_search ToolSpec descriptions. Does not change tool behaviour, schemas, or the pinned prompt contract tests.

## Impact
All model-facing routing now agrees. Wording-pinning tests (react.rs dev_prompt_tests/trust_tests, delegate.rs prompt tests) stay green; the routing lines themselves are not pinned, so future edits to the table are cheap. Follow-up: keep future tool descriptions consistent with the table; re-check small-model context size if the ## Tools section grows.

