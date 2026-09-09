# Project glossary

Project keywords and their meaning, with references to the code or documentation where they appear. One `## term` section per keyword, sorted alphabetically. Look terms up with read_glossary, search with find_glossary, add or update with remember_glossary.

## ADR
> Architecture Decision Record: a dated `.comrade/memory/NNNN-*.md` entry recording an important long-term choice.
Written with status, date, tags, summary, and sections Context, Decision, Rationale, Alternatives considered, Scope, Impact. Recorded with the `remember` tool only when an important thing happened that impacts architecture/design/product long term.

**References:**
- `crates/comrade-tool-memory/src/store.rs`
- `.comrade/memory/0039-decisions-become-adrs-project-glossary-added.md`

## decision
> A durable ADR entry stored under `.comrade/memory/` when an important long-term choice was made.
Search with `find_decisions`, read with `read_decision`, amend with `amend_decision`, record with `remember`.

**References:**
- `crates/comrade-tool-memory/src/lib.rs`

## glossary
> The project keyword dictionary: one `.comrade/memory/glossary.md` file mapping each keyword to its meaning and to references in code or docs where it can be found.
One `## term` section per keyword, sorted alphabetically; the `> ` quote line is the one-line meaning. Search with `find_glossary`, read with `read_glossary` (one term, or the whole file without a term), add/update with `remember_glossary`.

**References:**
- `crates/comrade-tool-memory/src/glossary.rs`
- `crates/comrade-tool-memory/src/lib.rs`
