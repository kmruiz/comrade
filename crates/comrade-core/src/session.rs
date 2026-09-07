use std::sync::{Arc, RwLock};

use comrade_tool::{PlanStatus, PlanStep, PlanTarget, SessionControl};
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
    ToolCall {
        name: String,
        args: String,
        justification: Option<String>,
        risk: Option<String>,
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
    /// Live context-usage snapshot. `tokens` is the real `prompt_tokens`
    /// reported by the model API when available; otherwise it is our estimate
    /// and `estimated` is true.
    ContextStats {
        tokens: usize,
        budget: usize,
        estimated: bool,
    },
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
                    status: PlanStatus::Pending,
                    note: None,
                });
                *next += 1;
            }
        }
        *self.plan.write().unwrap() = plan;
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
                step.status = status;
                if let Some(n) = note {
                    if !n.trim().is_empty() {
                        step.note = Some(n.trim().to_string());
                    }
                }
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
                    step.status = PlanStatus::Done;
                }
            }
        }
        *self.finished.write().unwrap() = summary.clone();
        self.emit(AgentEvent::PlanFinished(summary));
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
    use comrade_tool::{PlanStatus, PlanStepDraft, PlanTarget, SessionControl};

    fn draft(goal: &str, verification: &str) -> PlanStepDraft {
        PlanStepDraft {
            goal: goal.to_string(),
            verification: verification.to_string(),
        }
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

        // resetting the plan restarts ids at 1
        s.set_plan(vec![draft("only", "")]);
        assert_eq!(s.plan()[0].id, 1);

        s.set_status("main");
        assert_eq!(s.status(), "main");

        s.finish_plan(Some("done".into()));
        assert!(s.finished_summary().is_some());
        assert!(s.plan().iter().all(|p| p.status == PlanStatus::Done));
    }
}
