# 0067 - Show a delegated sub-agent's own reasoning in the chat
status: accepted
date: 2026-09-15
tags: tui, chat, delegate, reasoning, prompts
summary: A delegate's per-turn reasoning text is streamed to the chat as a 🧠 block under the delegate's name, and the delegate prompt now asks it to write one short sentence before each tool call — otherwise a native tool-calling turn carries empty content and nothing is emitted.

## Context
The `delegate`/`delegate_parallel`/`ask_advise` sub-agents already streamed their TOOL activity into the chat (DelegateToolCall/DelegateToolResult), but their own thinking was invisible. A first pass added the event plumbing (ActivityEvents::reasoning -> AgentEvent::DelegateThought -> tui Msg::reasoning), yet the chat still showed nothing: run_delegate_subagent only surfaces text the MODEL itself produced (`turn.content` on a native tool-calling turn, `turn_p.thought` in ReAct mode), the default protocol is Auto == native (config.rs native_enabled), and the native {protocol} string never told the sub-agent to write anything before a tool call. A native tool call therefore comes back with empty content, so no DelegateThought was ever emitted.

## Decision
Keep the transport as-is — a new defaulted `ActivityEvents::reasoning(author, text)` method, implemented by SessionEvents as `AgentEvent::DelegateThought { model, text }`, handled in the TUI by `push_msg(Msg::reasoning(model, text))` (a normal reasoning block, so it renders in focus mode like the main model's thinking per ADR 16/19). Emit it from run_delegate_subagent for both turn shapes: native (turn.content) and ReAct (turn_p.thought). The missing piece is supply: the native {protocol} instruction in render_subagent_system (crates/comrade-core/src/delegate.rs) now reads "Before each tool call, write one short sentence in the message content saying what you are about to do and why - your tech lead reads it as your reasoning."; the ReAct branch already required a `Thought:` line.

## Rationale
The cost of a reasoning block is one extra sentence per turn, and the sub-agent's own words are the only honest source of its reasoning — synthesising a line from the tool name would be fake. Putting the requirement in the model-facing prompt (the shared small-model recipe, cf. ADR #62) is the existing lever and needs no extra round-trip, unlike a harness nudge. Reusing MsgKind::Reasoning rather than a new message kind means the delegate's thinking gets the same look, banding and focus-mode behaviour as the main model's, and folding (ADR 16) keeps it visible without new render code.

## Alternatives considered
Change `layout_run` to render a folded digest's Reasoning children in non-focus mode too: rejected — a digest is deliberately one summary row outside focus mode (ADR 16, and layout_run tests assert exactly one row), and it would change the main model's behaviour too. 2) One-shot harness nudge when a native tool-calling turn has empty content: rejected as the primary fix — it costs a model round-trip on every silent delegate turn; the prompt instruction is free. 3) Derive a reasoning line from the tool call ("Running fs_edit"): rejected — that is not the delegate's reasoning. 4) Stream the delegate's tokens live (chat_turn_once is non-streaming): rejected — streaming changes when text appears, not whether the model produces any, so it would not fix the empty-content case.

## Scope
Covers the delegate sub-agent reasoning display only: comrade-tool ActivityEvents, comrade-core delegate.rs/session.rs, comrade-tui on_agent_event, and the native delegate prompt. Does not change tool cards, the digest/folding rules, focus mode, the headless printer (which prints `🧠 {model}: {text}`), or the main model's own reasoning path.

## Impact
With the instruction in place a native delegate that complies shows a 🧠 <delegate> block before each tool call (its line, then its tool card); a delegate that still answers with empty content shows nothing — the feature depends on model compliance, so if it recurs the follow-up is a one-shot nudge. The reasoning row is a run member, so it folds into the completed stretch's digest and stays visible in focus mode (default ON); outside focus mode it is inside the folded digest. Tests: delegate::tests::delegate_thought_streams_as_a_chat_event, delegate::tests::native_subagent_protocol_asks_for_reasoning_before_tool_calls, tui::tests::delegate_thought_event_pushes_a_reasoning_block_under_the_delegate.

