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
    description: "Update one plan step's status (pending/ready/in_progress/done/blocked). Identify the step by its 1-based `index`. Pass `text` (a substring of the step's goal) only when you do not know the index.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal; used only when `index` is unknown." },
            "status": { "type": "string", "enum": ["pending", "ready", "in_progress", "done", "blocked"], "description": "New status. ready = delegate confirmed the context; done only after a green verification." },
            "note": { "type": "string", "description": "Optional note appended to the step." }
        },
        "required": ["status", "index"],
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
            let listing = ctx
                .session
                .plan()
                .iter()
                .map(|s| format!("  {}: {}", s.id, s.goal))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "no step matched the given index/text. Existing steps (retry with the numeric \
                 `index`):\n{listing}"
            );
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
    description: "Change which model runs a plan step: 'self' for you, or a delegate name. Identify the step by its 1-based `index` (or `text` when the index is unknown). Refused while the step is in_progress or done; only pending, ready or blocked steps can be reassigned.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal; used only when `index` is unknown." },
            "model": { "type": "string", "description": "The model that will now run this step: \"self\" for the main agent, or a configured delegate name (see the delegate tool's model listing)." }
        },
        "required": ["model", "index"],
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
    description: "Replace one plan step's context - the instructions its executing delegate receives (never shown in the UI). Identify the step by its 1-based `index` (or `text` when the index is unknown). Refused while in_progress or done; resets a `ready` step to pending.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step goal; used only when `index` is unknown." },
            "context": { "type": "string", "description": "The new summarised context for the executing model (replaces the step's current context entirely)." }
        },
        "required": ["context", "index"],
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

// ---------------------------------------------------------------------------
// ask_upwards
// ---------------------------------------------------------------------------

/// Ask the model that owns the session (the tech lead) a question. Registered
/// ONLY for delegated sub-agents: the main agent has no parent to ask. The
/// delegate sub-loop additionally caps how often it may be used per run.
pub struct AskUpwards;

static ASK_UPWARDS_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "ask_upwards".into(),
    description: "Ask the tech lead - the model that gave you this task - a question when you are stuck, e.g. the same error twice, or a decision you cannot make. Send ONE specific question: what you tried, the exact error, and what you need decided. Use it at most twice; after that decide yourself and continue. Read-only: it changes nothing in the repository.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "question": { "type": "string", "description": "The specific question, plus the evidence needed to answer it (what you tried and the exact error)." }
        },
        "required": ["question"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for AskUpwards {
    fn spec(&self) -> &ToolSpec {
        &ASK_UPWARDS_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            question: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let question = args.question.trim();
        if question.is_empty() {
            anyhow::bail!("`question` must not be empty");
        }
        let Some(upward) = ctx.session.upward() else {
            return Ok(
                "No tech lead is available to answer right now. Decide for yourself, make the \
                       smallest reasonable change, and continue."
                    .to_string(),
            );
        };
        // Give the parent the little bit of shared state it needs to answer
        // well: which session this is and what the plan says.
        let mut msg = String::new();
        let title = ctx.session.title();
        if !title.is_empty() {
            msg.push_str(&format!("Session: {title}\n"));
        }
        let plan = ctx.session.plan();
        if !plan.is_empty() {
            msg.push_str("Plan:\n");
            for step in &plan {
                msg.push_str(&format!("  {}: {} [{}]\n", step.id, step.goal, step.status));
            }
        }
        msg.push_str(&format!(
            "\nA sub-agent you delegated a step to is stuck and asks:\n{question}"
        ));
        let answer = upward.ask(&msg).await?;
        Ok(format!("Answer from your tech lead:\n{}", answer.trim()))
    }
}

/// The tools only delegated sub-agents get: escalation to their parent. The
/// main agent has no parent, so this is NOT part of [`all`].
pub fn upward_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(AskUpwards)]
}

#[cfg(test)]
mod tests;
