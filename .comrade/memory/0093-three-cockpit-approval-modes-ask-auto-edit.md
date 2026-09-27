# 0093 - Three cockpit approval modes: ask, auto, edit
status: accepted
date: 2026-09-27
tags: tui, approval, mode, ux
summary: The TUI has three approval modes cycled by Ctrl-Space — ask (blue), auto (orange), edit (purple); edit auto-approves mutations but still stops at ask_form questions.

## Context
Users wanted routine mutations to land without a prompt while still being asked the questions that genuinely need a human answer. A single boolean (auto-accept) could not express that: it either asked about everything or auto-answered everything, including questions.

## Decision
The cockpit has three approval modes, held in one `Mode` enum (crates/comrade-tui/src/tui.rs): `Ask` (blue mode line, nothing auto-answered), `Auto` (orange, approvals AND questions auto-answered from their recommended values), `Edit` (purple, approvals auto-accepted but questions still stop for the human). Ctrl-Space cycles ask -> auto -> edit -> ask (M-x `cycle-mode`). A run's prompts are routed through the free function `auto_reply_for(mode, &prompt)`; tool approvals are `UserPrompt::Confirm`, questions are `UserPrompt::Form`. A fresh app starts in `Auto` when the config `[security] autonomy = "auto"` (or the `--auto` CLI flag is set), otherwise `Ask`.

## Alternatives considered
Keeping the binary `auto_accept` bool (rejected: cannot express "approve mutations but still ask questions"); making the mode config-only (rejected: modes are a runtime choice the user flips); one M-x command per mode instead of a cycle (rejected: a single cycling key matches the existing one-keybinding design).

## Scope
Covers how the TUI answers a `UserPrompt` raised during a run. It does NOT change the core `ToolContext::auto_approve` short-circuit (that still skips Confirmations entirely when the config autonomy is auto) nor the headless runner, which has no interactive mode.

## Impact
Any new interactive prompt kind must be routed through `auto_reply_for(mode, &prompt)` so it obeys the mode; a `UserPrompt::Form` is a "question" that edit mode deliberately stops on. The mode-line colour/label come from `bar_bg(mode)` and `Mode::label()`.

