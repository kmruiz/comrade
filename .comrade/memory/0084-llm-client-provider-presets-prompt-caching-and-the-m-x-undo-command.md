# 0084 - LLM client: provider presets, prompt caching, and the M-x undo command
status: accepted
date: 2026-09-27
tags: llm, config, providers, anthropic, prompt-caching, tui, undo
summary: The LLM client gained named provider presets (including Anthropic via an OpenAI-compatibility shim) and opt-in prompt caching (an Anthropic-style cache_control marker on the system message and the last tool); the TUI also gained an M-x undo command that restores files from the run's undo log.

## Context
The provider/config surface and the request shape grew: more providers, a way to reuse the (large, stable) prompt prefix, and a way to roll back edits.

## Decision
Providers are selectable by `provider` preset (Ollama, OpenAI, DeepSeek, Mistral, Anthropic, OpenRouter, Groq, Together), with Anthropic reaching its OpenAI-compatible endpoint. Prompt caching is opt-in (`[llm].prompt_caching`) and marks the system message and the last tool with `cache_control: ephemeral`; providers that ignore it are unaffected. The M-x `undo` command restores files captured in the run's undo log. The merged sections below preserve the full detail.

## Merged: Opt-in prompt caching + M-x undo command
## Context
Two small roadmap items: prompt-cache support (E2, Anthropic cache_control / provider caching) and binding the already-existing UndoLog to a user command (C3, core/undo.rs existed but was unreferenced from the TUI).

## Decision
(E2) Add `[llm] prompt_caching` (default false). When on, ChatRequest serializes messages as loose JSON and adds a `cache_control: {type: "ephemeral"}` marker to the system message and the last tool definition; providers that don't support it ignore the unknown field. (C3) Add an `MxCommand::Undo` ("undo", M-x only) that runs the active session's `ToolContext.undo.undo_last()` in a background task and reports how many files were restored.

## Rationale
Both are low-risk, opt-in additions that fit existing structures (the OpenAI-compatible request builder; the session undo log already wired into ToolContext).

## Alternatives considered
Anthropic native content-block format for cache_control (correct for Anthropic but breaks OpenAI-compatible servers that expect string content) — rejected: the client is OpenAI-compatible. Making caching always-on — rejected: unknown fields may be rejected by strict servers, so it stays opt-in. A dedicated undo tool exposed to the model — rejected: undo is a human recovery action, so a TUI command is the right surface.

## Scope
LLM request building and the TUI command palette. Not a general-purpose cache accounting feature.

## Impact
Opt-in prompt caching can cut cost/latency on providers that support it; `M-x undo` lets a human roll back the last mutating tool writes. No behaviour change when flags are left at defaults. Follow-up: verify cache markers are accepted by the specific provider in use (they are non-standard on OpenAI-compatible endpoints).

## Merged: Support Anthropic models via the OpenAI-compatibility provider preset
## Context
Request: "Add support for anthropic models using an API key." Comrade speaks to every provider through one OpenAI-compatible LlmClient (Bearer auth, /chat/completions, native tool calls), and provider names resolve to preset base URLs in config::provider_base_url. An earlier decision (the prompt-caching section in this rollup) had already rejected the native Anthropic content-block format (cache_control) to keep the single OpenAI-compatible client. Anthropic ships an official OpenAI-compatibility layer at https://api.anthropic.com/v1 that speaks /chat/completions with the OpenAI SDK's Bearer api_key.

## Decision
Add `anthropic` (with `claude` as an alias) to provider_base_url -> https://api.anthropic.com/v1, so `[llm] provider = "anthropic"` + `api_key = "sk-ant-..."` routes Claude models through the existing OpenAI-compatible client with no new client code. Also add a `claude` -> 200_000 fallback in llm::context_window::heuristic_context (the compat layer may not advertise /models context), and document the provider in README.

## Rationale
Consistent with the project's single-OpenAI-compatible-client architecture and the prompt-caching decision in this rollup; a working Claude integration in ~30 lines versus a large new module. The compat layer is documented as functional and non-breaking; its caveats (no prompt caching, non-strict tool schema, system-message hoisting, temperature capped at 1) are acceptable for this agent's use.

## Alternatives considered
Native Anthropic Messages API client (new module: /v1/messages, x-api-key + anthropic-version headers, content/tool_use block translation, SSE parsing, ~500 lines) — rejected for now as a much larger, riskier change; Anthropic itself labels the compat layer test-oriented but non-breaking. Doing nothing / requiring a custom base_url — rejected: no discoverability and no preset.

## Scope
Config provider presets, the context-window heuristic fallback, and docs. Not a native Messages API client; no special handling of Anthropic-only features (prompt caching, thinking, citations) and no env-var expansion for llm.api_key.

## Impact
Users can run Claude (and delegate to it) with just `provider = "anthropic"` + an API key. Follow-up if reliability/features matter: implement a native /v1/messages client and honour `[llm] prompt_caching` with Anthropic cache_control. The compat layer drops prompt caching, so `prompt_caching = true` is a no-op there.

