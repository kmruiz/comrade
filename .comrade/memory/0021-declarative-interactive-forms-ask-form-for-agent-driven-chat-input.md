# 0021 - Declarative interactive forms (ask_form) for agent-driven chat input
status: accepted
date: 2026-09-13
tags: ui, tools, chat, forms
summary: Agents describe interactive input components as a JSON FormSpec rendered as a new UserPrompt::Form; the human's answers return as a UserReply::Form map and reach the agent as `id = value` lines.

## Context
Agents could only interact through free text (ask_user question, or Confirm yes/no). For bounded input — a number, a date, a pick-list, a boolean — free text is awkward and error-prone. We wanted a small library of chat components that agents can invoke via JSON, so a form's answers come back keyed by field id.

## Decision
Add a shared form contract in comrade-tool (crates/comrade-tool/src/form.rs): FormSpec { title, description, fields: Vec<FormField> } where FormField { id, label, kind: FieldKind, required, default } and FieldKind is a serde-tagged enum (kind = text|number|date|select|checkbox) with per-kind params (placeholder; min/max/step; options). Extend the UserIo contract with UserPrompt::Form(FormSpec) and UserReply::Form(BTreeMap<String,String>). Add helpers FormSpec::initial_values/is_complete/answer_lines (the last emits `id = value` lines). Expose a new session tool `ask_form` (comrade-tool-session) that parses the JSON spec, routes the prompt, and returns answer_lines. The TUI renders each field as an interactive component (Dialog carries a FormEdit: values + focused index; up/down or tab navigate, left/right steps number/select/date, space toggles checkbox, type edits text/number/date, enter submits, esc cancels, required fields gate submit). Headless UserIo auto-fills initial_values under Auto and prompts per field otherwise; sub-agent/advise UserIo auto-fills defaults too.

## Rationale
Putting the spec in comrade-tool keeps one contract shared by the tool crate, the TUI, and headless mode without new cross-crate coupling. A new UserPrompt/UserReply variant (rather than overloading Question) makes the reply typed (id->value) and lets the compiler force every UserIo to handle it. JSON with a serde-tagged `kind` makes the spec easy for models to emit and easy to extend with new component kinds.

## Alternatives considered
Reusing UserPrompt::Question with a JSON options blob (rejected: no typed reply, ambiguous with plain questions). Encoding the form as a special tool-result the UI parses out of chat text (rejected: fragile, no interaction state). A separate crate for the form library (rejected: the contract is small and both comrade-tool consumers already exist).

## Scope
Covers the form/component contract, the ask_form tool, TUI rendering + interaction, and headless/sub-agent fallbacks. Does NOT cover: auto-accepting forms in auto mode (they still require the human), multi-page/conditional forms, or validation beyond required/empty.

## Impact
New enum variants break exhaustive matches on UserPrompt/UserReply across the workspace (fixed with wildcard/default arms in core/fs/delegate/advise). New component kinds are added by extending FieldKind plus the TUI render/adjust arms and the ask_form JSON schema.


## Note
Addendum (2026-09-13): every form field (`FormField.recommended`) and every `ask_user` question (`UserPrompt::Question.recommended: Option<String>`) can now carry a **recommended** value. It prefills the field / free-text answer and is flagged " (recommended)" for options and form fields in the TUI. Auto-accept mode (M-x toggle, or headless `Autonomy::Auto`) then answers without the human: the question is answered with the recommended value, an option-question likewise, and a form is submitted with all recommended/default values — but only when every required field is satisfied, otherwise the form is still shown. The agent does not have to decide the recommendation alone: it can consult a delegate with `ask_advise` (guidance added to crates/comrade-core/prompts/tools-intro.md). Replaces the earlier "auto mode never auto-accepts forms/questions" scope note.

## Note
Addendum (2026-09-13): two follow-ups.

1. New component kind `diff_choice`: `FieldKind::DiffChoice { options: Vec<DiffOption> }` where `DiffOption { label, diff }`. The human picks one of several candidate code diffs; the answer is the chosen `label`. Seeded with the first option's label (or the field's `recommended`). The TUI cycles options with left/right (typing ignored) and renders the selected option's `diff` under the field; the ask_form JSON schema advertises `diff_choice` and accepts `{label, diff}` option objects.

2. `ask_user` was removed, together with the whole `UserPrompt::Question` variant and its machinery (the `AskUser` tool, the TUI question dialog, headless `question_on_stdin`/`auto_question_answer`, and the related core prompt/tests). `ask_form` supersedes it: a plain question is a single-field form (a `select` for bounded options, a `text` field for free input), and `recommended` still prefills/auto-answers. `UserReply::Answer` remains (used by `UserPrompt::Confirm`).

3. The TUI now records questions in the transcript so focus mode shows them: a new `MsgKind::Question` (rendered yellow, and `focus_visible` -> true) is pushed when an ask_form form is asked and when it is answered, whereas tool cards and grey `Meta` notes stay hidden in focus mode.

## Note
Bug fix (commit 62b31ec): `handle_form_key` (crates/comrade-tui/src/tui.rs) returned `true` unconditionally. That bool is the event loop's quit flag (`handle_event` -> `break Ok(())`, tui.rs ~3441), so the FIRST key press while an ask_form dialog was open - e.g. an arrow used to move between fields - quit the app cleanly: no panic, no stacktrace. It now returns `true` never; a key consumed by an open form leaves the app running. Regression test `tui::tests::form_dialog_keys_do_not_quit`. Invariant to honour when adding dialog/form handlers: those bools mean "quit the app", so a dialog handler must return `false` for every key it consumes (only the top-level Ctrl+C / M-x Quit paths return `true`).
