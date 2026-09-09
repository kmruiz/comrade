# Project glossary

Project keywords and their meaning, with references to the code or documentation where they appear. One `## term` section per keyword, sorted alphabetically. Look terms up with read_glossary, search with find_glossary, add or update with remember_glossary.

## ask_advise
> Tool (comrade-core/src/advise.rs) the tech-lead model calls to consult one configured delegate for ADVICE without delegating work: args `model` + `question` (+ optional `context`). The delegate runs a READ-ONLY sub-agent (browse/search/git-log/memory/web only) and replies with advice; no plan-step is marked working, no approval, nothing mutates. Contrast with `delegate` (hand-off that executes with full tools). Only registered when `[[delegates]]` exist.

**References:**
- `crates/comrade-core/src/advise.rs`
- `crates/comrade-tui/src/main.rs (advise_registry)`
- `.comrade/memory/0001-ask-advise-tool-read-only-advisory-consult-of-a-delegate.md`

**Notes:**
Advisor tool registry = the main loop's read-only classification (agent::is_read_only / AskAdviseTool::read_only_for_advice), built in comrade-tui advise_registry(). Delegates cannot call ask_advise (DENIED_FOR_DELEGATES). Output envelope `advice from <name> (<display>):\n<advice>` parsed by TUI parse_advice_reply.

