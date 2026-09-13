use std::fmt;

/// Reserved `PlanStepDraft::model` value naming the main agent model itself.
///
/// Plan steps must always state who runs them. Writing `AGENT_MODEL` ("self")
/// means the main tech-lead model executes the step; any other non-empty model
/// must be the name of a configured delegate (see the `delegate` tool's model
/// listing). Delegation enforcement relies on this: steps whose `model` is not
/// `AGENT_MODEL` are delegated steps and cannot be completed by the lead alone.
pub const AGENT_MODEL: &str = "self";

/// Status of a single plan step.
///
/// The lifecycle for a step assigned to a delegate model is
/// `Pending -> Ready -> InProgress -> Done` (or `Blocked`): the tech lead first
/// asks the step's delegate (ask_advise `step` = id) whether its context is
/// sufficient, and once the delegate confirms the context the step becomes
/// [`PlanStatus::Ready`] — "ready to pick up". Only then is the step delegated
/// (which moves it to `InProgress`/working). A step whose delegate still needs
/// more context stays `Pending` with an "awaiting context: ..." note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Created by `set_plan`, or put back when more context is needed before
    /// the assigned delegate can pick the step up.
    Pending,
    /// The step's assigned delegate was consulted (ask_advise `step` = id) and
    /// confirmed that the step's context is sufficient for it to do the work.
    /// Sits between `Pending` and `InProgress`; delegating the step moves it to
    /// `InProgress`.
    Ready,
    /// A model (the lead or a delegate) is currently working the step.
    InProgress,
    /// Work finished and verified.
    Done,
    /// Could not be completed as planned.
    Blocked,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::Ready => "ready",
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
            "ready" | "prepared" | "confirmed" | "ok" => Some(PlanStatus::Ready),
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

/// A step draft as submitted by `set_plan`. Each step is self-contained (goal +
/// verification), so steps can later be executed by independent agents.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanStepDraft {
    /// What this step aims to accomplish.
    pub goal: String,
    /// How to know the step succeeded (a command to run, a check, expected
    /// outcome). Optional but strongly encouraged for isolatable steps.
    #[serde(default)]
    pub verification: String,
    /// Which model will execute this step. Mandatory: the reserved value
    /// [`AGENT_MODEL`] ("self") when the main model runs it, or a configured
    /// delegate's name. Shown in the UI.
    #[serde(default)]
    pub model: String,
    /// Summarised context the executing model needs for this step. Fed to the
    /// delegate verbatim; intentionally never rendered in the UI.
    #[serde(default)]
    pub context: String,
}

/// One row of the session plan the agent presents in the UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    /// Stable 1-based id assigned when the plan is (re)set.
    pub id: u64,
    /// What this step aims to accomplish.
    pub goal: String,
    /// How to verify the step succeeded (may be empty).
    #[serde(default)]
    pub verification: String,
    /// Which model will execute this step. [`AGENT_MODEL`] ("self") means the
    /// main model; anything else is a delegate's name.
    #[serde(default)]
    pub model: String,
    /// Summarised context for the executing model (never shown in the UI).
    #[serde(default)]
    pub context: String,
    pub status: PlanStatus,
    #[serde(default)]
    pub note: Option<String>,
    /// Wall-clock millis (UNIX epoch) when the step was first marked
    /// `InProgress` (started being worked), None until then.
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    /// Elapsed millis once the step reached a terminal status (`Done` or
    /// `Blocked`), measured from `started_at_ms`. None while the step is still
    /// running or when it finished without ever being started (e.g. force-finished
    /// by `finish_plan` while still `Pending`).
    #[serde(default)]
    pub took_ms: Option<u64>,
}

impl PlanStep {
    pub fn draft(goal: &str, verification: &str) -> PlanStepDraft {
        PlanStepDraft {
            goal: goal.to_string(),
            verification: verification.to_string(),
            model: String::new(),
            context: String::new(),
        }
    }

    /// Transition this step to `status` with a fresh note, recording how long
    /// the step took (see [`Self::update_at`] for the timing rules).
    pub fn update(&mut self, status: PlanStatus, note: Option<String>) {
        self.update_at(status, note, now_ms());
    }

    /// Transition this step to `status` using `now_ms` as the current wall
    /// clock (injected so tests are deterministic).
    ///
    /// Timing rules:
    /// - becoming `InProgress` from a terminal status (Done/Blocked) restarts
    ///   the clock; otherwise the first `InProgress` sets the start time (fix
    ///   rounds keep the same start);
    /// - becoming `Done`/`Blocked` freezes the elapsed time;
    /// - going back to `Pending` (e.g. a delegate run aborted) clears any
    ///   partial timing so the next attempt measures only itself.
    pub fn update_at(&mut self, status: PlanStatus, note: Option<String>, now_ms: u64) {
        if let Some(n) = note {
            let t = n.trim();
            if !t.is_empty() {
                self.note = Some(t.to_string());
            }
        }
        if self.status == status {
            return;
        }
        match status {
            PlanStatus::Pending | PlanStatus::Ready => {
                self.started_at_ms = None;
                self.took_ms = None;
            }
            PlanStatus::InProgress => {
                if matches!(self.status, PlanStatus::Done | PlanStatus::Blocked) {
                    self.started_at_ms = None;
                    self.took_ms = None;
                }
                self.started_at_ms.get_or_insert(now_ms);
            }
            PlanStatus::Done | PlanStatus::Blocked => {
                if let Some(started) = self.started_at_ms {
                    self.took_ms = Some(now_ms.saturating_sub(started));
                }
            }
        }
        self.status = status;
    }
}

/// Current wall-clock time in millis since the UNIX epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Selector used by `update_plan`.
#[derive(Debug, Clone)]
pub enum PlanTarget {
    /// 1-based step id as reported by `set_plan`/`plan()`.
    Id(u64),
    /// First step whose goal contains this text.
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
    fn set_plan(&self, steps: Vec<PlanStepDraft>);
    /// Current plan snapshot.
    fn plan(&self) -> Vec<PlanStep>;

    /// Transition a step's status. Returns false if the target did not match.
    fn update_plan(&self, target: PlanTarget, status: PlanStatus, note: Option<String>) -> bool;

    /// Mark the whole plan finished with an optional closing summary.
    fn finish_plan(&self, summary: Option<String>);

    /// Record that the `delegate` tool ran the given plan step (a delegate
    /// reply was received). Used to enforce that steps assigned a delegate
    /// model are executed on that delegate and cannot be silently completed by
    /// the tech lead itself.
    fn mark_step_delegated(&self, _step_id: u64) {}

    /// True when the `delegate` tool has run this plan step at least once.
    /// Defaults to false for implementations that do not track delegation.
    fn step_was_delegated(&self, _step_id: u64) -> bool {
        false
    }

    /// Reassign which model runs an existing plan step, e.g. to hand a step
    /// assigned to the main model ([`AGENT_MODEL`]) to a delegate, or to take a
    /// delegate-assigned step back onto yourself.
    ///
    /// Allowed only while the step is `Pending`, `Ready` or `Blocked`: a step
    /// that is `InProgress` (a model is already working it) or `Done` keeps its
    /// model.
    ///
    /// Reassignment also clears the step's delegation record ([`Self::step_was_delegated`]
    /// becomes false), so the newly assigned model must actually run the step
    /// before a delegated step can be marked done. A `Ready` step drops back to
    /// `Pending`: its readiness was confirmed by the old delegate, and the new
    /// model must be consulted again before the step is picked up.
    ///
    /// Returns `Ok(true)` when the step was reassigned, `Ok(false)` when no step
    /// matched `target`, and `Err(reason)` when the matched step may not be
    /// reassigned.
    fn reassign_step_model(&self, _target: &PlanTarget, _model: &str) -> Result<bool, String> {
        Err("reassigning a step's model is not supported by this session".to_string())
    }

    /// Replace the summarised context of an existing plan step. The context is
    /// fed to the executing model verbatim and never shown in the UI, so this
    /// is how a tech lead enriches a step after its delegate reported (via
    /// ask_advise `step` = id) that it needs more context to pick the step up.
    ///
    /// Allowed only while the step is `Pending`, `Ready` or `Blocked`: a step
    /// that is `InProgress` (a model is already working it) or `Done` keeps its
    /// context. Setting the context of a `Ready` step drops it back to
    /// `Pending` — the readiness confirmation described a different context, so
    /// the delegate must be consulted again before the step is picked up.
    ///
    /// Returns `Ok(true)` when the context was replaced, `Ok(false)` when no
    /// step matched `target`, and `Err(reason)` when the matched step may not
    /// be changed.
    fn set_step_context(&self, _target: &PlanTarget, _context: &str) -> Result<bool, String> {
        Err("setting a step's context is not supported by this session".to_string())
    }

    fn set_status(&self, status: &str);
    fn status(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step() -> PlanStep {
        PlanStep {
            id: 1,
            goal: "g".into(),
            verification: String::new(),
            model: String::new(),
            context: String::new(),
            status: PlanStatus::Pending,
            note: None,
            started_at_ms: None,
            took_ms: None,
        }
    }

    #[test]
    fn inprogress_then_done_records_elapsed() {
        let mut s = step();
        s.update_at(PlanStatus::InProgress, None, 1_000);
        assert_eq!(s.started_at_ms, Some(1_000));
        assert_eq!(s.took_ms, None);
        // note-only fix rounds don't restart the clock
        s.update_at(
            PlanStatus::InProgress,
            Some("working: fix 1/5".into()),
            5_000,
        );
        assert_eq!(s.started_at_ms, Some(1_000));
        s.update_at(PlanStatus::Done, None, 6_000);
        assert_eq!(s.took_ms, Some(5_000));
        assert_eq!(s.status, PlanStatus::Done);
        // repeated Done is a no-op for timing
        s.update_at(PlanStatus::Done, None, 9_999);
        assert_eq!(s.took_ms, Some(5_000));
    }

    #[test]
    fn pending_never_started_has_no_duration() {
        let mut s = step();
        s.update_at(PlanStatus::Done, None, 10_000);
        assert_eq!(s.status, PlanStatus::Done);
        assert_eq!(s.took_ms, None);
    }

    #[test]
    fn back_to_pending_clears_partial_timing() {
        let mut s = step();
        s.update_at(PlanStatus::InProgress, None, 100);
        s.update_at(
            PlanStatus::Pending,
            Some("delegate failed to run".into()),
            200,
        );
        assert_eq!(s.started_at_ms, None);
        assert_eq!(s.took_ms, None);
        // next attempt measures only itself
        s.update_at(PlanStatus::InProgress, None, 300);
        s.update_at(PlanStatus::Done, None, 350);
        assert_eq!(s.took_ms, Some(50));
        assert_eq!(s.note.as_deref(), Some("delegate failed to run"));
    }

    #[test]
    fn rerun_after_blocked_restarts_the_clock() {
        let mut s = step();
        s.update_at(PlanStatus::InProgress, None, 1_000);
        s.update_at(PlanStatus::Blocked, None, 2_000);
        assert_eq!(s.took_ms, Some(1_000));
        s.update_at(PlanStatus::InProgress, None, 3_000);
        s.update_at(PlanStatus::Done, None, 3_200);
        assert_eq!(s.took_ms, Some(200));
    }

    #[test]
    fn note_is_trimmed_and_blank_notes_ignored() {
        let mut s = step();
        s.update_at(PlanStatus::InProgress, Some("  hello  ".into()), 1);
        assert_eq!(s.note.as_deref(), Some("hello"));
        s.update_at(PlanStatus::InProgress, Some("   ".into()), 2);
        assert_eq!(s.note.as_deref(), Some("hello"));
    }

    #[test]
    fn ready_is_between_pending_and_inprogress() {
        assert_eq!(PlanStatus::Ready.as_str(), "ready");
        assert_eq!(PlanStatus::parse("ready"), Some(PlanStatus::Ready));
        assert_eq!(PlanStatus::parse("prepared"), Some(PlanStatus::Ready));

        // ready is a pre-work state: no clock while waiting to be picked up
        let mut s = step();
        s.update_at(PlanStatus::Ready, None, 1_000);
        assert_eq!(s.started_at_ms, None);
        assert_eq!(s.took_ms, None);
        // picking the step up starts the clock from the Ready state
        s.update_at(PlanStatus::InProgress, None, 2_000);
        s.update_at(PlanStatus::Done, None, 2_500);
        assert_eq!(s.took_ms, Some(500));
    }

    #[test]
    fn back_to_ready_clears_partial_timing() {
        let mut s = step();
        s.update_at(PlanStatus::InProgress, None, 100);
        // a delegate run aborted and the step returns to the ready state it was
        // in before being picked up: the aborted attempt must not count
        s.update_at(
            PlanStatus::Ready,
            Some("delegate failed to run".into()),
            200,
        );
        assert_eq!(s.started_at_ms, None);
        assert_eq!(s.took_ms, None);
        s.update_at(PlanStatus::InProgress, None, 300);
        s.update_at(PlanStatus::Done, None, 350);
        assert_eq!(s.took_ms, Some(50));
    }

    #[test]
    fn plan_step_serde_roundtrip() {
        let step = PlanStep {
            id: 1,
            goal: "test goal".to_string(),
            verification: "test verification".to_string(),
            model: "test model".to_string(),
            context: "test context".to_string(),
            status: PlanStatus::Ready,
            note: Some("n".to_string()),
            started_at_ms: Some(1),
            took_ms: Some(2),
        };

        let json = serde_json::to_string(&step).unwrap();
        let parsed: PlanStep = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, step.id);
        assert_eq!(parsed.goal, step.goal);
        assert_eq!(parsed.verification, step.verification);
        assert_eq!(parsed.model, step.model);
        assert_eq!(parsed.context, step.context);
        assert_eq!(parsed.status, step.status);
        assert_eq!(parsed.note, step.note);
        assert_eq!(parsed.started_at_ms, step.started_at_ms);
        assert_eq!(parsed.took_ms, step.took_ms);
    }
}
