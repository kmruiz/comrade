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
    AGENT_MODEL, FormSpec, PlanStatus, PlanTarget, Tool, ToolContext, ToolSpec, UserPrompt,
    UserReply,
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

/// Whether a step is still on someone's plate: not yet finished or blocked.
/// `Ready` counts as open — the step has been confirmed but not yet picked up.
fn is_open(status: &PlanStatus) -> bool {
    matches!(
        status,
        PlanStatus::Pending | PlanStatus::Ready | PlanStatus::InProgress
    )
}

/// All session-control tools.
pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SelfRenameSession),
        Box::new(SelfSetPlan),
        Box::new(SelfUpdatePlan),
        Box::new(SelfSetStepModel),
        Box::new(SelfSetStepContext),
        Box::new(SelfFinishPlan),
        Box::new(SelfSetStatusBar),
        Box::new(AskForm),
    ]
}

// ---------------------------------------------------------------------------
// self_rename_session
// ---------------------------------------------------------------------------

struct SelfRenameSession;

static SELF_RENAME_SESSION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_rename_session".into(),
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
impl Tool for SelfRenameSession {
    fn spec(&self) -> &ToolSpec {
        &SELF_RENAME_SESSION_SPEC
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
// self_set_plan
// ---------------------------------------------------------------------------

struct SelfSetPlan;

static SELF_SET_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_set_plan".into(),
    description: "Lay out the plan before doing work. Each step: a goal, a verification (how to prove it worked) and the `model` that runs it ('self' or a delegate). Replaces any existing plan; advance steps with self_update_plan.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string", "description": "What this step aims to accomplish, summarised, for a human to read." },
                        "verification": { "type": "string", "description": "How to prove the step worked (e.g. a test command to run)." },
                        "model": { "type": "string", "description": "REQUIRED. Who runs this step: \"self\" (you) or a configured delegate name." },
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
impl Tool for SelfSetPlan {
    fn spec(&self) -> &ToolSpec {
        &SELF_SET_PLAN_SPEC
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
// self_update_plan
// ---------------------------------------------------------------------------

struct SelfUpdatePlan;

static SELF_UPDATE_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_update_plan".into(),
    description: "Update one plan step's status (pending/ready/in_progress/done/blocked). Identify the step by its 1-based `index` (preferred) or by `text` in its goal.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal." },
            "status": { "type": "string", "enum": ["pending", "ready", "in_progress", "done", "blocked"], "description": "New status. ready = delegate confirmed the context; done only after a green verification." },
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
impl Tool for SelfUpdatePlan {
    fn spec(&self) -> &ToolSpec {
        &SELF_UPDATE_PLAN_SPEC
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
            _ => anyhow::bail!(
                "self_update_plan requires either a 1-based `index` or non-empty `text`"
            ),
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
            if let Some(step) = matched
                && is_delegate_model(&step.model)
                && !ctx.session.step_was_delegated(step.id)
            {
                anyhow::bail!(
                    "plan step {} is assigned to delegate {:?}, but the `delegate` tool \
                     has never run it — the tech lead cannot complete a delegated step \
                     itself. Run the step with the delegate tool (pass `step` = {}), verify \
                     the result, then mark it done. To do the step yourself instead, replace \
                     the plan with `self_set_plan` naming your own model ({AGENT_MODEL:?}).",
                    step.id,
                    step.model.trim(),
                    step.id
                );
            }
        }

        if ctx.session.update_plan(target, status, args.note) {
            let open = ctx
                .session
                .plan()
                .into_iter()
                .filter(|s| is_open(&s.status))
                .count();
            Ok(format!("Step marked {status}. {open} step(s) still open."))
        } else {
            anyhow::bail!("no step matched the given index/text");
        }
    }
}

// ---------------------------------------------------------------------------
// self_set_step_model
// ---------------------------------------------------------------------------

struct SelfSetStepModel;

static SELF_SET_STEP_MODEL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_set_step_model".into(),
    description: "Change which model runs a plan step: 'self' for you, or a delegate name. Refused while the step is in_progress or done; only pending, ready or blocked steps can be reassigned.".into(),
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
impl Tool for SelfSetStepModel {
    fn spec(&self) -> &ToolSpec {
        &SELF_SET_STEP_MODEL_SPEC
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
                "self_set_step_model needs the `model` that will now run the step: {AGENT_MODEL:?} \
                 (\"self\") for yourself, or one of the delegate names listed by the `delegate` tool"
            );
        }
        let target = match (args.index, args.text.as_deref()) {
            (Some(i), _) if i >= 1 => PlanTarget::Id(i),
            (None, Some(t)) if !t.is_empty() => PlanTarget::Text(t.to_string()),
            _ => anyhow::bail!(
                "self_set_step_model requires either a 1-based `index` or non-empty `text`"
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
                     pending, ready or blocked, not once a model is working it or it is done",
                    step.id,
                    step.status
                );
            }
            if step.model == model {
                let open = steps.iter().filter(|s| is_open(&s.status)).count();
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
                    .filter(|s| is_open(&s.status))
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
// self_set_step_context
// ---------------------------------------------------------------------------

struct SelfSetStepContext;

static SELF_SET_STEP_CONTEXT_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_set_step_context".into(),
    description: "Replace one plan step's context - the instructions its executing delegate receives (never shown in the UI). Refused while in_progress or done; resets a `ready` step to pending.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal." },
            "context": { "type": "string", "description": "The new summarised context for the executing model (replaces the step's current context entirely)." }
        },
        "required": ["context"],
        "oneOf": [
            { "required": ["index"] },
            { "required": ["text"] }
        ],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for SelfSetStepContext {
    fn spec(&self) -> &ToolSpec {
        &SELF_SET_STEP_CONTEXT_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            index: Option<u64>,
            #[serde(default)]
            text: Option<String>,
            context: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = match (args.index, args.text.as_deref()) {
            (Some(i), _) if i >= 1 => PlanTarget::Id(i),
            (None, Some(t)) if !t.is_empty() => PlanTarget::Text(t.to_string()),
            _ => anyhow::bail!(
                "self_set_step_context requires either a 1-based `index` or non-empty `text`"
            ),
        };

        // Report the matched step's id on success, like the other step tools.
        let steps = ctx.session.plan();
        let matched = steps.iter().find(|s| match &target {
            PlanTarget::Id(id) => s.id == *id,
            PlanTarget::Text(text) => s.goal.contains(text.as_str()),
        });
        let matched_id = matched.map(|s| s.id);

        match ctx.session.set_step_context(&target, &args.context) {
            Ok(true) => {
                let id = matched_id.expect("set_step_context matched, so the id exists");
                Ok(format!(
                    "Context of step {id} replaced. Re-run ask_advise step = {id} so the \
                     delegate can confirm the step is `ready`."
                ))
            }
            Ok(false) => anyhow::bail!("no step matched the given index/text"),
            Err(why) => anyhow::bail!("{why}"),
        }
    }
}

// ---------------------------------------------------------------------------
// self_finish_plan
// ---------------------------------------------------------------------------

struct SelfFinishPlan;

static SELF_FINISH_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_finish_plan".into(),
    description: "Mark the whole plan as finished, optionally with a closing summary. Use when the task is complete. Refuses while any delegate-assigned step has not been run by the delegate tool.".into(),
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
impl Tool for SelfFinishPlan {
    fn spec(&self) -> &ToolSpec {
        &SELF_FINISH_PLAN_SPEC
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
                    && is_open(&s.status)
                    && !ctx.session.step_was_delegated(s.id)
            })
            .map(|s| format!("step {} (delegate {:?})", s.id, s.model.trim()))
            .collect();
        if !stuck.is_empty() {
            anyhow::bail!(
                "cannot finish the plan: {} still assigned to a delegate but never run by the \
                 `delegate` tool: {}. Delegate each step (delegate tool with `step` = <id>), \
                 verify the result, or replace the plan with `self_set_plan` naming your own model \
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
// self_set_status_bar
// ---------------------------------------------------------------------------

struct SelfSetStatusBar;

static SELF_SET_STATUS_BAR_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "self_set_status_bar".into(),
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
impl Tool for SelfSetStatusBar {
    fn spec(&self) -> &ToolSpec {
        &SELF_SET_STATUS_BAR_SPEC
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
// ask_form
// ---------------------------------------------------------------------------

struct AskForm;

static ASK_FORM_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
        name: "ask_form".into(),
        description: "Render an interactive form (text/number/date/select/checkbox components) in the chat and return the human's answers as `id = value` lines. Use for choices a plain text question would make awkward: numbers, dates, bounded pick-lists, booleans. Set a field's `recommended` value to suggest an answer (consult another agent with ask_advise if unsure); it prefills the field and is auto-submitted in auto mode.".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Form heading." },
                "description": { "type": "string", "description": "Optional explanatory text." },
                "fields": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Unique identifier for the answer." },
                            "label": { "type": "string", "description": "Human-readable label." },
                            "kind": { 
                                "type": "string", 
                                "enum": ["text", "number", "date", "select", "checkbox", "diff_choice"],
                                "description": "Component type: text, number, date, select, checkbox, or diff_choice."
                            },
                            "required": { "type": "boolean", "description": "Field must be filled to submit." },
                            "default": { "type": "string", "description": "Optional initial value." },
                            "recommended": { "type": "string", "description": "Suggested answer. Prefills the field (marked 'recommended') and is what auto mode submits when every required field is satisfied. Consult another agent with ask_advise/delegate to obtain a good value if you are unsure." },
                            "placeholder": { "type": "string", "description": "Placeholder for text fields." },
                            "min": { "type": "number", "description": "Minimum value for number fields." },
                            "max": { "type": "number", "description": "Maximum value for number fields." },
                            "step": { "type": "number", "description": "Step size for number spinners." },
                            "options": { 
                                "type": "array", 
                                "items": {
                                    "anyOf": [
                                        { "type": "string" },
                                        { "type": "object", "properties": {
                                            "label": { "type": "string", "description": "Human-readable label handed back as the answer when chosen." },
                                            "diff": { "type": "string", "description": "The code diff to show (unified diff, or +/- lines)." }
                                        }, "required": ["label", "diff"], "additionalProperties": false }
                                    ]
                                },
                                "description": "For select: the option strings. For diff_choice: at least two {label, diff} objects the human picks between (the chosen label is the answer)."
                            }
                        },
                        "required": ["id", "label", "kind"],
                        "additionalProperties": false
                    },
                    "minItems": 1,
                    "description": "Fields in display order; each must have a unique `id`."
                }
            },
            "required": ["fields"],
            "additionalProperties": false
        }),
    }
});

#[async_trait]
impl Tool for AskForm {
    fn spec(&self) -> &ToolSpec {
        &ASK_FORM_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        let spec: FormSpec = serde_json::from_value(args)?;
        let reply = ctx.user.ask(UserPrompt::Form(spec.clone())).await?;
        match reply {
            UserReply::Form(answers) => Ok(spec.answer_lines(&answers)),
            UserReply::Answer(text) => Ok(text),
            UserReply::Denied => Ok("(user dismissed the form)".to_string()),
        }
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

    use super::{
        AskForm, SelfFinishPlan, SelfSetPlan, SelfSetStepContext, SelfSetStepModel, SelfUpdatePlan,
    };

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
                         pending, ready or blocked",
                        step.id, step.status
                    ));
                }
                step.model = model.trim().to_string();
                step.id
            };
            self.delegated.lock().unwrap().remove(&step_id);
            Ok(true)
        }
        fn set_step_context(
            &self,
            target: &PlanTarget,
            context: &str,
        ) -> std::result::Result<bool, String> {
            let context = context.trim();
            let (step_id, was_ready) = {
                let mut plan = self.plan.lock().unwrap();
                let Some(step) = plan.iter_mut().find(|s| match &target {
                    PlanTarget::Id(id) => s.id == *id,
                    PlanTarget::Text(text) => s.goal.contains(text.as_str()),
                }) else {
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
            if was_ready {
                self.update_plan(
                    PlanTarget::Id(step_id),
                    PlanStatus::Pending,
                    Some("ready reset: context changed".into()),
                );
            }
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
            match _p {
                UserPrompt::Form(spec) => Ok(UserReply::Form(spec.initial_values())),
                UserPrompt::Confirm { .. } => Ok(UserReply::Answer("yes".into())),
            }
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
            steer: None,
            compact: None,
            stop: None,
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
        let err = SelfUpdatePlan
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
        let out = SelfUpdatePlan
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
        SelfUpdatePlan
            .invoke(&c, json!({"index": 1, "status": "done"}))
            .await
            .unwrap();
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn finish_plan_allows_open_self_steps() {
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        SelfFinishPlan.invoke(&c, json!({})).await.unwrap();
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn set_plan_requires_a_model_per_step() {
        let c = ctx(StubSession::with_plan(vec![]));
        let err = SelfSetPlan
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
        let out = SelfSetPlan
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
        let err = SelfFinishPlan.invoke(&c, json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("step 1"), "{msg}");
        assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
    }

    #[tokio::test]
    async fn finish_plan_allows_delegated_steps_that_ran() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        c.session.mark_step_delegated(1);
        SelfFinishPlan.invoke(&c, json!({})).await.unwrap();
        assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
    }

    #[tokio::test]
    async fn set_step_model_reassigns_a_pending_self_step_to_a_delegate() {
        let c = ctx(StubSession::with_plan(vec![plain_step()]));
        let out = SelfSetStepModel
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
        let out = SelfSetStepModel
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
        let out = SelfSetStepModel
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
        let err = SelfSetStepModel
            .invoke(&c, json!({ "index": 1, "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("in_progress"), "{err}");
        assert!(
            err.to_string().contains("pending, ready or blocked"),
            "{err}"
        );
        let err = SelfSetStepModel
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
        let err = SelfSetStepModel
            .invoke(&c, json!({ "index": 1, "model": "   " }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
        assert!(err.to_string().contains("self"), "{err}");
        let err = SelfSetStepModel
            .invoke(&c, json!({ "index": 99, "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no step matched"), "{err}");
        let err = SelfSetStepModel
            .invoke(&c, json!({ "model": "cheap" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`index`"), "{err}");
    }

    #[tokio::test]
    async fn set_step_model_is_a_no_op_when_the_model_is_unchanged() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        let out = SelfSetStepModel
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
        SelfSetStepModel
            .invoke(&c, json!({ "index": 1, "model": "claude" }))
            .await
            .unwrap();
        assert_eq!(c.session.plan()[0].model, "claude");
    }

    #[tokio::test]
    async fn set_step_context_replaces_the_context_of_a_pending_step() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        let out = SelfSetStepContext
            .invoke(
                &c,
                json!({ "index": 1, "context": "the helper lives in crates/x" }),
            )
            .await
            .unwrap();
        assert!(out.contains("Context of step 1 replaced"), "{out}");
        assert_eq!(c.session.plan()[0].context, "the helper lives in crates/x");
    }

    #[tokio::test]
    async fn set_step_context_targets_by_goal_text() {
        let c = ctx(StubSession::with_plan(vec![plain_step(), delegated_step()]));
        SelfSetStepContext
            .invoke(&c, json!({ "text": "helper fn", "context": "new ctx" }))
            .await
            .unwrap();
        assert_eq!(c.session.plan()[1].context, "new ctx");
        assert_eq!(c.session.plan()[0].context, String::new());
    }

    #[tokio::test]
    async fn set_step_context_refuses_in_progress_and_done_steps() {
        let c = ctx(StubSession::with_plan(vec![
            delegated_step(),
            delegated_step(),
        ]));
        c.session
            .update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
        c.session
            .update_plan(PlanTarget::Id(2), PlanStatus::Done, None);
        let err = SelfSetStepContext
            .invoke(&c, json!({ "index": 1, "context": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("in_progress"), "{err}");
        let err = SelfSetStepContext
            .invoke(&c, json!({ "index": 2, "context": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("done"), "{err}");
    }

    #[tokio::test]
    async fn set_step_context_drops_a_ready_step_back_to_pending() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        c.session.update_plan(
            PlanTarget::Id(1),
            PlanStatus::Ready,
            Some("ready: ok".into()),
        );
        assert_eq!(c.session.plan()[0].status, PlanStatus::Ready);
        SelfSetStepContext
            .invoke(&c, json!({ "index": 1, "context": "new enriched ctx" }))
            .await
            .unwrap();
        assert_eq!(c.session.plan()[0].context, "new enriched ctx");
        assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
        assert_eq!(
            c.session.plan()[0].note.as_deref(),
            Some("ready reset: context changed")
        );
    }

    #[tokio::test]
    async fn set_step_context_rejects_blank_context_and_unknown_targets() {
        let c = ctx(StubSession::with_plan(vec![delegated_step()]));
        let err = SelfSetStepContext
            .invoke(&c, json!({ "index": 1, "context": "   " }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-empty"), "{err}");
        let err = SelfSetStepContext
            .invoke(&c, json!({ "index": 99, "context": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no step matched"), "{err}");
        let err = SelfSetStepContext
            .invoke(&c, json!({ "context": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`index`"), "{err}");
    }

    #[tokio::test]
    async fn ask_form_returns_id_equals_value_lines() {
        use std::collections::BTreeMap;

        // A UserIo that "fills" the form with chosen values.
        struct FormStubIo;
        #[async_trait]
        impl UserIo for FormStubIo {
            async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
                match prompt {
                    UserPrompt::Form(_) => Ok(UserReply::Form(BTreeMap::from([
                        ("guests".into(), "3".into()),
                        ("room".into(), "double".into()),
                        ("breakfast".into(), "true".into()),
                    ]))),
                    other => panic!("unexpected prompt: {other:?}"),
                }
            }
        }

        let ctx = ToolContext {
            project_root: PathBuf::from("/tmp/x"),
            cwd: PathBuf::from("/tmp/x"),
            session: Arc::new(StubSession::with_plan(vec![])),
            user: Arc::new(FormStubIo),
            undo: Arc::new(NoopUndo),
            auto_approve: true,
            approval: Arc::new(Mutex::new(None)),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };

        let json = json!({
            "title": "Room Booking",
            "description": "Choose your room and meal plan",
            "fields": [
                { "id": "guests", "label": "Number of guests", "kind": "number", "min": 1, "max": 4 },
                { "id": "room", "label": "Room type", "kind": "select", "options": ["single", "double"] },
                { "id": "breakfast", "label": "Breakfast", "kind": "checkbox", "default": "false" }
            ]
        });

        let out = AskForm.invoke(&ctx, json).await.unwrap();
        assert_eq!(out, "guests = 3\nroom = double\nbreakfast = true");
    }

    #[tokio::test]
    async fn ask_form_accepts_a_diff_choice_field() {
        use comrade_tool::FieldKind;
        use std::collections::BTreeMap;

        struct PickIo;
        #[async_trait]
        impl UserIo for PickIo {
            async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
                match prompt {
                    UserPrompt::Form(spec) => {
                        // The diff_choice parsed with both candidate diffs.
                        match spec.fields[0].kind {
                            FieldKind::DiffChoice { ref options } => {
                                assert_eq!(options.len(), 2);
                                assert_eq!(options[1].label, "B");
                            }
                            ref other => panic!("expected diff_choice, got {other:?}"),
                        }
                        Ok(UserReply::Form(BTreeMap::from([(
                            "pick".into(),
                            "B".into(),
                        )])))
                    }
                    other => panic!("unexpected prompt: {other:?}"),
                }
            }
        }

        let ctx = ToolContext {
            project_root: PathBuf::from("/tmp/x"),
            cwd: PathBuf::from("/tmp/x"),
            session: Arc::new(StubSession::with_plan(vec![])),
            user: Arc::new(PickIo),
            undo: Arc::new(NoopUndo),
            auto_approve: true,
            approval: Arc::new(Mutex::new(None)),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };

        let out = AskForm
            .invoke(
                &ctx,
                json!({
                    "title": "Pick a patch",
                    "fields": [
                        { "id": "pick", "label": "Which patch?", "kind": "diff_choice",
                          "options": [
                              { "label": "A", "diff": "-let x = 1;\n+let x = 2;" },
                              { "label": "B", "diff": "-let x = 1;\n+let x = 3;" }
                          ] }
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(out, "pick = B");
    }
}
