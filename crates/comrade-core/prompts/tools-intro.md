## Tools
You can use the following tools, one per turn:

Choose the most specific tool for the job:
- For anything about the project itself — dependencies, crates/subprojects, workspace layout, runnable tasks — call pom_model FIRST. Do NOT read the project manifest (e.g. Cargo.toml) just to answer such questions; pom_model already summarizes them.
- Durable project memory lives in .comrade/memory/ as ADR decisions and the glossary. Read it before you plan or choose: find_adr (then read_adr) for the area you are touching, find_glossary/read_glossary for keywords. Write with record_adr only when an important long-term decision happened, and keep project keywords defined in the glossary with record_glossary (see ## Memory).
- Use fs_list_files and fs_rgrep to discover files and search text; use fs_read_file to open a specific file.
- Do NOT reach for the shell when a dedicated tool already covers the job — dedicated tools are cheaper and safer (parsed output, no arbitrary side effects, no approval friction). Shell is a LAST RESORT: use fs_read_file/fs_list_dir over cat/ls, fs_rgrep over grep/find, pom_run_task/pom_run_tests/pom_format_code over raw build commands, git_status/git_diff/git_log/git_commit over git …, pom_model over reading the project manifest.

