//! Escalation contract: a delegated sub-agent asking its parent (the model that
//! owns the session, i.e. the tech lead) a question when it is stuck.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

/// Answered by the model that *owns* the session — the tech lead that delegated
/// the step — so a small sub-agent stuck on a decision can escalate instead of
/// looping or guessing. Implemented by `comrade-core`'s `ParentAsk` (a plain,
/// tool-less model call) and consumed by the `ask_upwards` tool.
///
/// The call must be bounded: whoever runs it is itself inside a run, so an
/// implementation should answer with one bounded model request and no tools.
/// The parent model's answer to a permission request ([`UpwardAsk::approve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The parent approved the action.
    Approved,
    /// The parent refused it, with the reason it gave.
    Denied(String),
    /// Nobody answered: no parent is wired, it did not answer in time, or it
    /// replied with something that is not a verdict. Callers must read this as a
    /// REFUSAL — a destructive action fails closed.
    Unavailable,
}

impl Verdict {
    /// Whether the action may proceed.
    pub fn is_approved(&self) -> bool {
        matches!(self, Verdict::Approved)
    }
}

#[async_trait]
pub trait UpwardAsk: Send + Sync {
    /// Ask the parent model `question` and return its answer.
    async fn ask(&self, question: &str) -> Result<String>;

    /// Ask the parent to APPROVE or REFUSE a destructive action: `title` names it
    /// (e.g. "overwrite src/lib.rs (deletes greet_works)") and `detail` says what
    /// it would destroy. A sub-agent calls this before deleting code its task did
    /// not ask it to touch, so the decision stays with the model that owns the
    /// task instead of a worker guessing.
    ///
    /// The default answers [`Verdict::Unavailable`], which callers read as a
    /// refusal: a parent that does not implement permission checks fails closed.
    async fn approve(&self, _title: &str, _detail: &str) -> Result<Verdict> {
        Ok(Verdict::Unavailable)
    }

    /// Summarise a long transcript so a sub-agent whose context overflowed can
    /// continue from a compact briefing instead of dying. `Ok(None)` means the
    /// parent cannot summarise (the default), which callers read as "no recovery
    /// available" so the sub-agent keeps its previous behaviour.
    async fn summarise(&self, _transcript: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Steer a sub-agent that is still RUNNING but has lost focus. The parent
    /// model is shown what the sub-agent has done so far and may return a short
    /// correction, which the caller injects into the sub-agent's conversation.
    ///
    /// `Ok(None)` - the default - means "leave it alone": no parent is wired, or
    /// the parent has no objection, so the caller keeps its current path.
    async fn supervise(&self, _briefing: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Decide how to recover from a sub-agent stuck in a loop. The parent is
    /// shown the sub-agent's status (task, context, what it has done) and picks
    /// one of [`Recovery`], which the caller reports back to the parent agent to
    /// act on.
    ///
    /// `Ok(None)` - the default - means the parent could not decide, so the
    /// caller falls back to a plain "stopped" answer.
    async fn recover(&self, _status: &str) -> Result<Option<Recovery>> {
        Ok(None)
    }
}

/// How the tech lead will recover from a delegated sub-agent that got stuck in a
/// loop (see [`UpwardAsk::recover`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// The lead will do the task itself instead of delegating it again.
    SelfWork,
    /// The lead will split the task into smaller, self-contained steps and
    /// delegate each of them again.
    Split,
    /// The lead will re-delegate the SAME task with a better context (carried in
    /// the string), for instance after supplying the information the delegate
    /// was missing.
    Restart(String),
}

/// Shared handle to the parent model, carried by the session ("this session's
/// own model") so every sub-agent of the session can reach it.
pub type Upward = Arc<dyn UpwardAsk>;

/// What a [`Guardrail`] says should happen to a RUNNING sub-agent that is being
/// watched mid-run (the tech lead's "guardrail mechanism").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Leave the sub-agent alone: it is on task and likely to make progress.
    Continue,
    /// The sub-agent needs a correction. The string is a short reason/hint
    /// (e.g. "it is missing context the lead has"); the caller hands it to the
    /// lead model, which writes the actual correction - a guardrail judges, it
    /// does not author prose.
    Steer(String),
    /// The sub-agent is stuck in a loop. Halt it and hand the decision back to
    /// the lead via [`UpwardAsk::recover`] (take the task over, split it, or
    /// restart it with a better context); the string is a short reason.
    Loop(String),
    /// Halt the sub-agent for any other reason and hand control back to the
    /// lead; the string is a short reason the caller reports to the lead.
    Stop(String),
}

/// One entry of a sub-agent's recent conversation, as shown to the guardrail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardMessage {
    /// `system` | `user` | `assistant` | `tool`.
    pub role: String,
    /// The message text (tool calls are included as a `[calls: …]` suffix).
    pub content: String,
}

/// What a guardrail judges: what the sub-agent was delegated plus the tail of
/// its conversation so far.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardInput {
    /// The delegated task (the parent's `task`).
    pub task: String,
    /// The background the parent gave the delegate (the parent's `context`;
    /// empty when it gave none).
    pub context: String,
    /// The most recent messages, oldest first.
    pub messages: Vec<GuardMessage>,
}

/// Decides what a tech lead should do about a sub-agent that is STILL RUNNING.
///
/// This is the guardrail seam: implemented by `comrade-core`'s `JevGuardrail`
/// (TypeSafe/Jev, `POST /v1/systemone`) and consulted at the supervision
/// interval. The implementation interrogates the conversation's state with
/// typed questions and returns a verdict; a [`GuardOutcome::Steer`] only says a
/// correction is needed, and the caller produces it with the lead model
/// ([`UpwardAsk::supervise`]).
///
/// The call must be bounded: whoever runs it is itself inside a run, so an
/// implementation must answer within its own timeout and must never block the
/// delegate indefinitely.
#[async_trait]
pub trait Guardrail: Send + Sync {
    /// Judge the running sub-agent from its task and recent conversation and
    /// return the action to take.
    async fn check(&self, input: &GuardInput) -> Result<GuardOutcome>;
}

/// Shared handle to the guardrail, carried by the session. `None` means no
/// guardrail is configured and the lead model supervises as before.
pub type Guard = Arc<dyn Guardrail>;
