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
use comrade_tool::{PlanStatus, PlanTarget, Tool, ToolContext, ToolSpec, UserPrompt, UserReply};
use serde::Deserialize;
use serde_json::{Value, json};

/// All session-control tools.
pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(RenameSession),
        Box::new(SetPlan),
        Box::new(UpdatePlan),
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
    description: "Lay out the plan as an ordered checklist of steps before doing work. Call once up front; advance steps with update_plan. Replaces any existing plan.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "steps": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "Ordered step descriptions."
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
        struct Args {
            steps: Vec<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.steps.is_empty() {
            anyhow::bail!("steps must contain at least one item");
        }
        ctx.session.set_plan(args.steps.clone());
        Ok(format!(
            "Plan set with {} step(s). Step ids are 1-based.",
            args.steps.len()
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
    description: "Update the status of one plan step (mark in_progress/done/blocked). Identify a step by its 1-based `index` (preferred) or by `text` that appears in its description.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "index": { "type": "integer", "minimum": 1, "description": "1-based step id." },
            "text": { "type": "string", "description": "Text contained in the step description." },
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
// finish_plan
// ---------------------------------------------------------------------------

struct FinishPlan;

static FINISH_PLAN_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "finish_plan".into(),
    description: "Mark the whole plan as finished, optionally with a closing summary. Use when the task is complete.".into(),
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
    description: "Ask the human a question and wait for their answer. Use to resolve ambiguity, request confirmation, or let the human pick between options. Prefer this over guessing when a choice materially affects the outcome.".into(),
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
