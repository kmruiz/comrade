# 0010 - Render tool-result code locations with emphasised path and line number
status: accepted
date: 2026-09-10
tags: tui, rendering, tool-result
summary: Tool-result lines that name a code location are rendered with a bold-cyan file path and bold-yellow line number; all other result lines keep the legacy single-colour wrap.

## Context
Tool results that are lists of code locations (ts_find_symbol, ts_list_symbols, ts_find_references, fs_rgrep, ts_structural_map) were rendered as flat single-colour text via plain_wrap in layout_tool, so the file name and line number did not stand out.

## Decision
Add pure helpers in crates/comrade-tui/src/tui.rs: split_path_line / loc_parts parse a result line into styled parts, wrap_styled lays them out, and result_rows maps a whole result string to styled rows. layout_tool's result block now iterates result_rows(result, card.ok, width) instead of plain_wrap. Recognised shapes: `path:line: snippet` / `path:line:col  snippet` (path bold cyan, line bold yellow, snippet default), `... @ line` (line bold yellow), and `== path ==` headers (bold cyan). Non-location lines keep the ok=Green / fail=Red fallback.

## Rationale
Keeps a single generic renderer driven only by the text shape, so every location-listing tool benefits without each tool crate emitting markup. Style conventions match the existing palette (Cyan = name/path, Yellow = highlight).

## Alternatives considered
(1) Have each tool crate return structured/markdown results — rejected: touches many crates and their prompts. (2) Syntax-highlight the snippet itself — rejected: heavier and not needed for the 'make file+line clearer' ask.

## Scope
Comrade TUI rendering of tool results only. Does not change any tool's output format or the model-facing text.

## Impact
Code-location results are easier to scan. Lines that merely look like `note: 5` are excluded by requiring the path to contain a '/' or '.'. Future tools emitting the same shapes get the styling for free.

