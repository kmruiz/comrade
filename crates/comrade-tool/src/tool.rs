use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::plan::SessionControl;

/// Static description of a tool, used both for the native function-calling
/// schema and the ReAct text protocol.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Unique snake_case name, e.g. `find_references`.
    pub name: String,
    /// Human/LLM readable description of what the tool does and when to use it.
    pub description: String,
    /// JSON Schema describing the tool arguments (object).
    pub json_schema: Value,
}

/// A callable tool available to the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    /// Execute the tool. `args` has already been validated against
    /// `spec().json_schema`. The returned string is the raw result injected
    /// back into the model context.
    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String>;
}

/// Registry of tools handed to the agent loop. Ordering is preserved and
/// controls the order tools are advertised to the model.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn extend(&mut self, tools: impl IntoIterator<Item = Box<dyn Tool>>) {
        self.tools.extend(tools);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|b| b.as_ref())
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|b| b.as_ref())
    }
}

/// Everything a tool invocation needs access to for the current session.
#[derive(Clone)]
pub struct ToolContext {
    /// Absolute path to the repository/project root the agent operates in.
    pub project_root: PathBuf,
    /// Current working directory (usually == project_root).
    pub cwd: PathBuf,
    /// Session state the UI tools mutate (title, plan, status bar).
    pub session: std::sync::Arc<dyn SessionControl>,
    /// Channel to the human (TUI dialog, or headless stdin/policy).
    pub user: std::sync::Arc<dyn UserIo>,
    /// First-write backup log used to roll back mutating tools.
    pub undo: std::sync::Arc<dyn UndoLog>,
    /// When true, mutating tools run without asking the user for confirmation.
    pub auto_approve: bool,
}

impl ToolContext {
    /// Ask the human to approve a mutating operation.
    ///
    /// When `auto_approve` is set this returns `Ok` immediately. Otherwise it
    /// routes a [`UserPrompt::Confirm`] and fails with [`UserReply::Denied`]
    /// unless the user consents.
    pub async fn confirm(&self, summary: impl Into<String>, diff: Option<String>) -> Result<()> {
        if self.auto_approve {
            return Ok(());
        }
        match self
            .user
            .ask(UserPrompt::Confirm {
                title: summary.into(),
                diff,
            })
            .await?
        {
            UserReply::Answer(text) if is_affirmative(&text) => Ok(()),
            UserReply::Answer(text) => anyhow::bail!("user denied request ({text:?})"),
            UserReply::Denied => anyhow::bail!("user denied request"),
        }
    }
}

fn is_affirmative(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "yes" | "y" | "ok" | "sure" | "1" | "true"
    )
}

/// A prompt directed at the human user through the UI.
#[derive(Debug, Clone)]
pub enum UserPrompt {
    /// A free-form question, optionally with predefined answer options.
    Question {
        prompt: String,
        options: Vec<String>,
    },
    /// A yes/no confirmation with an optional diff/preview body.
    Confirm { title: String, diff: Option<String> },
}

/// Outcome of routing a prompt to the human.
#[derive(Debug, Clone)]
pub enum UserReply {
    /// The human typed/picked an answer (options resolve to their text).
    Answer(String),
    /// The human dismissed the prompt (escaped / cancelled).
    Denied,
}

/// Abstraction over "talk to the human". Implemented by the TUI (modal
/// dialogs) and by a headless policy/stdin fallback.
#[async_trait]
pub trait UserIo: Send + Sync {
    async fn ask(&self, prompt: UserPrompt) -> Result<UserReply>;
}

/// First-write backup of files touched by mutating tools, so changes can be
/// reviewed and rolled back even when the repo is not under git.
#[async_trait]
pub trait UndoLog: Send + Sync {
    /// Remember the original content of `path` before a mutation. Only the
    /// first captured version per path is kept (that is the rollback target).
    /// The path is project-root relative.
    async fn capture(&self, path: &str, before: String) -> Result<()>;

    /// Undo the most recent captured mutation. Returns the number of entries
    /// remaining (0 if the log is now empty).
    async fn undo_last(&self) -> Result<usize>;

    /// True when nothing has been captured yet.
    async fn is_empty(&self) -> bool;

    /// Number of captured entries.
    async fn len(&self) -> usize;
}
