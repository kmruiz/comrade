# 0089 - Focus-mode progress reassurance is the agent's own narration, not a UI widget
status: accepted
date: 2026-09-27
tags: tui, prompt, focus-mode, ux
summary: Progress reassurance in focus mode comes from the agent's own words (a always-on system-prompt section, prompts/progress.md), not from a new UI element; focus mode keeps showing Reasoning blocks.

## Context
In focus mode the TUI hides tool cards (MsgKind::Tool) so the chat reads as pure conversation. With a model that emits no prose alongside its tool calls (common in native function-calling mode) a run showed only a rotating spinner for minutes, so the UI looked blocked/frozen. The user asked for the agent to tell them now and then that it is working. Rather than add a UI widget, we reuse the mechanism the TUI already has.

## Decision
Progress reassurance in focus mode is delivered by the AGENT NARRATING, not by a UI element. The system prompt unconditionally carries a "## Keeping the user informed" section (crates/comrade-core/prompts/progress.md, pushed by build_prompt in crates/comrade-core/src/react.rs immediately before the protocol section, so it applies to both the ReAct and native protocols) telling the agent to open each batch of tool calls with ONE short sentence saying what it is about to do and why, and to repeat it when it moves to a different step, without narrating every single call. Focus mode MUST keep rendering MsgKind::Reasoning: that is the channel by which the narration reaches the user.

## Rationale
The agent is the only party that knows WHAT it is doing and WHY; the UI can only see the current tool name. Focus mode already shows MsgKind::Reasoning, and the agent's own words are exactly what makes the exchange read as a conversation — which is focus mode's stated purpose. A UI widget would duplicate the information and still lack the intent, while keeping the change prompt-only means no new rendering code and no risk to the existing tool cards.

## Alternatives considered
(a) Enrich the TUI activity line/spinner (elapsed time, last tool, phase) — deterministic and always there, but it can only say WHICH tool runs, never what the agent is doing or why; (b) have the harness periodically inject a nudge telling the model to speak — far more machinery, and the model still has to write the note.

## Scope
Covers the agent's sign of life while it works in focus mode. Does NOT cover long silent delegate sub-runs or a time-based liveness indicator: the existing activity_line spinner (focus mode, status "running") stays as-is.

## Impact
Prompt-only change: no TUI rendering behaviour changed. The narration rides the existing path — streamed assistant text before a tool call is committed by App::commit_stream_reasoning into one MsgKind::Reasoning message per tool batch (MsgKind::Reasoning is focus_visible), and the ReAct Thought line does the same via AgentEvent::Thought. Pinned by tests: react.rs progress_prompt_tests (section present in both protocols) and tui.rs a_progress_sentence_before_a_tool_call_survives_focus_mode / one_progress_sentence_covers_a_whole_batch_of_tool_calls / react_thought_is_focus_visible_while_its_tool_card_is_hidden. If the agent still looks silent, strengthen prompts/progress.md rather than adding a UI indicator.

