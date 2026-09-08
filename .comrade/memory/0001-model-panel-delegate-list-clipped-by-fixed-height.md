# 0001 - Model panel delegate list clipped by fixed height
status: accepted
tags: tui, delegates, ratatui, layout, fix
summary: comrade-tui "model" panel showed an empty delegate list because the panel was a fixed 6 rows and delegate names were drawn into leftover space that never existed.

## Context
User report: "in the model section of the UI, the delegate list is empty" even though [[delegates]] are configured and delegation works. The delegate list lives in the top-right "model" panel of the ratatui TUI (crates/comrade-tui/src/tui.rs, draw_stats).

## Decision
1. Reproduce by reading the layout: draw() splits the right column as [Constraint::Length(6) stats panel, Min(0) plan]; draw_stats renders 3 fixed rows (model label, gauge, usage) plus a "delegates:" header + one line per cfg.delegates entry into the leftover rows[3]. With height 6, inner height is 4, so rows[3] is 1 line: only the header fits, every delegate name is clipped.
2. Fix: in draw(), compute stats_h = 6 + app.cfg.delegates.len() as u16 and use Constraint::Length(stats_h); the plan panel keeps whatever is left (Min(0)). Verified with `cargo check -p comrade-tui` and `cargo test -p comrade-tui` (35 pass).

## Consequences
When no delegates are configured the height is unchanged (6), so the default layout is identical. With many delegates the model panel grows at the plan panel's expense (plan is Min(0)); many-delegate configs could squeeze the plan to zero height on small terminals.

