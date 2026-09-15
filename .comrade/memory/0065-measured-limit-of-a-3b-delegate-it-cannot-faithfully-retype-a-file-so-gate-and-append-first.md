# 0065 - Measured limit of a 3B delegate: it cannot faithfully retype a file, so gate and append-first
status: accepted
date: 2026-09-15
tags: small-model, delegation, measurement, fs_edit, reliability
summary: Measured: a 3B delegate corrupts source it retypes from context and picks the wrong lines to edit, so more iterations do not help; the harness fixes (flat schemas, actionable errors, the destructive-write gate, append-first prompt rule) are what make the failures cheap instead of destructive.

## Context
Repeated trials of ministralai/ministral-3-3b (LM Studio, OpenAI-compatible) implementing one small feature (add `shout` + a unit test to src/lib.rs of a tiny Cargo lib) through Comrade's delegate path. Harness: /tmp/comrade-smoke (git repo with `greet` + `greet_works`), /tmp/comrade-deleg.cfg.toml (ministral as both lead and the `walter` delegate), /tmp/deleg-trial.sh, verified with `cargo test` plus grepping that BOTH pre-existing and new tests still exist (an earlier "pass" was only green because the delegate had deleted the pre-existing test).

## Decision
Record the measured limit and the recipe that follows from it. A 3B delegate CANNOT reliably edit an existing source file: it emits corrupt tokens when it retypes content out of context (`use super::*;` -> `use super::;`, `#[cfg(test)]` -> `#`, mangled `format!` escapes; a direct probe proves the model reproduces the same line correctly when the prompt CONTAINS it, so the corruption is context-retyping, not a tokenizer or transport fault), and its edit judgement picks the wrong lines to replace (it once rewrote `greet_works`'s assertion into a `shout` assertion). Therefore: (a) the destructive-write gate is required, not optional; (b) the delegate prompt tells it to APPEND additions by anchoring `old` on the file's last existing lines and never to reproduce a whole file; (c) the lead must hand a delegate a step small enough that it needs no judgement about existing content.

## Rationale
The measurements are unambiguous and they change what "working with a 3B" should mean: the harness-side levers (flat schemas, actionable errors, stdin-null, nudges, the permission gate) are what is fixable, and the remaining failure is model capability that no prompt or budget can paper over. Writing that down stops the next session from re-running the same trials and attributing the failure to the harness.

## Alternatives considered
(1) Keep raising the iteration/time budget - measured: 25 -> 60 iterations and 240s -> 420s did NOT change the outcome (the delegate burned all 60 and returned no answer, with 0 destructive-write refusals that run). (2) Let the delegate rewrite whole files freely so it can succeed more often - rejected: that is precisely how it deleted the crate's own test and reported success; the corruption probe shows it also mangles the content it retypes. (3) Treat the failures as a prompt-wording problem - not supported: the fix that did move the needle was schema shape (a flat tool schema), not prose.

## Scope
Measured on ministral-3-3b at 3B parameters over LM Studio. Not a claim about larger local models (qwen models failed to load in this environment and were never measured).

## Impact
Expect a 3B delegate to complete a one-file "add a self-contained function" step only when nothing existing must be reproduced; complex edits remain out of reach at this size, and the honest configuration is a stronger delegate (or a bigger lead) for real work. The gate and the append-first rule are what keep the failures cheap (a refused call and a bounded run) instead of destructive (deleted tests, a green-looking result).

