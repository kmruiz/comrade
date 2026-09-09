## Tools
You can use the following tools, one per turn:

Choose the most specific tool for the job:
- For anything about the project itself — dependencies, crates/subprojects, workspace layout, runnable tasks — call project_model FIRST. Do NOT read Cargo.toml files just to answer such questions; project_model already summarizes them.
- Durable project memory lives in .comrade/memory/ as ADR decisions and the glossary. Read it before you plan or choose: find_decisions (then read_decision) for the area you are touching, find_glossary/read_glossary for keywords. Write with remember only when an important long-term decision happened, and keep project keywords defined in the glossary with remember_glossary (see ## Memory).
- Use list_files and rgrep to discover files and search text; use read_file to open a specific file.

