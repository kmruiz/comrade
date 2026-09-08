//! Session/UI control tools.
//!
//! These tools do not touch the repository. They drive the *session* the
//! agent is running in: its title, the plan checklist shown in the UI, the
//! status bar, and interactive questions for the human. They talk to the
//! outside world exclusively through [`comrade_tool::SessionControl`] and
//! [`comrade_tool::UserIo`], so they stay decoupled from the TUI and core.

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{
    AGENT_MODEL, PlanStatus, PlanTarget, Tool, ToolContext, ToolSpec, UserPrompt, UserReply,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Whether a plan step's `model` names a delegate rather than the main agent.
/// The main model is [`AGENT_MODEL`] ("self"); any other non-empty model is a
/// configured delegate, so its step can only be completed via the `delegate`
/// tool (an empty model, from direct session writes, also counts as self).
fn is_delegate_model(model: &str) -> bool {
    let m = model.trim();
    !m.is_empty() && m != AGENT_MODEL
}

/// All session-control tools.
pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(RenameSession),
        Box::new(SetPlan),
        Box::new(UpdatePlan),
        Box::new(SetStepModel),
        Box::new(FinishPlan),
        Box::new(SetStatusBar),
        Box::new(AskQuestion),
    ]
}

// ---------------------------------------------------------------------------
// rename_session
// ---------------------------------------------------------------------------

struct RenameSession;

static RENAME_SESSION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "rename_session".into(),
    description: "Change the display title of the current session. Call early to give the task a short, descriptive name.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "New session title." }
        },
        "required": ["title"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RenameSession {
    fn spec(&self) -> &ToolSpec {
        &RENAME_SESSION_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            title: String,
        }
        let args: Args = serde_json::from_value(args)?;
        ctx.session.set_title(&args.title);
        Ok(format!("Session renamed to {:?}.", args.title))
    }
}

// ---------------------------------------------------------------------------
// set_plan
// ---------------------------------------------------------------------------

struct SetPlan;

static SET_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "set_plan".into(),
    description: "Lay out the plan before doing work. Each step is an isolated unit with a goal, a verification (how to prove it succeeded) and the model that will run it, so steps can later be run independently or delegated. Replaces any existing plan; advance steps with update_plan.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string", "description": "What this step aims to accomplish, summarised, for a human to read." },
                        "verification": { "type": "string", "description": "How to verify the step succeeded, e.g. \"cargo test passes\" or \"rgrep finds the new call sites\"." },
                        "model": { "type": "string", "description": "REQUIRED. The model that will execute this step: write \"self\" when you (the main agent model) will run it yourself, or one of the configured delegate names (see the delegate tool's model listing). Every step must name who runs it." },
                        "context": { "type": "string", "description": "Summarised context the executing model needs for this step (mandatory; never shown in the UI)." }
                    },
                    "required": ["goal", "model", "verification", "context"],
                    "additionalProperties": false
                },
                "minItems": 1,
                "description": "Ordered steps; keep each one small and self-contained."
            }
        },
        "required": ["steps"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for SetPlan {
    fn spec(&self) -> &ToolSpec {
        &SET_PLAN_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct StepArg {
            goal: String,
            #[serde(default)]
            verification: String,
            #[serde(default)]
            model: String,
            #[serde(default)]
            context: String,
        }
        #[derive(Deserialize)]
        struct Args {
            steps: Vec<StepArg>,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.steps.is_empty() {
            anyhow::bail!("steps must contain at least one item");
        }
        for step in &args.steps {
            if step.goal.trim().is_empty() {
                anyhow::bail!("each step needs a non-empty `goal`");
            }
            if step.model.trim().is_empty() {
                anyhow::bail!(
                    "each step must name the `model` that will run it: {AGENT_MODEL:?} \
                     (\"self\") when you will run it yourself, or one of the delegate names \
                     listed by the `delegate` tool"
                );
            }
        }
        let drafts: Vec<comrade_tool::PlanStepDraft> = args
            .steps
            .into_iter()
            .map(|s| comrade_tool::PlanStepDraft {
                goal: s.goal,
                verification: s.verification,
                model: s.model.trim().to_string(),
                context: s.context,
            })
            .collect();
        ctx.session.set_plan(drafts.clone());
        Ok(format!(
            "Plan set with {} step(s). Step ids are 1-based.",
            drafts.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// update_plan
// ---------------------------------------------------------------------------

struct UpdatePlan;

static UPDATE_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "update_plan".into(),
    description: "Update the status of one plan step (mark in_progress/done/blocked). Identify a step by its 1-based `index` (preferred) or by `text` that appears in its goal. A step assigned a delegate `model` can only be marked done after the `delegate` tool has run it.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal." },
            "status": { "type": "string", "enum": ["pending", "in_progress", "done", "blocked"] },
            "note": { "type": "string", "description": "Optional note appended to the step." }
        },
        "required": ["status"],
        "oneOf": [
            { "required": ["index"] },
            { "required": ["text"] }
        ],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for UpdatePlan {
    fn spec(&self) -> &ToolSpec {
        &UPDATE_PLAN_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            index: Option<u64>,
            #[serde(default)]
            text: Option<String>,
            status: String,
            #[serde(default)]
            note: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let status = PlanStatus::parse(&args.status)
            .ok_or_else(|| anyhow::anyhow!("unknown status {:?}", args.status))?;

        let target = match (args.index, args.text.as_deref()) {
            (Some(i), _) if i >= 1 => PlanTarget::Id(i),
            (None, Some(t)) if !t.is_empty() => PlanTarget::Text(t.to_string()),
            _ => anyhow::bail!("update_plan requires either a 1-based `index` or non-empty `text`"),
        };

        // A step assigned to a delegate model can only be completed once the
        // `delegate` tool has actually run it: the tech lead must not do a
        // delegated step's work itself and then mark it done. Steps assigned
        // to the main model ("self") are the lead's own work and need no run.
        if status == PlanStatus::Done {
            let steps = ctx.session.plan();
            let matched = steps.iter().find(|s| match &target {
                PlanTarget::Id(id) => s.id == *id,
                PlanTarget::Text(text) => s.goal.contains(text.as_str()),
            });
            if let Some(step) = matched {
                if is_delegate_model(&step.model) && !ctx.session.step_was_delegated(step.id) {
                    anyhow::bail!(
                        "plan step {} is assigned to delegate {:?}, but the `delegate` tool \
                         has never run it — the tech lead cannot complete a delegated step \
                         itself. Run the step with the delegate tool (pass `step` = {}), verify \
                         the result, then mark it done. To do the step yourself instead, replace \
                         the plan with `set_plan` naming your own model ({AGENT_MODEL:?}).",
                        step.id,
                        step.model.trim(),
                        step.id
                    );
                }
            }
        }

        if ctx.session.update_plan(target, status, args.note) {
            let open = ctx
                .session
                .plan()
                .into_iter()
                .filter(|s| matches!(s.status, PlanStatus::Pending | PlanStatus::InProgress))
                .count();
            Ok(format!("Step marked {status}. {open} step(s) still open."))
        } else {
            anyhow::bail!("no step matched the given index/text");
        }
    }
}

// ---------------------------------------------------------------------------
// set_step_model
// ---------------------------------------------------------------------------

struct SetStepModel;

static SET_STEP_MODEL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "set_step_model".into(),
    description: "Change which model runs an existing plan step, e.g. to hand a pending \\\"self\\\" step to a delegate or to take a delegate-assigned step back onto yourself. Identify the step by its 1-based `index` (preferred) or by `text` in its goal, and give the `model` that will run it: \\\"self\\\" for you, or a configured delegate name. Refused while the step is in_progress or done: only pending or blocked steps can be reassigned. Reassigning clears the step's delegation record, so the newly assigned model must actually run the step before it can be marked done.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal." },
            "model": { "type": "string", "description": "The model that will now run this step: \"self\" for the main agent, or a configured delegate name (see the delegate tool's model listing)." }
        },
        "required": ["model"],
        "oneOf": [
            { "required": ["index"] },
            { "required": ["text"] }
        ],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for SetStepModel {
    fn spec(&self) -> &ToolSpec {
        &SET_STEP_MODEL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            index: Option<u64>,
            #[serde(default)]
            text: Option<String>,
            model: String,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.model.trim().is_empty() {
            anyhow::bail!(
                "set_step_model needs the `model` that will now run the step: {AGENT_MODEL:?} \
                 (\"self\") for yourself, or one of the delegate names listed by the `delegate` tool"
            );
        }
        let target = match (args.index, args.text.as_deref()) {
            (Some(i), _) if i >= 1 => PlanTarget::Id(i),
            (None, Some(t)) if !t.is_empty() => PlanTarget::Text(t.to_string()),
            _ => anyhow::bail!(
                "set_step_model requires either a 1-based `index` or non-empty `text`"
            ),
        };
        let model = args.model.trim().to_string();

        // Friendly pre-checks against a snapshot: only pending or blocked steps
        // may change model, and a no-op is reported as such.
        let steps = ctx.session.plan();
        let matched = steps.iter().find(|s| match &target {
            PlanTarget::Id(id) => s.id == *id,
            PlanTarget::Text(text) => s.goal.contains(text.as_str()),
        });
        if let Some(step) = matched {
            if matches!(step.status, PlanStatus::InProgress | PlanStatus::Done) {
                anyhow::bail!(
                    "plan step {} is {} — a step's model can only be changed while it is \
                     pending or blocked, not once a model is working it or it is done",
                    step.id,
                    step.status
                );
            }
            if step.model == model {
                let open = steps
                    .iter()
                    .filter(|s| matches!(s.status, PlanStatus::Pending | PlanStatus::InProgress))
                    .count();
                return Ok(format!(
                    "Step {} is already assigned to {model:?}; nothing changed. {open} step(s) \
                     still open.",
                    step.id
                ));
            }
        }

        match ctx.session.reassign_step_model(&target, &model) {
            Ok(true) => {
                let open = ctx
                    .session
                    .plan()
                    .into_iter()
                    .filter(|s| matches!(s.status, PlanStatus::Pending | PlanStatus::InProgress))
                    .count();
                Ok(format!(
                    "Step now assigned to model {model:?}. Reassignment cleared its delegation \
                     record, so that model must run the step before it can be marked done. {open} \
                     step(s) still open."
                ))
            }
            Ok(false) => anyhow::bail!("no step matched the given index/text"),
            Err(why) => anyhow::bail!("{why}"),
        }
    }
}

// ---------------------------------------------------------------------------
// finish_plan
// ---------------------------------------------------------------------------

struct FinishPlan;

static FINISH_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "finish_plan".into(),
    description: "Mark the whole plan as finished, optionally with a closing summary. Use when the task is complete. Refuses while any step assigned a delegate model has never been run by the `delegate` tool.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string", "description": "Optional closing summary." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for FinishPlan {
    fn spec(&self) -> &ToolSpec {
        &FINISH_PLAN_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            summary: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;

        // Refuse to auto-close any step that is assigned a delegate model but
        // has never been run by the `delegate` tool. Steps assigned to the main
        // model ("self") are the lead's own work and may be auto-finished.
        let stuck: Vec<String> = ctx
            .session
            .plan()
            .iter()
            .filter(|s| {
                is_delegate_model(&s.model)
                    && matches!(s.status, PlanStatus::Pending | PlanStatus::InProgress)
                    && !ctx.session.step_was_delegated(s.id)
            })
            .map(|s| format!("step {} (delegate {:?})", s.id, s.model.trim()))
            .collect();
        if !stuck.is_empty() {
            anyhow::bail!(
                "cannot finish the plan: {} still assigned to a delegate but never run by the \
                 `delegate` tool: {}. Delegate each step (delegate tool with `step` = <id>), \
                 verify the result, or replace the plan with `set_plan` naming your own model \
                 ({AGENT_MODEL:?}) and do those steps yourself.",
                stuck.len(),
                stuck.join(", ")
            );
        }

        ctx.session.finish_plan(args.summary);
        Ok("Plan finished.".to_string())
    }
}

// ---------------------------------------------------------------------------
// set_status_bar
// ---------------------------------------------------------------------------

struct SetStatusBar;

static SET_STATUS_BAR_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "set_status_bar".into(),
    description: "Set the freeform text shown in the UI status bar (e.g. the active git branch or current focus).".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "text": { "type": "string", "description": "Text to display." }
        },
        "required": ["text"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for SetStatusBar {
    fn spec(&self) -> &ToolSpec {
        &SET_STATUS_BAR_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            text: String,
        }
        let args: Args = serde_json::from_value(args)?;
        ctx.session.set_status(&args.text);
        Ok(format!("Status bar set to {:?}.", args.text))
    }
}

// ---------------------------------------------------------------------------
// ask_question
// ---------------------------------------------------------------------------

struct AskQuestion;

static ASK_QUESTION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ask_question".into(),
    description: "Ask the human a question and wait for their answer. Use to resolve ambiguity, request confirmation, or let the human pick between options. Prefer this over guessing when a choice materially affects the outcome. If you recommend one specific answer or approach, be explicit about it.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "question": { "type": "string", "description": "The question." },
            "options": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional predefined answers; omit for free-form input."
            }
        },
        "required": ["question"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for AskQuestion {
    fn spec(&self) -> &ToolSpec {
        &ASK_QUESTION_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            question: String,
            #[serde(default)]
            options: Vec<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let prompt = UserPrompt::Question {
            prompt: args.question,
            options: args.options,
        };
        let reply = ctx.user.ask(prompt).await?;
        Ok(match reply {
            UserReply::Answer(answer) => answer,
            UserReply::Denied => "(user dismissed the question)".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use comrade_tool::{
        AGENT_MODEL, PlanStatus, PlanStep, PlanStepDraft, PlanTarget, SessionControl, Tool,
        ToolContext, UndoLog, UserIo, UserPrompt, UserReply,
    };
    use serde_json::json;

    use super::{FinishPlan, SetPlan, SetStepModel, UpdatePlan};

    /// A real-enough session: stores the plan and which steps the `delegate`
    /// tool has run, exactly like `AgentSession` does.
    struct StubSession {
        plan: Mutex<Vec<PlanStep>>,
        delegated: Mutex<HashSet<u64>>,
    }

    impl StubSession {
        fn with_plan(steps: Vec<PlanStepDraft>) -> Self {
            let plan = steps
                .into_iter()
                .enumerate()
                .map(|(i, d)| PlanStep {
                    id: (i + 1) as u64,
                    goal: d.goal,
                    verification: d.verification,
                    model: d.model,
                    context: d.context,
                    status: PlanStatus::Pending,
                    note: None,
                    started_at_ms: None,
                    took_ms: None,
                })
                .collect();
            StubSession {
                plan: Mutex::new(plan),
                delegated: Mutex::new(HashSet::new()),
            }
        }
    }

    impl SessionControl for StubSession {
        fn set_title(&self, _t: &str) {}
        fn title(&self) -> String {
            "test".into()
        }
        fn set_plan(&self, _s: Vec<PlanStepDraft>) {}
        fn plan(&self) -> Vec<PlanStep> {
            self.plan.lock().unwrap().clone()
        }
        fn update_plan(
            &self,
            target: PlanTarget,
            status: PlanStatus,
            note: Option<String>,
        ) -> bool {
            let mut plan = self.plan.lock().unwrap();
            let Some(step) = plan.iter_mut().find(|s| match &target {
                PlanTarget::Id(id) => s.id == *id,
                PlanTarget::Text(text) => s.goal.contains(text.as_str()),
            }) else {
                return false;
            };
            step.update(status, note);
            true
        }
        fn finish_plan(&self, _summary: Option<String>) {
            let mut plan = self.plan.lock().unwrap();
            for step in plan.iter_mut() {
                if !matches!(step.status, PlanStatus::Done | PlanStatus::Blocked) {
                    step.update(PlanStatus::Done, None);
                }
            }
        }
        fn mark_step_delegated(&self, id: u64) {
            self.delegated.lock().unwrap().insert(id);
        }
        fn step_was_delegated(&self, id: u64) -> bool {
            self.delegated.lock().unwrap().contains(&id)
        }
        fn reassign_step_model(
            &self,
            target: &PlanTarget,
            model: &str,
        ) -> std::result::Result<bool, String> {
            let step_id = {
                let mut plan = self.plan.lock().unwrap();
                let Some(step) = plan.iter_mut().find(|s| match &target {
                    PlanTarget::Id(id) => s.id == *id,
                    PlanTarget::Text(text) => s.goal.contains(text.as_str()),
                }) else {
                    return Ok(false);
                };
                if matches!(step.status, PlanStatus::InProgress | PlanStatus::Done) {
                    return Err(format!(
                        "plan step {} is {} — a step's model can only be changed while it is \
                         pending or blocked",
                        step.id, step.status
                    ));
                }
                step.model = model.trim().to_string();
                step.id
            };
            self.delegated.lock().unwrap().remove(&step_id);
            Ok(true)
        }
        fn set_status(&self, _s: &str) {}
        fn status(&self) -> String {
            String::new()
        }
    }

    struct NoopIo;
    #[async_trait]
    impl UserIo for NoopIo {
        async fn ask(&self, _p: UserPrompt) -> Result<UserReply> {
            Ok(UserReply::Answer("yes".into()))
        }
    }

    struct NoopUndo;
    #[async_trait]
    impl UndoLog for NoopUndo {
        async fn capture(&self, _p: &str, _b: String) -> Result<()> {
            Ok(())
        }
        async fn undo_last(&self) -> Result<usize> {
            Ok(0)
        }
        async fn is_empty(&self) -> bool {
            true
        }
        async fn len(&self) -> usize {
            0
        }
    }

    fn ctx(session: StubSession) -> ToolContext {
        ToolContext {
            project_root: PathBuf::from("/tmp/x"),
            cwd: PathBuf::from("/tmp/x"),
            session: Arc::new(session),
            user: Arc::new(NoopIo),
            undo: Arc::new(NoopUndo),
            auto_approve: true,
            approval: Arc::new(Mutex::new(None)),
            events: Arc::new(comrade_tool::NoopEvents),
        }
    }

    fn delegated_step() -> PlanStepDraft {
        PlanStepDraft {
            goal: "write the helper fn".into(),
            verification: "cargo test passes".into(),
            model: "cheap".into(),
            context: String::new(),
        }
    }

    fn plain_step() -> PlanStepDraft {
        PlanStepDraft {
            goal: "plain step".into(),
            verification: String::new(),
            model: AGENT_MODEL.into(),
            context: String::new(),
        }
    }

    #[tokio::test]
    async fn cannot_mark_delegated_step_done_before_delegate_ran() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        let err = UpdatePlan
            .invoke(&c, json!({"index": 1, "status": "done"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("delegate"), "{err}");
        assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
    }

    #[tokio::test]
    async fn can_mark_delegated_step_done_after_delegate_ran() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        c.session.mark_step_delegated(1);
        let out = UpdatePlan
            .invoke(&c, json!({"index": 1, "status": "done"}))
            .await
            .unwrap();
        assert!(out.contains("Step marked done"), "{out}");
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn can_still_mark_a_plain_step_done() {
        // A step the main model runs itself ("self") is the lead's own work:
        // it can be marked done without any `delegate` run.
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        UpdatePlan
            .invoke(&c, json!({"index": 1, "status": "done"}))
            .await
            .unwrap();
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn finish_plan_allows_open_self_steps() {
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        FinishPlan.invoke(&c, json!({})).await.unwrap();
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn set_plan_requires_a_model_per_step() {
        let c = ctx(StubSession::with_plan(vec![]));
        let err = SetPlan
            .invoke(
                &c,
                json!({ "steps": [{ "goal": "do a thing", "verification": "x", "model": "  " }] }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
        assert!(err.to_string().contains("self"), "{err}");
    }

    #[tokio::test]
    async fn set_plan_accepts_self_and_delegate_models() {
        let c = ctx(StubSession::with_plan(vec![]));
        let out = SetPlan
            .invoke(
                &c,
                json!({ "steps": [
                    { "goal": "I do this", "model": "self" },
                    { "goal": "delegate does this", "model": "cheap" }
                ] }),
            )
            .await
            .unwrap();
        assert!(out.contains("2 step(s)"), "{out}");
    }

    #[tokio::test]
    async fn finish_plan_refuses_open_never_delegated_steps() {
        let c = ctx(StubSession::with_plan(vec![delegated_step(), plain_step()]));
        let err = FinishPlan.invoke(&c, json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("step 1"), "{msg}");
        assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
    }

    #[tokio::test]
    async fn finish_plan_allows_delegated_steps_that_ran() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        c.session.mark_step_delegated(1);
        FinishPlan.invoke(&c, json!({})).await.unwrap();
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn set_step_model_reassigns_a_pending_self_step_to_a_delegate() {
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        let out = SetStepModel
            .invoke(&c, json!({ "index": 1, "model": "cheap" }))
            .await
            .unwrap();
        assert!(out.contains("\"cheap\""), "{out}");
        assert_eq!(c.session.plan()[0].model, "cheap");
    }

    #[tokio::test]
    async fn set_step_model_takes_a_delegate_step_back_onto_self() {
        // Reassigning clears the delegation record: the "cheap" run no longer
        // counts, so the step cannot be marked done without a new delegate run.
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        c.session.mark_step_delegated(1);
        assert!(c.session.step_was_delegated(1));
        let out = SetStepModel
            .invoke(&c, json!({ "index": 1, "model": "self" }))
            .await
            .unwrap();
        assert!(out.contains("cleared"), "{out}");
        assert_eq!(c.session.plan()[0].model, AGENT_MODEL);
        assert!(!c.session.step_was_delegated(1));
    }

    #[tokio::test]
    async fn set_step_model_reassigns_by_goal_text_and_allows_blocked_steps() {
        let c = ctx(StubSession::with_plan(vec![
            plain_step(),
            delegated_step(),
            plain_step(),
        ]));
        // block step 2 (goal "write the helper fn")
        c.session
            .update_plan(PlanTarget::Id(2), PlanStatus::Blocked, None);
        let out = SetStepModel
            .invoke(&c, json!({ "text": "helper fn", "model": "groq" }))
            .await
            .unwrap();
        assert!(out.contains("\"groq\""), "{out}");
        assert_eq!(c.session.plan()[1].model, "groq");
    }

    #[tokio::test]
    async fn set_step_model_refuses_in_progress_and_done_steps() {
        let c = ctx(StubSession::with_plan(vec![plain_step(), plain_step()]));
        c.session
            .update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
        c.session
            .update_plan(PlanTarget::Id(2), PlanStatus::Done, None);
        let err = SetStepModel
            .invoke(&c, json!({ "index": 1, "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("in_progress"), "{err}");
        assert!(err.to_string().contains("pending or blocked"), "{err}");
        let err = SetStepModel
            .invoke(&c, json!({ "index": 2, "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("done"), "{err}");
        assert_eq!(c.session.plan()[0].model, AGENT_MODEL);
        assert_eq!(c.session.plan()[1].model, AGENT_MODEL);
    }

    #[tokio::test]
    async fn set_step_model_rejects_blank_models_and_unknown_targets() {
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        let err = SetStepModel
            .invoke(&c, json!({ "index": 1, "model": "   " }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
        assert!(err.to_string().contains("self"), "{err}");
        let err = SetStepModel
            .invoke(&c, json!({ "index": 99, "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no step matched"), "{err}");
        let err = SetStepModel
            .invoke(&c, json!({ "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`index`"), "{err}");
    }

    #[tokio::test]
    async fn set_step_model_is_a_no_op_when_the_model_is_unchanged() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        let out = SetStepModel
            .invoke(&c, json!({ "index": 1, "model": "cheap" }))
            .await
            .unwrap();
        assert!(out.contains("already assigned"), "{out}");
        assert_eq!(c.session.plan()[0].model, "cheap");
    }

    #[tokio::test]
    async fn set_step_model_accepts_an_unknown_model_value() {
        // Delegate names are validated by the `delegate` tool, not here (mirrors
        // set_plan): an arbitrary non-blank model is stored verbatim.
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        SetStepModel
            .invoke(&c, json!({ "index": 1, "model": "claude" }))
            .await
            .unwrap();
        assert_eq!(c.session.plan()[0].model, "claude");
    }
}
