//! Project object model (POM) and task-runner tools for Cargo projects.
//!
//! - `project_model` describes the workspace/project + subprojects (Cargo
//!   workspace members), their dependencies, and available tasks — the Comrade
//!   analogue of a Maven POM (project + modules).
//! - `run_task` executes a named task (a cargo verb, a cargo alias, or a
//!   `!shell` alias) optionally scoped to one subproject.

mod pom;
mod tasks;

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub use pom::{ProjectModel, load as load_model, render as render_model};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![Box::new(ProjectModelTool), Box::new(RunTask)]
}

// ---------------------------------------------------------------------------
// project_model
// ---------------------------------------------------------------------------

struct ProjectModelTool;

static PROJECT_MODEL_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "project_model".into(),
    description: "Report this Cargo project's dependencies, subprojects, and tasks (the project object model, POM). Use this INSTEAD of reading Cargo.toml files whenever the user asks about dependencies, crates/modules, the workspace layout, or what tasks can be run. Read-only.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ProjectModelTool {
    fn spec(&self) -> &ToolSpec {
        &PROJECT_MODEL_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        let model = pom::load(&ctx.project_root)?;
        Ok(pom::render(&model))
    }
}

// ---------------------------------------------------------------------------
// run_task
// ---------------------------------------------------------------------------

struct RunTask;

static RUN_TASK_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "run_task".into(),
    description: "Run a named project task and return its output. Tasks come from project_model: cargo verbs (build, run, check, test, clippy, fmt, doc, bench, release) and aliases defined in .cargo/config.toml (!-prefixed aliases run as shell). Optionally scope to a subproject with its directory (relative to the root). Interactive: you approve each run unless autonomy is auto.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "task": { "type": "string", "description": "Task name, e.g. \"test\" or a cargo alias." },
            "subproject": { "type": "string", "description": "Optional subproject directory relative to the root, e.g. \"crates/app\"." },
            "args": { "type": "string", "description": "Extra arguments appended to the command." },
            "timeout_secs": { "type": "integer", "minimum": 1, "default": 600, "description": "Kill the task after this many seconds." }
        },
        "required": ["task"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RunTask {
    fn spec(&self) -> &ToolSpec {
        &RUN_TASK_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            task: String,
            #[serde(default)]
            subproject: Option<String>,
            #[serde(default)]
            args: Option<String>,
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
        }
        fn default_timeout() -> u64 {
            600
        }
        let args: Args = serde_json::from_value(args)?;

        let extra: Vec<String> = args
            .args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();

        let resolved = tasks::resolve(&ctx.project_root, &args.task, &args.subproject, &extra)?;

        ctx.confirm(format!("run_task: {}", resolved.describe), None)
            .await?;

        let output = tasks::run(&resolved, args.timeout_secs).await?;
        Ok(output)
    }
}
