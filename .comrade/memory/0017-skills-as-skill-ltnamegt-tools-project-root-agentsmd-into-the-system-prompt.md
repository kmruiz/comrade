# 0017 - Skills as skill_&lt;name&gt; tools; project-root AGENTS.md into the system prompt
status: accepted
date: 2026-09-13
tags: skills, prompt, tools, agents-md
summary: Skills are exposed as one `skill_<name>` tool each, discovered from .comrade/skills and the common Claude dirs; the project-root AGENTS.md is injected into the system prompt.

## Context
Users wanted comrade to support Claude-format skills and AGENTS.md, so a repo can ship reusable instruction packs and a project-wide rules file the agent follows.

## Decision
Model each Claude-format skill as its own tool named `skill_<name>` (not one generic skill tool). A skill is a directory `<name>/SKILL.md` (optional YAML frontmatter name/description + markdown body). Discovery is hardcoded, in precedence order: `<project>/.comrade/skills` (default), `<project>/.claude/skills`, `$HOME/.comrade/skills`, `$HOME/.claude/skills`; on a name clash the first directory wins. Invoking `skill_<name>` returns the SKILL.md body plus the skill's directory (so bundled scripts can be read with fs tools). Frontmatter is parsed by hand (no YAML dependency). Skills are registered in the main, delegate and advisor registries (they only return text, so they are read-only and safe). Separately, the project-root AGENTS.md (if present) is read and injected into the system prompt as a "Project instructions (AGENTS.md)" section by `comrade_core::load_project_instructions`, called from `react::build_system_prompt`.

## Rationale
Per-skill tools keep skills first-class in the model's tool list with their own descriptions, matching the user's 'called like tools' request. A `skill_` prefix avoids collisions with built-ins. Reading AGENTS.md at prompt-build time requires no new plumbing beyond passing the project root.

## Alternatives considered
(a) One generic `skill` tool with a name argument (Claude's own model) — rejected: the user asked for skills 'called like tools', and one tool per skill gives each a description in the tool list and a fixed schema. (b) Bare skill names as tools — rejected: risks colliding with built-in tools; prefixed with `skill_`. (c) Adding a YAML dependency for frontmatter — rejected: the format is simple and the workspace carries no YAML crate, so a small hand parser suffices. (d) AGENTS.md walking parents and merging CLAUDE.md — rejected for now: user chose root-only AGENTS.md. (e) Config-driven extra skill dirs — deferred; discovery locations are hardcoded.

## Scope
Covers skill discovery/parsing/exposure and AGENTS.md loading. Does not add config knobs for extra skill directories, does not merge parent/CLAUDE.md instruction files, and does not add UI affordances for listing skills.

## Impact
A new crate comrade-tool-skill (workspace member) plus a comrade-core instructions module. The TUI's build_tools/delegate_registry/advise_registry now take the project root. Registering per-skill tools means the tool count and the system-prompt tool list grow with the number of skills; react.rs already truncates each tool line to 170 chars so long skill descriptions stay bounded.

