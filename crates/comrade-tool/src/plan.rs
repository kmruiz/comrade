use std::fmt;

/// Status of a single plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Done => "done",
            PlanStatus::Blocked => "blocked",
        }
    }

    /// Recover a status from its string form; case- and punctuation-insensitive.
    pub fn parse(s: &str) -> Option<PlanStatus> {
        let n = s.to_ascii_lowercase().replace(['-', ' '], "_");
        match n.as_str() {
            "pending" | "todo" | "waiting" => Some(PlanStatus::Pending),
            "in_progress" | "inprogress" | "running" | "doing" | "active" => {
                Some(PlanStatus::InProgress)
            }
            "done" | "complete" | "completed" | "finished" => Some(PlanStatus::Done),
            "blocked" | "stuck" | "waiting_on" | "deferred" => Some(PlanStatus::Blocked),
            _ => None,
        }
    }
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the session plan the agent presents in the UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanStep {
    /// Stable 1-based id assigned when the plan is (re)set.
    pub id: u64,
    pub description: String,
    pub status: PlanStatus,
    pub note: Option<String>,
}

/// Selector used by `update_plan`.
#[derive(Debug, Clone)]
pub enum PlanTarget {
    /// 1-based step id as reported by `set_plan`/`plan()`.
    Id(u64),
    /// First step whose description contains this text.
    Text(String),
}

/// The session state UI tools can read and mutate.
///
/// `comrade-core` owns the real implementation (an observable struct that also
/// emits events for the TUI). Tool crates only ever talk to this trait.
pub trait SessionControl: Send + Sync {
    fn set_title(&self, title: &str);
    fn title(&self) -> String;

    /// Replace the whole plan with the given steps. ids are assigned
    /// sequentially starting at 1.
    fn set_plan(&self, steps: Vec<String>);
    /// Current plan snapshot.
    fn plan(&self) -> Vec<PlanStep>;

    /// Transition a step's status. Returns false if the target did not match.
    fn update_plan(&self, target: PlanTarget, status: PlanStatus, note: Option<String>) -> bool;

    /// Mark the whole plan finished with an optional closing summary.
    fn finish_plan(&self, summary: Option<String>);

    fn set_status(&self, status: &str);
    fn status(&self) -> String;
}
