use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

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

/// UI -> running-task steering channel. While a run is in flight the human can
/// type a short message ("steer") that whichever model currently owns the run —
/// the main agent or, nested inside it, a delegated sub-agent — sees as a user
/// message at its next rest point (between model requests). One [`Steer`] is
/// created per run and cloned into every context that runs a model loop, so all
/// loops drain the same receiver and the one executing at the moment gets the
/// message. A run with no steering UI (headless, tests) has
/// [`ToolContext::steer`] set to `None`.
#[derive(Clone)]
pub struct Steer {
    rx: std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>>,
}

impl Steer {
    /// Create a steering pipe. Keep the returned sender on the UI side of the
    /// run and hand the [`Steer`] (receiver side) to the run context. Sending
    /// fails once the run has ended and dropped its receiver, which is how the
    /// UI learns a steer was too late.
    pub fn channel() -> (Steer, tokio::sync::mpsc::UnboundedSender<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Steer {
                rx: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
            },
            tx,
        )
    }

    /// Take every steering message queued so far, without waiting.
    pub async fn drain(&self) -> Vec<String> {
        let mut rx = self.rx.lock().await;
        let mut out = Vec::new();
        while let Ok(text) = rx.try_recv() {
            out.push(text);
        }
        out
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
    /// One-shot notes the model supplied for the next confirmation (why the
    /// action should run, and what could go wrong). Written by the agent loop
    /// before invoking a tool, read and cleared by [`ToolContext::confirm`].
    pub approval: std::sync::Arc<std::sync::Mutex<Option<ApprovalNotes>>>,
    /// Live activity a long-running tool wants to surface in the UI while it
    /// runs. The agent core wires this to the session's event channel so the
    /// `delegate` tool can stream what its sub-agent is doing, as it happens.
    pub events: std::sync::Arc<dyn ActivityEvents>,
    /// Cancel token for the currently running agent turn. Long-running tool
    /// loops that can stall indefinitely (the `delegate` sub-agent) race their
    /// in-flight model requests against it so a user cancel aborts them instead
    /// of freezing the run at "working". The agent loop sets it at run start;
    /// `None` in tests, headless runs and contexts with no live run.
    pub stop: Option<CancellationToken>,
    /// Live steering pipe from the UI into this run, when one exists (see
    /// [`Steer`]). The agent loop and the delegate sub-agent loop both drain it
    /// at their rest points, so a message typed while either is running is
    /// delivered to whichever owns the loop. `None` in headless runs and tests.
    pub steer: Option<Steer>,
}

/// Sink a tool can report UI-visible activity through while it runs (e.g. the
/// `delegate` tool streaming its sub-agent's tool calls). `author` names the
/// model performing the action; callers without a UI use [`NoopEvents`].
#[async_trait]
pub trait ActivityEvents: Send + Sync {
    /// A tool call began.
    async fn tool_call(&self, author: &str, name: &str, args: &str);
    /// A tool call finished.
    async fn tool_result(&self, author: &str, name: &str, output: &str, ok: bool);
}

/// An [`ActivityEvents`] sink that discards everything: the default when no UI
/// is attached (tests, headless runs without a viewer).
pub struct NoopEvents;

#[async_trait]
impl ActivityEvents for NoopEvents {
    async fn tool_call(&self, _author: &str, _name: &str, _args: &str) {}
    async fn tool_result(&self, _author: &str, _name: &str, _output: &str, _ok: bool) {}
}

impl ToolContext {
    /// Model-supplied reasoning attached to the next approval prompt.
    pub fn set_approval(&self, notes: ApprovalNotes) {
        *self.approval.lock().unwrap() = Some(notes);
    }

    /// Drop any pending approval notes (called at the top of each agent turn).
    pub fn clear_approval(&self) {
        *self.approval.lock().unwrap() = None;
    }

    /// Take (and clear) any pending approval notes.
    fn take_approval(&self) -> Option<ApprovalNotes> {
        self.approval.lock().unwrap().take()
    }

    /// Ask the human to approve a mutating operation.
    ///
    /// When `auto_approve` is set this returns `Ok` immediately. Otherwise it
    /// routes a [`UserPrompt::Confirm`] and fails with [`UserReply::Denied`]
    /// unless the user consents. Any [`ApprovalNotes`] left by the agent are
    /// rendered above the diff so the human sees the model's reasoning before
    /// deciding.
    pub async fn confirm(&self, summary: impl Into<String>, diff: Option<String>) -> Result<()> {
        if self.auto_approve {
            return Ok(());
        }
        let notes = self.take_approval();
        let body = match notes {
            Some(notes) => {
                let mut text = notes.render();
                if let Some(d) = diff {
                    if !d.is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&d);
                    }
                }
                Some(text)
            }
            None => diff,
        };
        match self
            .user
            .ask(UserPrompt::Confirm {
                title: summary.into(),
                diff: body,
            })
            .await?
        {
            UserReply::Answer(text) if is_affirmative(&text) => Ok(()),
            UserReply::Answer(text) => anyhow::bail!("user denied request ({text:?})"),
            UserReply::Denied => anyhow::bail!("user denied request"),
        }
    }
}

/// Reasoning the model attaches to an action that needs human approval.
#[derive(Debug, Clone, Default)]
pub struct ApprovalNotes {
    /// Why this action should run.
    pub justification: String,
}

impl ApprovalNotes {
    pub fn render(&self) -> String {
        format!("Justification: {}", self.justification.trim())
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use crate::plan::{PlanStatus, PlanStep, PlanTarget, SessionControl};

    use super::*;

    struct NoopSession;
    impl SessionControl for NoopSession {
        fn set_title(&self, _t: &str) {}
        fn title(&self) -> String {
            "test".into()
        }
        fn set_plan(&self, _s: Vec<crate::plan::PlanStepDraft>) {}
        fn plan(&self) -> Vec<PlanStep> {
            vec![]
        }
        fn update_plan(&self, _t: PlanTarget, _s: PlanStatus, _n: Option<String>) -> bool {
            true
        }
        fn finish_plan(&self, _s: Option<String>) {}
        fn set_status(&self, _s: &str) {}
        fn status(&self) -> String {
            String::new()
        }
    }

    struct CaptureIo {
        last: Arc<Mutex<Option<String>>>,
    }
    #[async_trait]
    impl UserIo for CaptureIo {
        async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
            if let UserPrompt::Confirm { diff, .. } = prompt {
                *self.last.lock().unwrap() = diff;
            }
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

    #[tokio::test]
    async fn confirm_includes_model_justification() {
        let last = Arc::new(Mutex::new(None));
        let ctx = ToolContext {
            project_root: PathBuf::from("/tmp/x"),
            cwd: PathBuf::from("/tmp/x"),
            session: Arc::new(NoopSession),
            user: Arc::new(CaptureIo { last: last.clone() }),
            undo: Arc::new(NoopUndo),
            auto_approve: false,
            approval: Default::default(),
            events: Arc::new(crate::NoopEvents),
            steer: None,
            stop: None,
        };
        ctx.set_approval(ApprovalNotes {
            justification: "completes the requested rename".into(),
        });
        ctx.confirm("rename foo -> bar", None).await.unwrap();
        let shown = last.lock().unwrap().clone().unwrap();
        assert!(shown.contains("Justification: completes the requested rename"));
    }

    #[tokio::test]
    async fn confirm_passes_diff_through_when_no_notes() {
        let last = Arc::new(Mutex::new(None));
        let ctx = ToolContext {
            project_root: PathBuf::from("/tmp/x"),
            cwd: PathBuf::from("/tmp/x"),
            session: Arc::new(NoopSession),
            user: Arc::new(CaptureIo { last: last.clone() }),
            undo: Arc::new(NoopUndo),
            auto_approve: false,
            approval: Default::default(),
            events: Arc::new(crate::NoopEvents),
            steer: None,
            stop: None,
        };
        ctx.confirm("edit", Some("--- a.rs".into())).await.unwrap();
        let shown = last.lock().unwrap().clone().unwrap();
        assert_eq!(shown, "--- a.rs");
    }
}
