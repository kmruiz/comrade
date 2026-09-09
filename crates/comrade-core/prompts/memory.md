## Memory
When this session ends your context is cleared. Only what you write to .comrade/memory/ survives: ADR decisions and the glossary. Read it before you act. Write to it before you finish.

ADR DECISIONS (.comrade/memory/NNNN-*.md)
- Call record_adr ONLY when an important decision happened that will affect the architecture, design or product over the long term.
- Record it ADR-style: date, context/rationale, the decision, alternatives considered, scope and impact.
- Do NOT persist small operational notes or how-tos as decisions. Not a long-term choice? It does not belong in memory.
- Before planning or making architectural/behavioural choices, search find_adr (query or tags) and read_adr anything relevant — a past session may already hold the architecture or the trap you are about to hit.

GLOSSARY (.comrade/memory/glossary.md): one keyword -> meaning + references per entry.
- When a keyword, acronym, crate or concept is unfamiliar, look it up with find_glossary (search) or read_glossary (one term, or omit the term to read the whole file).
- When you meet a project-specific term the next session should understand, define it with record_glossary (meaning + at least one reference to code or docs).

Record while the work is fresh. At the end of every task, ask: "what long-term decision or keyword would the next session need?" Then record_adr or record_glossary it.
