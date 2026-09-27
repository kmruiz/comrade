## Memory
When this session ends your context is cleared. Only what you write to .comrade/memory/ survives: ADR decisions and the glossary. Read it before you act. Write to it before you finish.

ADR DECISIONS (.comrade/memory/NNNN-*.md) — ARCHITECTURAL GUIDELINES, nothing else.
- An ADR is a durable RULE or DESIGN that other developers and DELEGATES must follow when they work on this codebase. Record one ONLY for an important architectural/design decision with lasting consequences.
- Write it to be READ BY SOMEONE IMPLEMENTING A FEATURE: the decision, why it was chosen, the alternatives rejected, its scope, and its impact/constraints. Other agents cite it — you can hand a delegate "read ADR NNNN, then implement X".
- Do NOT record task notes, session logs, bugfixes, refactors, "we added X", how-tos, or anything only this session cares about. Ask "would another developer or delegate need this as a RULE?" — if not, it is NOT an ADR; leave it out.
- Before planning or making an architectural/behavioural choice, search find_adr (query or tags) and read_adr anything relevant — a past ADR may already be the rule you must follow, or the trap you are about to hit.

GLOSSARY (.comrade/memory/glossary.md): one keyword -> meaning + references per entry.
- When a keyword, acronym, crate or concept is unfamiliar, look it up with find_glossary (search) or read_glossary (one term, or omit the term to read the whole file).
- When you meet a project-specific term the next session should understand, define it with record_glossary (meaning + at least one reference to code or docs).

Record sparingly, while the work is fresh: only the decisions that survive as guidelines and the keywords the next session must understand. At the end of a task, ask "would another developer or delegate need this as a rule?" — if not, record nothing.
