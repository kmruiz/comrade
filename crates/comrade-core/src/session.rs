use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use comrade_tool::{ActivityEvents, PlanStatus, PlanStep, PlanTarget, SessionControl};
use tokio::sync::mpsc;

/// Events emitted by the session and the agent loop, consumed by the UI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Session title changed (re-read via the session object).
    TitleChanged,
    /// Status bar text changed (re-read via the session object).
    StatusChanged,
    /// The plan checklist changed (re-read via the session object).
    PlanChanged,
    /// The plan was finished.
    PlanFinished(Option<String>),
    /// The agent run started.
    RunStart,
    /// A message from the human user.
    User(String),
    /// A raw assistant message arrived (may contain Thought/Tool text).
    AssistantText(String),
    /// A chunk of the assistant message as it is being streamed.
    Delta(String),
    /// An isolated "Thought:" line from an assistant message.
    Thought(String),
    /// A tool call began, with the model's supplied reasoning when present.
    /// `tokens` is the real usage (prompt + completion) the model API reported
    /// for the request that produced this call, when available; for a turn
    /// that issued several calls it is attached to the first one only, so the
    /// UI can sum per-run usage without double counting.
    ToolCall {
        name: String,
        args: String,
        justification: Option<String>,
        tokens: Option<usize>,
    },
    /// A tool call began (legacy marker; details are in `ToolCall`).
    ToolStart { name: String, args: String },
    /// A tool call finished.
    ToolResult {
        name: String,
        output: String,
        ok: bool,
    },
    /// The agent produced its final answer.
    FinalAnswer(String),
    /// A non-fatal error surfaced during the run.
    Error(String),
    /// The run finished (success or not).
    RunEnd,
    /// Provider account balance refreshed after a run finished.
    AccountBalance(String),
    /// Live context-usage snapshot. `tokens` is the real `prompt_tokens`
    /// reported by the model API when available; otherwise it is our estimate
    /// and `estimated` is true.
    ContextStats {
        tokens: usize,
        budget: usize,
        estimated: bool,
    },
    /// The user asked to compact the context (M-c): the running history was
    /// replaced by a model-written summary.
    ContextCompacted {
        before_messages: usize,
        after_messages: usize,
        before_tokens: usize,
        after_tokens: usize,
    },
    /// A tool call made by a delegated sub-agent started. `model` is the
    /// delegate's configured name, so the UI can show the action under the
    /// delegate instead of the main model.
    DelegateToolCall {
        model: String,
        name: String,
        args: String,
    },
    /// A tool call made by a delegated sub-agent finished.
    DelegateToolResult {
        model: String,
        name: String,
        output: String,
        ok: bool,
    },
}

/// Bridges a session's UI event channel to the [`ActivityEvents`] sink carried
/// by every [`comrade_tool::ToolContext`], so tools that run their own
/// sub-agent loop (the `delegate` tool) can stream what that sub-agent is
/// doing into the chat as it happens. The agent loop attaches one of these to
/// the run's context at start; events are tagged with the delegate's name.
pub struct SessionEvents(pub mpsc::Sender<AgentEvent>);

#[async_trait]
impl ActivityEvents for SessionEvents {
    async fn tool_call(&self, author: &str, name: &str, args: &str) {
        let _ = self
            .0
            .send(AgentEvent::DelegateToolCall {
                model: author.to_string(),
                name: name.to_string(),
                args: args.to_string(),
            })
            .await;
    }

    async fn tool_result(&self, author: &str, name: &str, output: &str, ok: bool) {
        let _ = self
            .0
            .send(AgentEvent::DelegateToolResult {
                model: author.to_string(),
                name: name.to_string(),
                output: output.to_string(),
                ok,
            })
            .await;
    }
}

/// Observable session state. Doubles as the [`SessionControl`] implementation
/// the session tools mutate, and emits [`AgentEvent`]s on every change so the
/// UI can repaint.
pub struct AgentSession {
    tx: mpsc::Sender<AgentEvent>,
    title: RwLock<String>,
    status: RwLock<String>,
    plan: RwLock<Vec<PlanStep>>,
    finished: RwLock<Option<String>>,
    next_id: RwLock<u64>,
    /// Plan step ids the `delegate` tool has run at least once (so steps
    /// assigned a delegate model cannot be completed by the root itself).
    delegated: RwLock<HashSet<u64>>,
}

impl AgentSession {
    pub fn new(tx: mpsc::Sender<AgentEvent>) -> Self {
        Self {
            tx,
            title: RwLock::new("New session".to_string()),
            status: RwLock::new(String::new()),
            plan: RwLock::new(Vec::new()),
            finished: RwLock::new(None),
            next_id: RwLock::new(1),
            delegated: RwLock::new(HashSet::new()),
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.tx.try_send(event);
    }

    pub fn finished_summary(&self) -> Option<String> {
        self.finished.read().unwrap().clone()
    }

    pub fn as_control(self: Arc<Self>) -> Arc<dyn SessionControl> {
        self
    }

    /// Restore a saved session: replace title, status bar, plan, delegation
    /// records and finished summary, then notify the UI.
    pub fn restore(
        &self,
        title: String,
        status: String,
        plan: Vec<PlanStep>,
        delegated: HashSet<u64>,
        finished: Option<String>,
    ) {
        *self.title.write().unwrap() = title;
        *self.status.write().unwrap() = status;
        let max_id = plan.iter().map(|s| s.id).max().unwrap_or(0);
        *self.next_id.write().unwrap() = max_id + 1;
        *self.plan.write().unwrap() = plan;
        *self.delegated.write().unwrap() = delegated;
        *self.finished.write().unwrap() = finished;
        self.emit(AgentEvent::TitleChanged);
        self.emit(AgentEvent::StatusChanged);
        self.emit(AgentEvent::PlanChanged);
    }

    /// Snapshot the plan step ids the delegate tool has run (for saving).
    pub fn delegated_ids(&self) -> HashSet<u64> {
        self.delegated.read().unwrap().clone()
    }

    /// Next plan step id to be assigned (for testing).
    pub fn next_id(&self) -> u64 {
        *self.next_id.read().unwrap()
    }
}

impl SessionControl for AgentSession {
    fn set_title(&self, title: &str) {
        let t = title.trim();
        if t.is_empty() {
            return;
        }
        *self.title.write().unwrap() = t.to_string();
        self.emit(AgentEvent::TitleChanged);
    }

    fn title(&self) -> String {
        self.title.read().unwrap().clone()
    }

    fn set_plan(&self, steps: Vec<comrade_tool::PlanStepDraft>) {
        let mut plan = Vec::new();
        {
            let mut next = self.next_id.write().unwrap();
            *next = 1;
            for draft in steps {
                plan.push(PlanStep {
                    id: *next,
                    goal: draft.goal,
                    verification: draft.verification,
                    model: draft.model,
                    context: draft.context,
                    status: PlanStatus::Pending,
                    note: None,
                    started_at_ms: None,
                    took_ms: None,
                });
                *next += 1;
            }
        }
        *self.plan.write().unwrap() = plan;
        *self.delegated.write().unwrap() = HashSet::new();
        *self.finished.write().unwrap() = None;
        self.emit(AgentEvent::PlanChanged);
    }

    fn plan(&self) -> Vec<PlanStep> {
        self.plan.read().unwrap().clone()
    }

    fn update_plan(&self, target: PlanTarget, status: PlanStatus, note: Option<String>) -> bool {
        let mut plan = self.plan.write().unwrap();
        let hit = plan.iter_mut().find(|s| match &target {
            PlanTarget::Id(id) => s.id == *id,
            PlanTarget::Text(text) => s.goal.contains(text.as_str()),
        });
        match hit {
            Some(step) => {
                // Route the transition through PlanStep::update so per-step
                // timing (started_at_ms / took_ms) is recorded.
                step.update(status, note);
                drop(plan);
                self.emit(AgentEvent::PlanChanged);
                true
            }
            None => false,
        }
    }

    fn finish_plan(&self, summary: Option<String>) {
        {
            let mut plan = self.plan.write().unwrap();
            for step in plan.iter_mut() {
                if !matches!(step.status, PlanStatus::Done | PlanStatus::Blocked) {
                    step.update(PlanStatus::Done, None);
                }
            }
        }
        *self.finished.write().unwrap() = summary.clone();
        self.emit(AgentEvent::PlanFinished(summary));
    }

    fn mark_step_delegated(&self, step_id: u64) {
        self.delegated.write().unwrap().insert(step_id);
    }

    fn step_was_delegated(&self, step_id: u64) -> bool {
        self.delegated.read().unwrap().contains(&step_id)
    }

    fn reassign_step_model(&self, target: &PlanTarget, model: &str) -> Result<bool, String> {
        let (step_id, was_ready) = {
            let mut plan = self.plan.write().unwrap();
            let hit = plan.iter_mut().find(|s| match target {
                PlanTarget::Id(id) => s.id == *id,
                PlanTarget::Text(text) => s.goal.contains(text.as_str()),
            });
            let Some(step) = hit else {
                return Ok(false);
            };
            if matches!(step.status, PlanStatus::InProgress | PlanStatus::Done) {
                return Err(format!(
                    "plan step {} is {} — a step's model can only be changed while it is \
                     pending, ready or blocked",
                    step.id, step.status
                ));
            }
            step.model = model.trim().to_string();
            (step.id, step.status == PlanStatus::Ready)
        };
        // A new model must actually run the step: drop the old delegation
        // record so update_plan/finish_plan keep enforcing the delegate run.
        self.delegated.write().unwrap().remove(&step_id);
        // A Ready step's readiness was confirmed by the OLD delegate: a new
        // model must be consulted again before the step is picked up.
        if was_ready {
            self.update_plan(
                PlanTarget::Id(step_id),
                PlanStatus::Pending,
                Some("ready reset: a new model must confirm the context".into()),
            );
        }
        self.emit(AgentEvent::PlanChanged);
        Ok(true)
    }

    fn set_step_context(&self, target: &PlanTarget, context: &str) -> Result<bool, String> {
        let context = context.trim();
        let (step_id, was_ready) = {
            let mut plan = self.plan.write().unwrap();
            let hit = plan.iter_mut().find(|s| match target {
                PlanTarget::Id(id) => s.id == *id,
                PlanTarget::Text(text) => s.goal.contains(text.as_str()),
            });
            let Some(step) = hit else {
                return Ok(false);
            };
            if matches!(step.status, PlanStatus::InProgress | PlanStatus::Done) {
                return Err(format!(
                    "plan step {} is {} — a step's context can only be changed while it is \
                     pending, ready or blocked",
                    step.id, step.status
                ));
            }
            if context.is_empty() {
                return Err(format!("plan step {} needs a non-empty context", step.id));
            }
            step.context = context.to_string();
            (step.id, step.status == PlanStatus::Ready)
        };
        // The readiness confirmation described the old context: replacing the
        // context sends the step back to pending so the delegate is consulted
        // again (ask_advise step = id) before it is picked up.
        if was_ready {
            self.update_plan(
                PlanTarget::Id(step_id),
                PlanStatus::Pending,
                Some("ready reset: context changed".into()),
            );
        }
        self.emit(AgentEvent::PlanChanged);
        Ok(true)
    }

    fn set_status(&self, status: &str) {
        *self.status.write().unwrap() = status.to_string();
        self.emit(AgentEvent::StatusChanged);
    }

    fn status(&self) -> String {
        self.status.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comrade_tool::{AGENT_MODEL, PlanStatus, PlanStepDraft, PlanTarget, SessionControl};

    fn draft(goal: &str, verification: &str) -> PlanStepDraft {
        PlanStepDraft {
            goal: goal.to_string(),
            verification: verification.to_string(),
            model: String::new(),
            context: String::new(),
        }
    }

    #[test]
    fn delegated_steps_are_tracked_and_cleared_when_plan_is_reset() {
        let (tx, _rx) = mpsc::channel(16);
        let s = AgentSession::new(tx);
        s.set_plan(vec![PlanStepDraft {
            goal: "delegate me".into(),
            verification: String::new(),
            model: "cheap".into(),
            context: String::new(),
        }]);
        assert!(!s.step_was_delegated(1));
        s.mark_step_delegated(1);
        assert!(s.step_was_delegated(1));

        // a brand-new plan restarts the delegation record with the ids
        s.set_plan(vec![draft("only", "")]);
        assert!(!s.step_was_delegated(1));
    }

    #[test]
    fn plan_lifecycle() {
        let (tx, _rx) = mpsc::channel(16);
        let s = AgentSession::new(tx);
        s.set_title("Rename it");
        assert_eq!(s.title(), "Rename it");

        s.set_plan(vec![
            draft("find references", "rgrep shows all call sites"),
            draft("rename", "cargo check passes"),
        ]);
        let plan = s.plan();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].id, 1);
        assert_eq!(plan[0].goal, "find references");
        assert_eq!(plan[0].verification, "rgrep shows all call sites");

        assert!(s.update_plan(PlanTarget::Id(2), PlanStatus::InProgress, None));
        assert_eq!(s.plan()[1].status, PlanStatus::InProgress);

        // model + context round-trip from draft into the live plan
        s.set_plan(vec![PlanStepDraft {
            goal: "delegate me".into(),
            verification: "".into(),
            model: "groq".into(),
            context: "summarised parent context".into(),
        }]);
        assert_eq!(s.plan()[0].model, "groq");
        assert_eq!(s.plan()[0].context, "summarised parent context");

        // resetting the plan restarts ids at 1
        s.set_plan(vec![draft("only", "")]);
        assert_eq!(s.plan()[0].id, 1);

        s.set_status("main");
        assert_eq!(s.status(), "main");

        s.finish_plan(Some("done".into()));
        assert!(s.finished_summary().is_some());
        assert!(s.plan().iter().all(|p| p.status == PlanStatus::Done));
    }

    #[test]
    fn reassign_changes_model_and_clears_delegation_record() {
        let (tx, _rx) = mpsc::channel(16);
        let s = AgentSession::new(tx);
        s.set_plan(vec![
            PlanStepDraft {
                goal: "step one".into(),
                verification: String::new(),
                model: "mistral".into(),
                context: String::new(),
            },
            PlanStepDraft {
                goal: "step two".into(),
                verification: String::new(),
                model: AGENT_MODEL.into(),
                context: String::new(),
            },
            PlanStepDraft {
                goal: "step three".into(),
                verification: String::new(),
                model: AGENT_MODEL.into(),
                context: String::new(),
            },
            PlanStepDraft {
                goal: "step four".into(),
                verification: String::new(),
                model: AGENT_MODEL.into(),
                context: String::new(),
            },
        ]);
        s.mark_step_delegated(1);
        assert!(s.step_was_delegated(1));

        // pending step: model changes and the delegation record is cleared so
        // the newly assigned model must actually run the step.
        assert!(
            s.reassign_step_model(&PlanTarget::Id(1), AGENT_MODEL)
                .unwrap()
        );
        assert_eq!(s.plan()[0].model, AGENT_MODEL);
        assert!(!s.step_was_delegated(1));

        // delegate -> delegate via text target also works on a blocked step.
        s.update_plan(PlanTarget::Id(2), PlanStatus::Blocked, None);
        assert!(
            s.reassign_step_model(&PlanTarget::Text("step two".into()), "groq")
                .unwrap()
        );
        assert_eq!(s.plan()[1].model, "groq");

        // done / in_progress steps are protected.
        s.update_plan(PlanTarget::Id(3), PlanStatus::Done, None);
        s.update_plan(PlanTarget::Id(4), PlanStatus::InProgress, None);
        let err = s
            .reassign_step_model(&PlanTarget::Id(3), "mistral")
            .unwrap_err();
        assert!(err.contains("done"), "unexpected error: {err}");
        let err = s
            .reassign_step_model(&PlanTarget::Id(4), "mistral")
            .unwrap_err();
        assert!(err.contains("in_progress"), "unexpected error: {err}");
        assert_eq!(s.plan()[3].model, AGENT_MODEL);
        assert_eq!(s.plan()[2].model, AGENT_MODEL);

        // unknown target -> Ok(false), nothing changed.
        assert!(
            !s.reassign_step_model(&PlanTarget::Id(99), "mistral")
                .unwrap()
        );
    }

    #[test]
    fn ready_steps_drop_to_pending_on_reassign() {
        let (tx, _rx) = mpsc::channel(16);
        let s = AgentSession::new(tx);
        s.set_plan(vec![PlanStepDraft {
            goal: "delegate me".into(),
            verification: "".into(),
            model: "mistral".into(),
            context: "ctx".into(),
        }]);
        // The delegate confirmed the context, then the lead changes the model:
        // the readiness was for the old delegate, so the step goes back to
        // pending.
        s.update_plan(
            PlanTarget::Id(1),
            PlanStatus::Ready,
            Some("ready: ok".into()),
        );
        assert_eq!(s.plan()[0].status, PlanStatus::Ready);
        assert!(s.reassign_step_model(&PlanTarget::Id(1), "groq").unwrap());
        assert_eq!(s.plan()[0].model, "groq");
        assert_eq!(s.plan()[0].status, PlanStatus::Pending);
        assert_eq!(
            s.plan()[0].note.as_deref(),
            Some("ready reset: a new model must confirm the context")
        );
    }

    #[test]
    fn set_step_context_replaces_context_and_resets_ready() {
        let (tx, _rx) = mpsc::channel(16);
        let s = AgentSession::new(tx);
        s.set_plan(vec![PlanStepDraft {
            goal: "delegate me".into(),
            verification: "".into(),
            model: "mistral".into(),
            context: "old context".into(),
        }]);

        // ready step: context replacement sends it back to pending.
        s.update_plan(
            PlanTarget::Id(1),
            PlanStatus::Ready,
            Some("ready: ok".into()),
        );
        assert!(
            s.set_step_context(&PlanTarget::Id(1), "new enriched context")
                .unwrap()
        );
        assert_eq!(s.plan()[0].context, "new enriched context");
        assert_eq!(s.plan()[0].status, PlanStatus::Pending);

        // pending step: just replaces the context.
        assert!(
            s.set_step_context(&PlanTarget::Text("delegate me".into()), "more context")
                .unwrap()
        );
        assert_eq!(s.plan()[0].context, "more context");
        assert_eq!(s.plan()[0].status, PlanStatus::Pending);

        // in_progress / done steps are protected; empty context is rejected.
        s.update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
        let err = s.set_step_context(&PlanTarget::Id(1), "x").unwrap_err();
        assert!(err.contains("in_progress"), "unexpected error: {err}");
        s.update_plan(PlanTarget::Id(1), PlanStatus::Done, None);
        let err = s.set_step_context(&PlanTarget::Id(1), "x").unwrap_err();
        assert!(err.contains("done"), "unexpected error: {err}");
        s.update_plan(PlanTarget::Id(1), PlanStatus::Blocked, None);
        let err = s.set_step_context(&PlanTarget::Id(1), "   ").unwrap_err();
        assert!(err.contains("non-empty"), "unexpected error: {err}");
        // blocked steps may still get a context update (to unblock them).
        assert!(s.set_step_context(&PlanTarget::Id(1), "x").unwrap());
        assert_eq!(s.plan()[0].context, "x");

        // unknown target -> Ok(false).
        assert!(!s.set_step_context(&PlanTarget::Id(99), "x").unwrap());
    }

    #[test]
    fn restore_replaces_title_status_plan_delegated_and_finished() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let s = AgentSession::new(tx);

        // Initial state
        s.set_title("Initial Title");
        s.set_status("Initial Status");
        s.set_plan(vec![draft("initial step", "")]);
        s.mark_step_delegated(1);
        s.finish_plan(Some("Initial finished".into()));

        // Prepare restored data
        let restored_plan = vec![
            PlanStep {
                id: 10,
                goal: "restored step 1".into(),
                verification: "verify 1".into(),
                model: "model1".into(),
                context: "ctx1".into(),
                status: PlanStatus::Done,
                note: None,
                started_at_ms: None,
                took_ms: None,
            },
            PlanStep {
                id: 20,
                goal: "restored step 2".into(),
                verification: "verify 2".into(),
                model: "model2".into(),
                context: "ctx2".into(),
                status: PlanStatus::Pending,
                note: None,
                started_at_ms: None,
                took_ms: None,
            },
        ];
        let mut restored_delegated = HashSet::new();
        restored_delegated.insert(10);

        // Restore
        s.restore(
            "Restored Title".into(),
            "Restored Status".into(),
            restored_plan,
            restored_delegated,
            Some("Restored finished".into()),
        );

        // Verify all fields were replaced
        assert_eq!(s.title(), "Restored Title");
        assert_eq!(s.status(), "Restored Status");
        assert_eq!(s.plan().len(), 2);
        assert_eq!(s.plan()[0].goal, "restored step 1");
        assert_eq!(s.plan()[1].goal, "restored step 2");
        assert_eq!(s.finished_summary(), Some("Restored finished".into()));
        assert!(s.step_was_delegated(10));
        assert!(!s.step_was_delegated(20));
    }

    #[test]
    fn restore_sets_next_id_past_max_restored_id() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let s = AgentSession::new(tx);

        // Restore a plan with ids 1..3
        let restored_plan = vec![
            PlanStep {
                id: 1,
                goal: "step 1".into(),
                verification: "".into(),
                model: "".into(),
                context: "".into(),
                status: PlanStatus::Pending,
                note: None,
                started_at_ms: None,
                took_ms: None,
            },
            PlanStep {
                id: 2,
                goal: "step 2".into(),
                verification: "".into(),
                model: "".into(),
                context: "".into(),
                status: PlanStatus::Pending,
                note: None,
                started_at_ms: None,
                took_ms: None,
            },
            PlanStep {
                id: 3,
                goal: "step 3".into(),
                verification: "".into(),
                model: "".into(),
                context: "".into(),
                status: PlanStatus::Pending,
                note: None,
                started_at_ms: None,
                took_ms: None,
            },
        ];

        s.restore(
            "Title".into(),
            "Status".into(),
            restored_plan,
            HashSet::new(),
            None,
        );

        // next_id should be 4 (max restored id + 1)
        assert_eq!(s.next_id(), 4);
    }

    #[test]
    fn delegated_ids_returns_snapshot() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let s = AgentSession::new(tx);

        s.set_plan(vec![draft("step 1", ""), draft("step 2", "")]);
        s.mark_step_delegated(1);
        s.mark_step_delegated(2);

        let ids = s.delegated_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }
}
