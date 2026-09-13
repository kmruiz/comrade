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
