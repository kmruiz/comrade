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
}

/// Shared handle to the parent model, carried by the session ("this session's
/// own model") so every sub-agent of the session can reach it.
pub type Upward = Arc<dyn UpwardAsk>;
