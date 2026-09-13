# 0019 - Chat spoken blocks: reasoning and final answer share one model-banded look
status: accepted
date: 2026-09-13
tags: tui, chat, reasoning, colors, design
summary: The reasoning block now renders like the assistant final-answer block (one-line header + markdown body, no card arrow), headed by a single 🧠 in the model name colour; both reasoning and final-answer rows, plus delegate replies, are tinted with the model's dimmed background band so the spoken blocks read as one integrated chat.

## Context
The chat had drifted into inconsistent shapes: the reasoning block was a collapsible card (`v/> 🧠 reasoning · author`, Yellow bold) with a DarkGray plain-wrapped body; the assistant final answer was a Green author header + markdown body, unbanded; delegate replies were indented and tinted with the delegate's colour band (ADR #9). The user asked reasoning to "look like the summary but with the brain emoji" and to use the model background colour "as we do with the advise tool", then asked the final-answer block to look the same so all chat components look integrated.

## Decision
Render both the reasoning block and the assistant final answer as the same "spoken block": a one-line header in the model's name colour over the message text as markdown (via md_to_lines), with no left rule and no collapsible card arrow. Reasoning's header is a single 🧠 glyph; the final answer's header is the model label. A new pure helper `row_band(owner, colors, delegates, focus)` in tui.rs chooses each row's dimmed background band: user turns -> user_band_bg(); reasoning and assistant -> ModelColors::band_color(author); delegate replies -> band_color (as before); and, in focus mode only, a folded run digest -> band_color of its reasoning child. draw_chat now derives band from row_band and no longer special-cases the user turn inline. The reasoning body switched from plain_wrap to md_to_lines; the assistant header colour switched from a fixed Color::Green to colors.name_color(author).

## Rationale
Giving the two spoken model blocks (thinking and answer) the same header+body+model-band shape removes the "one is a tool card, one is prose" split and makes author identity carried by colour (already the convention for delegate sub-chats). Routing the band through one helper keeps the draw loop's per-row decision in one place and lets the folded-digest focus case reuse it. The 🧠 glyph still visually distinguishes reasoning from the final answer, which the user wanted.

## Alternatives considered
1) Keep reasoning a collapsible card and only recolour it: rejected, the user wanted it to read as the answer block (markdown, no card). 2) Band assistant/reasoning directly in draw_chat without a helper: rejected, the logic is shared by three message kinds plus the focus digest case. 3) Also band delegate-author-less rows generically: not done; delegate rows keep their indented sub-chat treatment and rule.

## Scope
Covers tui.rs only: layout_reasoning, the MsgKind::Assistant arm, the row_band helper, draw_chat's band wiring, and doc/comment wording. Does not change the user-turn heading (still `> ` Green + section rule), the delegate sub-chat indent/rule, tool cards, or the model panel.

## Impact
In the chat the model's thinking and its final answer now share the assistant look, both tinted with the model's colour band; delegate replies stay indented with their own band, so a session reads as model-coloured spoken blocks separated by dimmed tool activity. Note the final-answer header is no longer fixed Green (it follows the model's palette colour). Tests added: reasoning block shape, spoken-block banding, assistant header colour/band; the focus-mode reasoning test was adjusted for markdown span splitting.

