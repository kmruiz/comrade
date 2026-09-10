# 0009 - Per-agent colors and delegate sub-chat in the chat window
status: accepted
date: 2026-09-10
tags: tui, ui, delegate, colors
summary: Assign every model a stable palette color at start; use it for the delegate's name and, heavily dimmed, as the background band of an indented delegate sub-chat in the chat window.

## Context
The chat window interleaved a delegate's sub-agent activity (its tool calls and final reply) flatly with the parent's, making it hard to see who was doing what. The model window also had no visual key tying a delegate's name to its entries in the transcript.

## Decision
New module crates/comrade-tui/src/colors.rs: `ModelColors` assigns each model name a stable hue from an 8-entry `AGENT_PALETTE` on first sight (assigned once at App construction and re-assigned on config reload; draw paths only read via `name_color`/`band_color`). `name_color` colors a delegate's name (chat author tag, model panel, model-pick overlay); `band_color` blends that hue over the chat base (DIFF_BASE_BG) at BAND_ALPHA 0.14 for a dim per-agent band. Delegate-authored chat rows (delegate tool cards and the delegate reply) render as an indented sub-chat: render_row_line gained a `sub: Option<Color>` param that draws a "| " rule in the agent color plus a 2-column indent; draw_chat detects sub-chat rows via `subchat_model(author, cfg.delegates)` and applies the dim band. Delegate rows are laid out two columns narrower (layout_chat_rows) so the indent fits.

## Rationale
A single name->color source of truth keeps chat, model panel and pick overlay consistent, and assigning colors up front keeps every draw function read-only (&App).

## Alternatives considered
Rejected: recoloring the main agent too (only delegates were requested); hashing the name to a color (palette collisions, arbitrary hues); folding the delegate's tool calls out of view (they stay visible as the sub-chat).

## Scope
Chat window rendering, model panel and model-pick overlay, plus config reload color assignment, all in comrade-tui. Does not change message content or plan-panel colors.

## Impact
Delegate activity is visually separated and color-keyed. Folded activity digests (MsgKind::Run) keep no sub-chat styling because their owning message carries no author; expanding a run shows plain rows. The palette/alpha are the single knob to retune for the terminal theme.

