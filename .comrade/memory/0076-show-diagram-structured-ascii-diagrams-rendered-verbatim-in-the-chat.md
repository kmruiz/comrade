# 0076 - `show_diagram`: structured ASCII diagrams, rendered verbatim in the chat
status: accepted
date: 2026-09-15
tags: tools, tui, session, diagram
summary: Models render ASCII diagrams via a new `show_diagram` session tool (structured "flow" or framed "raw"), and the TUI renders its sentinel-delimited output verbatim so the art never re-wraps.

## Context
Models answer "explain this flow/concept" requests far better with a picture, but a model drawing ASCII by hand misaligns boxes as soon as labels differ in length. The chat also word-wrapped every tool-result line, which shreds any ASCII art wider than the pane.

## Decision
Add a `show_diagram` tool to comrade-tool-session (crates/comrade-tool-session/src/diagram.rs) that renders a STRUCTURED description deterministically: kind="flow" lays out a sequence of steps as boxes joined by arrows (horizontal, auto-falling back to vertical when the row exceeds `width`, default 100); kind="raw" frames ASCII the model supplies. The tool wraps its result between two sentinel lines, `--- diagram (ascii) ---` and `--- end diagram ---`. The TUI's `result_rows` (crates/comrade-tui/src/tui.rs) treats a diagram block specially: one chat row per source line, never re-wrapped (ratatui clips an over-wide span), so alignment survives. The tool is advertised to the model by a new bullet in crates/comrade-core/prompts/tools-intro.md.

## Rationale
A structured input plus a deterministic renderer is the only way to guarantee alignment; a sentinel-delimited block is the least invasive way to teach the existing chat renderer to keep it verbatim, and keeping the tool in the session crate matches its no-side-effects nature (like ask_form).

## Alternatives considered
(a) Prompt-only guidance telling the model to draw ASCII by hand — rejected: a model cannot reliably keep boxes aligned as label lengths change. (b) A general graph/tree layout engine over nodes+edges — rejected for now as more layout code and more failure modes than the common pipeline/process case needs. (c) A separate crate comrade-tool-diagram — rejected: nothing here touches the repository or the outside world; it belongs with the session/UI tools.

## Scope
Covers the tool, its two kinds (flow/raw), the verbatim chat rendering and the model-facing guidance. Does NOT cover general graph/tree layout, exporting diagrams to files, or showing diagrams in a dedicated TUI pane.

## Impact
Models can now emit a correct, aligned diagram with one tool call. The tool is not approval-gated (it only formats text). Adding a tree/graph kind later means extending `render_flow`'s neighbours, not the wire format. The wire format is a CONTRACT shared by the tool and the TUI: changing the sentinel strings means changing both `diagram::DIAGRAM_OPEN/DIAGRAM_CLOSE` and `tui::DIAGRAM_OPEN/DIAGRAM_CLOSE`. `show_diagram` is NOT in the delegate registry's reduced CORE toolbox.

