use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::form::FormSpec;
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
///
/// Every registry carries a live on/off switch ([`ToolRegistry::disabled`]),
/// empty by default so all tools are enabled. When the UI (the TUI's
/// list-mcp-servers modal) switches a tool off, [`ToolRegistry::iter`] stops
/// advertising it and [`ToolRegistry::get`] refuses to run it — the switch is
/// shared by `Arc` with the running agent loop, so a toggle reaches the next
/// model iteration immediately. [`ToolRegistry::iter_all`] ignores the filter
/// so disabled tools can still be listed and re-enabled.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    /// Tool names switched off at runtime (empty = every tool enabled). Shared
    /// by Arc so the UI can read/write it live while the agent loop runs.
    disabled: Arc<RwLock<HashSet<String>>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            disabled: Arc::new(RwLock::new(HashSet::new())),
        }
    }
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

    /// Look a tool up by name. Returns `None` when the tool is disabled or not
    /// registered, so a disabled tool can never be invoked.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        if !self.is_enabled(name) {
            return None;
        }
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|b| b.as_ref())
    }

    /// Iterate over every enabled tool, in registration order. The disabled set
    /// is snapshotted once up front so the returned iterator never holds the
    /// lock across yields.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        let disabled = self.disabled_snapshot();
        self.tools
            .iter()
            .filter(move |t| !disabled.contains(&t.spec().name))
            .map(|b| b.as_ref())
    }

    /// Iterate over every registered tool regardless of the enabled filter —
    /// the UI builds its tool list from this so disabled tools stay visible and
    /// can be turned back on.
    pub fn iter_all(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|b| b.as_ref())
    }

    /// True when `name` is registered and not switched off.
    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled_snapshot().contains(name)
    }

    /// Switch a tool on or off at runtime. Off tools disappear from
    /// [`ToolRegistry::iter`] and are refused by [`ToolRegistry::get`].
    pub fn set_enabled(&self, name: &str, enabled: bool) {
        let mut disabled = self.lock_disabled();
        if enabled {
            disabled.remove(name);
        } else {
            disabled.insert(name.to_string());
        }
    }

    /// Flip a tool's enabled state and report the new state (`true` = enabled).
    pub fn toggle(&self, name: &str) -> bool {
        let mut disabled = self.lock_disabled();
        if disabled.remove(name) {
            true
        } else {
            disabled.insert(name.to_string());
            false
        }
    }

    /// The live on/off set itself, for UIs that read it repeatedly while
    /// rendering (e.g. to show per-tool enable markers and counts).
    pub fn disabled_handle(&self) -> Arc<RwLock<HashSet<String>>> {
        self.disabled.clone()
    }

    fn disabled_snapshot(&self) -> HashSet<String> {
        match self.disabled.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn lock_disabled(&self) -> std::sync::RwLockWriteGuard<'_, HashSet<String>> {
        self.disabled.write().unwrap_or_else(|p| p.into_inner())
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

/// A one-shot request from the UI to compact the running agent's context into
/// a summary. Cheap to clone (it shares one `Arc<AtomicBool>`): the UI creates
/// one and hands it to the running [`ToolContext`], and the agent loop takes
/// (and clears) a pending request at its next rest point, rewriting the
/// history into a model-written summary.
#[derive(Clone, Default)]
pub struct CompactRequest(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CompactRequest {
    /// A fresh, un-requested handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the running loop to compact at its next rest point.
    pub fn request(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Take a pending request, clearing it. `true` when one was pending.
    pub fn take(&self) -> bool {
        self.0.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// `true` while a request is pending (not yet taken).
    pub fn is_pending(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
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
    /// One-shot "compact the context now" request from the UI, when one exists
    /// (see [`CompactRequest`]). The agent loop takes it at its next rest point
    /// and replaces the history with a model-written summary. `None` in
    /// headless runs and tests.
    pub compact: Option<CompactRequest>,
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
            // A form answer to a yes/no confirmation is nonsense: treat as not consenting.
            UserReply::Form(_) => anyhow::bail!("user denied request"),
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
    /// A yes/no confirmation with an optional diff/preview body.
    Confirm { title: String, diff: Option<String> },
    /// A declarative interactive form (custom components) for the human to fill.
    /// The UI renders the fields and returns a [`UserReply::Form`] keyed by id.
    Form(FormSpec),
}

/// Outcome of routing a prompt to the human.
#[derive(Debug, Clone)]
pub enum UserReply {
    /// The human typed an answer (e.g. `yes`/`no` to a confirmation).
    Answer(String),
    /// The human dismissed the prompt (escaped / cancelled).
    Denied,
    /// Answers to a [`UserPrompt::Form`], keyed by field id.
    Form(BTreeMap<String, String>),
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
    async fn confirm_passes_diff_through() {
        let last = Arc::new(Mutex::new(None));
        let ctx = ToolContext {
            project_root: PathBuf::from("/tmp/x"),
            cwd: PathBuf::from("/tmp/x"),
            session: Arc::new(NoopSession),
            user: Arc::new(CaptureIo { last: last.clone() }),
            undo: Arc::new(NoopUndo),
            auto_approve: false,
            events: Arc::new(crate::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        };
        ctx.confirm("edit", Some("--- a.rs".into())).await.unwrap();
        let shown = last.lock().unwrap().clone().unwrap();
        assert_eq!(shown, "--- a.rs");
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    struct Stub {
        spec: ToolSpec,
    }

    #[async_trait]
    impl Tool for Stub {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            Ok(String::new())
        }
    }

    fn tool(name: &str) -> Box<dyn Tool> {
        Box::new(Stub {
            spec: ToolSpec {
                name: name.to_string(),
                description: format!("desc {name}"),
                json_schema: serde_json::json!({}),
            },
        })
    }

    fn names<'a>(it: impl Iterator<Item = &'a dyn Tool>) -> Vec<String> {
        it.map(|t| t.spec().name.clone()).collect()
    }

    fn registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(tool("a"));
        reg.register(tool("b"));
        reg.register(tool("c"));
        reg
    }

    #[test]
    fn nothing_disabled_by_default() {
        let reg = registry();
        assert_eq!(names(reg.iter()), vec!["a", "b", "c"]);
        assert!(reg.is_enabled("a"));
        assert!(reg.get("a").is_some());
    }

    #[test]
    fn set_enabled_filters_iter_and_blocks_get() {
        let reg = registry();
        reg.set_enabled("b", false);
        assert_eq!(names(reg.iter()), vec!["a", "c"]);
        assert!(
            reg.get("b").is_none(),
            "disabled tool must not be invocable"
        );
        assert!(reg.get("a").is_some());
        assert!(!reg.is_enabled("b"));
        reg.set_enabled("b", true);
        assert_eq!(names(reg.iter()), vec!["a", "b", "c"]);
        assert!(reg.get("b").is_some());
    }

    #[test]
    fn toggle_reports_new_state() {
        let reg = registry();
        assert!(!reg.toggle("b"), "first toggle disables");
        assert_eq!(names(reg.iter()), vec!["a", "c"]);
        assert!(reg.toggle("b"), "second toggle re-enables");
        assert_eq!(names(reg.iter()), vec!["a", "b", "c"]);
    }

    #[test]
    fn iter_all_still_yields_disabled_tools() {
        let reg = registry();
        reg.set_enabled("b", false);
        assert_eq!(names(reg.iter_all()), vec!["a", "b", "c"]);
    }

    #[test]
    fn disabled_handle_mutations_are_live() {
        let reg = registry();
        let handle = reg.disabled_handle();
        handle.write().unwrap().insert("a".to_string());
        assert!(!reg.is_enabled("a"));
        assert_eq!(names(reg.iter()), vec!["b", "c"]);
        handle.write().unwrap().remove("a");
        assert!(reg.is_enabled("a"));
    }

    #[test]
    fn iter_snapshots_disabled_set() {
        let reg = registry();
        let it = reg.iter();
        // Mutating the set mid-iteration must not poison the lock or change an
        // already-created iterator.
        reg.set_enabled("a", false);
        assert_eq!(names(it), vec!["a", "b", "c"]);
        assert_eq!(names(reg.iter()), vec!["b", "c"]);
    }
}
