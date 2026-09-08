//! Cockpit TUI: markdown chat + plan checklist + prompt bar + modal dialogs.
//!
//! Chat layout:
//! - no emojis
//! - user messages are a soft lighter band (chat background lifted by a touch,
//!   see `USER_BG_ALPHA`) with a cyan rule on the left of every wrapped line
//! - agent tool calls render as a compact card (tool + justification); clicking
//!   (or the mouse wheel) opens the details
//! - assistant/user text is rendered as markdown

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use arboard::Clipboard;
use async_trait::async_trait;
use comrade_core::{
    AgentEvent, AgentSession, ChatMessage, ContextManager, DelegateCfg, Role,
    build_session_context, run_agent_with_history,
};
use comrade_tool::{
    AGENT_MODEL, PlanStatus, PlanStep, PlanTarget, SessionControl, ToolContext, UserIo, UserPrompt,
    UserReply,
};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::editor::{Editor, LayoutRow, cursor_col, cursor_row, wrap_rows};
use crate::{Deps, new_session};

/// Width of the `"> "` gutter on the prompt line (also used as the indent
/// for continuation rows).
const PROMPT_GUTTER: u16 = 2;
/// Max rows the prompt editor may occupy before it scrolls internally.
const PROMPT_MAX_ROWS: usize = 5;
/// Mode-line background while auto-approve is active: a warm orange so the
/// bar reads as "warning: changes are applied without asking".
const AUTO_BAR_BG: Color = Color::Rgb(203, 106, 15);
/// App name shown right-aligned on the mode line (Emacs-style), where the
/// keybinding legend used to live.
const APP_TAG: &str = " comrade ";

// ---------------------------------------------------------------------------
// UserIo bridging into the UI event loop
// ---------------------------------------------------------------------------

struct PendingAsk {
    prompt: UserPrompt,
    reply: oneshot::Sender<UserReply>,
}

struct TuiUserIo {
    tx: mpsc::Sender<PendingAsk>,
}

#[async_trait]
impl UserIo for TuiUserIo {
    async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(PendingAsk { prompt, reply: tx }).await?;
        rx.await.context("UI closed before answering")
    }
}

// ---------------------------------------------------------------------------
// chat model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum MsgKind {
    /// A message from the human user.
    User,
    /// A message written by the (main) model: replies and final answers.
    Assistant,
    /// A tool call card (also covers the parent's `delegate` hand-offs).
    Tool,
    /// Small grey status note ("run finished", "plan finished", ...).
    Meta,
    /// A collapsed failing-test block.
    Failure,
    /// A collapsible "thinking" block: model reasoning between actions.
    Reasoning,
    /// A reply from a delegated model, shown under that model's name.
    Delegate,
    /// A folded digest of one completed stretch of activity (tool calls,
    /// reasoning, failures, meta notes) between two spoken messages. The
    /// original messages are kept in `Msg::children` and unfolded back on
    /// demand, so per-card expand/copy/search keep working.
    Run,
}

#[derive(Clone)]
struct ToolCard {
    name: String,
    /// Display name of the model that invoked the tool (None for legacy rows).
    author: Option<String>,
    args: String,
    justification: Option<String>,
    risk: Option<String>,
    result: Option<String>,
    ok: bool,
    open: bool,
    /// When the call reached the UI (wall clock), to measure how long it took.
    started: Option<std::time::Instant>,
    /// Elapsed wall time once the call finished (None while still running or
    /// when the run was interrupted before the result arrived).
    taken_ms: Option<u128>,
    /// Real tokens of the model request that produced this call, when the
    /// endpoint reported usage (first call of a multi-call turn only, so
    /// per-run sums count each request once).
    tokens: Option<usize>,
}

/// One failed test: name + captured failure detail.
#[derive(Clone)]
struct TestFail {
    name: String,
    detail: String,
    open: bool,
}

/// Structured summary of a `run_tests` invocation.
#[derive(Default)]
struct TestSummary {
    passed: usize,
    failed: usize,
    duration: String,
    /// (name, detail) for every failing test.
    cases: Vec<(String, String)>,
}

#[derive(Clone)]
struct Msg {
    kind: MsgKind,
    text: String,
    tool: Option<ToolCard>,
    fail: Option<TestFail>,
    /// Who produced this entry: "you", the main model's display label, or the
    /// delegate's name. None for Meta/Failure rows.
    author: Option<String>,
    /// Open state of a collapsible Reasoning block.
    open: bool,
    /// The original messages behind a folded [`MsgKind::Run`] digest. Empty for
    /// every other kind.
    children: Vec<Msg>,
}

impl Msg {
    fn text(kind: MsgKind, text: impl Into<String>) -> Self {
        Msg {
            kind,
            text: text.into(),
            tool: None,
            fail: None,
            author: None,
            open: false,
            children: Vec::new(),
        }
    }
    fn authored(kind: MsgKind, author: impl Into<String>, text: impl Into<String>) -> Self {
        Msg {
            kind,
            text: text.into(),
            tool: None,
            fail: None,
            author: Some(author.into()),
            open: false,
            children: Vec::new(),
        }
    }
    fn tool(card: ToolCard) -> Self {
        Msg {
            kind: MsgKind::Tool,
            text: String::new(),
            tool: Some(card),
            fail: None,
            author: None,
            open: false,
            children: Vec::new(),
        }
    }
    fn failure(name: String, detail: String) -> Self {
        Msg {
            kind: MsgKind::Failure,
            text: String::new(),
            tool: None,
            fail: Some(TestFail {
                name,
                detail,
                open: false,
            }),
            author: None,
            open: false,
            children: Vec::new(),
        }
    }
    /// A folded digest holding the messages of one completed activity stretch.
    fn run(children: Vec<Msg>) -> Self {
        Msg {
            kind: MsgKind::Run,
            text: String::new(),
            tool: None,
            fail: None,
            author: None,
            open: false,
            children,
        }
    }
    /// A thinking block under `author`. Reasoning is visible (expanded) by
    /// default; the user can still collapse it with Tab.
    fn reasoning(author: impl Into<String>, text: impl Into<String>) -> Self {
        let mut m = Msg::authored(MsgKind::Reasoning, author, text);
        m.open = true;
        m
    }
}

/// A fully laid-out chat row.
struct RenderRow {
    /// None (no rule) or Some(color) for the left vertical rule.
    rule: Option<Color>,
    spans: Vec<Span<'static>>,
    /// Some(msg index) when this row is the clickable header of a tool card.
    tool_header: Option<usize>,
}

/// Cached row layout of `chat` (the live stream preview is laid out fresh on
/// every frame, so it is not part of this cache).
///
/// Rebuilding rows means re-tokenising and re-wrapping every message body via
/// `md_to_lines`, which with a long transcript costs milliseconds per frame.
/// Frames where nothing chat-affecting changed (typing in the input, scrolling,
/// selection moves, search navigation, most streaming deltas) reuse this cache
/// and only render the visible rows.
struct ChatRowsCache {
    width: usize,
    /// Value of `App::chat_epoch` when this cache was built.
    epoch: u64,
    rows: Vec<RenderRow>,
    /// Owning chat-message index per row (parallel to `rows`).
    owner: Vec<Option<usize>>,
    /// Per chat-message row span (start row, height).
    ranges: Vec<(usize, usize)>,
}

/// Return the chat row layout for `(epoch, width)`, rebuilding it from
/// `chat`/`collapsed` via [`layout_chat_rows`] when the cache is stale or
/// absent, and reusing it otherwise.
fn chat_cache<'a>(
    cache: &'a mut Option<ChatRowsCache>,
    epoch: u64,
    width: usize,
    chat: &[Msg],
    collapsed: &[bool],
) -> &'a ChatRowsCache {
    let stale = !matches!(cache, Some(c) if c.epoch == epoch && c.width == width);
    if stale {
        let (rows, owner, ranges) = layout_chat_rows(chat, collapsed, "", width);
        *cache = Some(ChatRowsCache {
            width,
            epoch,
            rows,
            owner,
            ranges,
        });
    }
    cache.as_ref().expect("cache just (re)built")
}

/// Active incremental search over the chat history (Ctrl-S).
struct Search {
    /// Raw query as typed by the user (matched case-insensitively).
    query: String,
    /// Chat-message indices that contain the query, ascending.
    matches: Vec<usize>,
    /// Position of the current match inside `matches`.
    cur: usize,
}

impl Search {
    fn new() -> Self {
        Search {
            query: String::new(),
            matches: Vec::new(),
            cur: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

struct Dialog {
    prompt: UserPrompt,
    buf: String,
    reply: oneshot::Sender<UserReply>,
}

/// A plan step offered in the Ctrl-A "assign a model" overlay.
struct PickStep {
    id: u64,
    goal: String,
    model: String,
}

/// Human-in-the-loop "assign a model" overlay (Ctrl-A): the human picks one
/// plan step (pending/blocked only — a step being worked or done is never
/// offered), then picks the model that will run it ("self" or a configured
/// delegate).
struct ModelPick {
    steps: Vec<PickStep>,
    /// Cursor over `steps` (while picking the step) or over `models` (once
    /// `model_sel` is Some).
    sel: usize,
    models: Vec<String>,
    /// None = still choosing the step; Some(i) = choosing the model at `i`.
    model_sel: Option<usize>,
}

// ---------------------------------------------------------------------------
// M-x command palette (Alt+X)
// ---------------------------------------------------------------------------

/// Every cockpit command invocable from the M-x palette. The `name` is the
/// descriptive, Emacs-style identifier typed into the palette; `keys` is the
/// keybinding hint shown after a command runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MxCommand {
    AssignModel,
    BackwardKillWord,
    BackwardWord,
    BeginningOfLine,
    CancelRun,
    Copy,
    EndOfLine,
    ForwardWord,
    InsertNewline,
    KillWord,
    MoveBlockDown,
    MoveBlockUp,
    MoveUserDown,
    MoveUserUp,
    NewSession,
    Quit,
    ReloadConfig,
    SearchChat,
    SubmitPrompt,
    ToggleAutoAccept,
    ToggleToolCard,
}

impl MxCommand {
    /// Palette order (alphabetical by descriptive name).
    const ALL: &'static [MxCommand] = &[
        MxCommand::AssignModel,
        MxCommand::BackwardKillWord,
        MxCommand::BackwardWord,
        MxCommand::BeginningOfLine,
        MxCommand::CancelRun,
        MxCommand::Copy,
        MxCommand::EndOfLine,
        MxCommand::ForwardWord,
        MxCommand::InsertNewline,
        MxCommand::KillWord,
        MxCommand::MoveBlockDown,
        MxCommand::MoveBlockUp,
        MxCommand::MoveUserDown,
        MxCommand::MoveUserUp,
        MxCommand::NewSession,
        MxCommand::Quit,
        MxCommand::ReloadConfig,
        MxCommand::SearchChat,
        MxCommand::SubmitPrompt,
        MxCommand::ToggleAutoAccept,
        MxCommand::ToggleToolCard,
    ];

    fn name(self) -> &'static str {
        match self {
            MxCommand::AssignModel => "assign-model-to-step",
            MxCommand::BackwardKillWord => "backward-kill-word",
            MxCommand::BackwardWord => "backward-word",
            MxCommand::BeginningOfLine => "beginning-of-line",
            MxCommand::CancelRun => "cancel-run",
            MxCommand::Copy => "copy",
            MxCommand::EndOfLine => "end-of-line",
            MxCommand::ForwardWord => "forward-word",
            MxCommand::InsertNewline => "insert-newline",
            MxCommand::KillWord => "kill-word",
            MxCommand::MoveBlockDown => "move-block-down",
            MxCommand::MoveBlockUp => "move-block-up",
            MxCommand::MoveUserDown => "move-user-down",
            MxCommand::MoveUserUp => "move-user-up",
            MxCommand::NewSession => "new-session",
            MxCommand::Quit => "quit",
            MxCommand::ReloadConfig => "reload-config",
            MxCommand::SearchChat => "search-chat-history",
            MxCommand::SubmitPrompt => "submit-prompt",
            MxCommand::ToggleAutoAccept => "toggle-auto-accept",
            MxCommand::ToggleToolCard => "toggle-tool-card",
        }
    }

    /// Emacs-style keybinding hint; `None` when the command is unbound.
    fn keys(self) -> Option<&'static str> {
        match self {
            MxCommand::AssignModel => Some("C-a"),
            MxCommand::BackwardKillWord => Some("M-<backspace>"),
            MxCommand::BackwardWord => Some("M-<left>"),
            MxCommand::BeginningOfLine => Some("<home>"),
            MxCommand::CancelRun => Some("esc"),
            MxCommand::Copy => Some("C-S-c"),
            MxCommand::EndOfLine => Some("<end>"),
            MxCommand::ForwardWord => Some("M-<right>"),
            MxCommand::InsertNewline => Some("S-<return>"),
            MxCommand::KillWord => Some("M-<delete>"),
            MxCommand::MoveBlockDown => Some("C-n"),
            MxCommand::MoveBlockUp => Some("C-p"),
            MxCommand::MoveUserDown => Some("C-S-n"),
            MxCommand::MoveUserUp => Some("C-S-p"),
            // Starts a fresh session: unbound, run it from the M-x palette.
            MxCommand::NewSession => None,
            MxCommand::Quit => Some("C-c"),
            MxCommand::ReloadConfig => Some("C-r"),
            MxCommand::SearchChat => Some("C-s"),
            MxCommand::SubmitPrompt => Some("<return>"),
            MxCommand::ToggleAutoAccept => Some("C-SPC"),
            MxCommand::ToggleToolCard => Some("tab"),
        }
    }

    fn desc(self) -> &'static str {
        match self {
            MxCommand::AssignModel => "assign a delegate model to a plan step",
            MxCommand::BackwardKillWord => "delete the word before the prompt cursor",
            MxCommand::BackwardWord => "move the prompt cursor back one word",
            MxCommand::BeginningOfLine => "move the prompt cursor to the start of the line",
            MxCommand::CancelRun => "stop the running agent",
            MxCommand::Copy => "copy the prompt selection or the chat block under the cursor",
            MxCommand::EndOfLine => "move the prompt cursor to the end of the line",
            MxCommand::ForwardWord => "move the prompt cursor forward one word",
            MxCommand::InsertNewline => "insert a newline in the prompt",
            MxCommand::KillWord => "delete the word after the prompt cursor",
            MxCommand::MoveBlockDown => "move to the next chat block",
            MxCommand::MoveBlockUp => "move to the previous chat block",
            MxCommand::MoveUserDown => "jump to the next message you sent",
            MxCommand::MoveUserUp => "jump to the previous message you sent",
            MxCommand::NewSession => "start a fresh session (clears the chat, plan and context)",
            MxCommand::Quit => "quit the cockpit",
            MxCommand::ReloadConfig => "reload the config file without restarting",
            MxCommand::SearchChat => "search the chat history",
            MxCommand::SubmitPrompt => "send the prompt to the agent",
            MxCommand::ToggleAutoAccept => "toggle auto-accept of approvals",
            MxCommand::ToggleToolCard => {
                "expand or collapse the selected tool card (or a whole user-turn section)"
            }
        }
    }
}

/// The Alt+X command palette: type to narrow, enter runs the highlighted
/// command. After a command with a keybinding runs, `done` holds the "you can
/// run this command with <keys>" hint shown in the palette row.
struct Mx {
    query: String,
    /// Commands matching `query`, in [`MxCommand::ALL`] order.
    matches: Vec<MxCommand>,
    sel: usize,
    /// Some(hint) after a command ran: the palette stays open showing the hint
    /// until the next key dismisses it.
    done: Option<String>,
}

impl Mx {
    fn open() -> Self {
        let mut mx = Mx {
            query: String::new(),
            matches: Vec::new(),
            sel: 0,
            done: None,
        };
        mx.refresh();
        mx
    }

    /// Recompute the match list from the typed query and keep the cursor valid.
    fn refresh(&mut self) {
        let q = self.query.to_lowercase();
        self.matches = MxCommand::ALL
            .iter()
            .filter(|c| c.name().contains(&q))
            .copied()
            .collect();
        if self.sel >= self.matches.len() {
            self.sel = 0;
        }
    }

    fn step(&mut self, dir: isize) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.sel = (((self.sel as isize + dir) % len) + len) as usize % len as usize;
    }

    /// Emacs-style Tab completion: extend the query to the longest common
    /// prefix shared by every current match. With a single match that is the
    /// full command name. When nothing is typed (or the matches share no
    /// longer prefix) the query is left alone and the full candidate list
    /// stays visible, mirroring `M-x`'s "Tab shows the completions" step.
    fn complete(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let lcp = self
            .matches
            .iter()
            .map(|c| c.name())
            .reduce(|acc, name| common_prefix(acc, name))
            .unwrap_or("");
        if lcp.len() > self.query.len() {
            self.query = lcp.to_string();
            self.refresh();
        }
    }
}

/// Longest common prefix of two strings.
fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let n = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    &a[..a.char_indices().nth(n).map_or(a.len(), |(i, _)| i)]
}

struct App {
    cfg: Arc<comrade_core::Config>,
    client: Arc<comrade_core::LlmClient>,
    tools: Arc<comrade_tool::ToolRegistry>,
    root: std::path::PathBuf,
    /// Path the live config was read from (None = defaults only), so a reload
    /// command can re-read it without restarting.
    config_source: Option<std::path::PathBuf>,
    /// True when the CLI forced autonomy=auto; re-applied on config reloads.
    auto_forced: bool,

    session: Arc<AgentSession>,
    ctx_base: ToolContext,
    /// Rolling conversation history shared across task runs in this session:
    /// kept between prompts (never wiped at task end) and compacted
    /// automatically as it approaches the token budget.
    history: Arc<tokio::sync::Mutex<ContextManager>>,

    events_tx: mpsc::Sender<AgentEvent>,
    events_rx: mpsc::Receiver<AgentEvent>,
    asks_rx: mpsc::Receiver<PendingAsk>,

    stop: Option<CancellationToken>,
    running: bool,
    /// Auto-accept mode: approvals are answered "yes" without prompting.
    auto_accept: bool,
    /// Latest repo snapshot for the mode line.
    git: GitBarInfo,
    git_rx: mpsc::Receiver<GitBarInfo>,
    git_tx: mpsc::Sender<GitBarInfo>,
    /// True while a background git refresh is in flight.
    git_inflight: bool,
    /// Earliest instant the background refresh may run again (backed off to
    /// 10 s when the cwd is not a repo so we don't spawn failing `git`s).
    git_gate: Option<Instant>,
    chat: Vec<Msg>,
    /// Bumped on every change to `chat` content or `section_collapsed` so the
    /// cached row layout (`chat_rows_cache`) is rebuilt on the next draw.
    chat_epoch: u64,
    /// Cached row layout of `chat`, reused across frames while nothing that
    /// affects the layout changed (see [`ChatRowsCache`]).
    chat_rows_cache: Option<ChatRowsCache>,
    /// Org-style section (one exchange per user turn) collapse state, indexed
    /// by section ordinal = the turn's rank among `MsgKind::User` messages.
    /// User messages are only ever appended (run-digest folds splice only
    /// Tool/Reasoning/Failure/Meta), so ordinals never shift or get reused.
    section_collapsed: Vec<bool>,
    /// Raw current model output (not yet committed to a message).
    stream: String,
    input: Editor,
    /// Active Ctrl-S search over chat history (None when closed).
    search: Option<Search>,
    dialogs: Vec<Dialog>,
    /// True while the open dialog buffers a follow-up question to the model.
    dialog_ask: bool,
    /// Human-driven "assign a model to a plan step" overlay (Ctrl-A), when open.
    pick: Option<ModelPick>,
    /// The Alt+X (M-x) command palette, when open.
    mx: Option<Mx>,
    /// Sends a follow-up question's answer back from the ask-the-model task.
    dialog_ask_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Follow-up Q/A shown inside the confirm dialog.
    dialog_conv: Vec<String>,

    // geometry/metrics refreshed on every draw
    chat_rect: Rect,
    row_targets: Vec<Option<usize>>,
    /// Owning chat-message index for each rendered row (None = live preview).
    row_msg: Vec<Option<usize>>,
    /// Per chat-message row span (start row, height) over the last layout.
    msg_ranges: Vec<(usize, usize)>,
    view_rows: usize,
    /// Currently selected chat block (Emacs-style navigation).
    sel: Option<usize>,
    scroll_top: usize,
    follow: bool,
    /// True while the previous frame sat at the bottom of the chat, so new
    /// content keeps the view pinned there (autoscroll).
    was_at_bottom: bool,
    /// Latest context-usage snapshot for the gauge.
    ctx_tokens: usize,
    ctx_budget: usize,
    ctx_estimated: bool,
    /// Account balance display (DeepSeek), when available.
    balance: Option<String>,
    /// Name of the tool currently running (auto status while no agent text).
    activity: Option<String>,
}

/// Snapshot of the repo state shown on the emacs-style mode line, refreshed in
/// the background (see [`App::refresh_git`]).
#[derive(Clone, Debug, Default)]
struct GitBarInfo {
    /// Current branch name (absent when not inside a git work tree).
    branch: Option<String>,
    /// Total inserted/removed lines vs HEAD (staged + unstaged).
    ins: u64,
    del: u64,
    /// Newly added/untracked files and deleted files (file-level git status).
    added_files: u64,
    deleted_files: u64,
    /// True when the working directory is inside a git work tree.
    repo: bool,
}

impl App {
    /// Display label of the main model: the name shown next to every message,
    /// action and thought this session's assistant produces.
    fn actor_label(&self) -> String {
        self.cfg.llm.display()
    }

    /// Commit the text currently streaming in the live preview as a Reasoning
    /// block (the model's visible reasoning right before a tool call), then
    /// drop the preview.
    fn commit_stream_reasoning(&mut self) {
        if let Some(visible) = reasoning_from_stream(&self.stream) {
            self.push_reasoning(visible);
        }
        self.stream.clear();
    }

    /// Push a thinking block under the main model's name. Reasoning is visible
    /// (expanded) by default; the user can still collapse it with Tab.
    fn push_reasoning(&mut self, text: impl Into<String>) {
        let author = self.actor_label();
        self.push_msg(Msg::reasoning(author, text));
    }

    fn push_msg(&mut self, msg: Msg) {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        if self.chat.len() >= 400 {
            self.chat.remove(0);
        }
        self.chat.push(msg);
    }

    fn push_meta(&mut self, text: impl Into<String>) {
        self.push_msg(Msg::text(MsgKind::Meta, text));
    }

    fn last_tool_mut(&mut self, name: &str) -> Option<&mut ToolCard> {
        // Callers mutate the returned card (result/open/taken_ms), which affects
        // the row layout: conservatively invalidate the layout cache.
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        self.chat.iter_mut().rev().find_map(|m| match &mut m.tool {
            Some(c) if c.name == name || name.is_empty() => Some(c),
            _ => None,
        })
    }

    /// Find the most recent still-visible tool card authored by a delegate
    /// (name *and* model both match), so a delegate's result attaches to the
    /// delegate's own card even when the main model calls the same tool later
    /// in the same stretch.
    fn last_delegate_tool_mut(&mut self, name: &str, model: &str) -> Option<&mut ToolCard> {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        self.chat.iter_mut().rev().find_map(|m| match &mut m.tool {
            Some(c) if c.name == name && c.author.as_deref() == Some(model) => Some(c),
            _ => None,
        })
    }

    /// Record the elapsed wall time of a delegate's `name` card once its result
    /// arrives.
    fn stamp_delegate_taken(&mut self, name: &str, model: &str) {
        if let Some(card) = self.last_delegate_tool_mut(name, model) {
            if card.taken_ms.is_none() {
                if let Some(started) = card.started {
                    card.taken_ms = Some(started.elapsed().as_millis());
                }
            }
        }
    }

    /// Record the elapsed wall time of the last `name` card once its result
    /// arrives (a no-op for cards that never started or already got stamped).
    fn stamp_taken(&mut self, name: &str) {
        if let Some(card) = self.last_tool_mut(name) {
            if card.taken_ms.is_none() {
                if let Some(started) = card.started {
                    card.taken_ms = Some(started.elapsed().as_millis());
                }
            }
        }
    }

    // --- org-mode style exchange sections ---------------------------------

    /// Expand the section a chat message belongs to (no-op when not collapsed).
    fn expand_section_at(&mut self, msg_idx: usize) {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        expand_section(&mut self.section_collapsed, &self.chat, msg_idx);
    }

    /// Toggle the section a chat message belongs to (no-op outside a section).
    fn toggle_section_at(&mut self, msg_idx: usize) {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        toggle_section(
            &mut self.section_collapsed,
            &self.chat,
            self.running,
            msg_idx,
        );
    }

    fn toggle_tool(&mut self, idx: usize) {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        // A user-turn heading is an org-style section header: toggling it
        // folds/unfolds the whole exchange, not a single card.
        if matches!(self.chat.get(idx).map(|m| m.kind), Some(MsgKind::User)) {
            self.toggle_section_at(idx);
            return;
        }
        let is_run = match self.chat.get(idx).map(|m| m.kind) {
            Some(MsgKind::Run) => true,
            _ => false,
        };
        if is_run {
            // Toggling a folded digest unfolds it back into its messages.
            self.expand_run(idx);
            return;
        }
        if let Some(m) = self.chat.get_mut(idx) {
            match m.kind {
                MsgKind::Tool => {
                    if let Some(card) = &mut m.tool {
                        card.open = !card.open;
                    }
                }
                MsgKind::Failure => {
                    if let Some(fail) = &mut m.fail {
                        fail.open = !fail.open;
                    }
                }
                MsgKind::Reasoning => m.open = !m.open,
                _ => {}
            }
        }
    }

    /// Replace one folded digest with its children in place, then move the
    /// selection onto the first child so Tab keeps drilling into it.
    fn expand_run(&mut self, idx: usize) {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        let Some(&(start, _)) = self.msg_ranges.get(idx) else {
            unfold_run(&mut self.chat, idx);
            if idx < self.chat.len() {
                self.sel = Some(idx);
            }
            return;
        };
        unfold_run(&mut self.chat, idx);
        self.sel = Some(idx.min(self.chat.len().saturating_sub(1)));
        self.follow = false;
        self.was_at_bottom = false;
        self.scroll_top = start;
    }

    /// Fold every completed stretch of activity into a one-line digest. Runs
    /// while a search is open would shift the search's message indices, so it
    /// waits until the search closes. The selection cursor is re-anchored when
    /// the fold swallows the message it pointed at.
    fn fold_completed(&mut self) {
        if self.search.is_some() {
            return;
        }
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        let sel = self.sel;
        let spans = fold_completed_runs(&mut self.chat);
        if spans.is_empty() {
            return;
        }
        if let Some(mut s) = sel {
            for &(start, len) in &spans {
                if s > start && s < start + len {
                    s = start;
                } else if s >= start + len {
                    s = s - (len - 1);
                }
            }
            self.sel = (s < self.chat.len()).then_some(s);
        }
    }

    /// Copy the plain text of the message currently under the cursor
    /// (the selected block) to the system clipboard. Bound to Ctrl+Shift+C.
    fn copy_selected(&mut self) {
        let Some(idx) = self.sel else { return };
        let Some(msg) = self.chat.get(idx) else {
            return;
        };
        let text = msg_searchable(msg).trim().to_string();
        self.copy_text(&text);
    }

    /// Put `text` on the system clipboard, reporting failures in the chat.
    fn copy_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut clip = match Clipboard::new() {
            Ok(clip) => clip,
            Err(e) => {
                self.push_meta(format!("copy failed: {e}"));
                return;
            }
        };
        if let Err(e) = clip.set_text(text.to_string()) {
            self.push_meta(format!("copy failed: {e}"));
        }
    }

    fn push_failure(&mut self, name: String, detail: String) {
        self.push_msg(Msg::failure(name, detail));
    }

    /// Move to the next (`+1`) or previous (`-1`) visible chat block, stepping
    /// over the messages hidden inside a collapsed section.
    fn move_block(&mut self, dir: isize) {
        if let Some(next) = step_visible(&self.chat, &self.section_collapsed, self.sel, dir) {
            self.select_block(next);
        }
    }

    /// Move to the next (`+1`) or previous (`-1`) user message.
    fn move_user(&mut self, dir: isize) {
        let users: Vec<usize> = self
            .chat
            .iter()
            .enumerate()
            .filter(|(_, m)| m.kind == MsgKind::User)
            .map(|(i, _)| i)
            .collect();
        if let Some(i) = step_user(&users, self.sel, dir) {
            self.select_block(i);
        }
    }

    /// Select a chat block and scroll it into view (top-aligned).
    fn select_block(&mut self, idx: usize) {
        if idx >= self.chat.len() {
            return;
        }
        self.sel = Some(idx);
        self.follow = false;
        self.was_at_bottom = false;
        if let Some(&(start, _)) = self.msg_ranges.get(idx) {
            self.scroll_top = start;
        }
    }

    // --- Ctrl-S search over chat history ----------------------------------

    /// All chat indices whose message matches the current query (folded run
    /// digests match through their children).
    fn collect_matches(&self) -> Vec<usize> {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return Vec::new();
        };
        let ql = query.to_lowercase();
        self.chat
            .iter()
            .enumerate()
            .filter(|(_, m)| msg_matches(m, &ql))
            .map(|(i, _)| i)
            .collect()
    }

    /// Recompute match indices from the current query, then jump to the first.
    fn refresh_search(&mut self) {
        let matches = self.collect_matches();
        if let Some(s) = &mut self.search {
            s.matches = matches;
            s.cur = 0;
        }
        self.goto_search_match();
    }

    /// Scroll the current search match into view; expand collapsed cards so the
    /// matched content is actually visible. A match inside a folded run digest
    /// unfolds the digest first, then jumps to the child that actually matched.
    fn goto_search_match(&mut self) {
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
        let Some(idx) = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.cur).copied())
        else {
            return;
        };
        let mut idx = idx;
        // A match hiding inside a collapsed exchange must unfold its section
        // first, or the jump would land on an invisible row.
        self.expand_section_at(idx);
        let folded = match self.chat.get(idx).map(|m| m.kind) {
            Some(MsgKind::Run) => true,
            _ => false,
        };
        if folded {
            let anchor = idx;
            unfold_run(&mut self.chat, idx);
            let matches = self.collect_matches();
            if matches.is_empty() {
                // Nothing matched once unfolded (should not happen): park on the
                // first child of the digest.
                idx = idx.min(self.chat.len().saturating_sub(1));
                if let Some(s) = &mut self.search {
                    s.matches.clear();
                    s.cur = 0;
                }
            } else {
                let cur = matches.iter().position(|&m| m >= anchor).unwrap_or(0);
                idx = matches[cur];
                if let Some(s) = &mut self.search {
                    s.matches = matches;
                    s.cur = cur;
                }
            }
        }
        self.select_block(idx);
        if let Some(m) = self.chat.get_mut(idx) {
            if let Some(card) = &mut m.tool {
                card.open = true;
            } else if let Some(fail) = &mut m.fail {
                fail.open = true;
            } else if m.kind == MsgKind::Reasoning {
                m.open = true;
            }
        }
    }

    /// Move the search cursor by `dir` (+1 = next, -1 = previous) and jump.
    fn step_search(&mut self, dir: isize) {
        let Some(cur) = search_step(self.search.as_ref().map(|s| (s.cur, s.matches.len())), dir)
        else {
            return;
        };
        self.search.as_mut().unwrap().cur = cur;
        self.goto_search_match();
    }

    fn start_run(&mut self, prompt: String) {
        if self.running || prompt.trim().is_empty() {
            return;
        }
        let cfg = self.cfg.clone();
        let client = self.client.clone();
        let tools = self.tools.clone();
        let ctx = self.ctx_base.clone();
        // One conversation per session: every task appends to the same history,
        // which the agent compacts itself as it approaches the token budget.
        let history = self.history.clone();
        let tx = self.events_tx.clone();
        let balance_tx = self.events_tx.clone();
        let stop = CancellationToken::new();
        self.stop = Some(stop.clone());
        self.running = true;
        self.follow = true;
        self.sel = None;
        tokio::spawn(async move {
            {
                // Runs are serialized (self.running), so the guard is
                // uncontended; it only exists to give the task owned access.
                let mut history = history.lock().await;
                let _ = run_agent_with_history(
                    &cfg,
                    &client,
                    ctx,
                    &tools,
                    prompt,
                    &mut history,
                    tx,
                    stop,
                )
                .await;
            }
            // Refresh the provider account balance after the run finishes.
            if let Some(balance) = client.fetch_account_balance().await {
                let _ = balance_tx.send(AgentEvent::AccountBalance(balance)).await;
            }
        });
    }

    fn cancel_run(&mut self) {
        if let Some(stop) = &self.stop {
            stop.cancel();
        }
        self.push_meta("cancelling...");
    }

    /// Re-read the config file from disk and swap the live model client, tool
    /// registry (so `[[delegates]]` changes take effect) and approval policy.
    /// Only applies while idle: a run in flight keeps the config it started
    /// with. On any error the old config stays active and the failure is shown.
    fn reload_config(&mut self) {
        if self.running {
            self.push_meta("cannot reload config while a run is in flight");
            return;
        }
        let loaded = match comrade_core::Config::load(self.config_source.as_deref()) {
            Ok(l) => l,
            Err(e) => {
                self.push_meta(format!("config reload failed: {e:#}"));
                return;
            }
        };
        let mut cfg = loaded.config;
        let source = loaded.source;
        if self.auto_forced {
            cfg.security.autonomy = comrade_core::Autonomy::Auto;
        }
        // The context window and model version are detected against the live
        // endpoint at startup; keep them across a reload unless the new config
        // pins them explicitly.
        if cfg.llm.context_window.is_none() {
            cfg.llm.context_window = self.cfg.llm.context_window;
        }
        if cfg.llm.model_version.is_none() {
            cfg.llm.model_version = self.cfg.llm.model_version.clone();
        }
        let client = match comrade_core::LlmClient::new(&cfg.llm) {
            Ok(c) => c,
            Err(e) => {
                self.push_meta(format!("config reload failed: {e:#}"));
                return;
            }
        };
        let tools = match crate::build_tools(&cfg) {
            Ok(t) => t,
            Err(e) => {
                self.push_meta(format!("config reload failed: {e:#}"));
                return;
            }
        };
        self.ctx_base.auto_approve = cfg.auto_approve();
        self.ctx_budget = cfg.effective_budget();
        let from = source
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "defaults".into());
        self.push_meta(format!("config reloaded from {from}"));
        self.cfg = Arc::new(cfg);
        self.client = Arc::new(client);
        self.tools = Arc::new(tools);
    }

    /// Start a fresh session in place, replacing the current one: a brand-new
    /// [`AgentSession`] (plan/status/title reset), a new undo log and tool
    /// context, a clean rolling conversation history, and an empty chat
    /// transcript. The human channel (ask dialogs) and the agent-event channel
    /// are reused, so the UI event loop keeps working untouched. Only applies
    /// while idle: a run in flight keeps the session it started with.
    fn new_session(&mut self) {
        if self.running {
            self.push_meta("cannot start a fresh session while a run is in flight");
            return;
        }
        let user = self.ctx_base.user.clone();
        let undo = Arc::new(comrade_core::MemoryUndo::new(self.root.clone()));
        let session = Arc::new(AgentSession::new(self.events_tx.clone()));
        let ctx_base = ToolContext {
            project_root: self.root.clone(),
            cwd: self.root.clone(),
            session: session.clone().as_control(),
            user,
            undo,
            auto_approve: self.cfg.auto_approve(),
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
        };
        self.session = session;
        self.ctx_base = ctx_base;
        self.history = Arc::new(tokio::sync::Mutex::new(build_session_context(
            &self.cfg,
            &self.root.to_string_lossy(),
            &self.tools,
        )));
        // Clear the transcript and everything derived from it; push_meta below
        // bumps chat_epoch so the row-layout cache is invalidated.
        self.chat.clear();
        self.section_collapsed.clear();
        self.chat_rows_cache = None;
        self.stream.clear();
        self.search = None;
        self.sel = None;
        self.scroll_top = 0;
        self.follow = true;
        self.was_at_bottom = true;
        self.ctx_tokens = 0;
        self.ctx_estimated = true;
        self.push_meta("started a fresh session");
    }

    /// True when approvals run without prompting: either the config autonomy
    /// is `auto` (`ctx_base.auto_approve`) or the user toggled ctrl-space.
    fn auto_mode_on(&self) -> bool {
        self.auto_accept || self.ctx_base.auto_approve
    }

    /// Store a freshly fetched repo snapshot.
    fn on_git(&mut self, info: GitBarInfo) {
        let backoff = if info.repo {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(10)
        };
        self.git_gate = Some(Instant::now() + backoff);
        self.git_inflight = false;
        self.git = info;
    }

    /// Kick a background `git` refresh, at most once per backoff window.
    fn refresh_git(&mut self) {
        if self.git_inflight {
            return;
        }
        if self.git_gate.is_some_and(|gate| Instant::now() < gate) {
            return;
        }
        self.git_inflight = true;
        let tx = self.git_tx.clone();
        let root = self.root.clone();
        tokio::spawn(async move {
            let info = fetch_git_bar(&root).await;
            let _ = tx.send(info).await;
        });
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::RunStart => {
                self.ctx_tokens = 0;
                self.ctx_estimated = true;
                self.activity = None;
            }
            AgentEvent::RunEnd => {
                self.running = false;
                self.stop = None;
                self.stream.clear();
                self.activity = None;
                // Compact whatever the run left behind (interrupted runs end
                // here without a final answer) before the status note.
                self.fold_completed();
                self.push_meta("run finished");
            }
            AgentEvent::User(u) => {
                self.stream.clear();
                self.fold_completed();
                self.push_msg(Msg::authored(MsgKind::User, "you", u));
            }
            AgentEvent::Delta(d) => {
                self.stream.push_str(&d);
                if self.stream.chars().count() > 40_000 {
                    self.stream = self
                        .stream
                        .chars()
                        .skip(self.stream.chars().count() - 40_000)
                        .collect();
                }
            }
            AgentEvent::ToolCall {
                name,
                args,
                justification,
                risk,
                tokens,
            } => {
                // This turn produced a tool call: keep whatever the model was
                // saying before the call as a visible reasoning block, then
                // show a compact card. Important cards (diffs, and
                // run_tests/run_task results) open by default.
                self.commit_stream_reasoning();
                self.activity = Some(name.clone());
                let open_default = matches!(
                    name.as_str(),
                    "apply_patch" | "apply_edit" | "run_tests" | "run_task"
                );
                self.push_msg(Msg::tool(ToolCard {
                    name,
                    author: Some(self.actor_label()),
                    args,
                    justification,
                    risk,
                    result: None,
                    ok: true,
                    open: open_default,
                    started: Some(std::time::Instant::now()),
                    taken_ms: None,
                    tokens,
                }));
            }
            AgentEvent::ToolStart { .. } => {}
            AgentEvent::ToolResult { name, output, ok } => {
                self.stream.clear();
                self.activity = None;
                if name == "delegate" {
                    self.on_delegate_result(&output, ok);
                } else if name == "run_tests" {
                    if output.contains("test result:") {
                        // Render a rich summary card + one collapsible block per
                        // failing test instead of a wall of text.
                        let summary = parse_test_summary(&output);
                        let fails = summary.failed;
                        let passed = summary.passed;
                        let duration = if summary.duration.is_empty() {
                            String::new()
                        } else {
                            format!(", {}", summary.duration)
                        };
                        if let Some(card) = self.last_tool_mut("run_tests") {
                            card.ok = fails == 0;
                            card.result =
                                Some(format!("{passed} passed, {fails} failed{duration}"));
                        }
                        for (test_name, detail) in summary.cases {
                            self.push_failure(test_name, detail);
                        }
                        if fails == 0 && passed > 0 {
                            self.push_meta(format!("all {passed} tests passed"));
                        }
                    } else if let Some(card) = self.last_tool_mut("run_tests") {
                        card.result = Some(output);
                        card.ok = ok;
                    }
                } else if let Some(card) = self.last_tool_mut(&name) {
                    card.result = Some(output);
                    card.ok = ok;
                }
                self.stamp_taken(&name);
            }
            AgentEvent::AssistantText(_) => {
                // The full assistant text is redundant with the `Delta` stream;
                // it is committed (as reasoning or as the final answer) when
                // the turn ends in a tool call or a final answer.
            }
            AgentEvent::Thought(t) => {
                // ReAct mode reports the isolated reasoning line: surface it as
                // a thinking block under the main model's name instead of
                // dropping it. The streamed turn text is its duplicate, so the
                // preview is cleared here.
                self.stream.clear();
                let t = t.trim();
                if !t.is_empty() {
                    self.push_reasoning(t.to_string());
                }
            }
            AgentEvent::FinalAnswer(a) => {
                self.stream.clear();
                self.activity = None;
                // The stretch of tools that produced this answer is done: fold
                // it to a digest so the answer reads cleanly above it.
                self.fold_completed();
                let visible = strip_react_scaffolding(&a);
                if !visible.trim().is_empty() {
                    let author = self.actor_label();
                    self.push_msg(Msg::authored(MsgKind::Assistant, author, visible));
                }
            }
            AgentEvent::Error(e) => {
                self.stream.clear();
                self.activity = None;
                self.push_meta(format!("error: {e}"));
            }
            AgentEvent::TitleChanged | AgentEvent::StatusChanged | AgentEvent::PlanChanged => {}
            AgentEvent::PlanFinished(s) => match s {
                Some(s) => self.push_meta(format!("plan finished: {s}")),
                None => self.push_meta("plan finished"),
            },
            AgentEvent::ContextStats {
                tokens,
                budget,
                estimated,
            } => {
                self.ctx_tokens = tokens;
                self.ctx_budget = budget.max(1);
                self.ctx_estimated = estimated;
            }
            AgentEvent::AccountBalance(balance) => {
                self.balance = Some(balance);
            }
            AgentEvent::DelegateToolCall { model, name, args } => {
                // A delegated sub-agent started a tool call: surface it as its
                // own card under the delegate's model name, so the chat shows
                // what the delegate is doing while the main model is parked
                // waiting on the hand-off.
                self.activity = Some(name.clone());
                let open_default = matches!(
                    name.as_str(),
                    "apply_patch" | "apply_edit" | "run_tests" | "run_task"
                );
                self.push_msg(Msg::tool(ToolCard {
                    name,
                    author: Some(model),
                    args,
                    justification: None,
                    risk: None,
                    result: None,
                    ok: true,
                    open: open_default,
                    started: Some(std::time::Instant::now()),
                    taken_ms: None,
                    tokens: None,
                }));
            }
            AgentEvent::DelegateToolResult {
                model,
                name,
                output,
                ok,
            } => {
                self.activity = None;
                if name == "run_tests" && output.contains("test result:") {
                    let summary = parse_test_summary(&output);
                    let fails = summary.failed;
                    let passed = summary.passed;
                    let duration = if summary.duration.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", summary.duration)
                    };
                    if let Some(card) = self.last_delegate_tool_mut(&name, &model) {
                        card.ok = fails == 0;
                        card.result = Some(format!("{passed} passed, {fails} failed{duration}"));
                    }
                    for (test_name, detail) in summary.cases {
                        self.push_failure(test_name, detail);
                    }
                    if fails == 0 && passed > 0 {
                        self.push_meta(format!("{model}: all {passed} tests passed"));
                    }
                } else if let Some(card) = self.last_delegate_tool_mut(&name, &model) {
                    card.result = Some(output);
                    card.ok = ok;
                }
                self.stamp_delegate_taken(&name, &model);
            }
        }
    }

    /// A `delegate` tool call finished: show the delegate's reply as its own
    /// chat entry under the delegate's model name. The parent's tool card keeps
    /// a compact summary; the full text lives in the delegate's message.
    fn on_delegate_result(&mut self, output: &str, ok: bool) {
        if ok {
            if let Some((model, reply)) = parse_delegate_reply(output) {
                if let Some(card) = self.last_tool_mut("delegate") {
                    card.ok = true;
                    card.open = false;
                    card.result = Some(format!("replied ({} chars)", reply.chars().count()));
                }
                if !reply.trim().is_empty() {
                    // The delegate hand-off stretch (reads + the delegate call)
                    // is done: fold it so the reply reads as a clean block.
                    self.fold_completed();
                    self.push_msg(Msg::authored(MsgKind::Delegate, model, reply));
                }
                return;
            }
        }
        // Unparseable or failed hand-off: keep the plain tool-card behaviour so
        // the error/raw text is still visible.
        if let Some(card) = self.last_tool_mut("delegate") {
            card.result = Some(output.to_string());
            card.ok = ok;
        }
    }

    fn answer_top(&mut self, reply: UserReply) {
        if !self.dialogs.is_empty() {
            let text = match &reply {
                UserReply::Answer(a) => format!("answer: {a}"),
                UserReply::Denied => "dismissed".to_string(),
            };
            self.push_msg(Msg::text(MsgKind::Meta, text));
            let d = self.dialogs.remove(0);
            let _ = d.reply.send(reply);
        }
    }

    /// Toggle auto-approve mode (Ctrl-Space): flips the flag, reports the new
    /// state in the chat, and accepts an already-waiting approval when turning
    /// on.
    fn toggle_auto_accept(&mut self) {
        self.auto_accept = !self.auto_accept;
        self.push_msg(Msg::text(
            MsgKind::Meta,
            if self.auto_accept {
                "auto-accept ON: approvals will be accepted automatically (ctrl-space to disable)"
                    .to_string()
            } else {
                "auto-accept off".to_string()
            },
        ));
        if self.auto_accept {
            self.accept_top_confirm();
        }
    }

    /// Execute an M-x command. Returns true when the app should quit.
    fn run_command(&mut self, cmd: MxCommand) -> bool {
        match cmd {
            MxCommand::AssignModel => self.open_model_pick(),
            MxCommand::BackwardKillWord => self.input.backspace_word(),
            MxCommand::BackwardWord => self.input.move_word_left(false),
            MxCommand::BeginningOfLine => self.input.move_home(false),
            MxCommand::CancelRun => self.cancel_run(),
            MxCommand::Copy => {
                // Mirrors Ctrl+Shift+C: copy the prompt's selection when there
                // is one, otherwise the chat message under the cursor.
                if let Some(sel) = self.input.selected_text() {
                    let sel = sel.to_string();
                    self.copy_text(&sel);
                } else {
                    self.copy_selected();
                }
            }
            MxCommand::EndOfLine => self.input.move_end(false),
            MxCommand::ForwardWord => self.input.move_word_right(false),
            MxCommand::InsertNewline => self.input.insert('\n'),
            MxCommand::KillWord => self.input.delete_word(),
            MxCommand::MoveBlockDown => self.move_block(1),
            MxCommand::MoveBlockUp => self.move_block(-1),
            MxCommand::MoveUserDown => self.move_user(1),
            MxCommand::MoveUserUp => self.move_user(-1),
            MxCommand::NewSession => self.new_session(),
            MxCommand::Quit => return true,
            MxCommand::ReloadConfig => self.reload_config(),
            MxCommand::SearchChat => self.search = Some(Search::new()),
            MxCommand::SubmitPrompt => {
                if !self.running {
                    let prompt = self.input.take_text();
                    self.start_run(prompt);
                }
            }
            MxCommand::ToggleAutoAccept => self.toggle_auto_accept(),
            MxCommand::ToggleToolCard => {
                if let Some(idx) = self.sel {
                    self.toggle_tool(idx);
                }
            }
        }
        false
    }

    /// Auto-accept the confirmation currently on top of the dialog stack
    /// (used when the mode is turned on while an approval is waiting).
    fn accept_top_confirm(&mut self) {
        let is_confirm = self
            .dialogs
            .first()
            .is_some_and(|d| matches!(d.prompt, UserPrompt::Confirm { .. }));
        if !is_confirm || self.dialog_ask {
            return;
        }
        if let Some(d) = self.dialogs.first() {
            if let UserPrompt::Confirm { title, .. } = &d.prompt {
                let action = one_line(title, 80);
                self.push_msg(Msg::text(
                    MsgKind::Meta,
                    format!("auto-accept on → approved: {action}"),
                ));
            }
        }
        self.answer_top(UserReply::Answer("yes".into()));
    }

    /// Send the buffered text to the model as a follow-up question about the
    /// action pending in the top dialog, then wait for its answer (rendered
    /// inside the dialog) before the human confirms or denies.
    fn ask_followup(&mut self) {
        let question = match self.dialogs.first_mut() {
            Some(d) => std::mem::take(&mut d.buf).trim().to_string(),
            None => String::new(),
        };
        if question.is_empty() {
            self.dialog_ask = false;
            return;
        }
        let (summary, body) = match self.dialogs.first() {
            Some(d) => match &d.prompt {
                UserPrompt::Confirm { title, diff } => {
                    (title.clone(), diff.clone().unwrap_or_default())
                }
                _ => (String::new(), String::new()),
            },
            None => (String::new(), String::new()),
        };
        self.dialog_ask = false;
        self.dialog_conv.push(format!("Q: {question}"));
        let Some(tx) = self.dialog_ask_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let system = "You are helping a human review a proposed action inside \
                a command-line agent before they approve it. Answer the human's \
                follow-up question about the pending action briefly and factually, \
                quoting the exact command or files involved. Never run anything.";
            let user = format!(
                "PENDING ACTION:\n{summary}\n\nDETAILS:\n{body}\n\nHUMAN QUESTION:\n{question}"
            );
            let messages = [
                ChatMessage::new(Role::System, system.to_string()),
                ChatMessage::new(Role::User, user),
            ];
            let answer = match client.chat(&messages).await {
                Ok(text) => text.trim().to_string(),
                Err(e) => format!("error asking model: {e:#}"),
            };
            let _ = tx.send(answer);
        });
    }

    fn answer_from_buf(&mut self) {
        let can_submit = self.dialogs.first().is_some_and(|d| match &d.prompt {
            UserPrompt::Question { .. } => !d.buf.trim().is_empty(),
            _ => true,
        });
        if !can_submit {
            return;
        }
        let reply = match self.dialogs.first_mut() {
            Some(d) => UserReply::Answer(std::mem::take(&mut d.buf)),
            None => return,
        };
        self.answer_top(reply);
    }

    fn pick_option(&mut self, n: usize) {
        let pick = match self.dialogs.first() {
            Some(d) => match &d.prompt {
                UserPrompt::Question { options, .. } if n >= 1 && n <= options.len() => {
                    Some(options[n - 1].clone())
                }
                _ => None,
            },
            None => None,
        };
        if let Some(p) = pick {
            self.answer_top(UserReply::Answer(p));
        }
    }

    /// Open the Ctrl-A overlay: offer every plan step that is still pending or
    /// blocked (a step a model is working on, or already done, keeps its model)
    /// plus the candidate models: "self" and each configured delegate.
    fn open_model_pick(&mut self) {
        if self.pick.is_some() || !self.dialogs.is_empty() {
            return;
        }
        let steps: Vec<PickStep> = self
            .session
            .plan()
            .into_iter()
            .filter(|s| !matches!(s.status, PlanStatus::Done | PlanStatus::InProgress))
            .map(|s| PickStep {
                id: s.id,
                goal: s.goal,
                model: s.model,
            })
            .collect();
        if steps.is_empty() {
            self.push_meta(
                "no plan step can be reassigned right now: every step is in progress or done. \
                 Ctrl-A assigns a model to a pending or blocked step.",
            );
            return;
        }
        let mut models = vec![AGENT_MODEL.to_string()];
        models.extend(self.cfg.delegates.iter().map(|d| d.name.clone()));
        self.pick = Some(ModelPick {
            steps,
            sel: 0,
            models,
            model_sel: None,
        });
    }

    /// Move the Ctrl-A cursor (`dir` = -1/1). In step phase it walks the plan
    /// steps; once a model column is open it walks the candidate models.
    fn pick_nav(&mut self, dir: isize) {
        let Some(p) = &mut self.pick else {
            return;
        };
        let len = if p.model_sel.is_some() {
            p.models.len()
        } else {
            p.steps.len()
        };
        if len == 0 {
            return;
        }
        let cur = if p.model_sel.is_some() {
            p.model_sel.unwrap_or(0) as isize
        } else {
            p.sel as isize
        };
        let next = (cur + dir).rem_euclid(len as isize) as usize;
        if p.model_sel.is_some() {
            p.model_sel = Some(next);
        } else {
            p.sel = next;
        }
    }

    /// Apply the chosen model to the chosen step via the session, then close
    /// the overlay. Guard errors (e.g. a delegate started the step meanwhile)
    /// surface as a chat Meta message.
    fn apply_pick(&mut self) {
        let Some(p) = &self.pick else {
            return;
        };
        let step = p.steps[p.sel.min(p.steps.len().saturating_sub(1))].id;
        let Some(model_idx) = p.model_sel else {
            return;
        };
        let Some(model) = p.models.get(model_idx) else {
            return;
        };
        let (step_id, model) = (step, model.clone());
        self.pick = None;
        match self
            .session
            .reassign_step_model(&PlanTarget::Id(step_id), &model)
        {
            Ok(true) => self.push_meta(format!(
                "plan step {step_id} now assigned to model {model:?}"
            )),
            Ok(false) => self.push_meta(format!("plan step {step_id} no longer exists")),
            Err(why) => self.push_meta(format!("cannot reassign plan step {step_id}: {why}")),
        }
    }

    /// Keys while the Ctrl-A overlay is open.
    fn handle_pick_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.pick = None,
            KeyCode::Char('a') if ctrl => self.pick = None,
            // Emacs-style movement: ctrl-p previous, ctrl-n next (arrows work too).
            KeyCode::Up => self.pick_nav(-1),
            KeyCode::Down => self.pick_nav(1),
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'p') => self.pick_nav(-1),
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'n') => self.pick_nav(1),
            KeyCode::Right | KeyCode::Enter => {
                // Open the model column on the currently selected step.
                if self
                    .pick
                    .as_ref()
                    .is_some_and(|p| p.model_sel.is_none() && !p.models.is_empty())
                {
                    if let Some(p) = &mut self.pick {
                        p.model_sel = Some(0);
                    }
                } else if self.pick.as_ref().is_some_and(|p| p.model_sel.is_some()) {
                    // Enter confirms a highlighted model.
                    self.apply_pick();
                }
            }
            KeyCode::Left | KeyCode::Backspace => {
                if let Some(p) = &mut self.pick {
                    if p.model_sel.is_some() {
                        p.model_sel = None;
                    }
                }
            }
            KeyCode::Char(c)
                if !ctrl && self.pick.as_ref().is_some_and(|p| p.model_sel.is_some()) =>
            {
                if let Some(d) = c.to_digit(10) {
                    if d >= 1 {
                        self.apply_pick_to_index(d as usize - 1);
                    }
                }
            }
            _ => {}
        }
    }

    /// Digits 1..=N pick a model directly (single keypress, no Enter).
    fn apply_pick_to_index(&mut self, model_idx: usize) {
        let Some(p) = &self.pick else {
            return;
        };
        let Some(_) = p.models.get(model_idx) else {
            return;
        };
        if let Some(p) = &mut self.pick {
            p.model_sel = Some(model_idx);
        }
        self.apply_pick();
    }
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

pub async fn run(deps: &Deps) -> Result<()> {
    let (asks_tx, asks_rx) = mpsc::channel::<PendingAsk>(16);
    let (dialog_ans_tx, mut dialog_ans_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (git_tx, git_rx) = mpsc::channel::<GitBarInfo>(4);
    let user = Arc::new(TuiUserIo { tx: asks_tx });
    let (bundle, events_tx, events_rx) = new_session(deps, user);

    let mut app = App {
        cfg: deps.cfg.clone(),
        client: deps.client.clone(),
        tools: deps.tools.clone(),
        root: deps.root.clone(),
        config_source: deps.config_source.clone(),
        auto_forced: deps.auto_forced,
        session: bundle.session.clone(),
        ctx_base: bundle.ctx_base.clone(),
        history: Arc::new(tokio::sync::Mutex::new(build_session_context(
            &deps.cfg,
            &deps.root.to_string_lossy(),
            &deps.tools,
        ))),
        events_tx,
        events_rx,
        asks_rx,
        stop: None,
        running: false,
        auto_accept: false,
        git: GitBarInfo::default(),
        git_rx,
        git_tx,
        git_inflight: false,
        git_gate: None,
        chat: Vec::new(),
        chat_epoch: 0,
        chat_rows_cache: None,
        section_collapsed: Vec::new(),
        stream: String::new(),
        input: Editor::new(),
        search: None,
        dialogs: Vec::new(),
        dialog_ask: false,
        dialog_ask_tx: None,
        dialog_conv: Vec::new(),
        pick: None,
        mx: None,
        chat_rect: Rect::default(),
        row_targets: Vec::new(),
        row_msg: Vec::new(),
        msg_ranges: Vec::new(),
        view_rows: 0,
        sel: None,
        scroll_top: 0,
        follow: true,
        was_at_bottom: true,
        ctx_tokens: 0,
        ctx_budget: deps
            .cfg
            .llm
            .context_window
            .unwrap_or(deps.cfg.context.budget_tokens),
        ctx_estimated: true,
        balance: deps.balance.clone(),
        activity: None,
    };
    app.dialog_ask_tx = Some(dialog_ans_tx);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);

    // Terminal events arrive on a background thread.
    let (kev_tx, mut kev_rx) = mpsc::channel::<Event>(128);
    std::thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(100)).ok() != Some(true) {
                continue;
            }
            if let Ok(ev) = event::read() {
                if kev_tx.blocking_send(ev).is_err() {
                    break;
                }
            }
        }
    });

    // First frame immediately, so the UI is visible before any input.
    let _ = terminal.draw(|f| draw(&mut app, f));
    app.refresh_git();

    // A periodic wake-up while a run is in flight, so the plan window's spinner
    // keeps rotating even when no agent or terminal events arrive (e.g. during
    // a silent, long-running tool call). Idle frames never poll it (guard), and
    // a wake-up only reaches the shared redraw below the select.
    let mut spin = tokio::time::interval(Duration::from_millis(100));
    spin.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let res = loop {
        tokio::select! {
            ev = kev_rx.recv() => {
                match ev {
                    Some(ev) => if handle_event(&mut app, ev) { break Ok(()); },
                    None => break Err(anyhow::anyhow!("terminal closed")),
                }
            }
            ae = app.events_rx.recv() => {
                match ae {
                    Some(e) => app.on_agent_event(e),
                    None => break Err(anyhow::anyhow!("agent event channel closed")),
                }
            }
            ask = app.asks_rx.recv() => {
                match ask {
                    Some(ask) => {
                        // Auto-accept mode answers approvals immediately.
                        let is_confirm =
                            matches!(ask.prompt, UserPrompt::Confirm { .. });
                        if app.auto_accept && is_confirm {
                            let action = match &ask.prompt {
                                UserPrompt::Confirm { title, .. } => one_line(title, 80),
                                _ => String::new(),
                            };
                            let _ = ask.reply.send(UserReply::Answer("yes".into()));
                            app.push_msg(Msg::text(
                                MsgKind::Meta,
                                if action.is_empty() {
                                    "auto-accept: approved".to_string()
                                } else {
                                    format!("auto-accept → approved: {action}")
                                },
                            ));
                        } else {
                            app.dialogs.push(Dialog { prompt: ask.prompt, buf: String::new(), reply: ask.reply });
                            app.dialog_ask = false;
                            app.dialog_conv.clear();
                            app.push_meta("waiting for your input");
                        }
                    }
                    None => break Err(anyhow::anyhow!("ask channel closed")),
                }
            }
            ans = dialog_ans_rx.recv() => {
                if let Some(answer) = ans {
                    app.dialog_conv.push(format!("A: {answer}"));
                }
            }
            git = app.git_rx.recv() => {
                match git {
                    Some(info) => app.on_git(info),
                    None => break Err(anyhow::anyhow!("git refresh channel closed")),
                }
            }
            _ = spin.tick(), if app.running => {
                // Spinner wake-up only: the redraw below re-renders the plan
                // panel with the next glyph frame.
            }
        }
        app.refresh_git();
        let _ = terminal.draw(|f| draw(&mut app, f));
    };

    let _ = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    res
}

/// Returns true when the app should quit.
fn handle_event(app: &mut App, ev: Event) -> bool {
    match ev {
        Event::Key(key) => {
            let ctrl_space = key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char(' ') | KeyCode::Char('\0'));
            if ctrl_space {
                app.toggle_auto_accept();
                return false;
            }
            if let KeyCode::Char(ch) = key.code {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl && ch.eq_ignore_ascii_case(&'c') {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        // Ctrl+Shift+C copies the prompt's text selection when
                        // one exists, otherwise the chat message under the
                        // cursor, to the system clipboard.
                        if let Some(sel) = app.input.selected_text() {
                            let sel = sel.to_string();
                            app.copy_text(&sel);
                        } else {
                            app.copy_selected();
                        }
                        return false;
                    }
                    // Plain Ctrl+C quits.
                    return true;
                }
            }
            if app.pick.is_some() {
                app.handle_pick_key(key);
                return false;
            }
            if app.search.is_some() {
                return handle_search_key(app, key);
            }
            if !app.dialogs.is_empty() {
                return handle_dialog_key(app, key.code);
            }
            // The M-x command palette, when open.
            if app.mx.is_some() {
                match handle_mx_key(app, key) {
                    MxKeyOutcome::Handled => return false,
                    MxKeyOutcome::Quit => return true,
                    // Closed without consuming the key: fall through so e.g.
                    // Ctrl-S pressed while the palette is open still searches.
                    MxKeyOutcome::Closed => {}
                }
            }
            // Ctrl-A opens the "assign a model to a plan step" overlay.
            if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.open_model_pick();
                return false;
            }
            // Ctrl-S opens the chat-history search.
            if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.search = Some(Search::new());
                return false;
            }
            // Ctrl-R re-reads the config file without restarting.
            if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.reload_config();
                return false;
            }
            // Emacs-style chat navigation. Plain Ctrl+p/n move block to block;
            // Ctrl+Shift and Alt variants (P/N) jump between user messages.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if let KeyCode::Char(ch) = key.code {
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                    let ctrl_p = ch.eq_ignore_ascii_case(&'p');
                    let ctrl_n = ch.eq_ignore_ascii_case(&'n');
                    if ctrl_p && shift {
                        app.move_user(-1);
                        return false;
                    }
                    if ctrl_n && shift {
                        app.move_user(1);
                        return false;
                    }
                    if ctrl_p {
                        app.move_block(-1);
                        return false;
                    }
                    if ctrl_n {
                        app.move_block(1);
                        return false;
                    }
                }
            }
            if key.modifiers.contains(KeyModifiers::ALT) {
                if let KeyCode::Char(ch) = key.code {
                    if ch.eq_ignore_ascii_case(&'x') {
                        // M-x: open the command palette.
                        app.mx = Some(Mx::open());
                        return false;
                    }
                    if ch.eq_ignore_ascii_case(&'p') {
                        app.move_user(-1);
                        return false;
                    }
                    if ch.eq_ignore_ascii_case(&'n') {
                        app.move_user(1);
                        return false;
                    }
                }
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            match key.code {
                KeyCode::Esc => {
                    if app.running {
                        app.cancel_run();
                    }
                }
                KeyCode::Enter => {
                    if shift {
                        // Shift+Enter inserts a newline instead of submitting.
                        app.input.insert('\n');
                    } else if !app.running {
                        let prompt = app.input.take_text();
                        app.start_run(prompt);
                    }
                }
                KeyCode::Tab => {
                    // Toggle the selected block (the one under the "> " marker).
                    if let Some(idx) = app.sel {
                        app.toggle_tool(idx);
                    }
                }
                KeyCode::Char(c) if !ctrl && !alt => app.input.insert(c),
                KeyCode::Backspace => {
                    if alt {
                        app.input.backspace_word();
                    } else {
                        app.input.backspace();
                    }
                }
                KeyCode::Delete => {
                    if alt {
                        app.input.delete_word();
                    } else {
                        app.input.delete();
                    }
                }
                KeyCode::Left if alt => app.input.move_word_left(shift),
                KeyCode::Left => app.input.move_left(shift),
                KeyCode::Right if alt => app.input.move_word_right(shift),
                KeyCode::Right => app.input.move_right(shift),
                KeyCode::Home => app.input.move_home(shift),
                KeyCode::End => app.input.move_end(shift),
                _ => {}
            }
            false
        }
        Event::Mouse(mouse) => {
            handle_mouse(app, mouse);
            false
        }
        _ => false,
    }
}

/// Keys while the Ctrl-S search bar is active. Returns true when the app should quit.
fn handle_search_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        // Close the search bar (Esc or Ctrl-S).
        KeyCode::Esc => app.search = None,
        KeyCode::Char('s') if ctrl => app.search = None,
        // Cycle through matches: enter/↓ next, shift-enter/↑ previous.
        KeyCode::Enter | KeyCode::Down | KeyCode::PageDown if !shift => app.step_search(1),
        KeyCode::Enter | KeyCode::Up | KeyCode::PageUp if shift => app.step_search(-1),
        KeyCode::Up | KeyCode::PageUp => app.step_search(-1),
        // Edit the query: incremental, jumps to the first match live.
        KeyCode::Char(c) if !ctrl && !alt => {
            if let Some(s) = &mut app.search {
                s.query.push(c);
            }
            app.refresh_search();
        }
        KeyCode::Backspace if !ctrl => {
            if let Some(s) = &mut app.search {
                s.query.pop();
            }
            app.refresh_search();
        }
        _ => {}
    }
    false
}

/// Result of feeding one key to the M-x palette.
enum MxKeyOutcome {
    /// The palette consumed the key (it stays open or dismissed itself).
    Handled,
    /// The palette ran `quit`; the whole app should exit.
    Quit,
    /// The palette dismissed itself without consuming the key, so the caller
    /// should dispatch the key normally (e.g. Ctrl-S pressed mid-typing).
    Closed,
}

/// Keys while the M-x command palette is open.
fn handle_mx_key(app: &mut App, key: KeyEvent) -> MxKeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // After a command ran, the row shows the "you can run this command with
    // <keys>" hint; the next key dismisses it (Esc/C-g also close).
    if app.mx.as_ref().is_some_and(|m| m.done.is_some()) {
        app.mx = None;
        return MxKeyOutcome::Handled;
    }
    match key.code {
        KeyCode::Esc => {
            app.mx = None;
            MxKeyOutcome::Handled
        }
        // C-g cancels the minibuffer (Emacs), as does M-x itself.
        KeyCode::Char('g') if ctrl => {
            app.mx = None;
            MxKeyOutcome::Handled
        }
        KeyCode::Char('x') if alt => {
            app.mx = None;
            MxKeyOutcome::Handled
        }
        KeyCode::Enter => {
            let cmd = app.mx.as_ref().and_then(|m| m.matches.get(m.sel)).copied();
            app.mx = None;
            match cmd {
                Some(cmd) => {
                    if app.run_command(cmd) {
                        return MxKeyOutcome::Quit;
                    }
                    // Commands that opened their own overlay (search, model
                    // pick) take over the screen; the hint would just sit in
                    // front of them, so close the palette instead.
                    let overlay_open = app.search.is_some() || app.pick.is_some();
                    match (overlay_open, cmd.keys()) {
                        (false, Some(keys)) => {
                            // Stay open in hint mode: the row tells the user
                            // how to run the command directly next time.
                            app.mx = Some(Mx {
                                query: String::new(),
                                matches: Vec::new(),
                                sel: 0,
                                done: Some(format!("you can run this command with {keys}")),
                            });
                        }
                        _ => {}
                    }
                }
                // Nothing matched: stay in the palette so the user can edit.
                None => app.mx = Some(Mx::open()),
            }
            MxKeyOutcome::Handled
        }
        KeyCode::Up => {
            if let Some(m) = &mut app.mx {
                m.step(-1);
            }
            MxKeyOutcome::Handled
        }
        KeyCode::Down => {
            if let Some(m) = &mut app.mx {
                m.step(1);
            }
            MxKeyOutcome::Handled
        }
        KeyCode::Char('p') if ctrl && !alt => {
            if let Some(m) = &mut app.mx {
                m.step(-1);
            }
            MxKeyOutcome::Handled
        }
        KeyCode::Char('n') if ctrl && !alt => {
            if let Some(m) = &mut app.mx {
                m.step(1);
            }
            MxKeyOutcome::Handled
        }
        // Tab completes emacs-style: with nothing typed it leaves the query
        // alone (the full task list stays up); with a shared prefix it fills
        // in as much as every match agrees on.
        KeyCode::Tab => {
            if let Some(m) = &mut app.mx {
                m.complete();
            }
            MxKeyOutcome::Handled
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            if let Some(m) = &mut app.mx {
                m.query.push(c);
                m.refresh();
            }
            MxKeyOutcome::Handled
        }
        KeyCode::Backspace if !ctrl => {
            if let Some(m) = &mut app.mx {
                m.query.pop();
                m.refresh();
            }
            MxKeyOutcome::Handled
        }
        // Anything else (other Ctrl/Alt chords) dismisses the palette and lets
        // the key do its normal job.
        _ => {
            app.mx = None;
            MxKeyOutcome::Closed
        }
    }
}

fn handle_dialog_key(app: &mut App, code: KeyCode) -> bool {
    let is_confirm = matches!(
        app.dialogs.first().unwrap().prompt,
        UserPrompt::Confirm { .. }
    );
    match code {
        KeyCode::Esc => {
            if app.dialog_ask {
                // Leave question mode; the confirmation is still open.
                app.dialog_ask = false;
            } else {
                app.answer_top(UserReply::Denied);
            }
        }
        KeyCode::Char('?') if is_confirm && !app.dialog_ask => {
            // Ask the model a follow-up question before deciding.
            app.dialog_ask = true;
            if let Some(d) = app.dialogs.first_mut() {
                d.buf.clear();
            }
        }
        KeyCode::Char('y') if is_confirm && !app.dialog_ask => {
            app.answer_top(UserReply::Answer("yes".into()))
        }
        KeyCode::Char('n') if is_confirm && !app.dialog_ask => {
            app.answer_top(UserReply::Answer("no".into()))
        }
        KeyCode::Enter => {
            if app.dialog_ask {
                app.ask_followup();
            } else {
                app.answer_from_buf();
            }
        }
        KeyCode::Char(c) if ('1'..='9').contains(&c) && !app.dialog_ask => {
            let n = c.to_digit(10).unwrap_or(0) as usize;
            let handled = app.dialogs.first().is_some_and(|d| {
                matches!(&d.prompt, UserPrompt::Question { options, .. } if n >= 1 && n <= options.len())
            });
            if handled {
                app.pick_option(n);
            } else if !is_confirm {
                app.dialogs.first_mut().unwrap().buf.push(c);
            }
        }
        KeyCode::Char(c) => {
            if let Some(d) = app.dialogs.first_mut() {
                d.buf.push(c);
            }
        }
        KeyCode::Backspace => {
            if let Some(d) = app.dialogs.first_mut() {
                d.buf.pop();
            }
        }
        _ => {}
    }
    false
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let inside = mouse.kind != MouseEventKind::Moved
        && mouse.column >= app.chat_rect.left()
        && mouse.column < app.chat_rect.right()
        && mouse.row >= app.chat_rect.top()
        && mouse.row < app.chat_rect.bottom();
    if !inside {
        return;
    }
    let y = (mouse.row - app.chat_rect.top()) as usize;
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_top = app.scroll_top.saturating_sub(3);
            app.follow = false;
            app.was_at_bottom = false;
        }
        MouseEventKind::ScrollDown => {
            app.scroll_top += 3;
            app.follow = false;
            app.was_at_bottom = false;
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let row = y + app.scroll_top;
            if let Some(Some(idx)) = app.row_msg.get(row) {
                app.sel = Some(*idx);
            }
            if let Some(Some(idx)) = app.row_targets.get(row) {
                app.toggle_tool(*idx);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// navigation math (pure)
// ---------------------------------------------------------------------------

/// Total number of org-style sections = the number of user turns.
fn section_count(chat: &[Msg]) -> usize {
    chat.iter().filter(|m| m.kind == MsgKind::User).count()
}

/// The section any chat message belongs to: `(ordinal, its user-turn index)`.
/// Every message after a user turn up to (not including) the next belongs to
/// that turn's section. Messages before the first user turn have no section.
fn section_of_msg(chat: &[Msg], idx: usize) -> Option<(usize, usize)> {
    if idx >= chat.len() {
        return None;
    }
    let mut n_users = 0;
    let mut last: Option<(usize, usize)> = None;
    for (i, m) in chat[..=idx].iter().enumerate() {
        if m.kind == MsgKind::User {
            last = Some((n_users, i));
            n_users += 1;
        }
    }
    last
}

/// Whether a section may be collapsed right now. The last (live) section of a
/// run that is still in flight must stay visible, so it is exempt.
fn section_collapsible(running: bool, ord: usize, count: usize) -> bool {
    !running || ord + 1 < count
}

/// Collapse state of the section with the given ordinal.
fn collapsed_at(collapsed: &[bool], ord: usize) -> bool {
    collapsed.get(ord).copied().unwrap_or(false)
}

fn set_collapsed_at(collapsed: &mut Vec<bool>, ord: usize, val: bool) {
    if ord >= collapsed.len() {
        collapsed.resize(ord + 1, false);
    }
    collapsed[ord] = val;
}

/// Expand the section a chat message belongs to (no-op when not collapsed).
fn expand_section(collapsed: &mut Vec<bool>, chat: &[Msg], msg_idx: usize) {
    if let Some((ord, _)) = section_of_msg(chat, msg_idx) {
        set_collapsed_at(collapsed, ord, false);
    }
}

/// Toggle the section a chat message belongs to (no-op outside a section).
/// The live last section of an in-flight run cannot be collapsed.
fn toggle_section(collapsed: &mut Vec<bool>, chat: &[Msg], running: bool, msg_idx: usize) {
    let Some((ord, _)) = section_of_msg(chat, msg_idx) else {
        return;
    };
    if collapsed_at(collapsed, ord) {
        set_collapsed_at(collapsed, ord, false);
    } else if section_collapsible(running, ord, section_count(chat)) {
        set_collapsed_at(collapsed, ord, true);
    }
}

/// Whether a chat message is currently exposed: a user-turn heading always is
/// (it is its own section's header); every other message shows only while its
/// section is expanded. Content before the first user turn is exposed.
fn chat_visible(chat: &[Msg], collapsed: &[bool], idx: usize) -> bool {
    match chat.get(idx).map(|m| m.kind) {
        Some(MsgKind::User) => true,
        Some(_) => section_of_msg(chat, idx).is_none_or(|(o, _)| !collapsed_at(collapsed, o)),
        None => false,
    }
}

/// Next/previous visible chat block from the current selection, skipping the
/// messages hidden inside a collapsed section. Returns None at the ends.
fn step_visible(chat: &[Msg], collapsed: &[bool], sel: Option<usize>, dir: isize) -> Option<usize> {
    if chat.is_empty() {
        return None;
    }
    let len = chat.len() as isize;
    let mut cur: isize = match sel {
        Some(s) => s as isize,
        // No selection: start just past the edge the step moves toward.
        None if dir < 0 => len,
        None => -1,
    };
    loop {
        cur += dir;
        if cur < 0 || cur >= len {
            return None;
        }
        if chat_visible(chat, collapsed, cur as usize) {
            return Some(cur as usize);
        }
    }
}

/// Next/previous user-message index given the user positions and selection.
fn step_user(users: &[usize], sel: Option<usize>, dir: isize) -> Option<usize> {
    if users.is_empty() {
        return None;
    }
    let Some(anchor) = sel else {
        return if dir < 0 {
            users.last().copied()
        } else {
            users.first().copied()
        };
    };
    let anchor = anchor as isize;
    if dir < 0 {
        users
            .iter()
            .rev()
            .find(|&&i| (i as isize) < anchor)
            .copied()
    } else {
        users.iter().find(|&&i| (i as isize) > anchor).copied()
    }
}

/// Step the search cursor by `dir` wrapping inside `(cur, len)`. Returns None
/// when there are no matches.
fn search_step(state: Option<(usize, usize)>, dir: isize) -> Option<usize> {
    let (cur, len) = state?;
    if len == 0 {
        return None;
    }
    Some((cur as isize + dir).rem_euclid(len as isize) as usize)
}

/// Lowercased, searchable text of a message (chat body + tool/failure cards).
/// A folded [`MsgKind::Run`] digest exposes every child, so search still finds
/// text inside a collapsed run and copy_selected copies the whole stretch.
fn msg_searchable(msg: &Msg) -> String {
    let mut s = msg.text.clone();
    if let Some(t) = &msg.tool {
        s.push_str(&format!(
            "\n{}\n{}\n{}\n{}\n{}",
            t.name,
            t.args,
            t.justification.as_deref().unwrap_or(""),
            t.risk.as_deref().unwrap_or(""),
            t.result.as_deref().unwrap_or(""),
        ));
    }
    if let Some(f) = &msg.fail {
        s.push_str(&format!("\n{}\n{}", f.name, f.detail));
    }
    for c in &msg.children {
        s.push('\n');
        s.push_str(&msg_searchable(c));
    }
    s
}

/// Whether a message matches the (case-insensitive) query.
fn msg_matches(msg: &Msg, query: &str) -> bool {
    !query.is_empty()
        && msg_searchable(msg)
            .to_lowercase()
            .contains(&query.to_lowercase())
}

// ---------------------------------------------------------------------------
// run_tests output parsing
// ---------------------------------------------------------------------------
/// Parse a `run_tests` summary: counts + duration + one (name, detail) per
/// failing test (from `---- <name> stdout ----` sections).
fn parse_test_summary(text: &str) -> TestSummary {
    let mut summary = TestSummary::default();
    let mut current: Option<(String, Vec<String>)> = None;

    let flush = |cases: &mut Vec<(String, String)>, cur: &mut Option<(String, Vec<String>)>| {
        if let Some((name, detail)) = cur.take() {
            cases.push((name, detail.join("\n")));
        }
    };

    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with("test result:") {
            summary.passed = number_before(raw, " passed").unwrap_or(summary.passed);
            summary.failed = number_before(raw, " failed").unwrap_or(summary.failed);
            if let Some(d) = after(raw, "finished in ") {
                let d = d.trim_end_matches(' ').to_string();
                summary.duration = d;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("---- ") {
            if let Some(name) = rest.strip_suffix(" stdout ----") {
                flush(&mut summary.cases, &mut current);
                current = Some((name.to_string(), Vec::new()));
                continue;
            }
            // a closing "---- name ----" or other separator: just flush
            flush(&mut summary.cases, &mut current);
            continue;
        }
        if let Some((_, detail)) = current.as_mut() {
            let t = trimmed;
            if t.starts_with("note:") || t.is_empty() {
                continue;
            }
            detail.push(raw.to_string());
        }
    }
    flush(&mut summary.cases, &mut current);
    summary
}

fn number_before(line: &str, needle: &str) -> Option<usize> {
    let i = line.find(needle)?;
    let digits: String = line[..i]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

fn after<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let i = line.find(needle)? + needle.len();
    Some(line[i..].trim())
}

// ---------------------------------------------------------------------------
// chat rendering / markdown
// ---------------------------------------------------------------------------

/// Path shown leftmost on the mode line: `~`-abbreviated when `root` sits
/// under `$HOME` (Emacs-style), absolute otherwise.
fn home_path(root: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    home_path_with(root, home.as_deref())
}

/// [`home_path`] with the home directory passed in, so it can be tested
/// without touching process-global env vars.
fn home_path_with(root: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if let Some(home) = home {
        if let Ok(rel) = root.strip_prefix(home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.to_string_lossy());
        }
    }
    root.to_string_lossy().into_owned()
}

fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Pre-wrap the prompt text so the row reserved for it can grow with the
    // content (search mode replaces the prompt with a fixed single row).
    let (prompt_rows, prompt_win, prompt_cur) = prompt_view(app, area.width);
    let prompt_h = if app.search.is_some() || app.mx.is_some() {
        1
    } else {
        prompt_rows.len().clamp(1, PROMPT_MAX_ROWS)
    };

    // No top bar: the mode line is the single status row (Emacs-style), so the
    // chat gets the whole height minus the prompt and the bottom bar.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(prompt_h as u16),
            Constraint::Length(1),
        ])
        .split(area);

    let run_state = if app.running { "RUNNING" } else { "IDLE" };

    let (status_msg, status_color) = {
        let agent = app.session.status();
        if !agent.trim().is_empty() {
            (agent, Color::White)
        } else if app.running {
            match &app.activity {
                Some(name) => (format!("running {name}"), Color::Magenta),
                None => ("working...".to_string(), Color::Green),
            }
        } else {
            (String::new(), Color::DarkGray)
        }
    };

    // ---- Emacs-style mode line (bottom row) -----------------------------
    // Left: repo (branch + inserted/removed lines, added/deleted files),
    // agent state (IDLE/RUNNING) and mode (auto/ask). The whole bar turns a
    // warm orange while auto-approve is active so the "changes land without
    // asking" mode reads as a warning. The app name sits on the far right
    // when the terminal is wide enough.
    let auto = app.auto_mode_on();
    let on_auto = |fg: Color| -> Color { if auto { Color::Black } else { fg } };
    let bar_style = Style::default()
        .bg(if auto { AUTO_BAR_BG } else { Color::Blue })
        .fg(if auto { Color::Black } else { Color::White });

    let mut spans: Vec<Span> = Vec::new();
    if app.git.repo {
        if let Some(branch) = &app.git.branch {
            spans.push(Span::styled(
                branch.clone(),
                bar_style.add_modifier(Modifier::BOLD),
            ));
        }
        let git = &app.git;
        if git.ins > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("+{}", git.ins),
                bar_style.fg(on_auto(Color::LightGreen)),
            ));
        }
        if git.del > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("-{}", git.del),
                bar_style.fg(on_auto(Color::LightRed)),
            ));
        }
        if git.added_files > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("+{}f", git.added_files),
                bar_style.fg(on_auto(Color::LightGreen)),
            ));
        }
        if git.deleted_files > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("-{}f", git.deleted_files),
                bar_style.fg(on_auto(Color::LightRed)),
            ));
        }
    }
    if !spans.is_empty() {
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
        run_state,
        bar_style
            .fg(if app.running {
                on_auto(Color::LightGreen)
            } else {
                on_auto(Color::Yellow)
            })
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        if auto { "auto" } else { "ask" },
        bar_style.add_modifier(Modifier::BOLD),
    ));
    if !status_msg.trim().is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            status_msg,
            bar_style
                .fg(on_auto(status_color))
                .add_modifier(Modifier::ITALIC),
        ));
    }

    // The bar spans the whole row; the app name is right-aligned in its own
    // segment and drops first on narrow terminals.
    let tag_w = (APP_TAG.chars().count() as u16).min(rows[2].width.saturating_sub(60));
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(tag_w)])
        .split(rows[2]);
    // Current directory goes leftmost on the mode line, abbreviated to ~/...
    // when it lives under $HOME (absolute otherwise).
    spans.insert(
        0,
        Span::styled(home_path(&app.root), bar_style.add_modifier(Modifier::BOLD)),
    );
    spans.insert(1, Span::raw(" "));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(bar_style),
        bottom[0],
    );
    if tag_w > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(APP_TAG))
                .style(bar_style.add_modifier(Modifier::BOLD))
                .alignment(Alignment::Right),
            bottom[1],
        );
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Percentage(30)])
        .split(rows[0]);
    draw_chat(app, frame, cols[0]);
    // The model panel shows three fixed rows (label, gauge, usage) plus one row
    // per wrapped delegate line under them; grow it so delegate text is never
    // clipped out of view. The plan panel takes whatever is left.
    // (6 = 2 border + 3 fixed rows + 1 header + the "delegates:" line's slack;
    // keeping the no-delegate layout unchanged at 6.)
    let delegate_rows = delegate_panel_rows(
        &app.cfg.delegates,
        usize::from(cols[1].width.saturating_sub(2)).max(1),
    );
    let delegate_h = delegate_rows.as_ref().map_or(0, |r| r.len() as u16 - 1);
    let stats_h = (6 + delegate_h).min(rows[0].height);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(stats_h), Constraint::Min(0)])
        .split(cols[1]);
    draw_stats(app, frame, right[0]);
    draw_plan(app, frame, right[1]);

    // The M-x palette or the search bar replace the prompt line while open.
    if let Some(mx) = &app.mx {
        let (label, body) = match &mx.done {
            Some(hint) => ("M-x", hint.clone()),
            None => ("M-x ", mx.query.clone()),
        };
        let mx_line = Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(if mx.done.is_some() { " " } else { "" }),
            Span::styled(body, Style::default().fg(Color::White)),
            if mx.done.is_none() {
                Span::styled("_", Style::default().fg(Color::Magenta))
            } else {
                Span::raw("")
            },
            if mx.done.is_some() {
                Span::styled(
                    "  press any key to close",
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::styled(
                    format!(
                        "  {}",
                        if mx.matches.is_empty() {
                            "no match"
                        } else {
                            "enter:run  tab:complete  esc:close"
                        }
                    ),
                    Style::default().fg(if mx.matches.is_empty() {
                        Color::Red
                    } else {
                        Color::DarkGray
                    }),
                )
            },
        ]);
        frame.render_widget(Paragraph::new(mx_line), rows[1]);
        draw_mx_list(mx, frame, rows[1]);
    } else if let Some(s) = &app.search {
        // Search bar replaces the prompt line while Ctrl-S is active.
        let total = s.matches.len();
        let counter = if s.query.is_empty() {
            "type to search".to_string()
        } else if total == 0 {
            "no match".to_string()
        } else {
            format!("{}/{}", s.cur + 1, total)
        };
        let search_line = Line::from(vec![
            Span::styled(
                "/",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(s.query.clone()),
            Span::styled("_", Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled(
                counter,
                Style::default().fg(if total == 0 {
                    Color::Red
                } else {
                    Color::Yellow
                }),
            ),
            Span::styled(
                "  enter/↓:next  shift-enter/↑:prev  esc:close",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(search_line), rows[1]);
    } else {
        draw_prompt(app, frame, rows[1], &prompt_rows, prompt_win, prompt_cur);
    }

    if let Some(d) = app.dialogs.first() {
        draw_dialog(app, d, frame);
    }
    if let Some(p) = &app.pick {
        draw_model_pick(p, frame);
    }
}

/// The completion popup listing the commands matching the M-x query, drawn
/// above the palette row. Hidden once a command ran (hint mode) or when
/// nothing matches.
fn draw_mx_list(mx: &Mx, frame: &mut Frame, prompt_area: Rect) {
    if mx.done.is_some() || mx.matches.is_empty() {
        return;
    }
    let area = frame.area();
    let w = area.width.saturating_sub(2).min(96);
    let shown = mx.matches.len().min(8);
    let h = shown as u16 + 3; // two border rows + one hint row
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = prompt_area.y.saturating_sub(h).max(area.y);
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" M-x ")
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let text_w = usize::from(inner.width.saturating_sub(2)).max(8);

    let mut lines: Vec<Line> = Vec::new();
    let mut sel_line = 0usize;
    for (i, cmd) in mx.matches.iter().enumerate().take(8) {
        let selected = i == mx.sel;
        let keys = cmd.keys().unwrap_or("");
        let name = cmd.name();
        let desc_w = text_w.saturating_sub(4 + name.chars().count() + keys.chars().count());
        let desc: String = cmd.desc().chars().take(desc_w).collect();
        let pad = desc_w.saturating_sub(desc.chars().count());
        if selected {
            sel_line = lines.len();
        }
        push_tok_line(
            &mut lines,
            &[
                tok(
                    if selected { "> " } else { "  " },
                    Style::default().fg(if selected {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                tok(
                    name,
                    if selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    },
                ),
                tok("  ", Style::default()),
                tok(
                    format!("{desc}{}", " ".repeat(pad)),
                    Style::default().fg(Color::DarkGray),
                ),
                tok(keys.to_string(), Style::default().fg(Color::Yellow)),
            ],
        );
    }
    let view = usize::from(rows[0].height).max(1);
    let scroll = sel_line.saturating_sub(view.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), rows[0]);
    let hint = format!(
        "type to filter · {} of {} shown · ctrl-p/n or ↑/↓ move · enter runs · tab completes",
        shown,
        mx.matches.len()
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

// ---------------------------------------------------------------------------
// prompt editor rendering
// ---------------------------------------------------------------------------

/// Wrap the prompt text into visual rows and choose which window of rows to
/// show. Returns `(rows, top visible row, row of the cursor)`. The window is
/// sized `PROMPT_MAX_ROWS` and kept so the cursor row is always visible.
fn prompt_view(app: &App, width: u16) -> (Vec<LayoutRow>, usize, usize) {
    let text_w = (width.saturating_sub(PROMPT_GUTTER)).max(1) as usize;
    let rows = wrap_rows(app.input.text(), text_w);
    let cur_row = cursor_row(&rows, app.input.cursor());
    let win = if rows.len() > PROMPT_MAX_ROWS {
        let max_win = rows.len() - PROMPT_MAX_ROWS;
        cur_row
            .saturating_add(1)
            .saturating_sub(PROMPT_MAX_ROWS)
            .min(max_win)
    } else {
        0
    };
    (rows, win, cur_row)
}

/// Draw the multi-line prompt bar (rows already pre-wrapped by `prompt_view`)
/// and place the terminal cursor over the text.
fn draw_prompt(
    app: &mut App,
    frame: &mut Frame,
    area: Rect,
    rows: &[LayoutRow],
    win: usize,
    cur_row: usize,
) {
    let sel = app.input.selection();
    let text = app.input.text().to_string();
    // The prompt bar is a distinct green-tinted band (like a REPL prompt), with
    // wrapped continuation rows dimmed below the active line.
    let band = prompt_bg();
    let mut lines = Vec::with_capacity(rows.len().min(PROMPT_MAX_ROWS));
    for (i, r) in rows.iter().enumerate().skip(win).take(PROMPT_MAX_ROWS) {
        let first = i == 0;
        let prefix_style = if first {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
                .bg(band)
        } else {
            Style::default().fg(Color::DarkGray).bg(band)
        };
        let mut spans = vec![Span::styled(if first { "> " } else { "  " }, prefix_style)];
        for s in selection_spans(&text, *r, sel) {
            // Continuation rows dim the unselected text; the selection keeps
            // its own inverse shading and the whole row sits on the band.
            let mut st = s.style;
            if !first && st.bg.is_none() {
                st = st.fg(Color::DarkGray);
            }
            if st.bg.is_none() {
                st = st.bg(band);
            }
            spans.push(Span::styled(s.content, st));
        }
        // Pad the band flush to the right edge like the chat's user rows.
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = usize::from(area.width).saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(band)));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);

    // A real (block) cursor replaces the old trailing "_" glyph. Keep it
    // hidden while a search bar or dialog owns the keyboard.
    if app.search.is_none() && app.dialogs.is_empty() {
        let row = rows[cur_row];
        let col = cursor_col(&text, row, app.input.cursor()) as u16;
        let x = area
            .x
            .saturating_add(PROMPT_GUTTER)
            .saturating_add(col)
            .min(area.x.saturating_add(area.width.saturating_sub(1)));
        let y = area.y.saturating_add((cur_row - win) as u16);
        frame.set_cursor_position((x, y));
    }
}

/// Split one visual row into spans, shading bytes covered by the selection.
fn selection_spans(text: &str, row: LayoutRow, sel: Option<(usize, usize)>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut seg_start = row.start;
    let mut seg_sel = sel.is_some_and(|(a, b)| row.start >= a && row.start < b);
    let mut i = row.start;
    while i < row.end {
        let ch = text[i..].chars().next().unwrap();
        let next = i + ch.len_utf8();
        let here = sel.is_some_and(|(a, b)| i >= a && i < b);
        if here != seg_sel {
            push_seg(&mut spans, text, seg_start, i, seg_sel);
            seg_start = i;
            seg_sel = here;
        }
        i = next;
    }
    push_seg(&mut spans, text, seg_start, row.end, seg_sel);
    spans
}

fn push_seg(spans: &mut Vec<Span<'static>>, text: &str, a: usize, b: usize, selected: bool) {
    if a == b {
        return;
    }
    let s = String::from(&text[a..b]);
    let span = if selected {
        Span::styled(s, Style::default().fg(Color::Black).bg(Color::Gray))
    } else {
        Span::raw(s)
    };
    spans.push(span);
}

/// Compose the visual `Line` for one chat row.
///
/// `sel` swaps the gutter to a yellow `> ` (the selected block); `in_match`
/// yellow-highlights the row's text (current search hit). `band` tints the
/// whole row edge-to-edge with a soft background (user messages): when set,
/// the span area is padded with styled spaces so the band runs flush to the
/// chat border on short lines.
fn render_row_line(
    r: &RenderRow,
    sel: bool,
    in_match: bool,
    band: Option<Color>,
    width: usize,
) -> Line<'static> {
    let prefix_style = |bg: Option<Color>, fg: Option<Color>| {
        let mut s = Style::default();
        if let Some(c) = bg {
            s = s.bg(c);
        }
        if let Some(c) = fg {
            s = s.fg(c);
        }
        s
    };
    let prefix = if sel {
        Span::styled(
            "> ",
            prefix_style(band, Some(Color::Yellow)).add_modifier(Modifier::BOLD),
        )
    } else {
        match r.rule {
            Some(color) => Span::styled(
                "| ",
                prefix_style(band, Some(color)).add_modifier(Modifier::BOLD),
            ),
            None => Span::styled("  ", prefix_style(band, None)),
        }
    };
    let mut spans: Vec<Span> = Vec::with_capacity(r.spans.len() + 1);
    spans.push(prefix);
    for s in &r.spans {
        let bg = if in_match { Some(Color::Yellow) } else { band };
        match bg {
            Some(c) => spans.push(s.clone().patch_style(Style::default().bg(c))),
            None => spans.push(s.clone()),
        }
    }
    if let Some(b) = band {
        let used: usize = r.spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = width.saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(b)));
        }
    }
    Line::from(spans)
}

/// Border title for the chat panel: the session's display name, padded like
/// the other panel titles (" plan ", " model ") and truncated to keep the
/// block's right border intact on narrow terminals.
fn chat_title(title: &str, width: u16) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return " chat ".to_string();
    }
    // cap() appends "…" on top of max, so width - 3 leaves room for it while
    // keeping the whole title inside width - 2 cells (off the right border).
    cap(
        &format!(" {trimmed} "),
        usize::from(width.saturating_sub(3)),
    )
}

fn draw_chat(app: &mut App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(chat_title(&app.session.title(), area.width));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(2) as usize; // prefix column + spacing
    // Reuse the row layout of `chat` across frames while nothing chat-affecting
    // changed (typing, scrolling, search, most streaming deltas), instead of
    // re-tokenising and re-wrapping every message body on each draw.
    let cache = chat_cache(
        &mut app.chat_rows_cache,
        app.chat_epoch,
        width,
        &app.chat,
        &app.section_collapsed,
    );

    // The live streaming preview is transient: laid out fresh every frame.
    let mut preview: Vec<Vec<Span<'static>>> = if app.stream.is_empty() {
        Vec::new()
    } else {
        let visible = strip_react_scaffolding(&app.stream);
        md_to_lines(&visible, width)
    };

    let total_rows = cache.rows.len() + preview.len();
    let max = total_rows.saturating_sub(inner.height as usize);
    // Autoscroll: stay pinned to the bottom while following a run or while the
    // user is already at the bottom of the chat.
    if app.follow || app.was_at_bottom {
        app.scroll_top = max;
    }
    app.scroll_top = app.scroll_top.min(max);
    let offset = app.scroll_top;
    app.was_at_bottom = app.scroll_top >= max;

    // Per-row bookkeeping the event handlers index into (mouse, selection,
    // search): chat rows come from the cache, preview rows are un-owned.
    app.chat_rect = inner;
    app.row_targets = cache.rows.iter().map(|r| r.tool_header).collect();
    app.row_targets.resize(total_rows, None);
    app.row_msg = cache.owner.clone();
    app.row_msg.resize(total_rows, None);
    app.msg_ranges = cache.ranges.clone();
    app.view_rows = inner.height as usize;

    let sel_start = app.sel.and_then(|i| app.msg_ranges.get(i)).map(|&(s, _)| s);
    // Row span of the currently selected search match, if any.
    let search_hl = app
        .search
        .as_ref()
        .and_then(|s| s.matches.get(s.cur))
        .and_then(|&idx| app.msg_ranges.get(idx).copied());

    // Build Lines only for the rows actually on screen.
    let height = inner.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(height.min(total_rows));
    let chat_rows = cache.rows.len();
    for row in offset..total_rows.min(offset + height) {
        if row < chat_rows {
            let r = &cache.rows[row];
            let in_match = search_hl.is_some_and(|(start, len)| row >= start && row < start + len);
            let band = cache.owner[row]
                .and_then(|i| app.chat.get(i))
                .filter(|m| m.kind == MsgKind::User)
                .map(|_| user_band_bg());
            lines.push(render_row_line(
                r,
                Some(row) == sel_start,
                in_match,
                band,
                width,
            ));
        } else {
            // Streaming preview row: un-owned, never banded/selected.
            let spans = std::mem::take(&mut preview[row - chat_rows]);
            let mut sp = Vec::with_capacity(spans.len() + 1);
            sp.push(Span::styled("  ", Style::default()));
            sp.extend(spans);
            lines.push(Line::from(sp));
        }
    }

    frame.render_widget(Paragraph::new(lines).scroll((0, 0)), inner);
}

/// Pure row layout for a chat transcript. Org-style sections: each user turn is
/// a heading ("prompt echo") and the exchange under it — every message up to
/// the next user turn — is its body. A collapsed section renders only its
/// heading plus a "… N more" marker, so the exchange folds to one tinted block.
fn layout_chat_rows(
    chat: &[Msg],
    collapsed: &[bool],
    stream: &str,
    width: usize,
) -> (Vec<RenderRow>, Vec<Option<usize>>, Vec<(usize, usize)>) {
    let mut out = Vec::new();
    let mut owner: Vec<Option<usize>> = Vec::new();
    let mut ranges = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
    // Ordinal of the section we are inside + whether that section is collapsed.
    // Re-synced whenever a user turn is reached (its own ordinal + state).
    let mut ord = 0usize;
    let mut hiding = false;
    let mut i = 0;
    while i < chat.len() {
        let msg = &chat[i];
        let start = out.len();
        match msg.kind {
            MsgKind::User => {
                // A new exchange begins: recompute this section's visibility.
                ord += 1;
                hiding = collapsed.get(ord - 1).copied().unwrap_or(false);
                // Count hidden interior messages for the collapse marker.
                let hidden = if hiding {
                    chat[i + 1..]
                        .iter()
                        .take_while(|m| m.kind != MsgKind::User)
                        .count()
                } else {
                    0
                };
                // The heading is a SLIME-style prompt echo: "> text" on the
                // first wrapped row, indented continuation rows beneath.
                let echo = Span::styled(
                    "> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                );
                let cont = Span::styled("  ", dim);
                for (k, spans) in md_to_lines(&msg.text, width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let mut row = Vec::with_capacity(spans.len() + 1);
                    row.push(if k == 0 { echo.clone() } else { cont.clone() });
                    row.extend(spans);
                    out.push(RenderRow {
                        rule: Some(Color::Green),
                        spans: row,
                        tool_header: Some(i),
                    });
                }
                if hiding && hidden > 0 {
                    let marker = cap(
                        &format!("  ··· {hidden} more · tab/click to expand"),
                        width.saturating_sub(2),
                    );
                    out.push(RenderRow {
                        rule: None,
                        spans: vec![Span::styled(marker, dim)],
                        tool_header: Some(i),
                    });
                }
            }
            _ if hiding => {
                // The body of a collapsed exchange: keep the flat index valid
                // but emit no rows until the next user turn starts.
                ranges.push((out.len(), 0));
                i += 1;
                continue;
            }
            MsgKind::Run => layout_run(&mut out, i, &msg.children),
            MsgKind::Failure => layout_failure(&mut out, i, msg.fail.as_ref().unwrap(), width),
            MsgKind::Tool => layout_tool(&mut out, i, msg.tool.as_ref().unwrap(), width),
            MsgKind::Reasoning => layout_reasoning(
                &mut out,
                i,
                msg.text.as_str(),
                msg.author.as_deref().unwrap_or("model"),
                msg.open,
                width,
            ),
            MsgKind::Meta => {
                for s in plain_wrap(&msg.text, width) {
                    out.push(RenderRow {
                        rule: None,
                        spans: vec![Span::styled(s, dim)],
                        tool_header: None,
                    });
                }
            }
            MsgKind::Assistant => {
                author_header(
                    &mut out,
                    msg.author.as_deref().unwrap_or("assistant"),
                    Color::Green,
                    width,
                );
                for spans in md_to_lines(&msg.text, width) {
                    out.push(RenderRow {
                        rule: None,
                        spans,
                        tool_header: None,
                    });
                }
            }
            MsgKind::Delegate => {
                author_header(
                    &mut out,
                    msg.author.as_deref().unwrap_or("delegate"),
                    Color::Magenta,
                    width,
                );
                let rule = Some(Color::Magenta);
                for spans in md_to_lines(&msg.text, width) {
                    out.push(RenderRow {
                        rule,
                        spans,
                        tool_header: None,
                    });
                }
            }
        }
        let end = out.len();
        owner.extend((start..end).map(|_| Some(i)));
        ranges.push((start, end - start));
        i += 1;
    }
    // Live streaming preview (scaffolding hidden), never committed.
    if !stream.is_empty() {
        let visible = strip_react_scaffolding(stream);
        for spans in md_to_lines(&visible, width) {
            out.push(RenderRow {
                rule: None,
                spans,
                tool_header: None,
            });
            owner.push(None);
        }
    }
    (out, owner, ranges)
}

// ---------------------------------------------------------------------------
// run folding: collapse a completed stretch of activity into one digest row
// ---------------------------------------------------------------------------

/// Kinds that belong to a folded run when they appear between two spoken
/// messages (User/Assistant/Delegate): tool calls, failure blocks, reasoning
/// and the grey status notes that interleave with them. Agent `error:` notes
/// are left out on purpose: they are the reason an interrupted run stopped, so
/// they stay visible instead of being buried in a digest.
fn is_run_member(m: &Msg) -> bool {
    match m.kind {
        MsgKind::Tool | MsgKind::Reasoning | MsgKind::Failure => true,
        MsgKind::Meta => !m.text.trim_start().starts_with("error:"),
        _ => false,
    }
}

/// Whether a folded span carries at least one tool call or failure. Pure
/// reasoning/meta notes are left as-is: they already read as one line each.
fn span_has_action(msgs: &[Msg]) -> bool {
    msgs.iter()
        .any(|m| matches!(m.kind, MsgKind::Tool | MsgKind::Failure))
}

/// A completed segment is folded into a single digest row whose children keep
/// every original message (closed, so the transcript stays short) for later
/// unfold. Spoken messages and already-folded digests split spans and are
/// never themselves folded. Returns the replaced `(start, len)` ranges so
/// callers can fix up cached message indices (selection cursor).
fn fold_completed_runs(chat: &mut Vec<Msg>) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    let n = chat.len();
    while i < n {
        if !is_run_member(&chat[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && is_run_member(&chat[i]) {
            i += 1;
        }
        let len = i - start;
        if span_has_action(&chat[start..i]) {
            spans.push((start, len));
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(chat.len());
    let mut cursor = 0;
    for &(start, len) in &spans {
        out.extend(chat[cursor..start].iter().cloned());
        let mut children: Vec<Msg> = chat[start..start + len].to_vec();
        // Auto-collapse stale open cards (diffs, test results, failures) now
        // that the stretch has finished; reasoning stays expanded by default.
        for m in &mut children {
            match m.kind {
                MsgKind::Tool => {
                    if let Some(c) = &mut m.tool {
                        c.open = false;
                    }
                }
                MsgKind::Failure => {
                    if let Some(f) = &mut m.fail {
                        f.open = false;
                    }
                }
                _ => {}
            }
        }
        out.push(Msg::run(children));
        cursor = start + len;
    }
    out.extend(chat[cursor..].iter().cloned());
    *chat = out;
    spans
}

/// Replace one folded [`MsgKind::Run`] digest with its original children, in
/// place, so every child becomes a normal selectable/searchable message again.
fn unfold_run(chat: &mut Vec<Msg>, idx: usize) {
    let is_run = match chat.get(idx).map(|m| m.kind) {
        Some(MsgKind::Run) => true,
        _ => false,
    };
    if !is_run {
        return;
    }
    let children = std::mem::take(&mut chat[idx].children);
    let mut rest = chat.split_off(idx + 1);
    chat.pop(); // drop the emptied digest
    chat.extend(children);
    chat.append(&mut rest);
}

/// Reading/search tools are the chat's filler: many calls, little news. They
/// are rendered dimmed and skipped in a run digest's action summary.
fn is_read_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "read_ranges"
            | "list_dir"
            | "list_files"
            | "rgrep"
            | "list_symbols"
            | "find_symbol"
            | "find_definition"
            | "read_symbol"
            | "structural_map"
            | "references_count"
            | "find_references"
            | "project_model"
    )
}

/// What a folded run did, boiled down for its one-line header: how many tool
/// calls, whether anything failed, and the tools used, aggregated into
/// "name ×n" items (rgrep ×5 reads as one item, never five). Mutating tools
/// come first; read/search tools trail them so a long read-heavy run still
/// shows the actions that changed something.
struct RunDigest {
    calls: usize,
    ok: bool,
    failed: usize,
    actions: String,
}

fn run_digest(children: &[Msg]) -> RunDigest {
    let calls = children.iter().filter(|m| m.kind == MsgKind::Tool).count();
    let failed = children
        .iter()
        .filter(|m| {
            m.kind == MsgKind::Failure
                || (m.kind == MsgKind::Tool && m.tool.as_ref().is_some_and(|t| !t.ok))
        })
        .count();
    // Tool names in first-seen order, counts folded in (×n) — reads included
    // so a stretch of lookups collapses into one countable token per tool.
    let mut seen: Vec<(String, usize)> = Vec::new();
    for m in children {
        if let Some(t) = &m.tool {
            if let Some((_, c)) = seen.iter_mut().find(|(n, _)| *n == t.name) {
                *c += 1;
            } else {
                seen.push((t.name.clone(), 1));
            }
        }
    }
    // Stable sort: mutating tools keep first-seen order and lead the summary;
    // read/search tools group dimly behind them.
    seen.sort_by_key(|(n, _)| is_read_tool(n));
    let actions = seen
        .iter()
        .map(|(n, c)| {
            if *c > 1 {
                format!("{n} ×{c}")
            } else {
                n.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    RunDigest {
        calls,
        ok: failed == 0,
        failed,
        actions,
    }
}

// Per-task header helpers: a light "custom UI" per tool family so a chat row
// is never just a bare task name.

/// Truncate to `max` characters, appending "…" when cut.
fn cap(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Collapse a (possibly multi-line) result into a short one-liner.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cap(&flat, max)
}

/// Icon + accent color per tool family, used as the card's leading glyph.
fn tool_icon(name: &str) -> (&'static str, Color) {
    match name {
        "apply_patch" | "apply_edit" => ("±", Color::Cyan),
        "write_file" => ("✎", Color::Cyan),
        "run_tests" => ("▶", Color::Yellow),
        "run_task" => ("▸", Color::Yellow),
        "shell" => ("$", Color::Green),
        "git_status" | "git_diff" | "git_log" | "git_commit" => ("↗", Color::Magenta),
        "delegate" => ("⇄", Color::Magenta),
        "read_file" | "read_ranges" => ("≡", Color::Blue),
        "list_dir" | "list_files" | "rgrep" | "list_symbols" | "find_symbol"
        | "find_definition" | "read_symbol" | "structural_map" | "references_count"
        | "find_references" | "project_model" => ("›", Color::DarkGray),
        _ => ("•", Color::Magenta),
    }
}

/// A one-line headline from a tool call's args, so the row shows *what* the
/// task targeted (file, pattern, symbol, command) instead of just its name.
fn tool_headline(name: &str, args: &str) -> Option<String> {
    use serde_json::Value;
    if args.trim().is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(args) else {
        return None;
    };
    let map = match value {
        Value::Object(m) => m,
        Value::String(s) => return Some(cap(&s, 80)),
        _ => return None,
    };
    let pick = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(v) = map.get(*k) {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                if !text.trim().is_empty() {
                    return Some(cap(text.trim(), 80));
                }
            }
        }
        None
    };
    match name {
        "read_file" | "read_ranges" | "write_file" => pick(&["path", "file"]),
        "list_dir" | "list_files" => pick(&["path", "dir", "glob"]),
        "rgrep" => pick(&["pattern", "glob", "query"]),
        "find_symbol" | "search_symbols" => pick(&["query", "symbol"]),
        "find_definition" | "read_symbol" | "rename" | "find_references" | "references_count" => {
            pick(&["symbol", "query"])
        }
        "structural_map" | "list_symbols" => pick(&["path", "kinds"]),
        "web_search" => pick(&["query"]),
        "run_task" | "run_tests" => pick(&["task", "command"]),
        "shell" => pick(&["command", "dir"]),
        "delegate" => {
            // Which delegate is engaged (ad-hoc), or which plan step (step).
            if let Some(m) = map
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(cap(m, 40))
            } else {
                map.get("step")
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| format!("step {n}"))
            }
        }
        _ => None,
    }
}

/// Parse the `delegate` tool's success output into (delegate name, reply):
/// `delegate <name> (<display>) replied:\n<reply>`.
fn parse_delegate_reply(output: &str) -> Option<(String, String)> {
    let rest = output.strip_prefix("delegate ")?;
    let open = rest.find(" (")?;
    let model = rest[..open].trim();
    if model.is_empty() {
        return None;
    }
    let after = &rest[open + 2..];
    let marker = ") replied:\n";
    let end = after.find(marker)?;
    let reply = after[end + marker.len()..].trim_end();
    Some((model.to_string(), reply.to_string()))
}

/// Visible reasoning extracted from a streamed assistant text: scaffold lines
/// (Thought:/Tool:/Args:/...) removed; `None` when nothing meaningful remains.
fn reasoning_from_stream(stream: &str) -> Option<String> {
    let visible = strip_react_scaffolding(stream);
    let visible = visible.trim();
    if visible.is_empty() {
        None
    } else {
        Some(visible.to_string())
    }
}

/// Present a tool call's args as readable `key = value` lines instead of a raw
/// JSON blob (flattening a single nested `args` object when present).
fn arg_lines(args: &str) -> Vec<String> {
    use serde_json::Value;
    let Ok(value) = serde_json::from_str::<Value>(args) else {
        let t = args.trim();
        return if t.is_empty() {
            vec![]
        } else {
            vec![cap(t, 300)]
        };
    };
    let mut map = match value {
        Value::Object(m) => m,
        Value::String(s) => return vec![cap(&s, 300)],
        _ => {
            return vec![cap(&args.trim(), 300)];
        }
    };
    if let Some(inner) = map.remove("args").and_then(|a| match a {
        Value::Object(m) => Some(m),
        _ => None,
    }) {
        map = inner;
    }
    let scalar = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(cap(s, 160)),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Array(items) if !items.is_empty() => {
                let joined = items
                    .iter()
                    .filter_map(|i| match i {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(cap(&joined, 160))
            }
            _ => None,
        }
    };
    let mut lines = Vec::new();
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for k in keys {
        let Some(v) = map.get(k) else {
            continue;
        };
        let Some(text) = scalar(v) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        lines.push(format!("{k}: {text}"));
    }
    lines
}

/// A one-row author tag ("you", the main model's label, or a delegate name)
/// rendered above a content block so the transcript shows *who* produced it.
fn author_header(out: &mut Vec<RenderRow>, author: &str, color: Color, width: usize) {
    let label = cap(author, width.saturating_sub(2));
    out.push(RenderRow {
        rule: None,
        spans: vec![Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )],
        tool_header: None,
    });
}

/// A collapsible "reasoning" block: the model's visible thinking between
/// actions, headed by an author tag and toggled like a tool card.
fn layout_reasoning(
    out: &mut Vec<RenderRow>,
    msg_idx: usize,
    text: &str,
    author: &str,
    open: bool,
    width: usize,
) {
    out.push(RenderRow {
        rule: None,
        spans: vec![
            Span::styled(
                if open { "v " } else { "> " },
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                "🧠 reasoning",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  · {author}"),
                Style::default().fg(Color::DarkGray),
            ),
        ],
        tool_header: Some(msg_idx),
    });
    if !open {
        return;
    }
    if !text.trim().is_empty() {
        for s in plain_wrap(text, width.saturating_sub(2)) {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled(
                    format!("  {s}"),
                    Style::default().fg(Color::DarkGray),
                )],
                tool_header: None,
            });
        }
    }
}

fn layout_failure(out: &mut Vec<RenderRow>, msg_idx: usize, fail: &TestFail, width: usize) {
    out.push(RenderRow {
        rule: None,
        spans: vec![
            Span::styled(
                if fail.open { "v " } else { "> " },
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                "FAILED ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(fail.name.clone(), Style::default().fg(Color::White)),
        ],
        tool_header: Some(msg_idx),
    });
    if !fail.open {
        return;
    }
    if !fail.detail.is_empty() {
        for s in plain_wrap(&fail.detail, width.saturating_sub(2)) {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled(
                    format!("  {s}"),
                    Style::default().fg(Color::Red),
                )],
                tool_header: None,
            });
        }
    }
}

/// Compact human duration from milliseconds: "140ms", "3.4s", "2m 5s".
fn fmt_dur_ms(ms: u128) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        let s = ms / 1_000;
        if ms % 1_000 == 0 {
            return format!("{s}s");
        }
        return format!("{s}.{}s", ms % 1_000 / 100);
    }
    let total_s = ms / 1_000;
    if total_s % 60 == 0 {
        format!("{}m", total_s / 60)
    } else {
        format!("{}m {}s", total_s / 60, total_s % 60)
    }
}

/// Compact token count: "231", "1.2k", "3.4M".
fn fmt_tokens(n: usize) -> String {
    let fmt = |v: f64, unit: &str| -> String {
        if (v - v.trunc()).abs() < 1e-9 {
            format!("{}{unit}", v as usize)
        } else {
            format!("{v:.1}{unit}")
        }
    };
    if n >= 1_000_000 {
        fmt(n as f64 / 1_000_000.0, "M")
    } else if n >= 1_000 {
        fmt(n as f64 / 1_000.0, "k")
    } else {
        n.to_string()
    }
}

/// Aggregated usage of a folded run's tool calls: the wall time of the whole
/// stretch (first call started .. last call finished) and the total real
/// tokens of the model requests behind them (each request counted once).
/// Returns `(None, 0)` when no finished call carries timing.
fn run_usage(children: &[Msg]) -> (Option<u128>, usize) {
    let mut tokens = 0usize;
    let mut start: Option<Instant> = None;
    let mut end: Option<Instant> = None;
    for m in children {
        let Some(c) = &m.tool else { continue };
        if let Some(t) = c.tokens {
            tokens += t;
        }
        let (Some(s), Some(ms)) = (c.started, c.taken_ms) else {
            continue;
        };
        let e = s + Duration::from_millis(ms as u64);
        start = Some(start.map_or(s, |st| st.min(s)));
        end = Some(end.map_or(e, |en| en.max(e)));
    }
    let ms = match (start, end) {
        (Some(s), Some(e)) => Some(e.duration_since(s).as_millis()),
        _ => None,
    };
    (ms, tokens)
}

/// One-line digest of a folded run: clicking/Tab unfolds it back into its
/// original messages.
fn layout_run(out: &mut Vec<RenderRow>, msg_idx: usize, children: &[Msg]) {
    let d = run_digest(children);
    let dim = Style::default().fg(Color::DarkGray);
    // Attribute the run to whoever did the work. The parent's `delegate`
    // hand-off card names the main model, not the sub-agent that actually ran
    // the stretch, so skip it when picking the author label.
    let author = children
        .iter()
        .filter_map(|m| m.tool.as_ref())
        .filter(|t| t.name != "delegate")
        .filter_map(|t| t.author.as_deref())
        .next();
    let status = if d.calls > 0 && d.ok {
        Some(Span::styled(
            "✓",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else if d.failed > 0 {
        Some(Span::styled(
            format!(
                "✗ {}{}",
                d.failed,
                if d.failed > 1 { " failed" } else { " failure" }
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else {
        None
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("> ", dim),
        Span::styled(
            "task run",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(a) = author {
        spans.push(Span::styled(format!(" · {a}"), dim));
    }
    // The aggregated tool names ARE the summary (rgrep ×5 · apply_patch), so
    // the raw call count is noise; the ✓/✗ state follows the actions.
    if !d.actions.is_empty() {
        spans.push(Span::styled(format!("  · {}", cap(&d.actions, 100)), dim));
    }
    if let Some(s) = status {
        spans.push(Span::styled("  ", dim));
        spans.push(s);
    }
    let (ms, tokens) = run_usage(children);
    if tokens > 0 {
        spans.push(Span::styled(format!("  · {} tok", fmt_tokens(tokens)), dim));
    }
    if let Some(ms) = ms {
        spans.push(Span::styled(format!("  · {}", fmt_dur_ms(ms)), dim));
    }
    out.push(RenderRow {
        rule: None,
        spans,
        tool_header: Some(msg_idx),
    });
}

fn layout_tool(out: &mut Vec<RenderRow>, msg_idx: usize, card: &ToolCard, width: usize) {
    let (icon, accent) = tool_icon(&card.name);
    // Reading/search tools are the chat's filler; dim them so prose and the
    // actions that changed something stand out (visual tier 3).
    let read = is_read_tool(&card.name);
    let glyph = if read {
        Span::styled(icon, Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(
            icon,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )
    };
    let name = if read {
        Span::styled(
            format!(" {}", card.name),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::styled(
            format!(" {}", card.name),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            if card.open { "v " } else { "> " },
            Style::default().fg(Color::DarkGray),
        ),
        glyph,
        name,
    ];
    // Collapsed rows are the task's name, nothing more: no author, no request
    // preview, no result tail — all of that lives behind the card (open it).
    // While the call is still running or awaiting approval its args matter
    // (a pending apply_patch must say what it touches), so the one-line
    // "what it targets" headline and justification ride along only then.
    let pending = card.result.is_none() && !card.open;
    if pending {
        if let Some(h) = tool_headline(&card.name, &card.args) {
            spans.push(Span::styled(
                format!("  {h}"),
                Style::default().fg(if read { Color::DarkGray } else { Color::White }),
            ));
        }
        if let Some(j) = card.justification.as_deref() {
            spans.push(Span::styled(
                format!("  · {j}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    // Status: a bare pass/fail mark once the call finished — no inline result.
    if card.result.is_some() {
        let ok = card.ok;
        spans.push(Span::styled(
            format!("  {}", if ok { "✓" } else { "✗" }),
            Style::default()
                .fg(if ok { Color::Green } else { Color::Red })
                .add_modifier(Modifier::BOLD),
        ));
    }
    // How long the call took is always shown once it finished; the real tokens
    // of the model request behind it are shown when the card is open.
    if let Some(ms) = card.taken_ms {
        spans.push(Span::styled(
            format!("  · {}", fmt_dur_ms(ms)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if card.open {
        if let Some(tok) = card.tokens {
            spans.push(Span::styled(
                format!("  · {} tok", fmt_tokens(tok)),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    out.push(RenderRow {
        rule: None,
        spans,
        tool_header: Some(msg_idx),
    });
    if !card.open {
        return;
    }
    if let Some(j) = card.justification.as_deref() {
        for s in plain_wrap(&format!("justification: {j}"), width) {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled(s, Style::default().fg(Color::DarkGray))],
                tool_header: None,
            });
        }
    }
    if let Some(risk) = &card.risk {
        for s in plain_wrap(&format!("risk: {risk}"), width) {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled(s, Style::default().fg(Color::Yellow))],
                tool_header: None,
            });
        }
    }
    if let Some((old, new)) = extract_diff_sides(&card.name, &card.args) {
        out.push(RenderRow {
            rule: None,
            spans: vec![Span::styled(
                edit_diff_label(&card.name, &card.args, card.result.as_deref()),
                Style::default().fg(Color::DarkGray),
            )],
            tool_header: None,
        });
        const MAX_DIFF_ROWS: usize = 200;
        let pairs = lcs_pairs(&old, &new);
        let shown = pairs.len().min(MAX_DIFF_ROWS);
        for pair in pairs.iter().take(shown) {
            let spans = build_diff_row(
                pair.0.as_deref(),
                pair.1.as_deref(),
                width,
                Some(diff_remove_bg()),
                Some(diff_add_bg()),
            );
            out.push(RenderRow {
                rule: None,
                spans,
                tool_header: None,
            });
        }
        if pairs.len() > MAX_DIFF_ROWS {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled(
                    format!("... {} more diff rows", pairs.len() - MAX_DIFF_ROWS),
                    Style::default().fg(Color::DarkGray),
                )],
                tool_header: None,
            });
        }
    } else {
        let lines = arg_lines(&card.args);
        if !lines.is_empty() {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled("args:", Style::default().fg(Color::DarkGray))],
                tool_header: None,
            });
            for line in lines {
                out.push(RenderRow {
                    rule: None,
                    spans: vec![Span::styled(line, Style::default().fg(Color::Magenta))],
                    tool_header: None,
                });
            }
        }
    }
    if let Some(result) = &card.result {
        let color = if card.ok { Color::Green } else { Color::Red };
        out.push(RenderRow {
            rule: None,
            spans: vec![Span::styled("result:", Style::default().fg(color))],
            tool_header: None,
        });
        for s in plain_wrap(result, width) {
            out.push(RenderRow {
                rule: None,
                spans: vec![Span::styled(s, Style::default().fg(color))],
                tool_header: None,
            });
        }
    }
}

/// The rows rendered under "delegates:" in the model panel, each one fitted to
/// `width` columns so no delegate line can spill past the panel's right edge
/// (or off the screen on a narrow terminal). `None` when no delegate is
/// configured. Mirrors the plan panel: long names/models/descriptions are
/// collapsed to single spaces and word-wrapped.
fn delegate_panel_rows(delegates: &[DelegateCfg], width: usize) -> Option<Vec<Line<'static>>> {
    if delegates.is_empty() {
        return None;
    }
    let dim = Style::default().fg(Color::DarkGray);
    let width = width.max(1);
    let mut rows = vec![Line::from(Span::styled(
        "delegates:",
        dim.add_modifier(Modifier::BOLD),
    ))];
    // Reserve two columns of left indent for the wrapped body.
    let body_width = width.saturating_sub(2).max(1);
    for d in delegates {
        let label = if d.name == d.llm.model {
            d.name.clone()
        } else {
            format!("{} ({})", d.name, d.llm.model)
        };
        let mut text = label;
        if !d.description.trim().is_empty() {
            text.push_str(" — ");
            text.push_str(d.description.trim());
        }
        for wrapped in wrap_toks(&[tok(flat(&text), dim)], body_width) {
            let mut spans = vec![Span::styled("  ", dim)];
            spans.extend(
                wrapped
                    .iter()
                    .map(|t| Span::styled(t.text.clone(), t.style)),
            );
            rows.push(Line::from(spans));
        }
    }
    Some(rows)
}

fn draw_stats(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" model ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tokens = app.ctx_tokens;
    let budget = app.ctx_budget.max(1);
    let ratio = (tokens as f64 / budget as f64).clamp(0.0, 1.0);
    let pct = (ratio * 100.0).round() as usize;
    let bar_color = if ratio < 0.7 {
        Color::Green
    } else if ratio < 0.9 {
        Color::Yellow
    } else {
        Color::Red
    };

    let width = inner.width as usize;
    let model = app.cfg.llm.model.clone();
    let mut model_label = model;
    if let Some(v) = &app.cfg.llm.model_version {
        if !v.is_empty() {
            model_label.push_str("  ");
            model_label.push_str(v);
        }
    }
    if let Some(b) = &app.balance {
        if !b.is_empty() {
            model_label.push_str("  |  ");
            model_label.push_str(b);
        }
    }
    if model_label.chars().count() > width {
        model_label = model_label.chars().take(width).collect();
    }

    let filled = (ratio * width as f64).floor() as usize;
    let mut bar_spans = Vec::new();
    bar_spans.push(Span::styled(
        "#".repeat(filled),
        Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
    ));
    bar_spans.push(Span::styled(
        "-".repeat(width.saturating_sub(filled)),
        Style::default().fg(Color::DarkGray),
    ));

    let mut usage = vec![Span::styled(
        format!("{pct}% used  "),
        Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
    )];
    usage.push(Span::styled(
        format!("{tokens} / {budget} tokens"),
        Style::default().fg(Color::White),
    ));
    usage.push(Span::styled(
        if app.ctx_estimated {
            " (est.)"
        } else {
            " (api)"
        },
        Style::default().fg(Color::DarkGray),
    ));

    let model_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(model_label, model_style))),
        rows[0],
    );
    frame.render_widget(Paragraph::new(Line::from(bar_spans)), rows[1]);
    frame.render_widget(Paragraph::new(Line::from(usage)), rows[2]);

    // Configured delegates ([[delegates]]) shown in the leftover space under
    // the model gauge; word-wrapped to the panel width so long names or
    // descriptions never spill past the right edge.
    if let Some(delegate_rows) = delegate_panel_rows(&app.cfg.delegates, width) {
        frame.render_widget(Paragraph::new(delegate_rows), rows[3]);
    }
    let _ = rows;
}

fn draw_plan(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" plan ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Word-wrap every step to the panel width so long goals never overflow the
    // right edge of the plan panel.
    let width = usize::from(inner.width).max(16);
    let steps = app.session.plan();
    let mut lines: Vec<Line> = Vec::new();
    if steps.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no plan yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for step in &steps {
        let color = match step.status {
            PlanStatus::Done => Color::Green,
            PlanStatus::InProgress => Color::Yellow,
            PlanStatus::Blocked => Color::Red,
            PlanStatus::Pending => Color::DarkGray,
        };
        // A finished step collapses to a single line: the note and the
        // verification (which mattered while it was being worked) are dropped,
        // and the goal is truncated so the row never wraps.
        if step.status == PlanStatus::Done {
            push_tok_line(&mut lines, &collapsed_done_toks(step, width));
            continue;
        }
        let text_color = Color::White;
        let now = now_ms();
        let mut toks = vec![
            tok(
                format!("{} ", plan_glyph(&step.status, now)),
                Style::default().fg(color),
            ),
            tok(
                format!("{}. ", step.id),
                Style::default().fg(Color::DarkGray),
            ),
            tok(flat(&step.goal), Style::default().fg(text_color)),
        ];
        // Which model runs this step, when one is assigned (context is never
        // rendered in the UI).
        if !step.model.is_empty() {
            toks.push(tok(
                format!("  [{}]", step.model),
                Style::default().fg(Color::Cyan),
            ));
        }
        for line in wrap_toks(&toks, width) {
            push_tok_line(&mut lines, &line);
        }
        if let Some(note) = &step.note {
            let note_toks = vec![
                tok("  - ", Style::default().fg(color)),
                tok(flat(note), Style::default().fg(color)),
            ];
            for line in wrap_toks(&note_toks, width) {
                push_tok_line(&mut lines, &line);
            }
        }
        // verification: how this step is proven (keeps steps isolated)
        let verify = step.verification.trim();
        if !verify.is_empty() {
            let verify_toks = vec![tok(
                format!("verify: {}", flat(&verify)),
                Style::default().fg(Color::DarkGray),
            )];
            for line in wrap_toks(&verify_toks, width) {
                push_tok_line(&mut lines, &line);
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Collapse runs of whitespace (incl. newlines) into single spaces so a plan
/// line wraps cleanly inside the panel.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Convert one wrapped token line into a ratatui `Line`.
fn push_tok_line(lines: &mut Vec<Line>, line: &[Tok]) {
    let spans: Vec<Span> = line
        .iter()
        .map(|t| Span::styled(t.text.clone(), t.style))
        .collect();
    lines.push(Line::from(spans));
}

/// Spinner frames cycled by wall-clock time for an in-progress step (all
/// width-1 braille glyphs, so a wrapped plan row's width does not change as it
/// rotates).
const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
/// Milliseconds one spinner frame stays on screen.
const SPINNER_FRAME_MS: u128 = 100;

/// Milliseconds since the Unix epoch (for picking the spinner frame).
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The status glyph shown before a plan step: "-" for pending, a rotating
/// spinner for in-progress, a tick for done and a cross for failed/blocked.
fn plan_glyph(s: &PlanStatus, now_ms: u128) -> &'static str {
    match s {
        PlanStatus::Pending => "-",
        PlanStatus::InProgress => {
            let frame = ((now_ms / SPINNER_FRAME_MS) as usize) % SPINNER_FRAMES.len();
            SPINNER_FRAMES[frame]
        }
        PlanStatus::Done => "✓",
        PlanStatus::Blocked => "✗",
    }
}

/// One styled, non-wrapping row for a finished plan step: the status glyph, the
/// step number, the goal truncated to fit `width`, the model that ran it and —
/// when the step was actually started — how long it took ("· 2m 5s"). The note
/// and verification details are intentionally omitted: done steps stay at one
/// line so the panel reads as a collapsed checklist.
fn collapsed_done_toks(step: &PlanStep, width: usize) -> Vec<Tok> {
    let status_glyph = tok(
        format!("{} ", plan_glyph(&PlanStatus::Done, 0)),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );
    let head = tok(
        format!("{}. ", step.id),
        Style::default().fg(Color::DarkGray),
    );
    let model = if step.model.is_empty() {
        None
    } else {
        Some(tok(
            format!("  [{}]", step.model),
            Style::default().fg(Color::Cyan),
        ))
    };
    let duration = step.took_ms.map(|ms| {
        tok(
            format!("  · {}", fmt_dur_ms(ms as u128)),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    });
    // Fixed-width parts (glyph, number, model tag, duration): the goal gets the
    // remainder so the row is exactly one line wide. One extra cell is reserved
    // for the "…" cap() appends when it truncates.
    let fixed: usize = 2 // "X " glyph + space
        + head.text.chars().count()
        + model.as_ref().map_or(0, |t| t.text.chars().count())
        + duration.as_ref().map_or(0, |t| t.text.chars().count());
    let goal_max = width.saturating_sub(fixed + 1);
    let goal = tok(
        cap(&flat(&step.goal), goal_max),
        Style::default().fg(Color::DarkGray),
    );
    let mut toks = vec![status_glyph, head, goal];
    if let Some(t) = model {
        toks.push(t);
    }
    if let Some(t) = duration {
        toks.push(t);
    }
    toks
}

/// The Ctrl-A "assign a model" overlay: choose a plan step (pending/blocked
/// only), then choose which model runs it.
fn draw_model_pick(pick: &ModelPick, frame: &mut Frame) {
    let area = frame.area();
    let w = area.width.saturating_sub(2).min(92);
    let max_h = area.height.saturating_sub(2);
    let width = usize::from(w).saturating_sub(4).max(16);
    let picking_model = pick.model_sel.is_some();

    let mut lines: Vec<Line> = Vec::new();
    let mut sel_line = 0usize;

    if picking_model {
        let step = &pick.steps[pick.sel.min(pick.steps.len().saturating_sub(1))];
        lines.push(Line::from(Span::styled(
            format!("which model runs step {}?", step.id),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(Span::styled(
            flat(&step.goal),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
        for (i, m) in pick.models.iter().enumerate() {
            let selected = pick.model_sel == Some(i);
            let mut st = Style::default().fg(if selected { Color::White } else { Color::Cyan });
            if selected {
                st = st.add_modifier(Modifier::BOLD);
            }
            let toks = vec![
                tok(
                    if selected { "> " } else { "  " },
                    Style::default().fg(if selected {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                tok(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
                tok(m.clone(), st),
            ];
            if selected {
                sel_line = lines.len();
            }
            for wl in wrap_toks(&toks, width) {
                push_tok_line(&mut lines, &wl);
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "pick the step to reassign (pending/blocked only):",
            Style::default().fg(Color::Yellow),
        )));
        for (i, s) in pick.steps.iter().enumerate() {
            let selected = i == pick.sel;
            let mut st = Style::default().fg(if selected {
                Color::White
            } else {
                Color::DarkGray
            });
            if selected {
                st = st.add_modifier(Modifier::BOLD);
            }
            let mut toks = vec![
                tok(
                    if selected { "> " } else { "  " },
                    Style::default().fg(if selected {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                tok(format!("{}. ", s.id), Style::default().fg(Color::DarkGray)),
                tok(flat(&s.goal), st),
            ];
            if !s.model.is_empty() {
                toks.push(tok(
                    format!("  [{}]", s.model),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if selected {
                sel_line = lines.len();
            }
            for wl in wrap_toks(&toks, width) {
                push_tok_line(&mut lines, &wl);
            }
        }
    }

    let hint = if picking_model {
        "ctrl-p/n or ↑/↓ move · 1..N assigns · enter confirms · esc/← back to steps"
    } else {
        "ctrl-p/n or ↑/↓ move · →/enter picks a model · ctrl-a/esc closes"
    };

    let content_h = lines.len() as u16;
    let h = (content_h + 3).clamp(6, max_h.max(6));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" assign model ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let view = usize::from(rows[0].height).max(1);
    let scroll = sel_line.saturating_sub(view.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), rows[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

fn draw_dialog(app: &App, dialog: &Dialog, frame: &mut Frame) {
    let area = frame.area();

    // Build the body lines for the kind of prompt.
    let (kind_label, is_question, options, mut body) = match &dialog.prompt {
        UserPrompt::Question { prompt, options } => {
            let text = if options.is_empty() {
                format!("{prompt}\n\n(Type your answer below)")
            } else {
                prompt.clone()
            };
            let mut lines = preview_lines(&text, 96);
            for (i, o) in options.iter().enumerate() {
                let mut spans = vec![Span::styled(
                    format!("{}. ", i + 1),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.push(Span::styled(o.clone(), Style::default().fg(Color::Cyan)));
                lines.push(Line::from(spans));
            }
            (" question ", true, options.clone(), lines)
        }
        UserPrompt::Confirm { title, diff } => {
            let mut lines = Vec::new();
            // The actual action (e.g. the shell command) goes on top so the
            // human always sees exactly what they are approving.
            lines.extend(preview_lines(title, 96));
            if let Some(d) = diff {
                if !d.trim().is_empty() {
                    lines.push(Line::from(""));
                    lines.extend(preview_lines(d, 96));
                }
            }
            if !app.dialog_conv.is_empty() {
                lines.push(Line::from(""));
                let conv = app.dialog_conv.join("\n");
                lines.extend(preview_lines(&conv, 96));
            }
            (" confirm  [y/n] ", false, Vec::new(), lines)
        }
    };
    if body.is_empty() {
        body.push(Line::from(""));
    }

    // Size the popup to fit, up to almost the whole terminal.
    let w = area.width.saturating_sub(2).min(100);
    let max_h = area.height.saturating_sub(2);
    let content_h = body.len() as u16 + 3; // body + input + hint
    let h = content_h.clamp(5, max_h.max(5));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);

    let mut kind_label = kind_label;
    let mut border_color = if is_question {
        Color::Cyan
    } else {
        Color::Yellow
    };
    if app.dialog_ask && !is_question {
        kind_label = " confirm  [ask the model] ";
        border_color = Color::Magenta;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(kind_label)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let inner_w = inner.width.saturating_sub(2) as usize;
    let row_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Body, scrollable if it does not fit.
    let body_view = row_layout[0].height as usize;
    let scroll = body.len().saturating_sub(body_view) as u16;
    frame.render_widget(Paragraph::new(body).scroll((scroll, 0)), row_layout[0]);

    // Input row.
    let input_prompt = if app.dialog_ask { "? " } else { "> " };
    let input = Line::from(vec![
        Span::styled(input_prompt, Style::default().fg(Color::Green)),
        Span::raw(dialog.buf.clone()),
        Span::styled("_", Style::default().fg(Color::Green)),
    ]);
    frame.render_widget(Paragraph::new(input), row_layout[1]);

    // Hint row.
    let hint = if is_question && !options.is_empty() {
        "number: pick    type + enter: submit    esc: cancel"
    } else if is_question {
        "type + enter: submit    esc: cancel"
    } else if app.dialog_ask {
        "type a question + enter: ask the model    esc: back to y/n"
    } else {
        "y / n    ?: ask the model about this action    esc: cancel    (or type a custom answer)"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        row_layout[2],
    );
    let _ = app;
    let _ = inner_w;
}

/// Pretty-print a free-form preview (approval diff bodies). Highlights
/// Justification/Risk labels and edit-style `--- remove ---` / `+++ insert
/// +++` blocks, plus unified diff markers.
fn preview_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut out: Vec<Line<'static>> = Vec::new();
    // mode: 0 = normal, 1 = inside a "--- remove ---" block, 2 = "+++ insert +++"
    let mut mode = 0u8;

    for line in text.lines() {
        let t = line.trim();
        if t.eq_ignore_ascii_case("--- remove ---") || t.starts_with("--- remove ") {
            mode = 1;
            push_span_line(&mut out, "  ─ remove ─", Color::Red, width, true);
            continue;
        }
        if t.eq_ignore_ascii_case("+++ insert +++") || t.starts_with("+++ insert ") {
            mode = 2;
            push_span_line(&mut out, "  ─ insert ─", Color::Green, width, true);
            continue;
        }
        if t.starts_with("Justification:") {
            mode = 0;
            push_span_line(&mut out, t, Color::Cyan, width, false);
            continue;
        }
        if t.starts_with("Q:") || t.starts_with("A:") {
            push_span_line(
                &mut out,
                t,
                if t.starts_with("Q:") {
                    Color::Yellow
                } else {
                    Color::Green
                },
                width,
                false,
            );
            continue;
        }
        if t.starts_with("Risk:") {
            mode = 0;
            push_span_line(&mut out, t, Color::Yellow, width, false);
            continue;
        }
        let mut color = Color::White;
        let kind: u8 = 0;
        if mode == 1 {
            color = Color::Red;
        } else if mode == 2 {
            color = Color::Green;
        } else if t.starts_with("+++")
            || t.starts_with("---")
            || t.starts_with("@@")
            || t.starts_with("diff ")
            || t.starts_with("index ")
        {
            color = Color::DarkGray;
        } else if let Some(rest) = t.strip_prefix('+') {
            color = Color::Green;
            push_span_line(&mut out, rest, color, width, true);
            continue;
        } else if let Some(rest) = t.strip_prefix('-') {
            color = Color::Red;
            push_span_line(&mut out, rest, color, width, true);
            continue;
        }
        let _ = kind;
        push_span_line(&mut out, t, color, width, mode != 0);
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

fn push_span_line(
    out: &mut Vec<Line<'static>>,
    text: &str,
    color: Color,
    width: usize,
    hard: bool,
) {
    let styled = |s: String| Span::styled(s, Style::default().fg(color));
    if hard {
        for cut in hard_cut(text, width) {
            out.push(Line::from(vec![styled(cut)]));
        }
    } else {
        for wrapped in plain_wrap(text, width) {
            out.push(Line::from(vec![styled(wrapped)]));
        }
    }
}

// ---------------------------------------------------------------------------
// markdown
// ---------------------------------------------------------------------------

/// A styled text token (no ratatui dependency in the pure core).
#[derive(Clone)]
struct Tok {
    text: String,
    style: Style,
}

fn tok(text: impl Into<String>, style: Style) -> Tok {
    Tok {
        text: text.into(),
        style,
    }
}

fn base_style() -> Style {
    Style::default().fg(Color::White)
}

/// Wrap a plain string into lines of at most `width` chars.
fn plain_wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn hard_cut(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    line.chars()
        .collect::<Vec<char>>()
        .chunks(width)
        .map(|c| c.iter().collect())
        .collect()
}

/// Parse markdown into wrapped, styled token lines.
fn md_tok_lines(md: &str, width: usize) -> Vec<Vec<Tok>> {
    let width = width.max(8);
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<Vec<Tok>> = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if trimmed.starts_with("```") {
            i += 1;
            let mut code = Vec::new();
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1; // closing fence
            }
            for line in code {
                for cut in hard_cut(line, width) {
                    out.push(vec![tok(cut, Style::default().fg(Color::Cyan))]);
                }
            }
            continue;
        }
        if let Some(level) = heading_level(trimmed) {
            let text = trimmed[level..].trim();
            let style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            for cut in plain_wrap(text, width) {
                out.push(vec![tok(cut, style)]);
            }
            i += 1;
            continue;
        }
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            let bar: String = "-".repeat(width.min(40));
            out.push(vec![tok(bar, Style::default().fg(Color::DarkGray))]);
            i += 1;
            continue;
        }
        if let Some(rest) = bullet(trimmed) {
            let indent = raw.len() - raw.trim_start().len();
            let prefix = if indent == 0 { "- " } else { "  " };
            for toks in wrap_toks(&inline_toks(rest, base_style()), width.saturating_sub(2)) {
                let mut line = vec![tok(prefix, Style::default().fg(Color::Yellow))];
                line.extend(toks);
                out.push(line);
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with('>') {
            let body = trimmed.trim_start_matches('>').trim();
            let toks = vec![tok("| ", Style::default().fg(Color::DarkGray))];
            for t in wrap_toks(&inline_toks(body, base_style()), width.saturating_sub(2)) {
                let mut line = toks.clone();
                line.extend(t);
                out.push(line);
            }
            i += 1;
            continue;
        }
        // plain paragraph: gather consecutive plain lines
        let mut para = String::new();
        while i < lines.len() {
            let t = lines[i].trim();
            if t.is_empty()
                || t.starts_with('#')
                || t.starts_with("```")
                || t == "---"
                || t.starts_with('>')
                || bullet(t).is_some()
            {
                break;
            }
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(t);
            i += 1;
        }
        for toks in wrap_toks(&inline_toks(&para, base_style()), width) {
            out.push(toks);
        }
    }
    out
}

/// Render a markdown string into wrapped, styled lines.
fn md_to_lines(md: &str, width: usize) -> Vec<Vec<Span<'static>>> {
    md_tok_lines(md, width)
        .into_iter()
        .map(tok_line_to_spans)
        .collect()
}

fn heading_level(s: &str) -> Option<usize> {
    let level = s.chars().take_while(|&c| c == '#').count();
    if level >= 1 && level <= 6 && s.len() > level && s.as_bytes()[level] == b' ' {
        Some(level)
    } else {
        None
    }
}

fn bullet(s: &str) -> Option<&str> {
    s.strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("+ "))
}

fn tok_line_to_spans(line: Vec<Tok>) -> Vec<Span<'static>> {
    line.into_iter()
        .map(|t| Span::styled(t.text, t.style))
        .collect()
}

/// Split inline markdown into styled tokens.
fn inline_toks(text: &str, base: Style) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let bold = rest.find("**");
        let code = rest.find('`');
        let em = rest.find('*');
        let pos = [bold, code, em].iter().flatten().copied().min();
        let Some(pos) = pos else {
            out.push(tok(rest.to_string(), base));
            break;
        };
        if pos > 0 {
            out.push(tok(rest[..pos].to_string(), base));
        }
        let tail = &rest[pos..];
        if let Some(inner) = tail.strip_prefix("**") {
            if let Some(end) = inner.find("**") {
                out.push(tok(
                    inner[..end].to_string(),
                    base.add_modifier(Modifier::BOLD),
                ));
                rest = &inner[end + 2..];
            } else {
                out.push(tok(tail.to_string(), base));
                break;
            }
        } else if let Some(inner) = tail.strip_prefix('`') {
            if let Some(end) = inner.find('`') {
                out.push(tok(
                    inner[..end].to_string(),
                    Style::default().fg(Color::Cyan),
                ));
                rest = &inner[end + 1..];
            } else {
                out.push(tok(tail.to_string(), base));
                break;
            }
        } else if let Some(inner) = tail.strip_prefix('*') {
            if let Some(end) = inner.find('*') {
                out.push(tok(
                    inner[..end].to_string(),
                    base.add_modifier(Modifier::ITALIC),
                ));
                rest = &inner[end + 1..];
            } else {
                out.push(tok(tail.to_string(), base));
                break;
            }
        } else {
            out.push(tok(tail.to_string(), base));
            break;
        }
    }
    out
}

/// Wrap styled tokens into lines of at most `width` chars, word-aware.
fn wrap_toks(tokens: &[Tok], width: usize) -> Vec<Vec<Tok>> {
    let mut out = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let mut cur_len = 0usize;
    let mut first = true;

    for t in tokens {
        for word in t.text.split(' ') {
            if word.is_empty() {
                continue;
            }
            let need = if first { 0 } else { 1 };
            if cur_len + need + word.chars().count() > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_len = 0;
                cur.push(tok(word.to_string(), t.style));
                cur_len += word.chars().count();
            } else {
                if !first {
                    cur.push(tok(" ".to_string(), Style::default()));
                    cur_len += 1;
                }
                cur.push(tok(word.to_string(), t.style));
                cur_len += word.chars().count();
            }
            first = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(vec![]);
    }
    out
}

/// Plain-text version of [`md_to_lines`] (used by tests/measuring).
#[cfg(test)]
fn md_text(md: &str, width: usize) -> Vec<String> {
    md_tok_lines(md, width)
        .into_iter()
        .map(|line| line.into_iter().map(|t| t.text).collect())
        .collect()
}

/// Remove ReAct scaffolding (Thought/Tool/Args/Justification/Risk lines and the
/// multi-line Args JSON) so only human-readable content is rendered.
fn strip_react_scaffolding(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut depth: i32 = 0; // brace/bracket depth while inside Args JSON
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if depth > 0 {
            let opens = (trimmed.matches('{').count() + trimmed.matches('[').count()) as i32;
            let closes = (trimmed.matches('}').count() + trimmed.matches(']').count()) as i32;
            depth += opens - closes;
            continue;
        }
        if [
            "Thought:",
            "Tool:",
            "Justification:",
            "Risk:",
            "Observation:",
            "Final:",
        ]
        .iter()
        .any(|m| trimmed.starts_with(m))
        {
            continue;
        }
        if trimmed.starts_with("Args:") {
            let opens = (trimmed.matches('{').count() + trimmed.matches('[').count()) as i32;
            let closes = (trimmed.matches('}').count() + trimmed.matches(']').count()) as i32;
            depth = opens - closes;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_prefix_shared() {
        assert_eq!(
            common_prefix("move-block-down", "move-block-up"),
            "move-block-"
        );
        assert_eq!(common_prefix("abc", "abd"), "ab");
        assert_eq!(common_prefix("quit", "quit"), "quit");
        assert_eq!(common_prefix("backward-word", "beginning-of-line"), "b");
        assert_eq!(common_prefix("kill-word", ""), "");
    }

    #[test]
    fn mx_complete_fills_shared_prefix() {
        let mut mx = Mx::open();
        // Empty query: Tab must not yank the first command in — the full list
        // stays visible (emacs shows the completion list instead).
        mx.complete();
        assert_eq!(mx.query, "");
        assert_eq!(mx.matches.len(), MxCommand::ALL.len());
        // Narrow to the two "move-block-*" commands and complete the prefix.
        mx.query = "move-b".to_string();
        mx.refresh();
        assert_eq!(mx.matches.len(), 2);
        mx.complete();
        assert_eq!(mx.query, "move-block-");
        assert_eq!(mx.matches.len(), 2);
        // Disambiguate: a single match completes to its full name.
        mx.query = "move-block-d".to_string();
        mx.refresh();
        assert_eq!(mx.matches.len(), 1);
        mx.complete();
        assert_eq!(mx.query, "move-block-down");
    }

    #[test]
    fn strips_react_scaffolding() {
        let md = "Thought: I will list\nTool: list_dir\nArgs: { \"path\": \".\" }\nJustification: find files\nRisk: none\n\nHere is the **answer**.";
        let out = strip_react_scaffolding(md);
        assert!(!out.contains("Thought:"));
        assert!(!out.contains("Tool:"));
        assert!(!out.contains("Args:"));
        assert!(!out.contains("Justification:"));
        assert!(out.contains("Here is the"));
        assert!(out.contains("**answer**"));
    }

    #[test]
    fn strips_multiline_args_json() {
        let md =
            "Tool: apply_edit\nArgs: {\n  \"old\": \"a\",\n  \"new\": \"b\"\n}\nThen the rest.";
        let out = strip_react_scaffolding(md);
        assert!(!out.contains("old"));
        assert!(!out.contains("new"));
        assert!(!out.contains("Args:"));
        assert!(!out.contains("apply_edit"));
        assert!(out.contains("rest"));
    }

    #[test]
    fn markdown_headings_and_code() {
        let lines = md_text("# Title\n\n```rs\nfn main() {}\n```\nplain", 40);
        let flat = lines.join("\n");
        assert!(flat.contains("Title"), "{flat}");
        assert!(flat.contains("fn main() {}"), "{flat}");
        assert!(flat.contains("plain"), "{flat}");
    }

    #[test]
    fn chat_title_shows_the_session_name() {
        // The panel reads the session name, padded like the other titles.
        assert_eq!(chat_title("my task", 40), " my task ");
        // The default session name is shown until the first prompt names it.
        assert_eq!(chat_title("New session", 40), " New session ");
    }

    #[test]
    fn chat_title_is_truncated_to_fit_the_panel() {
        // A long name must never eat the block's right border: at most
        // width - 2 cells, with the cap's ellipsis.
        let title = chat_title(&"x".repeat(80), 20);
        assert!(title.chars().count() <= 18, "{title}");
        assert!(title.ends_with('…'), "{title}");
        // Empty title falls back to the old placeholder.
        assert_eq!(chat_title("   ", 40), " chat ");
    }

    #[test]
    fn user_band_pads_row_to_width_keeping_rule() {
        let row = RenderRow {
            rule: Some(Color::Cyan),
            spans: vec![Span::raw("hi")],
            tool_header: None,
        };
        let line = render_row_line(&row, false, false, Some(Color::Rgb(1, 2, 3)), 10);
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // prefix "| " + "hi" + pad to the span-area width (10): 12 cells total,
        // which equals the chat area width for rows with a 2-cell gutter.
        assert_eq!(flat, "| hi        ", "{flat}");
        assert_eq!(flat.chars().count(), 12);
    }

    #[test]
    fn unbanded_row_is_not_padded() {
        let row = RenderRow {
            rule: None,
            spans: vec![Span::raw("hi")],
            tool_header: None,
        };
        let line = render_row_line(&row, false, false, None, 10);
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(flat, "  hi", "{flat}");
    }

    #[test]
    fn selected_row_swaps_rule_for_marker() {
        let row = RenderRow {
            rule: Some(Color::Cyan),
            spans: vec![Span::raw("hi")],
            tool_header: None,
        };
        let line = render_row_line(&row, true, false, None, 10);
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(flat, "> hi", "{flat}");
    }

    #[test]
    fn user_band_bg_lightens_the_chat_base() {
        let Color::Rgb(r, g, b) = user_band_bg() else {
            panic!("user band must be an rgb color");
        };
        // Every channel sits strictly above the assumed chat background.
        assert!(r > DIFF_BASE_BG.0 && g > DIFF_BASE_BG.1 && b > DIFF_BASE_BG.2);
    }

    #[test]
    fn wraps_long_paragraph() {
        let text = "word ".repeat(50);
        let lines = md_text(&text, 20);
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(l.chars().count() <= 20, "line too wide: {l:?}");
        }
    }

    #[test]
    fn home_path_tilde_abbreviates_under_home() {
        let home = std::path::Path::new("/home/alice");
        assert_eq!(
            home_path_with(std::path::Path::new("/home/alice/code/comrade"), Some(home)),
            "~/code/comrade"
        );
        // The home directory itself collapses to a bare tilde.
        assert_eq!(
            home_path_with(std::path::Path::new("/home/alice"), Some(home)),
            "~"
        );
    }

    #[test]
    fn home_path_stays_absolute_outside_home() {
        let home = std::path::Path::new("/home/alice");
        assert_eq!(
            home_path_with(std::path::Path::new("/srv/other/proj"), Some(home)),
            "/srv/other/proj"
        );
        // No $HOME set: fall back to the absolute path unchanged.
        assert_eq!(home_path_with(std::path::Path::new("/x/y"), None), "/x/y");
    }

    #[test]
    fn visible_block_navigation_wraps_at_edges() {
        // user(0), assistant(1), user(2), assistant(3) — all visible expanded.
        let chat = vec![
            Msg::authored(MsgKind::User, "you", "a"),
            Msg::authored(MsgKind::Assistant, "assistant", "r0"),
            Msg::authored(MsgKind::User, "you", "b"),
            Msg::authored(MsgKind::Assistant, "assistant", "r1"),
        ];
        let none = &[false, false];
        assert_eq!(step_visible(&chat, none, None, 1), Some(0));
        assert_eq!(step_visible(&chat, none, None, -1), Some(3));
        // Past an edge there is no visible block: no move.
        assert_eq!(step_visible(&chat, none, Some(0), -1), None);
        assert_eq!(step_visible(&chat, none, Some(3), 1), None);
        assert_eq!(step_visible(&chat, none, Some(1), 1), Some(2));
        assert_eq!(step_visible(&[], &[], Some(0), 1), None);
    }

    #[test]
    fn visible_block_navigation_skips_collapsed_interiors() {
        // Exchange 0 (idx 0..1) collapsed: only its heading is navigable, so
        // stepping down from it lands on exchange 1's heading (idx 2), and up
        // from that heading lands back on idx 0.
        let chat = vec![
            Msg::authored(MsgKind::User, "you", "a"),
            Msg::authored(MsgKind::Assistant, "assistant", "r0"),
            Msg::authored(MsgKind::User, "you", "b"),
            Msg::authored(MsgKind::Assistant, "assistant", "r1"),
        ];
        let collapsed = [true, false];
        assert_eq!(step_visible(&chat, &collapsed, Some(0), 1), Some(2));
        assert_eq!(step_visible(&chat, &collapsed, Some(2), -1), Some(0));
        assert_eq!(step_visible(&chat, &collapsed, Some(1), -1), Some(0));
        // chat_visible: headings always exposed; interiors only when expanded.
        assert!(chat_visible(&chat, &collapsed, 0));
        assert!(!chat_visible(&chat, &collapsed, 1));
        assert!(chat_visible(&chat, &collapsed, 2));
        assert!(chat_visible(&chat, &collapsed, 3));
        assert!(!chat_visible(&chat, &collapsed, 99));
    }

    #[test]
    fn user_navigation_steps_between_users() {
        // chat indices of user messages: 0 and 4 (tool/assistant in between)
        let users = vec![0usize, 4usize];
        assert_eq!(step_user(&users, None, 1), Some(0));
        assert_eq!(step_user(&users, None, -1), Some(4));
        assert_eq!(step_user(&users, Some(0), 1), Some(4));
        assert_eq!(step_user(&users, Some(0), -1), None); // already at first user
        assert_eq!(step_user(&users, Some(4), 1), None); // already at last user
        assert_eq!(step_user(&users, Some(4), -1), Some(0));
        assert_eq!(step_user(&[], Some(0), 1), None);
    }

    // --- run folding ------------------------------------------------------

    fn tool_card(name: &str, args: &str, ok: bool, open: bool) -> Msg {
        Msg::tool(ToolCard {
            name: name.into(),
            author: Some("model".into()),
            args: args.into(),
            justification: None,
            risk: None,
            result: None,
            ok,
            open,
            started: None,
            taken_ms: None,
            tokens: None,
        })
    }

    fn kinds(chat: &[Msg]) -> Vec<MsgKind> {
        chat.iter().map(|m| m.kind).collect()
    }

    #[test]
    fn run_folding_compresses_tool_spans_between_prose() {
        let mut chat = vec![
            Msg::authored(MsgKind::User, "you", "do the thing"),
            Msg::authored(MsgKind::Reasoning, "model", "let me look"),
            tool_card("read_file", r#"{"path":"src/x.rs"}"#, true, false),
            tool_card("apply_patch", r#"{"path":"src/x.rs"}"#, true, true),
            Msg::text(MsgKind::Meta, "all 3 tests passed"),
            Msg::authored(MsgKind::Assistant, "model", "done"),
            tool_card("run_task", r#"{"task":"fmt"}"#, true, false),
            Msg::authored(MsgKind::Assistant, "model", "also formatted"),
        ];
        let spans = fold_completed_runs(&mut chat);
        assert_eq!(spans, vec![(1, 4), (6, 1)]);
        assert_eq!(
            kinds(&chat),
            vec![
                MsgKind::User,
                MsgKind::Run,
                MsgKind::Assistant,
                MsgKind::Run,
                MsgKind::Assistant,
            ]
        );
        // Reasoning + read + apply_patch + the status note ride inside the
        // digest; nothing is lost.
        assert_eq!(chat[1].children.len(), 4);
        assert_eq!(chat[3].children.len(), 1);
        assert_eq!(chat[3].children[0].kind, MsgKind::Tool);
    }

    #[test]
    fn run_folding_skips_pure_meta_and_reasoning() {
        // A lone grey note ("run finished") is not an action stretch and stays
        // visible as-is.
        let mut chat = vec![
            Msg::authored(MsgKind::User, "you", "hi"),
            Msg::text(MsgKind::Meta, "waiting for your input"),
            Msg::authored(MsgKind::Assistant, "model", "ok"),
            Msg::authored(MsgKind::Reasoning, "model", "thinking only"),
        ];
        assert!(fold_completed_runs(&mut chat).is_empty());
        assert_eq!(
            kinds(&chat),
            vec![
                MsgKind::User,
                MsgKind::Meta,
                MsgKind::Assistant,
                MsgKind::Reasoning,
            ]
        );
    }

    #[test]
    fn error_notes_stay_visible_outside_the_digest() {
        // An interrupted run must not bury the "why it stopped" note inside the
        // folded digest: the error line acts as a boundary and stays on screen.
        let mut chat = vec![
            Msg::authored(MsgKind::User, "you", "go"),
            tool_card("apply_patch", "{}", true, false),
            Msg::text(MsgKind::Meta, "error: agent interrupted by user"),
        ];
        fold_completed_runs(&mut chat);
        assert_eq!(
            kinds(&chat),
            vec![MsgKind::User, MsgKind::Run, MsgKind::Meta]
        );
        assert!(chat[2].text.starts_with("error:"));
        assert_eq!(chat[1].children.len(), 1);
    }

    #[test]
    fn run_folding_auto_collapses_stale_open_bodies() {
        let mut chat = vec![
            tool_card("apply_patch", r#"{"path":"a"}"#, true, true),
            Msg::text(MsgKind::Meta, "approved"),
            tool_card("run_task", r#"{"task":"t"}"#, false, true),
        ];
        fold_completed_runs(&mut chat);
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].kind, MsgKind::Run);
        let children = &chat[0].children;
        assert!(!children[0].tool.as_ref().unwrap().open);
        assert!(!children[2].tool.as_ref().unwrap().open);
    }

    #[test]
    fn unfolded_run_restores_the_original_messages() {
        let mut chat = vec![
            Msg::authored(MsgKind::User, "you", "go"),
            tool_card("rgrep", r#"{"pattern":"x"}"#, true, false),
            tool_card("run_tests", "[]", true, true),
            Msg::authored(MsgKind::Assistant, "model", "done"),
        ];
        fold_completed_runs(&mut chat);
        assert_eq!(chat[1].kind, MsgKind::Run);
        let folded = chat.clone();
        unfold_run(&mut chat, 1);
        assert_eq!(kinds(&chat).len(), 4);
        assert_eq!(chat[1].kind, MsgKind::Tool);
        assert_eq!(chat[1].children.len(), 0);
        // Children come back in order; the digest itself is gone.
        let back: Vec<MsgKind> = folded[1].children.iter().map(|m| m.kind).collect();
        assert_eq!(back, vec![MsgKind::Tool, MsgKind::Tool]);
        // Unfolding a non-digest is a no-op.
        let mut chat2 = chat.clone();
        unfold_run(&mut chat2, 0);
        assert_eq!(chat2.len(), chat.len());
    }

    #[test]
    fn run_digest_counts_calls_and_aggregates_reads_into_actions() {
        let children = vec![
            tool_card("read_file", "{}", true, false),
            tool_card("rgrep", "{}", true, false),
            tool_card("rgrep", "{}", true, false),
            tool_card("apply_patch", "{}", true, false),
            tool_card("run_tests", "{}", true, false),
        ];
        let d = run_digest(&children);
        assert_eq!(d.calls, 5);
        assert!(d.ok);
        assert_eq!(d.failed, 0);
        // Every tool is in the summary now — reads are aggregated, not dropped,
        // and repeated lookups fold into one countable item (rgrep ×2).
        assert!(d.actions.contains("read_file"), "{}", d.actions);
        assert!(d.actions.contains("rgrep ×2"), "{}", d.actions);
        assert!(d.actions.contains("apply_patch"), "{}", d.actions);
        assert!(d.actions.contains("run_tests"), "{}", d.actions);

        let failing = vec![tool_card("run_task", "{}", false, false)];
        let d = run_digest(&failing);
        assert!(!d.ok);
        assert_eq!(d.failed, 1);

        // Repeated actions are counted (×n).
        let repeated = vec![
            tool_card("apply_patch", "{}", true, false),
            tool_card("apply_patch", "{}", true, false),
        ];
        assert!(run_digest(&repeated).actions.contains("apply_patch ×2"));
    }

    #[test]
    fn folded_run_is_searchable_and_copied_through_its_children() {
        let children = vec![tool_card("run_tests", r#"{"args":"tests"}"#, false, false)];
        let run = Msg::run(children);
        let searchable = msg_searchable(&run);
        assert!(searchable.contains("run_tests"));
        assert!(msg_matches(&run, "run_tests"));
        // Folded content is found even though only the digest row renders.
        assert!(msg_matches(&run, "tests"));
    }

    #[test]
    fn run_digest_renders_as_one_clickable_header_row() {
        let children = vec![
            tool_card("read_file", "{}", true, false),
            tool_card("apply_patch", "{}", true, false),
        ];
        let mut out = Vec::new();
        layout_run(&mut out, 7, &children);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool_header, Some(7));
        let flat: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(flat.contains("task run"), "{flat}");
        // The header names the tools (reads aggregated in, never a bare
        // "N calls" count) and marks the outcome.
        assert!(flat.contains("apply_patch"), "{flat}");
        assert!(flat.contains("read_file"), "{flat}");
        assert!(!flat.contains("calls"), "{flat}");
        assert!(flat.contains("✓"), "{flat}");
    }

    #[test]
    fn finished_tool_row_is_just_name_and_status() {
        // A finished call in a folded run renders as its task name + ✓/✗ and
        // duration — no request headline, no author, no inline result text.
        let card = ToolCard {
            name: "rgrep".into(),
            author: Some("model".into()),
            args: r#"{"pattern":"fold","glob":"*.rs"}"#.into(),
            justification: None,
            risk: None,
            result: Some("5 matches".into()),
            ok: true,
            open: false,
            started: Some(Instant::now()),
            taken_ms: Some(120),
            tokens: None,
        };
        let mut out = Vec::new();
        layout_tool(&mut out, 1, &card, 60);
        assert_eq!(out.len(), 1, "collapsed: header row only");
        let flat: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(flat.contains("rgrep"), "{flat}");
        assert!(flat.contains("✓"), "{flat}");
        assert!(flat.contains("120ms"), "{flat}");
        assert!(!flat.contains("fold"), "{flat}"); // no request preview
        assert!(!flat.contains("5 matches"), "{flat}"); // no result tail
        assert!(!flat.contains("model"), "{flat}"); // no author
        // Opening the card exposes the request and result again.
        let mut card = card;
        card.open = true;
        let mut out = Vec::new();
        layout_tool(&mut out, 1, &card, 60);
        let body: String = out
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(body.contains("args:"), "{body}");
        assert!(body.contains("5 matches"), "{body}");
    }

    // --- usage display: durations, tokens, reasoning default-open -----------

    /// A finished tool card carrying explicit timing/token data.
    fn timed_card(started: Instant, taken_ms: u128, tokens: Option<usize>) -> Msg {
        Msg::tool(ToolCard {
            name: "apply_patch".into(),
            author: Some("model".into()),
            args: "{}".into(),
            justification: None,
            risk: None,
            result: Some("done".into()),
            ok: true,
            open: false,
            started: Some(started),
            taken_ms: Some(taken_ms),
            tokens,
        })
    }

    #[test]
    fn durations_and_tokens_format_compactly() {
        assert_eq!(fmt_dur_ms(140), "140ms");
        assert_eq!(fmt_dur_ms(3_000), "3s");
        assert_eq!(fmt_dur_ms(3_450), "3.4s");
        assert_eq!(fmt_dur_ms(125_000), "2m 5s");
        assert_eq!(fmt_dur_ms(60_000), "1m");
        assert_eq!(fmt_tokens(231), "231");
        assert_eq!(fmt_tokens(1_000), "1k");
        assert_eq!(fmt_tokens(1_234), "1.2k");
        assert_eq!(fmt_tokens(3_400_000), "3.4M");
    }

    #[test]
    fn run_usage_spans_first_to_last_call_and_sums_tokens() {
        let t0 = Instant::now();
        let children = vec![
            timed_card(t0, 1_000, Some(100)),
            timed_card(t0 + Duration::from_millis(2_000), 500, Some(200)),
        ];
        let (ms, tokens) = run_usage(&children);
        assert_eq!(ms, Some(2_500));
        assert_eq!(tokens, 300);
        // No finished call: no timing; no reported usage: no tokens.
        let bare = vec![tool_card("apply_patch", "{}", true, false)];
        assert_eq!(run_usage(&bare), (None, 0));
    }

    #[test]
    fn digest_row_shows_time_and_tokens() {
        let t0 = Instant::now();
        let children = vec![
            timed_card(t0, 3_450, Some(1_234)),
            timed_card(t0 + Duration::from_millis(1_000), 1_000, None),
        ];
        let mut out = Vec::new();
        layout_run(&mut out, 3, &children);
        let flat: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(flat.contains("1.2k tok"), "{flat}");
        assert!(flat.contains("3.4s"), "{flat}");
    }

    #[test]
    fn run_digest_credits_the_delegate_not_the_handoff() {
        // A folded delegation stretch: the parent's `delegate` hand-off card is
        // authored by the main model, the delegate's own tools by the delegate.
        // The digest header must attribute the run to the delegate model.
        fn card(name: &str, author: Option<&str>) -> Msg {
            Msg::tool(ToolCard {
                name: name.into(),
                author: author.map(String::from),
                args: "{}".into(),
                justification: None,
                risk: None,
                result: Some("done".into()),
                ok: true,
                open: false,
                started: Some(Instant::now()),
                taken_ms: Some(10),
                tokens: None,
            })
        }
        let children = vec![
            card("delegate", Some("ollama/lead")),
            card("apply_patch", Some("mistral")),
            card("run_tests", Some("mistral")),
        ];
        let mut out = Vec::new();
        layout_run(&mut out, 3, &children);
        let flat: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(flat.contains("mistral"), "{flat}");
        assert!(!flat.contains("ollama/lead"), "{flat}");
    }

    #[test]
    fn card_header_shows_duration_always_and_tokens_when_open() {
        let row_text = |open: bool| -> String {
            let mut card = ToolCard {
                name: "run_tests".into(),
                author: Some("model".into()),
                args: "{}".into(),
                justification: None,
                risk: None,
                result: Some("3 passed".into()),
                ok: true,
                open,
                started: Some(Instant::now()),
                taken_ms: Some(3_450),
                tokens: Some(1_234),
            };
            card.open = open;
            let mut out = Vec::new();
            layout_tool(&mut out, 1, &card, 60);
            out[0].spans.iter().map(|s| s.content.as_ref()).collect()
        };
        // Collapsed: duration visible, tokens hidden.
        let closed = row_text(false);
        assert!(closed.contains("3.4s"), "{closed}");
        assert!(!closed.contains("tok"), "{closed}");
        // Open: both visible.
        let open = row_text(true);
        assert!(open.contains("3.4s"), "{open}");
        assert!(open.contains("1.2k tok"), "{open}");
    }

    #[test]
    fn reasoning_is_open_by_default_and_survives_folding() {
        let m = Msg::reasoning("model", "let me check the layout code");
        assert_eq!(m.kind, MsgKind::Reasoning);
        assert!(m.open);
        // Folding a stretch keeps reasoning expanded (only tool/failure cards
        // are force-collapsed).
        let mut chat = vec![
            Msg::reasoning("model", "think"),
            tool_card("apply_patch", "{}", true, true),
        ];
        fold_completed_runs(&mut chat);
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].kind, MsgKind::Run);
        let children = &chat[0].children;
        assert!(
            children[0].open,
            "reasoning must stay expanded after folding"
        );
        assert!(
            !children[1].tool.as_ref().unwrap().open,
            "tool cards are still force-collapsed"
        );
    }

    #[test]
    fn delegate_reply_is_parsed_into_model_and_text() {
        let out = "delegate mistral (ollama/mistral:7b) replied:\nHere is the code:\n```rust\nfn x() {}\n```\nVERIFICATION: passes";
        let (model, reply) = parse_delegate_reply(out).unwrap();
        assert_eq!(model, "mistral");
        assert!(reply.contains("fn x() {}"));
        assert!(reply.contains("VERIFICATION: passes"));
        // A fix round uses the very same envelope.
        let fix = format!("delegate mistral (mistral) replied:\n{out}");
        assert_eq!(parse_delegate_reply(&fix).unwrap().0, "mistral");
    }

    #[test]
    fn delegate_failure_output_is_not_a_reply() {
        assert!(parse_delegate_reply("ERROR: delegate mistral failed").is_none());
        assert!(parse_delegate_reply("").is_none());
        assert!(parse_delegate_reply("delegate replied:\nno name").is_none());
    }

    #[test]
    fn delegate_panel_rows_wrap_inside_the_panel_width() {
        // A delegate whose label + model + description is far wider than the
        // model panel must be wrapped, never left to spill past the edge.
        let mut d = DelegateCfg::default();
        d.name = "mistral".into();
        d.llm.model = "ollama/mistral:7b".into();
        d.description =
            "Cheap and fast, good for basic coding tasks and summarising long outputs.".into();
        let rows = delegate_panel_rows(&[d], 24).expect("rows for a configured delegate");
        assert_eq!(rows[0].width(), 10); // "delegates:"
        let joined: String = rows[1..]
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            rows.len() >= 4,
            "a description wider than the panel must wrap onto several rows"
        );
        // Wrapping may split the blurb across rows, but must never drop a word.
        for word in [
            "mistral",
            "ollama/mistral:7b",
            "Cheap",
            "summarising",
            "outputs.",
        ] {
            assert!(joined.contains(word), "lost {word} in {joined:?}");
        }
        for row in &rows {
            assert!(
                usize::from(row.width()) <= 24,
                "delegate row wider than the panel: {:?}",
                row
            );
        }
        // No delegate configured → the panel keeps its legacy fixed height.
        assert!(delegate_panel_rows(&[], 24).is_none());
    }

    #[test]
    fn reasoning_keeps_prose_and_drops_scaffolding() {
        let raw = "Thought: I will list the files\nTool: list_dir\nArgs: { \"path\": \".\" }\n\nLet me inspect the layout.";
        let got = reasoning_from_stream(raw).unwrap();
        assert!(!got.contains("Thought:"));
        assert!(!got.contains("list_dir"));
        assert!(!got.contains("Args:"));
        assert!(got.contains("inspect the layout"));
        assert!(
            reasoning_from_stream("Thought: just scaffolding\nTool: run_tests\nArgs: {}").is_none()
        );
        assert!(reasoning_from_stream("").is_none());
    }
}

#[cfg(test)]
mod test_parse_tests {
    use super::*;

    #[test]
    fn parses_counts_duration_and_failures() {
        let text = "\
test result: FAILED. 11 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.34s

failures:
    tests::bar

---- tests::bar stdout ----
thread 'tests::bar' panicked at src/lib.rs:10:5:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` for a backtrace
";
        let s = parse_test_summary(text);
        assert_eq!(s.passed, 11);
        assert_eq!(s.failed, 1);
        assert_eq!(s.duration, "2.34s");
        assert_eq!(s.cases.len(), 1);
        let (name, detail) = &s.cases[0];
        assert_eq!(name, "tests::bar");
        assert!(detail.contains("panicked at"), "{detail}");
        assert!(detail.contains("left: 1"), "{detail}");
        assert!(!detail.contains("note:"), "{detail}");
    }

    #[test]
    fn parses_passing_run() {
        let text = "test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s\n";
        let s = parse_test_summary(text);
        assert_eq!(s.passed, 4);
        assert_eq!(s.failed, 0);
        assert!(s.cases.is_empty());
    }
}

// ---------------------------------------------------------------------------
// side-by-side diff rendering for edit tools
// ---------------------------------------------------------------------------

/// A single `@@ -a,b +c,d @@` hunk header token inside `text`, if any.
fn hunk_token(text: &str) -> Option<String> {
    let start = text.find("@@ ")?;
    let rest = &text[start..];
    let end = rest.find(" @@")?;
    Some(rest[..end + 3].to_string())
}

/// Header label for an edit tool's diff rows: the edited file plus the hunk
/// line numbers. For `apply_edit` the numbers come from the tool result (which
/// now reports `(@@ -a,b +c,d @@)`); for `apply_patch` they are parsed from its
/// own `@@` header. Falls back to plain `diff:` when nothing is derivable.
fn edit_diff_label(name: &str, args_json: &str, result: Option<&str>) -> String {
    let value: Option<serde_json::Value> = serde_json::from_str(args_json).ok();
    let pick_path = |keys: &[&str]| -> Option<String> {
        let map = value.as_ref()?.as_object()?;
        for k in keys {
            if let Some(s) = map.get(*k).and_then(serde_json::Value::as_str) {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            }
        }
        None
    };
    let (rel, hunk) = match name {
        "apply_edit" => (pick_path(&["path", "file"]), result.and_then(hunk_token)),
        "apply_patch" => {
            let diff = value
                .as_ref()
                .and_then(|v| v.get("diff"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let files: Vec<&str> = diff
                .lines()
                .filter_map(|l| l.strip_prefix("+++ b/"))
                .map(str::trim)
                .collect();
            let rel = match files.as_slice() {
                [f] => Some((*f).to_string()),
                // Multi-file patches: don't pin the hunk numbers to one file.
                _ => None,
            };
            (
                rel,
                if files.len() == 1 {
                    hunk_token(diff)
                } else {
                    None
                },
            )
        }
        _ => (None, None),
    };
    match (rel, hunk) {
        (Some(rel), Some(hunk)) => format!("diff  {rel}  {hunk}"),
        (Some(rel), None) => format!("diff  {rel}"),
        (None, Some(hunk)) => format!("diff  {hunk}"),
        (None, None) => "diff:".to_string(),
    }
}

/// Pull the changed line sequences out of an edit tool's JSON args:
/// `(removed, added)`.
fn extract_diff_sides(name: &str, args_json: &str) -> Option<(Vec<String>, Vec<String>)> {
    let value: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    match name {
        "apply_patch" => {
            let diff = value.get("diff")?.as_str()?;
            for line in diff.lines() {
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    continue;
                } else if line.starts_with('+') {
                    added.push(line[1..].to_string());
                } else if line.starts_with('-') {
                    removed.push(line[1..].to_string());
                }
            }
        }
        "apply_edit" => {
            let old = value.get("old")?.as_str()?;
            let new = value.get("new")?.as_str()?;
            removed.extend(old.lines().map(str::to_string));
            added.extend(new.lines().map(str::to_string));
        }
        _ => return None,
    }
    Some((removed, added))
}

/// Longest-common-subsequence alignment of two line lists. Returns pairs where
/// `Some` = a removed / added line, `None` = a gap on that side. Lines that are
/// unchanged are dropped (compact diff).
fn lcs_pairs(a: &[String], b: &[String]) -> Vec<(Option<String>, Option<String>)> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut pairs: Vec<(Option<String>, Option<String>)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            pairs.push((Some(a[i].clone()), None));
            i += 1;
        } else {
            pairs.push((None, Some(b[j].clone())));
            j += 1;
        }
    }
    while i < n {
        pairs.push((Some(a[i].clone()), None));
        i += 1;
    }
    while j < m {
        pairs.push((None, Some(b[j].clone())));
        j += 1;
    }
    // Merge an adjacent removed-then-added (or added-then-removed) pair into a
    // single side-by-side row: one line replaced by another.
    let mut merged: Vec<(Option<String>, Option<String>)> = Vec::with_capacity(pairs.len());
    let mut k = 0usize;
    while k < pairs.len() {
        if k + 1 < pairs.len() {
            let (a, b) = (&pairs[k], &pairs[k + 1]);
            let removed_then_added =
                a.0.is_some() && a.1.is_none() && b.0.is_none() && b.1.is_some();
            let added_then_removed =
                a.0.is_none() && a.1.is_some() && b.0.is_some() && b.1.is_none();
            if removed_then_added || added_then_removed {
                let left = a.0.clone().or_else(|| b.0.clone());
                let right = a.1.clone().or_else(|| b.1.clone());
                merged.push((left, right));
                k += 2;
                continue;
            }
        }
        merged.push(pairs[k].clone());
        k += 1;
    }
    merged
}

// ---------------------------------------------------------------------------
// diff backgrounds: translucent-looking red/green, blended over the chat bg so
// full-saturation colors don't flashbang the developer.
// ---------------------------------------------------------------------------

/// The assumed chat background the diff tints sit on (a dark neutral; terminals
/// cannot report their real theme color to ratatui).
const DIFF_BASE_BG: (u8, u8, u8) = (0x13, 0x14, 0x18);
/// Removal tint (red) and addition tint (green), at partial opacity.
const DIFF_TINT_REMOVED: (u8, u8, u8) = (0xff, 0x47, 0x47);
const DIFF_TINT_ADDED: (u8, u8, u8) = (0x3c, 0xd0, 0x6c);
const DIFF_TINT_ALPHA: f32 = 0.22;
/// How far the user-message band is lifted above the chat background. This is
/// the single knob to retune when the terminal theme changes (light terminals
/// will want a darker tint over a light base instead).
const USER_BG_ALPHA: f32 = 0.08;
/// Green tint under the prompt bar: the SLIME/REPL "you are typing here" band.
/// Darker text themes read the bar as slightly green-black.
const PROMPT_BG_TINT: (u8, u8, u8) = (0x1f, 0x9e, 0x6f);
const PROMPT_BG_ALPHA: f32 = 0.16;

/// Background for a user-message band: the chat base lightened by a touch of
/// white so a user turn reads as a soft org-mode body block. Blended with the
/// same technique as the diff tints above.
fn user_band_bg() -> Color {
    blend_rgb(DIFF_BASE_BG, (0xff, 0xff, 0xff), USER_BG_ALPHA)
}

/// Background of the prompt bar at the bottom of the chat.
fn prompt_bg() -> Color {
    blend_rgb(DIFF_BASE_BG, PROMPT_BG_TINT, PROMPT_BG_ALPHA)
}

fn blend_rgb(base: (u8, u8, u8), tint: (u8, u8, u8), alpha: f32) -> Color {
    let mix = |b: u8, t: u8| (b as f32 * (1.0 - alpha) + t as f32 * alpha).round() as u8;
    Color::Rgb(
        mix(base.0, tint.0),
        mix(base.1, tint.1),
        mix(base.2, tint.2),
    )
}

/// Soft background for removed (left) diff lines.
fn diff_remove_bg() -> Color {
    blend_rgb(DIFF_BASE_BG, DIFF_TINT_REMOVED, DIFF_TINT_ALPHA)
}

/// Soft background for added (right) diff lines.
fn diff_add_bg() -> Color {
    blend_rgb(DIFF_BASE_BG, DIFF_TINT_ADDED, DIFF_TINT_ALPHA)
}

const CODE_KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "struct", "enum", "trait", "impl", "for", "while", "if", "else",
    "match", "use", "mod", "crate", "self", "Self", "return", "async", "await", "const", "static",
    "type", "where", "move", "ref", "loop", "break", "continue", "true", "false", "None", "Some",
    "Ok", "Err", "unsafe", "dyn", "in", "as", "super", "fn",
];

/// Light lexical colorizer (strings, comments, numbers, keywords).
fn lex_code(line: &str) -> Vec<(String, Color)> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out: Vec<(String, Color)> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            let mut s = String::new();
            while i < n && chars[i].is_whitespace() {
                s.push(chars[i]);
                i += 1;
            }
            out.push((s, Color::White));
        } else if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let mut s = String::new();
            while i < n {
                s.push(chars[i]);
                i += 1;
            }
            out.push((s, Color::DarkGray));
        } else if c == '"' || c == '\'' {
            let quote = c;
            let mut s = String::new();
            s.push(chars[i]);
            i += 1;
            while i < n {
                s.push(chars[i]);
                if chars[i] == '\\' && i + 1 < n {
                    s.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push((s, Color::Yellow));
        } else if c.is_ascii_digit() {
            let mut s = String::new();
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                s.push(chars[i]);
                i += 1;
            }
            out.push((s, Color::Cyan));
        } else if c.is_alphabetic() || c == '_' {
            let mut s = String::new();
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                s.push(chars[i]);
                i += 1;
            }
            let color = if CODE_KEYWORDS.contains(&s.as_str()) {
                Color::Magenta
            } else {
                Color::White
            };
            out.push((s, color));
        } else {
            let mut s = String::new();
            while i < n && !chars[i].is_whitespace() && !chars[i].is_ascii_alphanumeric() {
                s.push(chars[i]);
                i += 1;
            }
            if s.is_empty() {
                s.push(chars[i]);
                i += 1;
            }
            out.push((s, Color::White));
        }
    }
    out
}

/// Render one diff cell to `width` chars, optionally with a background.
fn cell_spans(text: Option<&str>, bg: Option<Color>, width: usize) -> Vec<Span<'static>> {
    let text = text.unwrap_or("");
    let tokens = lex_code(text);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (tok, color) in tokens {
        let remaining = width.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let shown: String = tok.chars().take(remaining).collect();
        let count = shown.chars().count();
        let style = match bg {
            Some(bg) => Style::default().fg(color).bg(bg),
            None => Style::default().fg(color),
        };
        spans.push(Span::styled(shown, style));
        used += count;
    }
    let pad = width.saturating_sub(used);
    if pad > 0 {
        let style = match bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        spans.push(Span::styled(" ".repeat(pad), style));
    }
    spans
}

/// One aligned side-by-side row: removed (left, red bg) / added (right, green).
fn build_diff_row(
    left: Option<&str>,
    right: Option<&str>,
    width: usize,
    left_bg: Option<Color>,
    right_bg: Option<Color>,
) -> Vec<Span<'static>> {
    let w = width.max(8);
    let left_w = (w / 2).saturating_sub(1);
    let right_w = w.saturating_sub(left_w + 1);
    let mut spans = cell_spans(left, left_bg, left_w);
    spans.push(Span::styled(" ", Style::default()));
    spans.extend(cell_spans(right, right_bg, right_w));
    spans
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn apply_patch_extracts_sides() {
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n";
        let args = serde_json::json!({ "diff": diff }).to_string();
        let (old, new) = extract_diff_sides("apply_patch", &args).unwrap();
        assert_eq!(old, vec!["fn old() {}"]);
        assert_eq!(new, vec!["fn new() {}"]);
    }

    #[test]
    fn edit_diff_label_shows_file_and_hunk_numbers() {
        // apply_patch: file + @@ come from its own diff text.
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n-fn old() {}\n+fn new() {}\n";
        let args = serde_json::json!({ "diff": diff }).to_string();
        assert_eq!(
            edit_diff_label("apply_patch", &args, None),
            "diff  a.rs  @@ -1,3 +1,3 @@"
        );
        // apply_edit: file from args, numbers from the tool result token.
        let args = serde_json::json!({ "path": "src/lib.rs", "old": "a", "new": "b" }).to_string();
        assert_eq!(
            edit_diff_label(
                "apply_edit",
                &args,
                Some("Edited src/lib.rs: replaced 1 exact block (@@ -12,2 +12,3 @@).")
            ),
            "diff  src/lib.rs  @@ -12,2 +12,3 @@"
        );
        // No result yet: file only.
        assert_eq!(
            edit_diff_label("apply_edit", &args, None),
            "diff  src/lib.rs"
        );
        // Nothing derivable falls back to the plain label.
        assert_eq!(edit_diff_label("rgrep", "{}", None), "diff:");
    }

    #[test]
    fn hunk_token_finds_last_terminator() {
        assert_eq!(hunk_token("@@ -1 +1 @@"), Some("@@ -1 +1 @@".to_string()));
        assert_eq!(hunk_token("no hunk here"), None);
    }

    #[test]
    fn apply_edit_uses_old_new() {
        let old = "a\nb\n";
        let new = "a\nc\n";
        let args = format!(
            r#"{{"old":{},"new":{}}}"#,
            serde_json::to_string(old).unwrap(),
            serde_json::to_string(new).unwrap()
        );
        let (removed, added) = extract_diff_sides("apply_edit", &args).unwrap();
        assert_eq!(removed, vec!["a", "b"]);
        assert_eq!(added, vec!["a", "c"]);
    }

    #[test]
    fn lcs_aligns_changed_lines() {
        let old: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let new: Vec<String> = vec!["a".into(), "x".into(), "c".into()];
        let pairs = lcs_pairs(&old, &new);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0.as_deref(), Some("b"));
        assert_eq!(pairs[0].1.as_deref(), Some("x"));
    }

    #[test]
    fn cell_background_pads_to_width() {
        let spans = cell_spans(Some("ok"), Some(Color::Green), 10);
        let total: usize = spans.iter().map(|s| s.width()).sum();
        assert_eq!(total, 10);
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn search_step_wraps_around() {
        assert_eq!(search_step(Some((0, 3)), 1), Some(1));
        assert_eq!(search_step(Some((2, 3)), 1), Some(0));
        assert_eq!(search_step(Some((0, 3)), -1), Some(2));
        assert_eq!(search_step(Some((1, 3)), -1), Some(0));
        assert_eq!(search_step(Some((0, 0)), 1), None);
        assert_eq!(search_step(None, 1), None);
    }

    #[test]
    fn search_matches_plain_text_case_insensitively() {
        let msg = Msg::text(MsgKind::Assistant, "Hello World");
        assert!(msg_matches(&msg, "hello"));
        assert!(msg_matches(&msg, "WORLD"));
        assert!(!msg_matches(&msg, "nope"));
    }

    #[test]
    fn search_covers_tool_and_failure_cards() {
        let card = Msg::tool(ToolCard {
            name: "run_task".into(),
            author: Some("ollama/x".into()),
            args: r#"{"task":"test"}"#.into(),
            justification: Some("verify the suite".into()),
            risk: Some("none".into()),
            result: Some("3 passed".into()),
            ok: true,
            open: true,
            started: None,
            taken_ms: None,
            tokens: None,
        });
        assert!(msg_matches(&card, "run_task"));
        assert!(msg_matches(&card, "verify"));
        assert!(msg_matches(&card, "passed"));

        let fail = Msg::failure("flaky_test".into(), "assert left == right".into());
        assert!(msg_matches(&fail, "flaky"));
        assert!(msg_matches(&fail, "left == right"));
    }
}

// ---------------------------------------------------------------------------
// Mode-line git snapshot (background refresh)
// ---------------------------------------------------------------------------

async fn run_git(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Count newly added files (staged `A` or untracked `??`) and deleted files
/// (`D` in either the index or the work tree) from `git status --porcelain`.
fn status_file_counts(porcelain: &str) -> (u64, u64) {
    let mut added = 0u64;
    let mut deleted = 0u64;
    for line in porcelain.lines() {
        let mut chars = line.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            continue;
        };
        match (x, y) {
            ('?', '?') => added += 1,
            ('A', _) => added += 1,
            ('D', _) | (_, 'D') => deleted += 1,
            _ => {}
        }
    }
    (added, deleted)
}

/// Sum inserted/removed lines from `git diff --numstat` output; binary rows
/// (`-\t-`) are skipped.
fn numstat_totals(numstat: &str) -> (u64, u64) {
    let mut ins = 0u64;
    let mut del = 0u64;
    for line in numstat.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(a), Some(d)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let (Ok(a), Ok(d)) = (a.parse::<u64>(), d.parse::<u64>()) {
            ins += a;
            del += d;
        }
    }
    (ins, del)
}

async fn fetch_git_bar(root: &std::path::Path) -> GitBarInfo {
    let inside = run_git(root, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
    if !inside {
        return GitBarInfo::default();
    }
    let branch = run_git(root, &["branch", "--show-current"])
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let porcelain = run_git(root, &["status", "--porcelain"])
        .await
        .unwrap_or_default();
    let (added_files, deleted_files) = status_file_counts(&porcelain);
    let (ins_a, del_a) = numstat_totals(
        &run_git(root, &["diff", "--numstat"])
            .await
            .unwrap_or_default(),
    );
    let (ins_b, del_b) = numstat_totals(
        &run_git(root, &["diff", "--cached", "--numstat"])
            .await
            .unwrap_or_default(),
    );
    GitBarInfo {
        branch,
        ins: ins_a + ins_b,
        del: del_a + del_b,
        added_files,
        deleted_files,
        repo: true,
    }
}

#[cfg(test)]
mod mode_bar_tests {
    use super::*;

    #[test]
    fn porcelain_counts_added_deleted_and_untracked() {
        let out =
            " M lib.rs\nA  new.rs\n?? scratch/x\n D gone.rs\nAM staged.rs\nR  old.rs -> new.rs\n";
        // added: A new.rs, ?? scratch/x, AM staged.rs; deleted: D gone.rs.
        assert_eq!(status_file_counts(out), (3, 1));
    }

    #[test]
    fn porcelain_ignores_clean_and_renames() {
        let out = " M mod.rs\nR  a.rs -> b.rs\n";
        assert_eq!(status_file_counts(out), (0, 0));
    }

    #[test]
    fn numstat_sums_only_numeric_rows() {
        let out = "1\t1\tsrc/a.rs\n-\t-\timg.png\n10\t3\tsrc/c d.rs\n";
        assert_eq!(numstat_totals(out), (11, 4));
        assert_eq!(numstat_totals(""), (0, 0));
    }
}

#[cfg(test)]
mod section_tests {
    use super::*;

    /// chat: preamble meta, then exchange 0 (user a + assistant + tool run),
    /// then exchange 1 (user b + assistant).
    fn sample_chat() -> Vec<Msg> {
        vec![
            Msg::text(MsgKind::Meta, "comrade ready"),
            Msg::authored(MsgKind::User, "you", "do the thing"),
            Msg::authored(MsgKind::Assistant, "assistant", "on it"),
            Msg::tool(ToolCard {
                name: "run_tests".into(),
                author: Some("assistant".into()),
                args: String::new(),
                justification: None,
                risk: None,
                result: Some("ok".into()),
                ok: true,
                open: false,
                started: None,
                taken_ms: None,
                tokens: None,
            }),
            Msg::authored(MsgKind::User, "you", "second"),
            Msg::authored(MsgKind::Assistant, "assistant", "done"),
        ]
    }

    #[test]
    fn section_count_is_user_turns() {
        assert_eq!(section_count(&sample_chat()), 2);
        assert_eq!(section_count(&[]), 0);
    }

    #[test]
    fn section_of_msg_belongs_to_previous_user_turn() {
        let chat = sample_chat();
        // The assistant reply and tool call live inside exchange 0.
        assert_eq!(section_of_msg(&chat, 2), Some((0, 1)));
        assert_eq!(section_of_msg(&chat, 3), Some((0, 1)));
        // Exchange 1's own messages.
        assert_eq!(section_of_msg(&chat, 4), Some((1, 4)));
        assert_eq!(section_of_msg(&chat, 5), Some((1, 4)));
        // Preamble before the first user turn has no section.
        assert_eq!(section_of_msg(&chat, 0), None);
        assert_eq!(section_of_msg(&chat, 99), None);
    }

    #[test]
    fn section_of_msg_handles_consecutive_users() {
        let chat = vec![
            Msg::authored(MsgKind::User, "you", "a"),
            Msg::authored(MsgKind::User, "you", "b"),
            Msg::authored(MsgKind::Assistant, "assistant", "reply to b"),
        ];
        assert_eq!(section_of_msg(&chat, 0), Some((0, 0)));
        assert_eq!(section_of_msg(&chat, 1), Some((1, 1)));
        assert_eq!(section_of_msg(&chat, 2), Some((1, 1)));
    }

    #[test]
    fn collapsible_refuses_the_live_last_section_while_running() {
        // Two sections, run in flight: only the older (ord 0) may collapse.
        assert!(section_collapsible(true, 0, 2));
        assert!(!section_collapsible(true, 1, 2));
        // Idle: everything may collapse.
        assert!(section_collapsible(false, 1, 2));
        assert!(section_collapsible(false, 0, 1));
        // Preamble-only edge: no section is ever the live one.
        assert!(!section_collapsible(true, 0, 0));
    }

    fn row_text(r: &RenderRow) -> String {
        r.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Two exchanges: 0 = user "first ask" + assistant "secret reply 0",
    /// 1 = user "second ask" + assistant "visible reply 1".
    fn two_exchanges() -> Vec<Msg> {
        vec![
            Msg::authored(MsgKind::User, "you", "first ask"),
            Msg::authored(MsgKind::Assistant, "assistant", "secret reply 0"),
            Msg::authored(MsgKind::User, "you", "second ask"),
            Msg::authored(MsgKind::Assistant, "assistant", "visible reply 1"),
        ]
    }

    #[test]
    fn user_turn_renders_as_prompt_echo_heading() {
        let (rows, owner, ranges) = layout_chat_rows(&two_exchanges(), &[], "", 60);
        // The user turn is a single "> first ask" row (heading, no "you" bar).
        assert_eq!(row_text(&rows[0]), "> first ask");
        assert_eq!(owner[0], Some(0));
        assert_eq!(ranges[0], (0, 1));
        // The first heading row is a clickable section header.
        assert_eq!(rows[0].tool_header, Some(0));
        // Exchange 0's body: assistant author bar + one text row (rows 1..3).
        assert_eq!(ranges[1], (1, 2));
        assert_eq!(row_text(&rows[1]), "assistant");
        // Exchange 1 starts its own heading right after.
        assert_eq!(row_text(&rows[3]), "> second ask");
        assert_eq!(rows[3].tool_header, Some(2));
    }

    #[test]
    fn collapsed_section_hides_interior_but_keeps_flat_ranges() {
        let (rows, owner, ranges) = layout_chat_rows(&two_exchanges(), &[true, false], "", 60);
        let all: String = rows.iter().map(row_text).collect::<Vec<_>>().join("|");
        // Exchange 0 collapsed: only its echo + a "… N more" marker show.
        assert!(all.contains("> first ask"), "{all}");
        assert!(all.contains("··· 1 more"), "{all}");
        assert!(!all.contains("secret reply 0"), "{all}");
        // Exchange 1 still renders in full.
        assert!(all.contains("> second ask"), "{all}");
        assert!(all.contains("visible reply 1"), "{all}");
        // Exchange 0 occupies rows 0-1 (echo + marker); its hidden interior
        // (idx 1) keeps a zero-length range and owns no rendered row.
        assert_eq!(ranges[0], (0, 2));
        assert_eq!(ranges[1], (2, 0));
        assert!(!owner.iter().any(|o| *o == Some(1)), "{owner:?}");
        assert_eq!(owner[0], Some(0));
        assert_eq!(owner[1], Some(0));
        assert_eq!(owner[2], Some(2));
        // Echo and marker rows are both clickable section headers.
        assert_eq!(rows.iter().filter(|r| r.tool_header == Some(0)).count(), 2);
    }

    #[test]
    fn expanding_a_section_restores_its_body() {
        let (rows, _, _) = layout_chat_rows(&two_exchanges(), &[true, false], "", 60);
        let all: String = rows.iter().map(row_text).collect::<Vec<_>>().join("|");
        assert!(!all.contains("secret reply 0"), "{all}");
        let (rows, _, _) = layout_chat_rows(&two_exchanges(), &[false, false], "", 60);
        let all: String = rows.iter().map(row_text).collect::<Vec<_>>().join("|");
        assert!(all.contains("secret reply 0"), "{all}");
        assert!(!all.contains("··· 1 more"), "{all}");
    }

    #[test]
    fn wrapped_user_echo_indents_continuation_rows() {
        let text = "abcdefghijkl mnopqrstuvwxyz 1234567890";
        let chat = vec![Msg::authored(MsgKind::User, "you", text)];
        let (rows, _, ranges) = layout_chat_rows(&chat, &[false], "", 12);
        assert!(
            rows.len() >= 2,
            "expected wrapping into rows, got {}",
            rows.len()
        );
        assert!(
            row_text(&rows[0]).starts_with("> "),
            "{}",
            row_text(&rows[0])
        );
        for r in &rows[1..] {
            assert!(
                row_text(r).starts_with("  "),
                "continuation must indent: {:?}",
                row_text(r)
            );
        }
        assert_eq!(ranges[0], (0, rows.len()));
    }

    #[test]
    fn toggle_section_folds_and_unfolds_an_exchange() {
        let chat = two_exchanges();
        let mut collapsed = Vec::new();
        // Collapse exchange 0 (idle: allowed), verify state, then unfold.
        toggle_section(&mut collapsed, &chat, false, 0);
        assert_eq!(collapsed, vec![true]);
        // idx 1 is exchange 0's interior, so toggling it expands exchange 0.
        toggle_section(&mut collapsed, &chat, false, 1);
        assert_eq!(collapsed, vec![false]);
        // idx 3 is exchange 1's interior message: toggling folds exchange 1.
        toggle_section(&mut collapsed, &chat, false, 3);
        assert_eq!(collapsed, vec![false, true]);
        // Toggling its own heading unfolds it again.
        toggle_section(&mut collapsed, &chat, false, 2);
        assert_eq!(collapsed, vec![false, false]);
        // Toggling a preamble message (no section) is a no-op.
        let preamble = vec![
            Msg::text(MsgKind::Meta, "ready"),
            Msg::authored(MsgKind::User, "you", "a"),
        ];
        let mut collapsed = Vec::new();
        toggle_section(&mut collapsed, &preamble, false, 0);
        assert!(collapsed.is_empty());
    }

    #[test]
    fn running_keeps_the_live_last_section_expanded() {
        let chat = two_exchanges();
        let mut collapsed = Vec::new();
        // A run in flight: only exchange 0 (older) may fold.
        toggle_section(&mut collapsed, &chat, true, 0);
        assert_eq!(collapsed, vec![true]);
        toggle_section(&mut collapsed, &chat, true, 2);
        assert!(collapsed.get(1) != Some(&true), "{collapsed:?}");
        // But an already-collapsed last section can be expanded mid-run.
        let mut collapsed = vec![false, true];
        toggle_section(&mut collapsed, &chat, true, 2);
        assert_eq!(collapsed, vec![false, false]);
    }

    #[test]
    fn expand_section_clears_state_for_any_member() {
        let chat = two_exchanges();
        let mut collapsed = vec![true, true];
        // Expanding from an interior body message of exchange 0 works too.
        expand_section(&mut collapsed, &chat, 1);
        assert_eq!(collapsed, vec![false, true]);
        // Preamble/out-of-range indices leave the state untouched.
        let mut collapsed = vec![true];
        expand_section(&mut collapsed, &chat, 99);
        assert_eq!(collapsed, vec![true]);
    }

    #[test]
    fn chat_cache_reuses_rows_while_epoch_and_width_are_stable() {
        let mut cache: Option<ChatRowsCache> = None;
        let chat = two_exchanges();
        // First call builds the layout; a second call with the same epoch and
        // width must reuse it unchanged (the row layout is frame-stable).
        let c = chat_cache(&mut cache, 7, 60, &chat, &[]);
        let ranges = c.ranges.clone();
        let owner = c.owner.clone();
        let rows = c.rows.len();
        let c2 = chat_cache(&mut cache, 7, 60, &chat, &[]);
        assert_eq!(c2.ranges, ranges);
        assert_eq!(c2.owner, owner);
        assert_eq!(c2.rows.len(), rows);
    }

    #[test]
    fn chat_cache_relayouts_on_width_change_and_epoch_bump() {
        let mut cache: Option<ChatRowsCache> = None;
        let chat = two_exchanges();
        chat_cache(&mut cache, 1, 60, &chat, &[]);
        // A narrower terminal width re-wraps text: the row count must change.
        let narrow = chat_cache(&mut cache, 1, 12, &chat, &[]);
        let narrow_rows = narrow.rows.len();
        // An epoch bump (any chat/collapse mutation) must also rebuild.
        let collapsed = chat_cache(&mut cache, 2, 12, &chat, &[true, false]);
        assert!(
            collapsed.rows.len() < narrow_rows,
            "collapsing exchange 0 must shrink the layout"
        );
        // The rebuilt cache equals a fresh pure layout of the same inputs.
        let (rows, owner, ranges) = layout_chat_rows(&chat, &[true, false], "", 12);
        assert_eq!(collapsed.rows.len(), rows.len());
        assert_eq!(collapsed.owner, owner);
        assert_eq!(collapsed.ranges, ranges);
    }
}

#[cfg(test)]
mod plan_step_tests {
    use super::*;

    fn done_step(id: u64, goal: &str, model: &str, took: Option<u64>) -> PlanStep {
        PlanStep {
            id,
            goal: goal.into(),
            verification: "cargo test".into(),
            model: model.into(),
            context: String::new(),
            status: PlanStatus::Done,
            note: Some("working: fix 1/5".into()),
            started_at_ms: Some(1_000),
            took_ms: took,
        }
    }

    fn line_text(toks: &[Tok]) -> String {
        toks.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn done_step_collapses_to_one_line_with_duration() {
        let step = done_step(2, "add section model", "self", Some(125_000));
        let toks = collapsed_done_toks(&step, 80);
        let text = line_text(&toks);
        assert!(text.starts_with("✓ 2. "), "{text}");
        assert!(text.contains("add section model"), "{text}");
        assert!(text.contains("[self]"), "{text}");
        assert!(text.contains("· 2m 5s"), "{text}");
        // no note, no verification on the collapsed line
        assert!(!text.contains("fix 1/5"), "{text}");
        assert!(!text.contains("cargo test"), "{text}");
        assert!(text.chars().count() <= 80, "{text}");
    }

    #[test]
    fn done_step_omits_duration_when_never_started() {
        let step = done_step(1, "short goal", "", None);
        let text = line_text(&collapsed_done_toks(&step, 80));
        assert!(!text.contains('·'), "{text}");
        assert!(!text.contains('['), "{text}");
        assert!(text.starts_with("✓ 1. short goal"), "{text}");
    }

    #[test]
    fn done_step_truncates_goal_to_fit_narrow_panel() {
        let long = "a very long goal ".repeat(6);
        let step = done_step(3, &long, "qwen", Some(3_500));
        let toks = collapsed_done_toks(&step, 24);
        let text = line_text(&toks);
        assert!(text.chars().count() <= 24, "{text}");
        // goal was truncated (ellipsis mid-line, before the model/duration)
        assert!(text.contains('…'), "{text}");
        assert!(text.contains("[qwen]"), "{text}");
        assert!(text.contains("· 3.5s"), "{text}");
    }

    #[test]
    fn short_durations_render_as_subsecond() {
        let step = done_step(1, "g", "", Some(420));
        let text = line_text(&collapsed_done_toks(&step, 60));
        assert!(text.contains("· 420ms"), "{text}");
        let step = done_step(1, "g", "", Some(3_500));
        let text = line_text(&collapsed_done_toks(&step, 60));
        assert!(text.contains("· 3.5s"), "{text}");
    }

    #[test]
    fn plan_glyphs_tick_cross_dash_and_spinner() {
        // Stable glyphs for the non-running statuses.
        assert_eq!(plan_glyph(&PlanStatus::Done, 0), "✓");
        assert_eq!(plan_glyph(&PlanStatus::Blocked, 0), "✗");
        assert_eq!(plan_glyph(&PlanStatus::Pending, 0), "-");
        // The in-progress spinner picks a frame from the rotation by time and
        // every frame is a single-width glyph.
        for ms in [0u128, 100, 350, 799] {
            let g = plan_glyph(&PlanStatus::InProgress, ms);
            assert!(SPINNER_FRAMES.contains(&g), "{g}");
            assert_eq!(g.chars().count(), 1);
        }
        // Advancing one full rotation period moves to the next frame.
        assert_ne!(
            plan_glyph(&PlanStatus::InProgress, 0),
            plan_glyph(&PlanStatus::InProgress, 100)
        );
    }
}
