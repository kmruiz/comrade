# 0039 - Decisions become ADRs; project glossary added
status: accepted
date: 2026-09-09
tags: memory, adr, glossary, architecture, comrade-tool-memory
summary: Decision memory is now ADR-style (dated, with rationale, alternatives, scope and impact), recorded only for long-term choices; a new single-file glossary maps project keywords to meanings and code/doc references

## Context
comrade-tool-memory originally stored every session note as a numbered decision file in a run-book shape (summary/context/decision/consequences, ACTION -> VERIFICATION how-tos). The user decided the memory model should change: (1) decisions should become more ADR-like — recording when they happened, rationale, scope, impact and alternatives considered — and the model should store one only when an important thing happened that will impact architecture/design/product on the long term; (2) a new glossary concept should store one file `.comrade/memory/glossary.md` with project keywords, what they mean, and references in code or documentation where they can be found; (3) there must be a way to read the whole glossary, not only search it.

## Decision
1. The decision store (`crates/comrade-tool-memory/src/store.rs`) now writes an ADR template: the header gains `date: YYYY-MM-DD` (defaults to today via a small civil-calendar conversion, no date crate) and the body sections are, in order: Context, Decision, Rationale, Alternatives considered, Scope, Impact (`Consequences` is gone). `find_decisions` lists the date when present. Existing files without a `date:` line still parse — backward compatible, no migration.
2. The `remember` tool is rewritten to the ADR contract (title, summary, context, decision, rationale, alternatives, scope, impact, tags). Its description and the system prompt now restrict `remember` to important long-term decisions; small operational notes, how-tos and step-by-step guides are explicitly not durable memory and are not persisted.
3. A new glossary store (`src/glossary.rs`) keeps a single `.comrade/memory/glossary.md`: one `## term` entry per keyword, sorted alphabetically; each entry has a `> ` one-line meaning (used by search) plus `**References:**` bullets pointing at code or docs. Upsert normalises order and dedupes case-insensitively, and preserves hand-edited sections verbatim.
4. Three new tools: `remember_glossary` (approval-gated add/update of a term: meaning + at least one reference), `find_glossary` (read-only search of terms), and `read_glossary` (read-only; pass a `term` for one entry, omit it to read the WHOLE `.comrade/memory/glossary.md`).
5. comrade-core wiring: `find_glossary`/`read_glossary` are read-only, `remember_glossary` is mutating + approval-gated (agent.rs lists); delegates keep all three (memory tools are not in DENIED_FOR_DELEGATES, and comrade-tui/src/main.rs chains `comrade_tool_memory::all()` for the delegate registry). The root system prompt Memory section (`react.rs`) now frames memory as ADRs + glossary instead of run books.
6. This repo's own `.comrade/memory/` dogfoods the change: entry #0039 is written in the new template and `.comrade/memory/glossary.md` seeds the keywords that define the memory model.

## Rationale
ADRs give the when/why/scope/impact needed to reason about long-term architecture choices, while run-book how-tos diluted the decision history with operational notes. The glossary is lookup data (term -> meaning -> where it lives), so one skimmable file suits it better than per-term numbered files; sharing the `.comrade/memory/` directory keeps all durable memory in one place. Keeping the parser backward compatible avoids a risky migration of the 38 historical entries.

## Alternatives considered
- Single `decisions.md` registry replacing per-decision numbered files: rejected — the per-file store gives cheap ids, per-entry amend/search and whole-file read only on demand.
- Rewriting/migrating all existing entries to the ADR template: rejected — history stays as written; the template applies to new decisions only.
- Glossary as per-term files mirroring decisions: rejected — the user asked for one `.comrade/memory/glossary.md`, sorted.
- Glossary maintained only through generic file tools: rejected — a dedicated upsert keeps the single-file format and sort consistent and is approval-gated like `remember`.
- No whole-glossary tool: rejected — the user explicitly required reading the entire glossary, so `read_glossary` without a `term` returns the whole file.

## Scope
In: comrade-tool-memory store + tool set, comrade-core gate lists and system-prompt guidance, delegate tool availability, this repo's seed glossary + this ADR.
Out: TUI screens, config changes, any rewrite of the pre-ADR entries in `.comrade/memory/`.

## Impact
- New decisions carry a date and richer ADR sections; `find_decisions` shows the date when present.
- The model is guided to persist only important long-term decisions; run-book how-tos are no longer stored (remember description + `## Memory` prompt section updated).
- The tool registry grows by three tools; delegates retain access because only git_commit/session/UI tools are denied to them.
- Existing `.comrade/memory/*.md` files remain readable, searchable and amendable unchanged.
