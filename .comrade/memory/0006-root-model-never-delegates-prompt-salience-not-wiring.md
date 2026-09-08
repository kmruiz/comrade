# 0006 - Root model "never delegates": prompt salience, not wiring
status: accepted
tags: delegate, prompt, react, system-prompt, salience
summary: Wiring was fine; prompt buried delegation after self-first working style. Added leading "## Delegate by default" section (react.rs) gated on delegate tool advertisement.

## Context
User reported the tech-lead/root model never calls the `delegate` tool even with a [[delegates]] entry configured (~/.config/comrade/config.toml, e.g. mistral via ollama, protocol=auto). Investigated the full chain: main.rs build_tools registers DelegateTool only when cfg.delegates is non-empty; react.rs build_system_prompt appends the delegation paragraph only when a tool named "delegate" is registered; update_plan/finish_plan enforce delegation once a step has a `model` assigned (decision #0002); parallel-delegate integration test proves the pipeline works when the model chooses to delegate. Conclusion: wiring was correct — the behavioural gap was prompt salience/order. The delegation pitch sat mid-prompt, AFTER the self-first "## Working style" that opens with "You are a tech lead - you write real code and tests yourself...", so weaker root models (deepseek/mistral) default to doing steps themselves and never assign `model` in set_plan, which also means the mechanical enforcement never triggers.

## Decision
Make delegation the FIRST thing the root reads when it is possible:
1. In build_system_prompt (crates/comrade-core/src/react.rs), when the registry advertises a "delegate" tool, push a new leading section "## Delegate by default" right after the "Context budget ... Be terse." intro and BEFORE "## Working style". Text: PREFER delegating well-bounded steps (single file/function, bugfix, refactor, transform, translation, test) via set_plan `model` + delegate step=<id>; keep only judgement/commit work yourself; remind that enforcement means a step with `model` can't close until the delegate tool ran it, so only assign `model` to steps you will delegate.
2. VERIFY: cargo test -p comrade-core passes; prompt tests lock both presence and ORDER: `prompt_urges_delegation_when_a_delegate_tool_is_advertised` asserts "## Delegate by default" occurs before "## Working style"; `prompt_stays_silent_about_delegation_without_delegates` asserts the section is absent when no delegate tool is registered.
3. Full suite: cargo test (whole workspace) green.

## Consequences
If the model STILL never delegates after this, next suspects: (a) process not restarted after config gained [[delegates]] (tool+paragraph are computed at startup from cfg.delegates), (b) verify the delegate tool actually appears in the tool list the model receives (native tools are advertised when protocol != React), (c) set_plan's `model` field cannot advertise delegate names (decision #0004 consequence) — a future fix could inject delegate names into set_plan's schema.

