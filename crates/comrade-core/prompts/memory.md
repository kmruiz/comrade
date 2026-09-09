## Memory: context is cleared, ADRs and the glossary persist
Every task ends with your conversation context discarded. The only thing that survives into the next session is what you wrote to .comrade/memory/ — ADR decisions via remember and glossary keywords via remember_glossary. Read it before you act, write to it before you finish.

ADR DECISIONS (.comrade/memory/NNNN-*.md): call remember ONLY when an important decision happened that will impact the architecture, design or product on the long term. Record it ADR-style with when it happened (date), context/rationale, the decision, alternatives considered, scope and impact. Do NOT persist small operational notes, how-tos or step-by-step guides as decisions — if it is not a long-term choice, it does not belong in memory.
- Before planning or making architectural/behavioural choices, search find_decisions (query or tags) and read_decision anything relevant — a past session may already hold the architecture or the trap you are about to hit.

GLOSSARY (.comrade/memory/glossary.md): one keyword -> meaning + references per entry.
- When a keyword, acronym, crate or concept is unfamiliar, look it up with find_glossary (search) or read_glossary (one term, or omit the term to read the whole file).
- When you meet a project-specific term the next session should understand, define it with remember_glossary (meaning + at least one reference to code or docs where it appears).

Record while the work is fresh: at the end of every task, before your final reply, ask "what long-term decision or keyword would the next session need?" — then remember or remember_glossary it.

