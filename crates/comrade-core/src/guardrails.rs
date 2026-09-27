//! The guardrail mechanism: an external service that JUDGES a running delegated
//! sub-agent, so the tech lead can continue, steer or stop it.
//!
//! Today the implementation is Jev (TypeSafe's SystemOne evaluation endpoint,
//! `POST https://api.typesafe.ai/v1/systemone`). Instead of asking Jev for the
//! action directly, Comrade sends the delegate's task and the tail of its
//! conversation as structured `state` and asks five typed `noul` (yes/no
//! probability) questions about the *situation*: is it on task, is it looping, is
//! it blocked, does it need context, is it making progress. Comrade then maps
//! those probabilities to a [`GuardOutcome`] with [`decide`] - the decision stays
//! here, tunable, and Jev only reports what it sees.
//!
//! Only the DECISION is made here. A `steer` verdict says a correction is needed
//! (with a one-line reason); the caller ([`crate::delegate`]'s supervision round)
//! asks the lead model to write the correction, because the endpoint returns
//! probabilities, not prose. `continue` costs no model call, and `stop` ends the
//! run. When no key is configured, the guardrail is disabled, or the request
//! fails, the lead model supervises exactly as before.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Guard, GuardInput, GuardMessage, GuardOutcome, Guardrail};
use serde_json::{Value, json};

use crate::config::GuardrailsCfg;
use crate::context::ContextManager;
use crate::jev::{Jev, noul};
use crate::llm::Role;

// Question ids, echoed back under `answers`.
const Q_ON_TASK: &str = "on_task";
const Q_LOOPING: &str = "looping";
const Q_NEEDS_CONTEXT: &str = "needs_context";
const Q_BLOCKED: &str = "blocked";
const Q_MAKING_PROGRESS: &str = "making_progress";

// Probabilities are the Jev `noul` value (0 = no, 1 = yes). The thresholds below
// turn "how likely" into an action; they are deliberate defaults and the single
// place to tune the mechanism.
/// At/above this, the sub-agent is treated as stuck in a loop.
const T_LOOPING: f64 = 0.8;
/// At/above this, the sub-agent is treated as blocked.
const T_BLOCKED: f64 = 0.8;
/// At/above this, the sub-agent is treated as missing context the lead has.
const T_NEEDS_CONTEXT: f64 = 0.7;
/// At/below this, the sub-agent is treated as having drifted off the task.
const T_ON_TASK_LOW: f64 = 0.3;
/// At/below this, the sub-agent is treated as on task but not progressing.
const T_PROGRESS_LOW: f64 = 0.3;

// Reasons/hints are neutral clauses (no "sub-agent" / "you") so the same string
// reads correctly whether it is handed to a lead about a delegate, or shown to
// the root agent about itself.
const HINT_NEEDS_CONTEXT: &str =
    "missing information it needs to proceed - fetch it, or ask the user";
const HINT_DRIFT: &str = "drifting from the given task - refocus on what was asked";
const HINT_PROGRESS: &str = "on task but not making progress - take the next concrete step";

/// A Jev/TypeSafe-backed guardrail.
pub struct JevGuardrail {
    jev: Jev,
}

impl JevGuardrail {
    pub fn new(jev: Jev) -> Self {
        Self { jev }
    }

    /// The structured `state` (task + recent conversation) and the five
    /// diagnostic `noul` questions.
    fn state_and_questions(input: &GuardInput) -> (Value, Value) {
        let conversation: Vec<Value> = input
            .messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect();
        let question = |instructions: &str, yes: &str, no: &str| {
            json!({
                "type": "noul",
                "instructions": instructions,
                "criteria": { "true": yes, "false": no }
            })
        };
        let state = json!({
            "delegated_task": { "task": input.task, "context": input.context },
            "recent_conversation": conversation,
        });
        let questions = json!({
            Q_ON_TASK: question(
                "Is the sub-agent still working on the delegated task, rather than on something \
                 else?",
                "Its actions serve the delegated task.",
                "It is working on something the task did not ask for.",
            ),
            Q_LOOPING: question(
                "Is the sub-agent repeating the same actions, or re-reading what it already \
                 knows, without making progress?",
                "It is repeating itself or cycling without progress.",
                "Each step is new and moves the work forward.",
            ),
            Q_NEEDS_CONTEXT: question(
                "Does the sub-agent lack information that its tech lead has and that it needs to \
                 proceed correctly?",
                "It is guessing or stalling for want of information the lead holds.",
                "It has everything it needs.",
            ),
            Q_BLOCKED: question(
                "Is the sub-agent stuck on an error or a decision it cannot resolve on its own?",
                "It is blocked and cannot get past the current problem.",
                "It is not blocked.",
            ),
            Q_MAKING_PROGRESS: question(
                "Is the sub-agent moving toward completing the delegated task?",
                "The work is advancing toward completion.",
                "It is not advancing (not yet or no longer).",
            ),
        });
        (state, questions)
    }
}

#[async_trait]
impl Guardrail for JevGuardrail {
    async fn check(&self, input: &GuardInput) -> Result<GuardOutcome> {
        let (state, questions) = Self::state_and_questions(input);
        let answers = self.jev.evaluate(state, questions).await?;
        Ok(parse_decision(&answers))
    }
}

/// Map Jev's answers to a [`GuardOutcome`]. Pure, so it is unit-tested without a
/// network.
fn parse_decision(answers: &HashMap<String, Value>) -> GuardOutcome {
    let probs: HashMap<String, f64> = [
        Q_ON_TASK,
        Q_LOOPING,
        Q_NEEDS_CONTEXT,
        Q_BLOCKED,
        Q_MAKING_PROGRESS,
    ]
    .into_iter()
    .filter_map(|k| noul(answers, k).map(|p| (k.to_string(), p)))
    .collect();
    decide(&probs)
}

/// Turn the five probabilities into an action. Missing answers take the benign
/// default (assume on task, progressing, not looping), so a partial or empty
/// response fails open to `Continue`.
///
/// Precedence: a loop or a block STOPS the run; otherwise a context gap, drift,
/// or a lack of progress STEERS it; otherwise it continues.
fn decide(probs: &HashMap<String, f64>) -> GuardOutcome {
    let get = |key: &str, default: f64| probs.get(key).copied().unwrap_or(default);
    let on_task = get(Q_ON_TASK, 1.0);
    let looping = get(Q_LOOPING, 0.0);
    let needs_context = get(Q_NEEDS_CONTEXT, 0.0);
    let blocked = get(Q_BLOCKED, 0.0);
    let making_progress = get(Q_MAKING_PROGRESS, 1.0);

    if looping >= T_LOOPING {
        GuardOutcome::Loop(
            "repeating actions without making progress - change approach, or finish".into(),
        )
    } else if blocked >= T_BLOCKED {
        GuardOutcome::Stop("stuck on an error or a decision it cannot resolve".into())
    } else if needs_context >= T_NEEDS_CONTEXT {
        GuardOutcome::Steer(HINT_NEEDS_CONTEXT.into())
    } else if on_task <= T_ON_TASK_LOW {
        GuardOutcome::Steer(HINT_DRIFT.into())
    } else if making_progress <= T_PROGRESS_LOW {
        GuardOutcome::Steer(HINT_PROGRESS.into())
    } else {
        GuardOutcome::Continue
    }
}

/// How many of a run's most recent messages a guardrail is shown.
const GUARD_MESSAGES: usize = 20;
/// Per-message cap so one huge tool result cannot blow up the guardrail request.
const GUARD_MESSAGE_CHARS: usize = 20_000;

/// The task a run is working on: its first non-tool user turn, or an empty string
/// when the transcript no longer holds it.
pub fn task_of(ctxm: &ContextManager) -> String {
    ctxm.messages()
        .iter()
        .find(|m| m.role == Role::User && m.tool_calls.is_none())
        .map(|m| m.content.trim().to_string())
        .unwrap_or_default()
}

/// Build the structured material a guardrail judges: the caller's `task` and
/// `context` plus the tail of `ctxm`'s conversation, oldest first. Shared by the
/// delegate and root-agent guardrails.
pub fn guard_input(ctxm: &ContextManager, task: &str, context: &str) -> GuardInput {
    let msgs = ctxm.messages();
    let start = msgs.len().saturating_sub(GUARD_MESSAGES);
    let messages = msgs[start..]
        .iter()
        .map(|m| {
            let mut content = m.content.clone();
            if let Some(calls) = &m.tool_calls {
                let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                if !names.is_empty() {
                    if !content.trim().is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("[calls: {}]", names.join(", ")));
                }
            }
            GuardMessage {
                role: match m.role {
                    Role::System => "system".into(),
                    Role::User => "user".into(),
                    Role::Assistant => "assistant".into(),
                    Role::Tool => "tool".into(),
                },
                content: clip_chars(&content, GUARD_MESSAGE_CHARS),
            }
        })
        .collect();
    GuardInput {
        task: task.trim().to_string(),
        context: context.trim().to_string(),
        messages,
    }
}

/// Clip `s` to at most `max` chars, appending a marker when it was cut.
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(" …[truncated]");
    out
}

/// The advisory text a guard outcome carries, or `None` for `Continue`. The root
/// agent's guardrail shows this in the chat and injects it as a harness note; the
/// delegate path renders its own lead-facing briefing from the variant.
pub fn render_advice(outcome: &GuardOutcome) -> Option<String> {
    match outcome {
        GuardOutcome::Continue => None,
        GuardOutcome::Steer(text) | GuardOutcome::Loop(text) | GuardOutcome::Stop(text) => {
            Some(text.clone())
        }
    }
}

/// Build the configured guardrail, or `None` when it is disabled or has no key.
pub fn guardrail_from_cfg(cfg: &GuardrailsCfg) -> Option<Guard> {
    Some(Arc::new(JevGuardrail::new(Jev::from_cfg(cfg)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probs(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// The `answers` map of a Jev response body, as [`parse_decision`] receives it.
    fn answers(body: &str) -> HashMap<String, Value> {
        serde_json::from_str::<Value>(body).unwrap()["answers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    #[test]
    fn clear_continue_situation() {
        let p = probs(&[
            (Q_ON_TASK, 0.95),
            (Q_LOOPING, 0.05),
            (Q_NEEDS_CONTEXT, 0.1),
            (Q_BLOCKED, 0.05),
            (Q_MAKING_PROGRESS, 0.9),
        ]);
        assert_eq!(decide(&p), GuardOutcome::Continue);
    }

    #[test]
    fn looping_is_a_loop_and_blocked_is_a_stop() {
        let looping = probs(&[(Q_ON_TASK, 0.6), (Q_LOOPING, 0.9), (Q_MAKING_PROGRESS, 0.2)]);
        match decide(&looping) {
            GuardOutcome::Loop(reason) => assert!(reason.contains("repeating"), "{reason}"),
            other => panic!("expected loop, got {other:?}"),
        }
        let blocked = probs(&[(Q_BLOCKED, 0.92), (Q_ON_TASK, 0.9)]);
        match decide(&blocked) {
            GuardOutcome::Stop(reason) => assert!(reason.contains("stuck"), "{reason}"),
            other => panic!("expected stop, got {other:?}"),
        }
    }

    #[test]
    fn context_gap_drift_and_stall_steer_with_a_reason() {
        let ctx = probs(&[
            (Q_ON_TASK, 0.9),
            (Q_NEEDS_CONTEXT, 0.8),
            (Q_MAKING_PROGRESS, 0.7),
        ]);
        match decide(&ctx) {
            GuardOutcome::Steer(hint) => assert!(hint.contains("missing information"), "{hint}"),
            other => panic!("expected steer, got {other:?}"),
        }
        let drift = probs(&[(Q_ON_TASK, 0.1), (Q_LOOPING, 0.1), (Q_MAKING_PROGRESS, 0.5)]);
        match decide(&drift) {
            GuardOutcome::Steer(hint) => assert!(hint.contains("drifting"), "{hint}"),
            other => panic!("expected steer, got {other:?}"),
        }
        let stall = probs(&[(Q_ON_TASK, 0.8), (Q_MAKING_PROGRESS, 0.1), (Q_LOOPING, 0.2)]);
        match decide(&stall) {
            GuardOutcome::Steer(hint) => assert!(hint.contains("next concrete step"), "{hint}"),
            other => panic!("expected steer, got {other:?}"),
        }
    }

    #[test]
    fn missing_or_broken_answers_fail_open_to_continue() {
        assert_eq!(decide(&HashMap::new()), GuardOutcome::Continue);
        // A loop signal wins over a partial response.
        assert!(matches!(
            decide(&probs(&[(Q_LOOPING, 1.0)])),
            GuardOutcome::Loop(_)
        ));
        // A partial response with only one benign answer falls open to continue.
        let partial = r#"{"answers":{"on_task":{"type":"noul","noul":0.9}}}"#;
        assert_eq!(parse_decision(&answers(partial)), GuardOutcome::Continue);
    }

    #[test]
    fn parses_a_full_response_into_a_decision() {
        let body = r#"{"model":"jev-1.13.0","answers":{
            "on_task":{"type":"noul","noul":0.9},
            "looping":{"type":"noul","noul":0.05},
            "needs_context":{"type":"noul","noul":0.75},
            "blocked":{"type":"noul","noul":0.1},
            "making_progress":{"type":"noul","noul":0.6}
        },"usage":{}}"#;
        match parse_decision(&answers(body)) {
            GuardOutcome::Steer(hint) => assert!(hint.contains("missing information")),
            other => panic!("expected steer, got {other:?}"),
        }
    }

    #[test]
    fn request_state_and_questions_carry_the_diagnostics() {
        let input = GuardInput {
            task: "write src/a.rs".into(),
            context: "the crate uses the pom".into(),
            messages: vec![
                comrade_tool::GuardMessage {
                    role: "user".into(),
                    content: "do the thing".into(),
                },
                comrade_tool::GuardMessage {
                    role: "assistant".into(),
                    content: "ok".into(),
                },
            ],
        };
        let (state, questions) = JevGuardrail::state_and_questions(&input);
        assert_eq!(state["delegated_task"]["task"], "write src/a.rs");
        assert_eq!(state["delegated_task"]["context"], "the crate uses the pom");
        assert_eq!(state["recent_conversation"][0]["role"], "user");
        assert_eq!(state["recent_conversation"][1]["content"], "ok");
        for q in [
            Q_ON_TASK,
            Q_LOOPING,
            Q_NEEDS_CONTEXT,
            Q_BLOCKED,
            Q_MAKING_PROGRESS,
        ] {
            assert_eq!(questions[q]["type"], "noul", "question {q}");
        }
    }

    #[test]
    fn only_an_active_config_builds_a_guardrail() {
        let mut cfg = GuardrailsCfg::default();
        assert!(
            guardrail_from_cfg(&cfg).is_none(),
            "no key means no guardrail"
        );
        cfg.jev_api_key = Some("  ".into());
        assert!(guardrail_from_cfg(&cfg).is_none(), "blank key is unset");
        cfg.jev_api_key = Some("jev_test".into());
        assert!(guardrail_from_cfg(&cfg).is_some());
        cfg.enabled = false;
        assert!(guardrail_from_cfg(&cfg).is_none());
        cfg.enabled = true;
        cfg.jev_timeout_secs = 0;
        assert!(guardrail_from_cfg(&cfg).is_none());
    }
}
