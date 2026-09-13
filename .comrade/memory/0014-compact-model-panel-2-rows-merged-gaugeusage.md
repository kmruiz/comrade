# 0014 - Compact model panel: 2 rows, merged gauge/usage
status: accepted
date: 2026-09-13
tags: tui, layout, model-panel
summary: The model panel renders two fixed rows (name+right-aligned balance; gauge merged with compact usage) sized to 2+borders+delegates, removing redundant and blank rows.

## Context
The TUI's right-hand 'model' panel (draw_stats) used 3 fixed inner rows — model name+version+balance, a full-width gauge bar, and a separate usage line — and its height was hard-coded to 6 + delegate rows, leaving one blank row when no delegates were configured. The user asked to remove the wasted space and redesign the fields.

## Decision
Compact the model panel to two fixed inner rows: row 0 = model name+version (left) with the balance right-aligned; row 1 = the gauge bar merged with 'NN% used/budget' and a compact token formatter (k/M). Drop the redundant '(api)' suffix (api is the default; keep a dim ' est' marker only when the count is estimated). Size the panel as MODEL_PANEL_FIXED_ROWS (2) + 2 borders + delegate rows, so there is no blank row. Helper fns label_line/gauge_line/short_tokens make the rows unit-testable.

## Rationale
The compaction keeps all the information the compact layout needs (name, live context usage, balance, delegates) while removing redundancy: the percentage duplicates the bar so it moves next to the numbers, and the bar shares the row instead of owning one.

## Alternatives considered
(a) Keep 3 rows and only fix the blank row — left the gauge/usage redundancy. (b) Move the model name into the block title — elegant but complicates title width/truncation and the delegate header. (c) A toggle to hide the panel — more config surface for little gain.

## Scope
Comrade TUI model panel layout only (crates/comrade-tui/src/tui.rs). Does not change the plan panel, the delegate list rendering, or any other widget.

## Impact
The panel is one row shorter with no delegates (two with), giving the plan panel more room; balance gets reclaimed horizontal space. Token counts are abbreviated. The '(est.)'/'(api)' text is now just ' est' when estimated. Unit tests cover short_tokens and gauge_line. Scope: only draw_stats, its helpers and the panel-height calc in draw; delegate_panel_rows and the plan panel are unchanged.

