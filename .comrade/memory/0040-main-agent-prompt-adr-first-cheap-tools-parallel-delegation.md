# 0040 - Main-agent prompt: ADR-first, glossary clarity, cheap tools, parallel delegation, mini-run-book step contexts
status: accepted
date: 2026-09-09
tags: prompt, react, system-prompt, planning, delegates, adr, glossary, comrade-core
summary: The root system prompt now tells the main agent to read ADRs/glossary before planning, ask for clarification, delegate + parallelise, prefer cheap tools, keep plan steps small for simpler models, and pack each step with an actionable context

## Context
After introducing ADR-style decisions and the glossary (#0039), the user specified how the main agent should behave. The previous default loop planned first and read memory second, treated plan steps loosely, encouraged delegation only by well-boundedness, and never mentioned tool cost, parallelisation, delegate-as-advisor, or that the executor of a plan step never sees the conversation. The user asked for seven directives: (1) look for relevant ADRs for the new functionality BEFORE writing the plan; (2) ensure everything is clear and use glossary concepts, asking for clarification when something is unclear and not in the glossary; (3) when the plan becomes complex, ask for advisory from one or more delegate models; (4) use the cheapest tools as much as possible; (5) delegate and parallelise work as much as possible; (6) plans should have many small steps handled by a simpler model that can run in a local environment; (7) each plan step should carry a context that makes it actionable (a run book for example) plus the background needed to understand the feature.

## Decision
1. The "Default loop for EVERY task" in build_system_prompt (crates/comrade-core/src/react.rs) is reordered: step 1 is now "Understand the request and read memory BEFORE planning" (find_glossary/read_glossary for concepts, find_decisions/read_decision for ADRs relevant to the new functionality, ask_question when something needed is unclear and not covered by the glossary); step 2 is planning (many small steps sized for a simpler/cheaper model, e.g. one running locally; every step carries an actionable "mini run book" context — what/why, exact files, functions, snippets, commands, how to verify, feature background — because the executor only sees the step, never the conversation, and it lives in the plan only, never in memory); step 3 is orienting with the CHEAPEST tool that answers (project_model, structural_map, rgrep/find_symbol/read_symbol, excerpts instead of full reads, never dumping whole files).
2. The "## Delegate by default" section and the tech-lead delegation paragraph (both gated on a delegate tool being advertised) gain: delegate as much as possible AND parallelise independent steps across delegates in parallel batches, matching each step to the simplest delegate (prefer cheaper/faster/local models); and when the plan or design gets complex, delegate a bounded advisory review ("critique this plan: gaps, risks, cheaper alternatives") to one or more delegates and integrate the answers.
3. Prompt tests (dev_prompt_tests) were updated to lock the new behaviour: memory lookup before planning, ask_question clarification, glossary-first, cheapest tools, PARALLELISE/advisors/simplest delegate/local environment in the delegation sections, "a mini run book" present as plan-step guidance, and the ## Memory section itself still never mentions run books (run-book how-tos remain not durable memory).
4. No wiring changes: the directives are prompt-only; enforcement (plan `model`, fix rounds, read guard) is untouched.

## Rationale
The main agent only acts on what its system prompt says; ADR/glossary lookup had to move ahead of set_plan or the model would lock in a plan shaped without the durable context. Delegation is strongest when steps are small, parallel and self-contained, because delegates run without the root conversation — the mini-run-book context is what makes a step executable end-to-end by a simpler (even local) model. Cost matters because every token read is paid for, and read-only excerpts answer most questions.

## Alternatives considered
- Keeping plan-first ordering and only nudging "read memory too": rejected — the user asked for ADR lookup before writing the plan, and ordering in the prompt drives behaviour.
- A separate upfront "ask the human" step always: rejected — clarification is conditional (only when something is unclear and absent from the glossary), so it lives inside the understand-first step.
- Making delegation-parallel guidance unconditional: rejected — it stays inside the delegate-gated sections so the prompt is silent when no delegates are configured.
- Prompt-only vs. forcing cheaper reads mechanically: rejected — tool-selection is already guided by ToolSpec descriptions; this sets the policy, not new enforcement.

## Scope
In: comrade-core/src/react.rs system-prompt text and its dev_prompt_tests. Out: tool behaviour, memory store, TUI, delegate configuration.

## Impact
- The root model should consult ADRs/glossary before committing to a plan and ask the human when requirements are ambiguous and undefined.
- Plans should become finer-grained, more parallel and cheaper to run (simpler models); step contexts must now stand alone as executable mini run books.
- Delegates may be used as advisors on complex plans, and independent steps run concurrently — consistent with the parallel-delegate batches already supported (#0003).
- Token spend on orientation should drop (excerpts/structural reads preferred over whole-file reads).
