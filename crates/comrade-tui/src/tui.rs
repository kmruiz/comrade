//! Cockpit TUI: markdown chat + plan checklist + prompt bar + modal dialogs.
//!
//! Chat layout:
//! - no emojis
//! - user messages are a soft lighter band (chat background lifted by a touch,
//!   see `USER_BG_ALPHA`) with a cyan rule on the left of every wrapped line
//! - agent tool calls render as a compact card (tool + justification); clicking
//!   (or the mouse wheel) opens the details
//! - assistant/user text is rendered as markdown

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use arboard::Clipboard;
use async_trait::async_trait;
use comrade_core::{
    AgentEvent, AgentSession, ChatMessage, ContextManager, DelegateCfg, Role,
    build_session_context, run_agent_with_history,
};
use comrade_tool::{
    AGENT_MODEL, FieldKind, FormSpec, PlanStatus, PlanStep, PlanTarget, SessionControl,
    ToolContext, UserIo, UserPrompt, UserReply,
};

use serde::{Deserialize, Serialize};

use crate::colors::ModelColors;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
// The kitty keyboard-enhancement protocol lets the terminal report SHIFT (and
// other) modifiers for chords like Ctrl+Shift+C that are otherwise byte-
// identical to their unshifted form. Unix terminals only.
#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use crate::session_store::SessionFile;
use crate::{Deps, TaggedEvent, session_bundle, spawn_tagged_relay};

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
    /// id of the session whose run raised this ask.
    session: u64,
    prompt: UserPrompt,
    reply: oneshot::Sender<UserReply>,
}

struct TuiUserIo {
    tx: mpsc::Sender<PendingAsk>,
    /// Session whose runs use this io; stamped onto every ask it sends.
    session: u64,
}

#[async_trait]
impl UserIo for TuiUserIo {
    async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(PendingAsk {
                session: self.session,
                prompt,
                reply: tx,
            })
            .await?;
        rx.await.context("UI closed before answering")
    }
}

// ---------------------------------------------------------------------------
// utilities
// ---------------------------------------------------------------------------

/// Extract the base model name from a display string that may include budget info.
/// For example: "deepseek (32768)" -> "deepseek", "codestral" -> "codestral"
fn base_model_name(display_name: &str) -> &str {
    // Find the first '(' and return the substring before it, or the whole string if no '(' found
    display_name
        .find('(')
        .map_or(display_name, |pos| &display_name[..pos])
        .trim()
}

// ---------------------------------------------------------------------------
// chat model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub(crate) enum MsgKind {
    /// A message from the human user.
    User,
    /// A message written by the (main) model: replies and final answers.
    Assistant,
    /// A tool call card (also covers the parent's `delegate` hand-offs).
    Tool,
    /// Small grey status note ("run finished", "plan finished", ...).
    Meta,
    /// A question the agent asked the human (an ask_form form) and the human's
    /// answer to it, recorded in the transcript. Unlike tool cards and grey
    /// notes it survives focus mode, so a focused transcript still shows what
    /// was asked.
    Question,
    /// A collapsed failing-test block.
    Failure,
    /// The model's reasoning between actions, rendered as a brain-headed spoken
    /// block tinted with the model's colour (like an assistant answer).
    Reasoning,
    /// A reply from a delegated model, shown under that model's name.
    Delegate,
    /// A folded digest of one completed stretch of activity (tool calls,
    /// reasoning, failures, meta notes) between two spoken messages. The
    /// original messages are kept in `Msg::children` and unfolded back on
    /// demand, so per-card expand/copy/search keep working.
    Run,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ToolCard {
    name: String,
    /// Display name of the model that invoked the tool (None for legacy rows).
    author: Option<String>,
    args: String,
    justification: Option<String>,
    result: Option<String>,
    ok: bool,
    open: bool,
    /// When the call reached the UI (wall clock), to measure how long it took.
    #[serde(skip)]
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
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TestFail {
    name: String,
    detail: String,
    open: bool,
}

/// Structured summary of a `pom_run_tests` invocation.
#[derive(Default)]
struct TestSummary {
    passed: usize,
    failed: usize,
    duration: String,
    /// (name, detail) for every failing test.
    cases: Vec<(String, String)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Msg {
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
    /// A thinking block under `author`, rendered like a spoken answer with a
    /// brain header, tinted with the model's colour. Visible by default.
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
    /// Value of App::focus_mode when this layout was built.
    focus: bool,
    rows: Vec<RenderRow>,
    /// Owning chat-message index per row (parallel to `rows`).
    owner: Vec<Option<usize>>,
    /// Per chat-message row span (start row, height).
    ranges: Vec<(usize, usize)>,
}

/// Return the chat row layout for `(epoch, width)`, rebuilding it from
/// `chat`/`collapsed` via [`layout_chat_rows`] when the cache is stale or
/// absent, and reusing it otherwise.
#[allow(clippy::too_many_arguments)]
fn chat_cache<'a>(
    cache: &'a mut Option<ChatRowsCache>,
    epoch: u64,
    width: usize,
    focus: bool,
    chat: &[Msg],
    collapsed: &[bool],
    delegates: &[DelegateCfg],
    colors: &ModelColors,
) -> &'a ChatRowsCache {
    let stale =
        !matches!(cache, Some(c) if c.epoch == epoch && c.width == width && c.focus == focus);
    if stale {
        let (rows, owner, ranges) =
            layout_chat_rows(chat, collapsed, "", width, delegates, colors, focus);
        *cache = Some(ChatRowsCache {
            width,
            epoch,
            focus,
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
    /// id of the session whose run is waiting on this dialog.
    session: u64,
    /// Editable state when `prompt` is a [`UserPrompt::Form`]; `None` otherwise.
    form: Option<FormEdit>,
}

/// Editable state for a [`UserPrompt::Form`]: one value per field, in order,
/// plus the index of the field that currently has focus.
struct FormEdit {
    values: Vec<String>,
    sel: usize,
}

impl FormEdit {
    fn new(spec: &FormSpec) -> Self {
        FormEdit {
            values: spec.fields.iter().map(|f| f.initial_value()).collect(),
            sel: 0,
        }
    }

    /// Move focus to the next (`forward`) or previous field, wrapping around.
    fn focus(&mut self, forward: bool) {
        let n = self.values.len();
        if n == 0 {
            return;
        }
        self.sel = if forward {
            (self.sel + 1) % n
        } else {
            (self.sel + n - 1) % n
        };
    }

    /// The answers keyed by field id, in field order.
    fn answers(&self, spec: &FormSpec) -> BTreeMap<String, String> {
        spec.fields
            .iter()
            .zip(&self.values)
            .map(|(f, v)| (f.id.clone(), v.clone()))
            .collect()
    }

    fn value_mut(&mut self) -> Option<&mut String> {
        self.values.get_mut(self.sel)
    }

    /// Apply a typed character to the focused field, driving its component.
    fn input(&mut self, spec: &FormSpec, c: char) {
        let kind = spec.fields.get(self.sel).map(|f| &f.kind);
        match kind {
            Some(FieldKind::Checkbox) => {
                if c == ' ' {
                    self.toggle(spec);
                }
            }
            // Select/diff-choice options are chosen with ←/→, not typed.
            Some(FieldKind::Select { .. }) | Some(FieldKind::DiffChoice { .. }) => {}
            Some(FieldKind::Number { .. }) => {
                if (c.is_ascii_digit() || c == '.')
                    && let Some(v) = self.value_mut()
                {
                    v.push(c);
                }
            }
            Some(FieldKind::Date) => {
                if (c.is_ascii_digit() || c == '-')
                    && let Some(v) = self.value_mut()
                {
                    v.push(c);
                }
            }
            Some(FieldKind::Text { .. }) => {
                if let Some(v) = self.value_mut() {
                    v.push(c);
                }
            }
            None => {}
        }
    }

    fn backspace(&mut self) {
        if let Some(v) = self.value_mut() {
            v.pop();
        }
    }

    fn toggle(&mut self, spec: &FormSpec) {
        if !matches!(
            spec.fields.get(self.sel).map(|f| &f.kind),
            Some(FieldKind::Checkbox)
        ) {
            return;
        }
        if let Some(v) = self.value_mut() {
            *v = if comrade_tool::truthy(v) {
                "false".to_string()
            } else {
                "true".to_string()
            };
        }
    }

    /// Step the focused component: `dir` = -1 (left / decrease) or +1 (right /
    /// increase). Numbers clamp to min/max, selects cycle options, dates shift a
    /// day, checkboxes toggle.
    fn adjust(&mut self, spec: &FormSpec, dir: i32) {
        let Some(field) = spec.fields.get(self.sel) else {
            return;
        };
        match &field.kind {
            FieldKind::Number { min, max, step } => {
                let cur: f64 = self
                    .values
                    .get(self.sel)
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0.0);
                let step = step.filter(|s| *s > 0.0).unwrap_or(1.0);
                let mut next = cur + f64::from(dir) * step;
                if let Some(lo) = min {
                    next = next.max(*lo);
                }
                if let Some(hi) = max {
                    next = next.min(*hi);
                }
                if let Some(v) = self.value_mut() {
                    *v = fmt_f64(next);
                }
            }
            FieldKind::Select { options } if !options.is_empty() => {
                let n = options.len() as i32;
                let cur = self
                    .values
                    .get(self.sel)
                    .and_then(|v| options.iter().position(|o| o == v))
                    .unwrap_or(0) as i32;
                let next = (cur + dir).rem_euclid(n) as usize;
                if let Some(v) = self.value_mut() {
                    *v = options[next].clone();
                }
            }
            FieldKind::DiffChoice { options } if !options.is_empty() => {
                let n = options.len() as i32;
                let cur = self
                    .values
                    .get(self.sel)
                    .and_then(|v| options.iter().position(|o| &o.label == v))
                    .unwrap_or(0) as i32;
                let next = (cur + dir).rem_euclid(n) as usize;
                if let Some(v) = self.value_mut() {
                    *v = options[next].label.clone();
                }
            }
            FieldKind::Date => {
                let cur = self.values.get(self.sel).cloned().unwrap_or_default();
                let shifted = shift_date(&cur, dir);
                if let Some(v) = self.value_mut() {
                    *v = shifted;
                }
            }
            FieldKind::Checkbox => self.toggle(spec),
            _ => {}
        }
    }
}

/// Format a number like the spinner shows it (whole numbers without `.0`).
fn fmt_f64(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Shift an ISO `YYYY-MM-DD` date by whole days (`days` may be negative).
/// An empty or unparseable value starts from 1970-01-01.
fn shift_date(cur: &str, days: i32) -> String {
    let (y, m, d) = parse_ymd(cur).unwrap_or((1970, 1, 1));
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) + i64::from(days));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parse `YYYY-MM-DD` into (year, month, day); `None` when malformed.
fn parse_ymd(s: &str) -> Option<(i64, i64, i64)> {
    let mut it = s.trim().split('-');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
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

/// One discovered MCP tool shown in the M-x list-mcp-servers modal.
#[derive(Clone)]
struct McpToolEntry {
    /// Full local tool name (`mcp_<server>_<tool>`), the agent-facing id that
    /// toggling enables or disables.
    name: String,
    /// Server-advertised description with the "[MCP server `X`] " prefix
    /// stripped (the group header already names the server).
    desc: String,
}

/// One server's tools, grouped under its header row in the modal.
#[derive(Clone)]
struct McpToolGroup {
    /// Configured server name (the group header).
    server: String,
    /// Registered tools of this server, in registry order.
    tools: Vec<McpToolEntry>,
}

/// Reference to one visible row of the modal list: a group header or a tool
/// row inside a group. Indexes are into [`McpServersView::groups`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowRef {
    Header(usize),
    Tool(usize, usize),
}

/// The M-x list-mcp-servers modal: every configured MCP server as a
/// collapsible group of its connected tools. The human toggles each tool on or
/// off (a live filter shared with the agent loop via the registry), filters by
/// tool or server name by just typing, and collapses groups with Tab.
struct McpServersView {
    groups: Vec<McpToolGroup>,
    /// Live on/off switch shared with the `ToolRegistry` the agent runs with
    /// (see `comrade_tool::ToolRegistry::disabled_handle`).
    disabled: Arc<RwLock<HashSet<String>>>,
    /// Filter text; matched case-insensitively against a server name OR a
    /// tool's full `mcp_...` name. Typing always edits it (no mode toggle).
    filter: String,
    /// Group indexes (into `groups`) whose tool rows are hidden.
    collapsed: HashSet<usize>,
    /// Selection over the visible flat row list (see [`RowRef`] and
    /// [`mcp_visible_rows`]).
    sel: usize,
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
    CompactContext,
    Copy,
    EndOfLine,
    FocusMode,
    ForkSession,
    ForwardWord,
    InsertNewline,
    KillSession,
    KillWord,
    ListMcpServers,
    LoadSession,
    MoveBlockDown,
    MoveBlockUp,
    MoveUserDown,
    MoveUserUp,
    NewSession,
    QueuePrompt,
    Quit,
    ReloadConfig,
    SaveSession,
    SearchChat,
    SteerPrompt,
    SubmitPrompt,
    SwitchSession,
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
        MxCommand::CompactContext,
        MxCommand::Copy,
        MxCommand::EndOfLine,
        MxCommand::FocusMode,
        MxCommand::ForkSession,
        MxCommand::ForwardWord,
        MxCommand::InsertNewline,
        MxCommand::KillSession,
        MxCommand::KillWord,
        MxCommand::ListMcpServers,
        MxCommand::LoadSession,
        MxCommand::MoveBlockDown,
        MxCommand::MoveBlockUp,
        MxCommand::MoveUserDown,
        MxCommand::MoveUserUp,
        MxCommand::NewSession,
        MxCommand::QueuePrompt,
        MxCommand::Quit,
        MxCommand::ReloadConfig,
        MxCommand::SaveSession,
        MxCommand::SearchChat,
        MxCommand::SteerPrompt,
        MxCommand::SubmitPrompt,
        MxCommand::SwitchSession,
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
            MxCommand::CompactContext => "compact-context",
            MxCommand::Copy => "copy",
            MxCommand::EndOfLine => "end-of-line",
            MxCommand::FocusMode => "focus-mode",
            MxCommand::ForkSession => "fork-session",
            MxCommand::ForwardWord => "forward-word",
            MxCommand::InsertNewline => "insert-newline",
            MxCommand::KillSession => "kill-session",
            MxCommand::KillWord => "kill-word",
            MxCommand::ListMcpServers => "list-mcp-servers",
            MxCommand::LoadSession => "load-session",
            MxCommand::MoveBlockDown => "move-block-down",
            MxCommand::MoveBlockUp => "move-block-up",
            MxCommand::MoveUserDown => "move-user-down",
            MxCommand::MoveUserUp => "move-user-up",
            MxCommand::NewSession => "new-session",
            MxCommand::QueuePrompt => "queue-prompt",
            MxCommand::Quit => "quit",
            MxCommand::ReloadConfig => "reload-config",
            MxCommand::SaveSession => "save-session",
            MxCommand::SearchChat => "search-chat-history",
            MxCommand::SteerPrompt => "steer",
            MxCommand::SubmitPrompt => "submit-prompt",
            MxCommand::SwitchSession => "switch-session",
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
            MxCommand::CompactContext => Some("M-c"),
            MxCommand::Copy => Some("C-S-c / M-w"),
            MxCommand::EndOfLine => Some("<end>"),
            MxCommand::FocusMode => Some("M-f"),
            MxCommand::ForkSession => Some("C-x C-w"),
            MxCommand::ForwardWord => Some("M-<right>"),
            MxCommand::InsertNewline => Some("S-<return>"),
            MxCommand::KillSession => Some("C-x C-k"),
            MxCommand::KillWord => Some("M-<delete>"),
            MxCommand::MoveBlockDown => Some("C-n"),
            MxCommand::MoveBlockUp => Some("C-p"),
            MxCommand::MoveUserDown => Some("C-S-n"),
            MxCommand::MoveUserUp => Some("C-S-p"),
            // Starts a fresh session: unbound, run it from the M-x palette.
            MxCommand::NewSession => None,
            // Palette-only: shows the configured MCP servers in a modal.
            MxCommand::ListMcpServers => None,
            MxCommand::LoadSession => Some("C-x C-f"),
            MxCommand::QueuePrompt => Some("C-<return>"),
            MxCommand::Quit => Some("C-c"),
            MxCommand::ReloadConfig => Some("C-r"),
            MxCommand::SaveSession => Some("C-x C-s"),
            MxCommand::SearchChat => Some("C-s"),
            MxCommand::SteerPrompt => Some("<return>"),
            MxCommand::SubmitPrompt => Some("<return>"),
            MxCommand::SwitchSession => Some("C-x C-b"),
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
            MxCommand::CompactContext => {
                "summarise the context and replace the conversation with that summary"
            }
            MxCommand::Copy => "copy the prompt selection or the chat block under the cursor",
            MxCommand::EndOfLine => "move the prompt cursor to the end of the line",
            MxCommand::FocusMode => "filter the chat to the conversation (hide tool calls)",
            MxCommand::ForkSession => "fork the current session into an independent copy",
            MxCommand::ForwardWord => "move the prompt cursor forward one word",
            MxCommand::InsertNewline => "insert a newline in the prompt",
            MxCommand::KillSession => "close the active session",
            MxCommand::KillWord => "delete the word after the prompt cursor",
            MxCommand::ListMcpServers => "view and toggle MCP server tools",
            MxCommand::LoadSession => "load a session from a file",
            MxCommand::MoveBlockDown => "move to the next chat block",
            MxCommand::MoveBlockUp => "move to the previous chat block",
            MxCommand::MoveUserDown => "jump to the next message you sent",
            MxCommand::MoveUserUp => "jump to the previous message you sent",
            MxCommand::NewSession => "open a new session (the current one keeps running)",
            MxCommand::QueuePrompt => "hold the prompt and submit it when the current run ends",
            MxCommand::Quit => "quit the cockpit",
            MxCommand::ReloadConfig => "reload the config file without restarting",
            MxCommand::SaveSession => "save the current session to a file",
            MxCommand::SearchChat => "search the chat history",
            MxCommand::SteerPrompt => "send the prompt to the running agent or delegate now",
            MxCommand::SubmitPrompt => "send the prompt to the agent",
            MxCommand::SwitchSession => "switch to another open session",
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

/// One session opened in this run. Every non-active slot owns its whole live
/// state ([`LiveState`]); the active slot's state lives in the App's own fields.
///
/// A session can keep running in the background: its slot's [`LiveState`] holds
/// the run handle, the chat, the context metrics, and the session's own
/// run-facing event sender, so events keep arriving and are routed back to it
/// even while another session is on screen.
struct OpenSession {
    /// Stable identity used to route [`crate::TaggedEvent`]s to this session.
    id: u64,
    /// Display name (the session's title) shown in the switcher.
    title: String,
    /// Path the session was last saved to or loaded from, when known.
    file: Option<std::path::PathBuf>,
    /// Parked live state of a non-active session (None for the active slot).
    live: Option<Box<LiveState>>,
}

/// The per-session half of the App's state: everything that belongs to one
/// session and is swapped in and out of the App's own fields when the active
/// session changes (or when an event for a background session must be applied).
///
/// Keeping this as a swappable struct lets a session keep running while parked:
/// its run task holds clones of the Arcs it needs, and its events are applied by
/// temporarily swapping its `LiveState` into the App.
struct LiveState {
    session: Arc<AgentSession>,
    ctx_base: ToolContext,
    /// Rolling conversation history for this session.
    history: Arc<tokio::sync::Mutex<ContextManager>>,
    /// This session's run-facing (bounded) event sender; its relay tags events
    /// with the session id and forwards them to the App's central queue.
    run_tx: mpsc::Sender<AgentEvent>,
    stop: Option<CancellationToken>,
    run_handle: Option<tokio::task::JoinHandle<()>>,
    running: bool,
    steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// One-shot "compact the context now" request handed to the running loop
    /// (`None` while idle).
    compact: Option<comrade_tool::CompactRequest>,
    queued_prompt: Option<String>,
    run_cancelled: bool,
    chat: Vec<Msg>,
    section_collapsed: Vec<bool>,
    chat_epoch: u64,
    chat_rows_cache: Option<ChatRowsCache>,
    stream: String,
    ctx_tokens: usize,
    ctx_budget: usize,
    ctx_estimated: bool,
    activity: Option<String>,
    session_file: Option<std::path::PathBuf>,
    sel: Option<usize>,
    scroll_top: usize,
    follow: bool,
    was_at_bottom: bool,
    search: Option<Search>,
}

/// What a save/load session path prompt does once confirmed.
#[derive(Clone, Copy, PartialEq)]
enum PathIntent {
    Save,
    Load,
}

/// The Ctrl-x C-s / Ctrl-x C-f minibuffer prompting for a session file path.
struct PathPrompt {
    intent: PathIntent,
    input: String,
}

/// The Ctrl-x C-b session switcher overlay.
struct SessionPick {
    sel: usize,
}

/// Expand a leading `~/` in a typed path to the user's home directory.
fn expand_tilde(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    input.to_string()
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

    /// Central, UI-facing event queue: every session's relay pushes
    /// `(session_id, event)` here (see [`crate::spawn_tagged_relay`]).
    events_tx: mpsc::UnboundedSender<TaggedEvent>,
    events_rx: mpsc::UnboundedReceiver<TaggedEvent>,
    /// Run-facing event sender of the ACTIVE session (moved here from its
    /// [`LiveState`]); the run task and the AgentSession stream into it.
    run_tx: mpsc::Sender<AgentEvent>,
    asks_rx: mpsc::Receiver<PendingAsk>,
    /// Clone of the ask channel sender, used to build each session's own
    /// [`TuiUserIo`] (see [`App::make_ctx_base`]).
    asks_tx: mpsc::Sender<PendingAsk>,

    stop: Option<CancellationToken>,
    /// JoinHandle of the in-flight run task, used by the cancel watchdog to
    /// abort a run that ignores the cancel token (see [`App::cancel_run`]).
    run_handle: Option<tokio::task::JoinHandle<()>>,
    running: bool,
    /// Sender end of the in-flight run's steering pipe (`None` while idle).
    /// Sending fails once the run has ended and dropped its receiver.
    steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// One-shot "compact the context now" request handed to the running loop
    /// (`None` while idle).
    compact: Option<comrade_tool::CompactRequest>,
    /// A prompt queued (ctrl-Enter) while a run was active: submitted as the
    /// next run when the current one ends, or given back to the prompt bar if
    /// the run was cancelled (never auto-run after an explicit cancel).
    queued_prompt: Option<String>,
    /// True once the in-flight run was cancelled by the user (Esc / cancel-run).
    run_cancelled: bool,
    /// Instant of the last terminal repaint, used to cap event-driven redraws
    /// to ~30 fps while a run is streaming (a fast local model can otherwise
    /// flood the repaint path; see freeze notes #25/#29).
    last_draw: std::time::Instant,
    /// Auto-accept mode: approvals are answered "yes" without prompting.
    auto_accept: bool,
    /// Focus mode: hide tool noise so the chat reads as pure conversation.
    focus_mode: bool,
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
    /// Per-model agent colors, assigned once at start (main model + delegates):
    /// they tint each delegate's name, its sub-chat band in the chat window,
    /// the model panel and the model-pick overlay.
    model_colors: ModelColors,
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
    /// The M-x list-mcp-servers modal, when open.
    mcp_view: Option<McpServersView>,
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
    /// Lazily-created system-clipboard handle, kept alive for the whole
    /// session. Creating and dropping a `Clipboard` per write makes arboard
    /// (X11) hand the clipboard window over to a clipboard manager and destroy
    /// it <100 ms after `set_text`, before the manager can grab the contents
    /// ("Clipboard was dropped very quickly after writing"); holding one
    /// handle for the app's lifetime keeps the X11 window and its server
    /// thread serving our contents until the next copy or app exit.
    clipboard: Option<Clipboard>,

    /// Sessions opened in this run; the active one's live state is in the App
    /// fields above, the others keep their own [`LiveState`] (see [`OpenSession`]).
    open_sessions: Vec<OpenSession>,
    /// Index of the active session within `open_sessions`.
    active: usize,
    /// Next session id to hand out (0 is the session opened at startup).
    next_session_id: u64,
    /// While an event for a background session is being applied, the index of
    /// that slot (so handlers like [`App::refresh_active_slot`] write to the
    /// right session). None = handling the active session.
    handling_bg: Option<usize>,
    /// Path the active session was last saved to or loaded from (Save default).
    session_file: Option<std::path::PathBuf>,
    /// True between a Ctrl-x prefix key and the key that selects the command.
    ctrl_x: bool,
    /// Open save/load session path prompt, when any.
    path_prompt: Option<PathPrompt>,
    /// Open session switcher overlay (Ctrl-x C-b), when any.
    session_pick: Option<SessionPick>,
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

    /// Push a thinking block under the main model's name, shown as a
    /// brain-headed block tinted with the model's colour.
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
        if let Some(card) = self.last_delegate_tool_mut(name, model)
            && card.taken_ms.is_none()
            && let Some(started) = card.started
        {
            card.taken_ms = Some(started.elapsed().as_millis());
        }
    }

    /// Record the elapsed wall time of the last `name` card once its result
    /// arrives (a no-op for cards that never started or already got stamped).
    fn stamp_taken(&mut self, name: &str) {
        if let Some(card) = self.last_tool_mut(name)
            && card.taken_ms.is_none()
            && let Some(started) = card.started
        {
            card.taken_ms = Some(started.elapsed().as_millis());
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
        let is_run = matches!(self.chat.get(idx).map(|m| m.kind), Some(MsgKind::Run));
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
                    s -= len - 1;
                }
            }
            self.sel = (s < self.chat.len()).then_some(s);
        }
    }

    /// Copy the plain text of the message currently under the cursor
    /// (the selected block) to the system clipboard. Bound to Ctrl+Shift+C
    /// and M-w.
    fn copy_selected(&mut self) {
        let Some(idx) = self.sel else { return };
        let Some(msg) = self.chat.get(idx) else {
            return;
        };
        let text = msg_searchable(msg).trim().to_string();
        self.copy_text(&text);
    }

    /// Copy the prompt's text selection when one exists, otherwise the chat
    /// message under the cursor. Backs both the Ctrl+Shift+C and M-w chords.
    fn copy_prompt_or_block(&mut self) {
        if let Some(sel) = self.input.selected_text() {
            let sel = sel.to_string();
            self.copy_text(&sel);
        } else {
            self.copy_selected();
        }
    }

    /// Emacs-style kill-line on the prompt editor, bound to C-k: cut the
    /// active selection, otherwise cut from the cursor to the end of the
    /// line; at the end of a line the newline is cut too, joining the next
    /// line. The killed text goes on the system clipboard. No-op at the end
    /// of the buffer (chat blocks are read-only and cannot be cut).
    fn kill_line(&mut self) {
        if let Some(text) = self.input.kill_line() {
            self.copy_text(&text);
        }
    }

    /// Paste the system clipboard into the prompt at the cursor, replacing
    /// any selection. Bound to C-y. Reports failures in the chat.
    fn paste_clipboard(&mut self) {
        match self.with_clipboard(|clip| Ok(clip.get_text()?)) {
            Ok(text) => self.input.insert_str(&text),
            Err(e) => self.push_meta(format!("paste failed: {e}")),
        }
    }

    /// Put `text` on the system clipboard, reporting failures in the chat.
    fn copy_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Err(e) = self.with_clipboard(|clip| Ok(clip.set_text(text.to_string())?)) {
            self.push_meta(format!("copy failed: {e}"));
        }
    }

    /// Run `f` against the system clipboard, creating the handle on first use
    /// and reusing it afterwards. The handle is stored on `self` (see the
    /// `clipboard` field) so it lives for the whole session instead of being
    /// dropped right after each write.
    fn with_clipboard<T>(&mut self, f: impl FnOnce(&mut Clipboard) -> Result<T>) -> Result<T> {
        if self.clipboard.is_none() {
            self.clipboard = Some(Clipboard::new().context("open system clipboard")?);
        }
        f(self.clipboard.as_mut().unwrap())
    }

    fn push_failure(&mut self, name: String, detail: String) {
        self.push_msg(Msg::failure(name, detail));
    }

    /// Move to the next (`+1`) or previous (`-1`) visible chat block, stepping
    /// over the messages hidden inside a collapsed section.
    fn move_block(&mut self, dir: isize) {
        if let Some(next) = step_visible(
            &self.chat,
            &self.section_collapsed,
            self.sel,
            dir,
            self.focus_mode,
        ) {
            self.select_block(next);
        }
    }

    /// Toggle focus mode (M-f / M-x focus-mode): hide tool calls, failure
    /// blocks, folded digests and status notes, keeping only the conversation
    /// (user turns, model replies, delegate advisories) and the reasoning.
    fn toggle_focus_mode(&mut self) {
        self.focus_mode = !self.focus_mode;
        // Re-anchor the selection onto a block that is still on screen.
        if let Some(s) = self.sel
            && !chat_visible(&self.chat, &self.section_collapsed, s, self.focus_mode)
        {
            let next = step_visible(
                &self.chat,
                &self.section_collapsed,
                Some(s),
                1,
                self.focus_mode,
            )
            .or_else(|| {
                step_visible(
                    &self.chat,
                    &self.section_collapsed,
                    Some(s),
                    -1,
                    self.focus_mode,
                )
            });
            match next {
                Some(i) => self.select_block(i),
                None => self.sel = None,
            }
        }
        if self.search.is_some() {
            self.refresh_search();
        }
        self.push_meta(if self.focus_mode {
            "focus mode on"
        } else {
            "focus mode off"
        });
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
            .filter(|(_, m)| msg_matches(m, &ql) && (!self.focus_mode || focus_visible(m)))
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
        let folded = matches!(self.chat.get(idx).map(|m| m.kind), Some(MsgKind::Run));
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
        // Every run gets a fresh steering pipe: the UI keeps the sender and the
        // run task keeps the receiver (via `ctx.steer`), so a message typed
        // mid-run reaches the agent loop - and, nested inside it, a delegate's
        // sub-loop, which clones the same context.
        let (steer, steer_tx) = comrade_tool::Steer::channel();
        // The UI keeps the compact-request handle (M-c); the run task keeps a
        // clone it takes at each rest point to summarise the history.
        let compact = comrade_tool::CompactRequest::new();
        let mut ctx = self.ctx_base.clone();
        ctx.steer = Some(steer);
        ctx.compact = Some(compact.clone());
        self.steer_tx = Some(steer_tx);
        self.compact = Some(compact);
        self.run_cancelled = false;
        let cfg = self.cfg.clone();
        let client = self.client.clone();
        let tools = self.tools.clone();
        // One conversation per session: every task appends to the same history,
        // which the agent compacts itself as it approaches the token budget.
        let history = self.history.clone();
        let tx = self.run_tx.clone();
        let balance_tx = self.run_tx.clone();
        let stop = CancellationToken::new();
        self.stop = Some(stop.clone());
        self.running = true;
        self.follow = true;
        self.sel = None;
        let handle = tokio::spawn(async move {
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
                // Cosmetic status: never let the run park behind a busy UI.
                let _ = balance_tx.try_send(AgentEvent::AccountBalance(balance));
            }
        });
        self.run_handle = Some(handle);
    }

    /// Compact the conversation (M-c): ask the model to summarise what has been
    /// done so far and replace the running history with that summary.
    ///
    /// While a run is in flight the request is handed to the agent loop, which
    /// performs it at its next rest point (before the budget is enforced). When
    /// idle, the summarisation runs now in a background task and reports back
    /// through the event queue.
    fn compact_context(&mut self) {
        if self.running {
            match &self.compact {
                Some(c) => {
                    c.request();
                    self.push_meta(
                        "compaction requested: the context will be summarised at the next step",
                    );
                }
                None => self.push_meta("nothing to compact yet"),
            }
            return;
        }
        self.push_meta("compacting context...");
        let client = self.client.clone();
        let history = self.history.clone();
        let tx = self.events_tx.clone();
        let id = self.active_id();
        tokio::spawn(async move {
            let mut history = history.lock().await;
            match comrade_core::compact_history(&client, &mut history).await {
                Ok(rep) => {
                    let _ = tx.send((
                        id,
                        AgentEvent::ContextCompacted {
                            before_messages: rep.before_messages,
                            after_messages: history.messages().len(),
                            before_tokens: rep.before_tokens,
                            after_tokens: rep.after_tokens,
                        },
                    ));
                }
                Err(e) => {
                    let _ = tx.send((
                        id,
                        AgentEvent::Error(format!("context compaction failed: {e:#}")),
                    ));
                }
            }
        });
    }

    fn cancel_run(&mut self) {
        if let Some(stop) = &self.stop {
            stop.cancel();
        }
        self.run_cancelled = true;
        self.push_meta("cancelling...");
        // Abort guarantee: Esc must always end the run. Several awaits in the
        // run path (tool.invoke, event-channel sends, the post-run balance
        // refresh) do not watch the cancel token, so a stalled local model
        // server (e.g. LM Studio) can wedge the run in one of them forever.
        // Watchdog: if the run task has not ended shortly after the cancel,
        // abort it outright and emit RunEnd so the UI always regains control.
        const GRACE: Duration = Duration::from_millis(1500);
        if let Some(handle) = self.run_handle.take() {
            let tx = self.events_tx.clone();
            let id = self.active_id();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + GRACE;
                loop {
                    if handle.is_finished() {
                        // The run ended on its own; its RunEnd is already queued.
                        return;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        handle.abort();
                        // The aborted task never sends RunEnd; synthesize it so
                        // the UI clears the running state. Duplicate RunEnds are
                        // harmless (see on_agent_event).
                        let _ = tx.send((id, AgentEvent::RunEnd));
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
    }

    /// Render a message the human sent in the chat (submitted prompts, steers
    /// and queued messages alike): fold whatever the run produced before it
    /// into a digest, then show the message as a user band.
    fn show_user(&mut self, text: &str) {
        self.stream.clear();
        self.fold_completed();
        self.push_msg(Msg::authored(MsgKind::User, "you", text));
    }

    /// Submit whatever is in the prompt bar: when a run is in flight the text
    /// steers the running agent/delegate; when idle it starts a new run.
    fn submit_prompt(&mut self) {
        let prompt = self.input.take_text();
        if prompt.trim().is_empty() {
            return;
        }
        if self.running {
            self.steer(prompt);
        } else {
            self.start_run(prompt);
        }
    }

    /// Send the prompt text straight to the currently running agent or
    /// delegate as a steering message: it is injected into that model's
    /// conversation at the run's next rest point. If the run ended just as the
    /// user pressed Enter the text is submitted as a normal new run instead,
    /// so it is never silently dropped.
    fn steer(&mut self, text: String) {
        self.show_user(&text);
        let delivered = self
            .steer_tx
            .as_ref()
            .is_some_and(|tx| tx.send(text.clone()).is_ok());
        if !delivered {
            self.push_meta("run ended before the steer landed; submitting as a new task");
            self.start_run(text);
        }
    }

    /// Queue whatever is in the prompt bar for the NEXT run (ctrl-Enter): the
    /// current run never sees it, and once the run ends the text is submitted
    /// automatically. When idle there is nothing to queue behind, so it
    /// behaves like a plain submit.
    fn queue_prompt(&mut self) {
        if !self.running {
            self.submit_prompt();
            return;
        }
        let text = self.input.take_text();
        if text.trim().is_empty() {
            return;
        }
        match self.queued_prompt.take() {
            Some(existing) => self.queued_prompt = Some(existing + "\n\n" + &text),
            None => self.queued_prompt = Some(text.clone()),
        }
        self.show_user(&text);
        self.push_meta("queued for the next run (ctrl-enter again to append)");
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
        let tools = match crate::build_tools(&cfg, &self.root) {
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
        // Pick up any newly-configured delegate: assign it a color the first
        // time it is seen (existing agents keep theirs) and refresh the chat
        // layout so the model panel and bands repaint.
        let mut agent_names: Vec<String> =
            self.cfg.delegates.iter().map(|d| d.name.clone()).collect();
        agent_names.push(self.cfg.llm.display());
        self.model_colors.assign(&agent_names);
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
    }

    /// M-x list-mcp-servers: open the modal showing every server configured
    /// under `[mcp.servers]` as a collapsible group of its connected tools,
    /// with a live on/off toggle per tool and a filter over server/tool names.
    /// An empty config still reports as a chat note.
    fn list_mcp_servers(&mut self) {
        let servers = &self.cfg.mcp.servers;
        if servers.is_empty() {
            self.push_meta("no MCP servers configured");
            return;
        }
        if self.pick.is_some() || self.mx.is_some() || self.search.is_some() {
            return;
        }
        self.mcp_view = Some(McpServersView {
            groups: mcp_tool_groups(servers, &self.tools),
            disabled: self.tools.disabled_handle(),
            filter: String::new(),
            collapsed: HashSet::new(),
            sel: 0,
        });
    }

    /// Move the modal selection by `dir` visible rows (-1 up, +1 down); larger
    /// jumps (page keys) step several rows at once. Clamped to the visible list.
    fn mcp_move(&mut self, dir: isize) {
        let Some(v) = &mut self.mcp_view else {
            return;
        };
        let rows = mcp_visible_rows(v).len();
        if rows == 0 {
            v.sel = 0;
            return;
        }
        let max = rows - 1;
        v.sel = if dir > 0 {
            v.sel.saturating_add(dir as usize).min(max)
        } else {
            v.sel.saturating_sub(dir.unsigned_abs())
        };
    }

    /// Re-clamp the selection after the visible rows changed (filter edits,
    /// collapse toggles).
    fn mcp_clamp_sel(&mut self) {
        let Some(v) = &mut self.mcp_view else {
            return;
        };
        let rows = mcp_visible_rows(v).len();
        v.sel = mcp_clamp(v.sel, rows);
    }

    /// The row the selection currently points at, when the list is non-empty.
    fn mcp_row_at(&self) -> Option<RowRef> {
        let v = self.mcp_view.as_ref()?;
        let rows = mcp_visible_rows(v);
        rows.get(mcp_clamp(v.sel, rows.len())).copied()
    }

    /// Group index owning the current selection.
    fn mcp_sel_group(&self) -> Option<usize> {
        match self.mcp_row_at()? {
            RowRef::Header(g) => Some(g),
            RowRef::Tool(g, _) => Some(g),
        }
    }

    /// Collapse/expand the group under the selection (Tab / Enter on a header).
    fn mcp_toggle_group(&mut self) {
        let Some(group) = self.mcp_sel_group() else {
            return;
        };
        let Some(v) = &mut self.mcp_view else {
            return;
        };
        if !v.collapsed.remove(&group) {
            v.collapsed.insert(group);
        }
        let rows = mcp_visible_rows(v).len();
        v.sel = mcp_clamp(v.sel, rows);
    }

    /// Force the collapse state of the group under the selection (Left/Right).
    fn mcp_collapse_group(&mut self, collapsed: bool) {
        let Some(group) = self.mcp_sel_group() else {
            return;
        };
        let Some(v) = &mut self.mcp_view else {
            return;
        };
        if collapsed {
            v.collapsed.insert(group);
        } else {
            v.collapsed.remove(&group);
        }
        let rows = mcp_visible_rows(v).len();
        v.sel = mcp_clamp(v.sel, rows);
    }

    /// Activate the row under the selection: a tool row flips its enabled state
    /// in the live filter (the agent loop stops advertising/invoking it from
    /// its next iteration); a group header toggles collapse instead.
    fn mcp_activate(&mut self) {
        let Some(row) = self.mcp_row_at() else {
            return;
        };
        match row {
            RowRef::Header(_) => self.mcp_toggle_group(),
            RowRef::Tool(group, tool) => {
                let name = self
                    .mcp_view
                    .as_ref()
                    .and_then(|v| v.groups.get(group))
                    .and_then(|g| g.tools.get(tool))
                    .map(|e| e.name.clone());
                let Some(name) = name else {
                    return;
                };
                let Some(v) = &mut self.mcp_view else {
                    return;
                };
                let mut disabled = v.disabled.write().unwrap_or_else(|p| p.into_inner());
                if !disabled.remove(&name) {
                    disabled.insert(name);
                }
            }
        }
    }

    /// Keys while the M-x list-mcp-servers modal is open. The filter is always
    /// active: any printable key edits the query live (so the human can just
    /// type to search); arrow keys navigate, Enter toggles, and Esc closes.
    fn handle_mcp_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Close the modal. Esc is the only close key — q and every other
            // printable key must stay free for typing filter text.
            KeyCode::Esc => self.mcp_view = None,
            // Edit the filter (always active).
            KeyCode::Backspace => {
                self.mcp_view.as_mut().unwrap().filter.pop();
            }
            KeyCode::Char('u') if ctrl => {
                self.mcp_view.as_mut().unwrap().filter.clear();
            }
            KeyCode::Char(c) if !ctrl && !alt => self.mcp_view.as_mut().unwrap().filter.push(c),
            // Enter activates the row under the cursor.
            KeyCode::Enter => self.mcp_activate(),
            // Tab collapses/expands the group under the cursor.
            KeyCode::Tab => self.mcp_toggle_group(),
            KeyCode::Up => self.mcp_move(-1),
            KeyCode::Down => self.mcp_move(1),
            KeyCode::PageUp => self.mcp_move(-10),
            KeyCode::PageDown => self.mcp_move(10),
            // Left/right collapse/expand the group under the cursor.
            KeyCode::Left => self.mcp_collapse_group(true),
            KeyCode::Right => self.mcp_collapse_group(false),
            // Emacs-style: ctrl-p previous row, ctrl-n next row.
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'p') => self.mcp_move(-1),
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'n') => self.mcp_move(1),
            _ => {}
        }
        self.mcp_clamp_sel();
    }

    /// Open a new empty session and switch to it, keeping the current one open
    /// and switchable — the emacs `C-x b <new-name>` scratch-buffer behaviour.
    /// The current session is parked in its slot (live state intact); if it has
    /// a run in flight, that run keeps running in the background and its events
    /// keep arriving into its own chat, so switching back shows the result.
    fn new_session(&mut self) {
        let id = self.next_id();
        let incoming = self.fresh_live(id);
        let outgoing = self.swap_live(incoming);
        self.open_sessions[self.active].live = Some(Box::new(outgoing));
        self.open_sessions.push(OpenSession {
            id,
            title: "New session".to_string(),
            file: None,
            live: None,
        });
        self.active = self.open_sessions.len() - 1;
        self.ctrl_x = false;
        self.path_prompt = None;
        self.session_pick = None;
        self.push_meta("opened a new session");
    }

    /// Close the active session (emacs `C-x k`): discard its slot and activate a
    /// neighbour (swapping the neighbour's live state in). Refuses to close the
    /// only open session, or one with a run in flight, so there is always a
    /// session to work in and no run is silently dropped.
    fn kill_session(&mut self) {
        if self.running {
            self.push_meta("cannot close a session while its run is in flight");
            return;
        }
        if self.open_sessions.len() <= 1 {
            self.push_meta("cannot close the only open session");
            return;
        }
        let killed = self.open_sessions[self.active].title.clone();
        self.open_sessions.remove(self.active);
        let idx = self.active.min(self.open_sessions.len() - 1);
        self.active = idx;
        if let Some(incoming) = self.open_sessions[idx].live.take() {
            // Swapping the neighbour in drops the killed session's live state.
            let _killed = self.swap_live(*incoming);
            self.chat_rows_cache = None;
        }
        self.push_meta(format!("closed session \"{killed}\""));
    }

    /// Snapshot the active session's observable state into a serializable form.
    fn session_snapshot(&self) -> SessionFile {
        let (history, rollup, evicted) = match self.history.try_lock() {
            Ok(h) => (h.history_clone(), h.rollup().to_string(), h.evicted),
            Err(_) => (Vec::new(), String::new(), 0),
        };
        SessionFile {
            version: 1,
            title: self.session.title(),
            status: self.session.status(),
            plan: self.session.plan(),
            delegated: self.session.delegated_ids().into_iter().collect(),
            finished: self.session.finished_summary(),
            chat: self.chat.clone(),
            section_collapsed: self.section_collapsed.clone(),
            ctx_tokens: self.ctx_tokens,
            ctx_budget: self.ctx_budget,
            ctx_estimated: self.ctx_estimated,
            history,
            rollup,
            evicted,
        }
    }

    /// Hand out the next session id.
    fn next_id(&mut self) -> u64 {
        let id = self.next_session_id;
        self.next_session_id += 1;
        id
    }

    /// id of the active session.
    fn active_id(&self) -> u64 {
        self.open_sessions[self.active].id
    }

    /// True while ANY open session has a run in flight (the active one or a
    /// background one), so the plan spinner keeps ticking.
    fn any_running(&self) -> bool {
        self.running
            || self
                .open_sessions
                .iter()
                .any(|s| s.live.as_ref().is_some_and(|l| l.running))
    }

    /// Slot index of the session with `id`, if it is still open.
    fn session_index(&self, id: u64) -> Option<usize> {
        self.open_sessions.iter().position(|s| s.id == id)
    }

    /// Build an empty live session: a fresh [`AgentSession`], undo log, tool
    /// context, rolling history and blank transcript, wired to its own tagged
    /// event relay.
    fn fresh_live(&self, id: u64) -> LiveState {
        let run_tx = spawn_tagged_relay(id, self.events_tx.clone());
        let session = Arc::new(AgentSession::new(run_tx.clone()));
        let ctx_base = self.make_ctx_base(
            id,
            session.clone(),
            Arc::new(comrade_core::MemoryUndo::new(self.root.clone())),
        );
        let history = Arc::new(tokio::sync::Mutex::new(build_session_context(
            &self.cfg,
            &self.root.to_string_lossy(),
            &self.tools,
        )));
        LiveState {
            session,
            ctx_base,
            history,
            run_tx,
            stop: None,
            run_handle: None,
            running: false,
            steer_tx: None,
            compact: None,
            queued_prompt: None,
            run_cancelled: false,
            chat: Vec::new(),
            section_collapsed: Vec::new(),
            chat_epoch: 0,
            chat_rows_cache: None,
            stream: String::new(),
            ctx_tokens: 0,
            ctx_budget: self
                .cfg
                .llm
                .context_window
                .unwrap_or(self.cfg.context.budget_tokens),
            ctx_estimated: true,
            activity: None,
            session_file: None,
            sel: None,
            scroll_top: 0,
            follow: true,
            was_at_bottom: true,
            search: None,
        }
    }

    /// Build a [`ToolContext`] for a session from the shared per-app bits.
    fn make_ctx_base(
        &self,
        id: u64,
        session: Arc<AgentSession>,
        undo: Arc<comrade_core::MemoryUndo>,
    ) -> ToolContext {
        ToolContext {
            project_root: self.root.clone(),
            cwd: self.root.clone(),
            session: session.as_control(),
            user: Arc::new(TuiUserIo {
                tx: self.asks_tx.clone(),
                session: id,
            }),
            undo,
            auto_approve: self.cfg.auto_approve(),
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            compact: None,
            stop: None,
        }
    }

    /// Build a live session from a loaded/forked [`SessionFile`].
    fn live_from_file(
        &self,
        id: u64,
        file: SessionFile,
        path: Option<std::path::PathBuf>,
    ) -> LiveState {
        let run_tx = spawn_tagged_relay(id, self.events_tx.clone());
        let session = Arc::new(AgentSession::new(run_tx.clone()));
        let ctx_base = self.make_ctx_base(
            id,
            session.clone(),
            Arc::new(comrade_core::MemoryUndo::new(self.root.clone())),
        );
        let delegated: HashSet<u64> = file.delegated.iter().copied().collect();
        let budget = file.ctx_budget.max(1);
        session.restore(file.title, file.status, file.plan, delegated, file.finished);
        let history = Arc::new(tokio::sync::Mutex::new(ContextManager::from_parts(
            budget,
            self.cfg.context.max_tool_output_chars,
            file.history,
            file.rollup,
            file.evicted,
        )));
        LiveState {
            session,
            ctx_base,
            history,
            run_tx,
            stop: None,
            run_handle: None,
            running: false,
            steer_tx: None,
            compact: None,
            queued_prompt: None,
            run_cancelled: false,
            chat: file.chat,
            section_collapsed: file.section_collapsed,
            chat_epoch: 0,
            chat_rows_cache: None,
            stream: String::new(),
            ctx_tokens: file.ctx_tokens,
            ctx_budget: budget,
            ctx_estimated: file.ctx_estimated,
            activity: None,
            session_file: path,
            sel: None,
            scroll_top: 0,
            follow: true,
            was_at_bottom: true,
            search: None,
        }
    }

    /// Swap the per-session fields between the App and `incoming`, returning the
    /// App's previous live state. Every field is moved (O(1)), so swapping is
    /// cheap enough to do per background event.
    fn swap_live(&mut self, mut incoming: LiveState) -> LiveState {
        std::mem::swap(&mut self.session, &mut incoming.session);
        std::mem::swap(&mut self.ctx_base, &mut incoming.ctx_base);
        std::mem::swap(&mut self.history, &mut incoming.history);
        std::mem::swap(&mut self.run_tx, &mut incoming.run_tx);
        std::mem::swap(&mut self.stop, &mut incoming.stop);
        std::mem::swap(&mut self.run_handle, &mut incoming.run_handle);
        std::mem::swap(&mut self.running, &mut incoming.running);
        std::mem::swap(&mut self.steer_tx, &mut incoming.steer_tx);
        std::mem::swap(&mut self.compact, &mut incoming.compact);
        std::mem::swap(&mut self.queued_prompt, &mut incoming.queued_prompt);
        std::mem::swap(&mut self.run_cancelled, &mut incoming.run_cancelled);
        std::mem::swap(&mut self.chat, &mut incoming.chat);
        std::mem::swap(&mut self.section_collapsed, &mut incoming.section_collapsed);
        std::mem::swap(&mut self.chat_epoch, &mut incoming.chat_epoch);
        std::mem::swap(&mut self.chat_rows_cache, &mut incoming.chat_rows_cache);
        std::mem::swap(&mut self.stream, &mut incoming.stream);
        std::mem::swap(&mut self.ctx_tokens, &mut incoming.ctx_tokens);
        std::mem::swap(&mut self.ctx_budget, &mut incoming.ctx_budget);
        std::mem::swap(&mut self.ctx_estimated, &mut incoming.ctx_estimated);
        std::mem::swap(&mut self.activity, &mut incoming.activity);
        std::mem::swap(&mut self.session_file, &mut incoming.session_file);
        std::mem::swap(&mut self.sel, &mut incoming.sel);
        std::mem::swap(&mut self.scroll_top, &mut incoming.scroll_top);
        std::mem::swap(&mut self.follow, &mut incoming.follow);
        std::mem::swap(&mut self.was_at_bottom, &mut incoming.was_at_bottom);
        std::mem::swap(&mut self.search, &mut incoming.search);
        incoming
    }

    /// Park the active session into its slot and swap the session at `idx` in.
    fn activate(&mut self, idx: usize) {
        if idx == self.active || idx >= self.open_sessions.len() {
            return;
        }
        let Some(incoming) = self.open_sessions[idx].live.take() else {
            return;
        };
        let outgoing = self.swap_live(*incoming);
        self.open_sessions[self.active].live = Some(Box::new(outgoing));
        self.active = idx;
        self.chat_rows_cache = None;
        self.chat_epoch = self.chat_epoch.wrapping_add(1);
    }

    /// The session that now owns `id` is not the active one: swap it in, apply
    /// the event to it, and swap it back, so its chat/metrics/plan keep updating
    /// while it runs in the background.
    fn on_agent_event_for(&mut self, id: u64, event: AgentEvent) {
        if id == self.active_id() {
            self.on_agent_event(event);
            return;
        }
        let Some(idx) = self.session_index(id) else {
            return; // session was closed; drop the event
        };
        let Some(incoming) = self.open_sessions[idx].live.take() else {
            return;
        };
        self.handling_bg = Some(idx);
        let active_live = self.swap_live(*incoming);
        self.on_agent_event(event);
        let bg = self.swap_live(active_live);
        self.open_sessions[idx].live = Some(Box::new(bg));
        // Keep the switcher's title in sync with the background session.
        self.open_sessions[idx].title = self.open_sessions[idx]
            .live
            .as_ref()
            .unwrap()
            .session
            .title();
        self.handling_bg = None;
    }

    /// Refresh the ACTIVE slot's (or, while a background event is handled, that
    /// slot's) title/file from the live session.
    fn refresh_active_slot(&mut self) {
        let idx = self.handling_bg.unwrap_or(self.active);
        if let Some(slot) = self.open_sessions.get_mut(idx) {
            slot.title = self.session.title();
            slot.file = self.session_file.clone();
        }
    }

    /// Switch the active session to the slot at `idx`.
    fn switch_to(&mut self, idx: usize) {
        if idx == self.active || idx >= self.open_sessions.len() {
            return;
        }
        let title = self.open_sessions[idx].title.clone();
        self.activate(idx);
        self.push_meta(format!("switched to session \"{title}\""));
    }

    /// Write the active session to `path`.
    fn save_to(&mut self, path: std::path::PathBuf) {
        let file = self.session_snapshot();
        match crate::session_store::save(&path, &file) {
            Ok(()) => {
                self.session_file = Some(path.clone());
                self.refresh_active_slot();
                self.push_meta(format!("saved session to {}", path.display()));
            }
            Err(e) => self.push_meta(format!("save failed: {e:#}")),
        }
    }

    /// Read a session from `path` and make it the active session (the current
    /// one is parked, live state intact).
    fn load_from(&mut self, path: std::path::PathBuf) {
        match crate::session_store::load(&path) {
            Ok(file) => {
                let mut file = file;
                let mut title = file.title.clone();
                if title.trim().is_empty() {
                    title = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "session".to_string());
                }
                file.title = title.clone();
                let id = self.next_id();
                let incoming = self.live_from_file(id, file, Some(path.clone()));
                let outgoing = self.swap_live(incoming);
                self.open_sessions[self.active].live = Some(Box::new(outgoing));
                self.open_sessions.push(OpenSession {
                    id,
                    title: title.clone(),
                    file: Some(path.clone()),
                    live: None,
                });
                self.active = self.open_sessions.len() - 1;
                self.chat_rows_cache = None;
                self.push_meta(format!(
                    "loaded session \"{title}\" from {}",
                    path.display()
                ));
            }
            Err(e) => self.push_meta(format!("load failed: {e:#}")),
        }
    }

    /// Fork the active session into an independent, immediately-active copy.
    fn fork_session(&mut self) {
        // Forking snapshots the active session; its rolling history is locked
        // while it runs, so refuse then (a background run does not block a fork
        // of the idle active session).
        if self.running {
            self.push_meta("cannot fork the active session while its run is in flight");
            return;
        }
        let mut fork = self.session_snapshot();
        let orig = fork.title.clone();
        let title = format!("{orig} (fork)");
        fork.title = title.clone();
        let id = self.next_id();
        let incoming = self.live_from_file(id, fork, None);
        let outgoing = self.swap_live(incoming);
        self.open_sessions[self.active].live = Some(Box::new(outgoing));
        self.open_sessions.push(OpenSession {
            id,
            title: title.clone(),
            file: None,
            live: None,
        });
        self.active = self.open_sessions.len() - 1;
        self.chat_rows_cache = None;
        self.push_meta(format!("forked session \"{orig}\" as \"{title}\""));
    }

    /// Open the path prompt to save the active session (Ctrl-x C-s).
    fn save_session_prompt(&mut self) {
        // Saving snapshots the active session; its rolling history is locked
        // while it runs, so refuse then (a background run does not block saving
        // the idle active session).
        if self.running {
            self.push_meta("cannot save the active session while its run is in flight");
            return;
        }
        let default = self
            .session_file
            .clone()
            .unwrap_or_else(|| crate::session_store::default_path(&self.root));
        self.path_prompt = Some(PathPrompt {
            intent: PathIntent::Save,
            input: default.to_string_lossy().to_string(),
        });
    }

    /// Open the path prompt to load a session (Ctrl-x C-f).
    fn load_session_prompt(&mut self) {
        let default = self
            .session_file
            .clone()
            .unwrap_or_else(|| crate::session_store::default_path(&self.root));
        self.path_prompt = Some(PathPrompt {
            intent: PathIntent::Load,
            input: default.to_string_lossy().to_string(),
        });
    }

    /// Open the session switcher overlay (Ctrl-x C-b).
    fn switch_session(&mut self) {
        if self.open_sessions.len() <= 1 {
            self.push_meta("only one session is open");
            return;
        }
        self.session_pick = Some(SessionPick { sel: self.active });
    }

    /// Keys while the session switcher overlay is open.
    fn handle_session_pick_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let max = self.open_sessions.len().saturating_sub(1);
        let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
        let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
        if up {
            if let Some(p) = &mut self.session_pick {
                p.sel = p.sel.saturating_sub(1);
            }
        } else if down {
            if let Some(p) = &mut self.session_pick {
                p.sel = (p.sel + 1).min(max);
            }
        } else {
            match key.code {
                KeyCode::Esc => self.session_pick = None,
                KeyCode::Char('g') if ctrl => self.session_pick = None,
                KeyCode::Enter => {
                    let idx = self
                        .session_pick
                        .as_ref()
                        .map(|p| p.sel)
                        .unwrap_or(self.active);
                    self.session_pick = None;
                    self.switch_to(idx);
                }
                _ => {}
            }
        }
    }

    /// Keys while the save/load session path prompt is open.
    fn handle_path_prompt_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.path_prompt = None,
            KeyCode::Char('g') if ctrl => self.path_prompt = None,
            KeyCode::Enter => {
                let Some(p) = self.path_prompt.take() else {
                    return;
                };
                let text = p.input.trim();
                if text.is_empty() {
                    self.push_meta("no path given");
                    return;
                }
                let raw = std::path::PathBuf::from(expand_tilde(text));
                let path = if raw.is_absolute() {
                    raw
                } else {
                    self.root.join(raw)
                };
                match p.intent {
                    PathIntent::Save => self.save_to(path),
                    PathIntent::Load => self.load_from(path),
                }
            }
            KeyCode::Backspace if !ctrl => {
                if let Some(p) = &mut self.path_prompt {
                    p.input.pop();
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(p) = &mut self.path_prompt {
                    p.input.push(c);
                }
            }
            _ => {}
        }
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
                let was_running = self.running;
                self.running = false;
                self.stop = None;
                self.run_handle = None;
                self.steer_tx = None;
                self.stream.clear();
                self.activity = None;
                // The cancel watchdog may emit a second RunEnd after aborting a
                // wedged run; report the transition (fold + status note) only
                // once.
                if was_running {
                    // Compact whatever the run left behind (interrupted runs
                    // end here without a final answer) before the status note.
                    self.fold_completed();
                    self.push_meta("run finished");
                    // A prompt queued while the run was active becomes the next
                    // run now -- unless the user cancelled, in which case the
                    // text is given back to the prompt bar instead of being
                    // auto-submitted against their intent.
                    let queued = self.queued_prompt.take();
                    let cancelled = self.run_cancelled;
                    self.run_cancelled = false;
                    if let Some(queued) = queued {
                        if cancelled {
                            for ch in queued.chars() {
                                self.input.insert(ch);
                            }
                            self.push_meta(
                                "run cancelled: the queued prompt is back in the prompt bar",
                            );
                        } else {
                            self.start_run(queued);
                        }
                    }
                }
            }
            AgentEvent::User(u) => self.show_user(&u),
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
                tokens,
            } => {
                // This turn produced a tool call: keep whatever the model was
                // saying before the call as a visible reasoning block, then
                // show a compact card. Important cards (diffs, and
                // pom_run_tests/pom_run_task results) open by default.
                self.commit_stream_reasoning();
                self.activity = Some(name.clone());
                let open_default =
                    matches!(name.as_str(), "fs_edit" | "pom_run_tests" | "pom_run_task");
                self.push_msg(Msg::tool(ToolCard {
                    name,
                    author: Some(self.actor_label()),
                    args,
                    justification,
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
                if name == "delegate" || name == "ask_advise" {
                    self.on_delegate_result(&name, &output, ok);
                } else if name == "pom_run_tests" {
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
                        if let Some(card) = self.last_tool_mut("pom_run_tests") {
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
                    } else if let Some(card) = self.last_tool_mut("pom_run_tests") {
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
            AgentEvent::TitleChanged => self.refresh_active_slot(),
            AgentEvent::StatusChanged | AgentEvent::PlanChanged => {}
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
            AgentEvent::ContextCompacted {
                before_messages,
                after_messages,
                before_tokens,
                after_tokens,
            } => {
                // The history shrank to a summary: reflect it in the gauge and
                // report the change in the chat.
                self.ctx_tokens = after_tokens;
                self.ctx_estimated = true;
                self.push_meta(format!(
                    "context compacted: {before_messages}->{after_messages} messages, \
                     ~{before_tokens}->~{after_tokens} tokens"
                ));
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
                let open_default =
                    matches!(name.as_str(), "fs_edit" | "pom_run_tests" | "pom_run_task");
                self.push_msg(Msg::tool(ToolCard {
                    name,
                    author: Some(model),
                    args,
                    justification: None,
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
                if name == "pom_run_tests" && output.contains("test result:") {
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

    /// A `delegate`/`ask_advise` tool call finished: show the delegate's reply
    /// (or advice) as its own chat entry under the delegate's model name. The
    /// parent's tool card keeps a compact summary; the full text lives in the
    /// delegate's message.
    fn on_delegate_result(&mut self, tool: &str, output: &str, ok: bool) {
        let parsed = match tool {
            "delegate" => parse_delegate_reply(output),
            "ask_advise" => parse_advice_reply(output),
            _ => None,
        };
        if ok && let Some((model, reply)) = parsed {
            if let Some(card) = self.last_tool_mut(tool) {
                card.ok = true;
                card.open = false;
                card.result = Some(format!("replied ({} chars)", reply.chars().count()));
            }
            if !reply.trim().is_empty() {
                // The sub-agent stretch (reads + the tool call) is done:
                // fold it so the reply reads as a clean block.
                self.fold_completed();
                self.push_msg(Msg::authored(MsgKind::Delegate, model, reply));
            }
            return;
        }
        // Unparseable or failed hand-off: keep the plain tool-card behaviour so
        // the error/raw text is still visible.
        if let Some(card) = self.last_tool_mut(tool) {
            card.result = Some(output.to_string());
            card.ok = ok;
        }
    }

    fn answer_top(&mut self, reply: UserReply) {
        if !self.dialogs.is_empty() {
            let (kind, text) = match &reply {
                UserReply::Answer(a) => (MsgKind::Meta, format!("answer: {a}")),
                UserReply::Denied => (MsgKind::Meta, "dismissed".to_string()),
                // A submitted answer is part of the question exchange, so it
                // stays visible in focus mode like the question itself.
                UserReply::Form(answers) => {
                    let shown = answers
                        .iter()
                        .map(|(k, v)| format!("{k} = {v}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    (MsgKind::Question, format!("form submitted:\n{shown}"))
                }
            };
            self.push_msg(Msg::text(kind, text));
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
            MxCommand::CompactContext => self.compact_context(),
            MxCommand::Copy => {
                // Mirrors the Ctrl+Shift+C / M-w chords: copy the prompt's
                // selection when there is one, otherwise the chat message
                // under the cursor.
                self.copy_prompt_or_block();
            }
            MxCommand::EndOfLine => self.input.move_end(false),
            MxCommand::ForkSession => self.fork_session(),
            MxCommand::ForwardWord => self.input.move_word_right(false),
            MxCommand::InsertNewline => self.input.insert('\n'),
            MxCommand::KillSession => self.kill_session(),
            MxCommand::KillWord => self.input.delete_word(),
            MxCommand::ListMcpServers => self.list_mcp_servers(),
            MxCommand::LoadSession => self.load_session_prompt(),
            MxCommand::MoveBlockDown => self.move_block(1),
            MxCommand::MoveBlockUp => self.move_block(-1),
            MxCommand::MoveUserDown => self.move_user(1),
            MxCommand::MoveUserUp => self.move_user(-1),
            MxCommand::NewSession => self.new_session(),
            MxCommand::QueuePrompt => self.queue_prompt(),
            MxCommand::Quit => return true,
            MxCommand::ReloadConfig => self.reload_config(),
            MxCommand::SaveSession => self.save_session_prompt(),
            MxCommand::SearchChat => self.search = Some(Search::new()),
            MxCommand::SteerPrompt => self.submit_prompt(),
            MxCommand::SubmitPrompt => self.submit_prompt(),
            MxCommand::SwitchSession => self.switch_session(),
            MxCommand::ToggleAutoAccept => self.toggle_auto_accept(),
            MxCommand::ToggleToolCard => {
                if let Some(idx) = self.sel {
                    self.toggle_tool(idx);
                }
            }
            MxCommand::FocusMode => self.toggle_focus_mode(),
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
        if let Some(d) = self.dialogs.first()
            && let UserPrompt::Confirm { title, .. } = &d.prompt
        {
            let action = one_line(title, 80);
            self.push_msg(Msg::text(
                MsgKind::Meta,
                format!("auto-accept on → approved: {action}"),
            ));
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
        let reply = match self.dialogs.first_mut() {
            Some(d) => UserReply::Answer(std::mem::take(&mut d.buf)),
            None => return,
        };
        self.answer_top(reply);
    }

    /// Submit the form on top of the dialog stack as a `UserReply::Form`.
    /// Refuses while a required field is still empty.
    fn submit_form(&mut self) {
        let Some(d) = self.dialogs.first() else {
            return;
        };
        let UserPrompt::Form(spec) = &d.prompt else {
            return;
        };
        let Some(form) = &d.form else {
            return;
        };
        let answers = form.answers(spec);
        if !spec.is_complete(&answers) {
            self.push_msg(Msg::text(
                MsgKind::Meta,
                "form: fill all required fields before submitting".to_string(),
            ));
            return;
        }
        self.answer_top(UserReply::Form(answers));
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
        models.extend(self.cfg.delegates.iter().filter(|d| d.enabled).map(|d| {
            if let Some(ctx_window) = d.llm.context_window {
                format!("{} ({})", d.name, ctx_window)
            } else {
                d.name.clone()
            }
        }));

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
                if let Some(p) = &mut self.pick
                    && p.model_sel.is_some()
                {
                    p.model_sel = None;
                }
            }
            KeyCode::Char(c)
                if !ctrl && self.pick.as_ref().is_some_and(|p| p.model_sel.is_some()) =>
            {
                if let Some(d) = c.to_digit(10)
                    && d >= 1
                {
                    self.apply_pick_to_index(d as usize - 1);
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

/// Build the modal's tool groups: one group per configured server (config
/// order) whose members are every registered tool whose full local name starts
/// with that server's MCP prefix (`mcp_<server>_`, see
/// [`comrade_tool_mcp::mcp_server_prefix`]). Servers with no connected tool
/// yield an empty group. Uses `iter_all` so tools the human already disabled
/// stay visible and can be turned back on.
fn mcp_tool_groups(
    servers: &[comrade_core::McpServerCfg],
    tools: &comrade_tool::ToolRegistry,
) -> Vec<McpToolGroup> {
    servers
        .iter()
        .map(|s| {
            let prefix = comrade_tool_mcp::mcp_server_prefix(&s.name);
            let desc_prefix = format!("[MCP server `{}`] ", s.name);
            let tools = tools
                .iter_all()
                .filter(|t| t.spec().name.starts_with(&prefix))
                .map(|t| {
                    let spec = t.spec();
                    let desc = spec
                        .description
                        .strip_prefix(&desc_prefix)
                        .unwrap_or(&spec.description);
                    McpToolEntry {
                        name: spec.name.clone(),
                        desc: desc.trim().to_string(),
                    }
                })
                .collect();
            McpToolGroup {
                server: s.name.clone(),
                tools,
            }
        })
        .collect()
}

/// Flat visible rows of the modal in draw order, honouring the filter and the
/// collapsed set:
///
/// * empty filter — every group header, then each group's tool rows unless the
///   group is collapsed;
/// * non-empty filter `q` — a group is kept when its server name matches `q`
///   (showing ALL of its tools) or any of its tools' names match (showing just
///   those rows). A collapsed group keeps only its header.
///
/// Each visible row renders as exactly one screen line, so a row's index here
/// doubles as its line offset.
fn mcp_visible_rows(view: &McpServersView) -> Vec<RowRef> {
    let q = view.filter.to_lowercase();
    let mut out = Vec::new();
    for (gi, group) in view.groups.iter().enumerate() {
        let server_hit = group.server.to_lowercase().contains(&q);
        let tool_hits: Vec<usize> = if q.is_empty() || server_hit {
            (0..group.tools.len()).collect()
        } else {
            group
                .tools
                .iter()
                .enumerate()
                .filter(|(_, t)| t.name.to_lowercase().contains(&q))
                .map(|(ti, _)| ti)
                .collect()
        };
        let kept = q.is_empty() || server_hit || !tool_hits.is_empty();
        if !kept {
            continue;
        }
        out.push(RowRef::Header(gi));
        if view.collapsed.contains(&gi) {
            continue;
        }
        out.extend(tool_hits.into_iter().map(|ti| RowRef::Tool(gi, ti)));
    }
    out
}

/// Number of visible rows; clamps a selection cursor into range (0 when empty).
fn mcp_clamp(sel: usize, rows: usize) -> usize {
    rows.saturating_sub(1).min(sel)
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

/// Assemble the App: one open session (id 0) wired to its own tagged event
/// relay, plus the shared UI plumbing (input, git bar, dialogs, colors).
#[allow(clippy::too_many_arguments)]
fn build_app(
    deps: &Deps,
    events_tx: mpsc::UnboundedSender<TaggedEvent>,
    events_rx: mpsc::UnboundedReceiver<TaggedEvent>,
    run_tx: mpsc::Sender<AgentEvent>,
    asks_tx: mpsc::Sender<PendingAsk>,
    asks_rx: mpsc::Receiver<PendingAsk>,
    git_tx: mpsc::Sender<GitBarInfo>,
    git_rx: mpsc::Receiver<GitBarInfo>,
) -> App {
    let user = Arc::new(TuiUserIo {
        tx: asks_tx.clone(),
        session: 0,
    });
    let bundle = session_bundle(deps, user, run_tx.clone());

    // Assign a stable color to each agent (main model + delegates) once, at
    // start: the chat's delegate sub-chats and the model windows use it.
    let mut model_colors = ModelColors::new();
    let mut agent_names: Vec<String> = deps.cfg.delegates.iter().map(|d| d.name.clone()).collect();
    agent_names.push(deps.cfg.llm.display());
    model_colors.assign(&agent_names);

    App {
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
        run_tx,
        asks_rx,
        asks_tx,
        stop: None,
        run_handle: None,
        running: false,
        steer_tx: None,
        compact: None,
        queued_prompt: None,
        run_cancelled: false,
        last_draw: std::time::Instant::now(),
        auto_accept: false,
        focus_mode: false,
        git: GitBarInfo::default(),
        git_rx,
        git_tx,
        git_inflight: false,
        git_gate: None,
        chat: Vec::new(),
        chat_epoch: 0,
        chat_rows_cache: None,
        model_colors,
        section_collapsed: Vec::new(),
        stream: String::new(),
        input: Editor::new(),
        search: None,
        dialogs: Vec::new(),
        dialog_ask: false,
        dialog_ask_tx: None,
        dialog_conv: Vec::new(),
        pick: None,
        mcp_view: None,
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
        clipboard: None,
        open_sessions: vec![OpenSession {
            id: 0,
            title: "New session".to_string(),
            file: None,
            live: None,
        }],
        active: 0,
        next_session_id: 1,
        handling_bg: None,
        session_file: None,
        ctrl_x: false,
        path_prompt: None,
        session_pick: None,
    }
}

pub async fn run(deps: &Deps) -> Result<()> {
    let (asks_tx, asks_rx) = mpsc::channel::<PendingAsk>(16);
    let (dialog_ans_tx, mut dialog_ans_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (git_tx, git_rx) = mpsc::channel::<GitBarInfo>(4);
    // One central, UI-facing queue for every session's events, plus the first
    // session (id 0) with its own run-facing sender that tags events with 0.
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<TaggedEvent>();
    let run_tx = spawn_tagged_relay(0, events_tx.clone());
    let mut app = build_app(
        deps, events_tx, events_rx, run_tx, asks_tx, asks_rx, git_tx, git_rx,
    );
    app.dialog_ask_tx = Some(dialog_ans_tx);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    // Ask the terminal for the kitty keyboard-enhancement protocol so that
    // Ctrl+Shift+C arrives as a distinct key (with SHIFT) instead of being
    // folded into plain Ctrl+C. Terminals that do not support it simply ignore
    // the request and keep sending legacy byte streams.
    #[cfg(unix)]
    let _ = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    );

    // Terminal events arrive on a background thread.
    let (kev_tx, mut kev_rx) = mpsc::channel::<Event>(128);
    std::thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(100)).ok() != Some(true) {
                continue;
            }
            if let Ok(ev) = event::read()
                && kev_tx.blocking_send(ev).is_err()
            {
                break;
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
                    Some((id, e)) => app.on_agent_event_for(id, e),
                    None => break Err(anyhow::anyhow!("agent event channel closed")),
                }
            }
            ask = app.asks_rx.recv() => {
                match ask {
                    Some(ask) => {
                        // Auto-accept mode answers any prompt that carries a
                        // recommended value (a confirm, or a form with
                        // recommendations) without asking the human.
                        if app.auto_accept
                            && let Some(reply) = auto_reply(&ask.prompt)
                        {
                            let label = auto_reply_label(&ask.prompt);
                            if let UserPrompt::Form(spec) = &ask.prompt {
                                app.push_msg(Msg::text(
                                    MsgKind::Question,
                                    form_question_text(spec),
                                ));
                            }
                            let _ = ask.reply.send(reply);
                            app.push_msg(Msg::text(MsgKind::Meta, label));
                        } else {
                            let form = match &ask.prompt {
                                UserPrompt::Form(spec) => Some(FormEdit::new(spec)),
                                _ => None,
                            };
                            // Record the question in the transcript so it stays
                            // visible in focus mode (the dialog alone is not).
                            if let UserPrompt::Form(spec) = &ask.prompt {
                                app.push_msg(Msg::text(
                                    MsgKind::Question,
                                    form_question_text(spec),
                                ));
                            }
                            let buf = String::new();
                            app.dialogs.push(Dialog { prompt: ask.prompt, buf, reply: ask.reply, session: ask.session, form });
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
            _ = spin.tick(), if app.any_running() => {
                // Spinner wake-up only: the redraw below re-renders the plan
                // panel with the next glyph frame.
            }
        }
        app.refresh_git();
        // Coalesce repaints: while a run is streaming, bursts of agent events
        // (a fast local model feeds a `Delta` every ~33 ms plus tool activity)
        // can arrive faster than the terminal can usefully repaint, and each
        // repaint costs more as the conversation grows. Cap event-driven
        // repaints at ~30 fps while running; idle frames are never throttled
        // (nothing floods when no run is in flight), and the plan spinner's
        // 100 ms tick clears the cap every time, so animation is unaffected.
        let now = std::time::Instant::now();
        let capped =
            app.running && now.duration_since(app.last_draw) < std::time::Duration::from_millis(33);
        if !capped {
            app.last_draw = now;
            let _ = terminal.draw(|f| draw(&mut app, f));
        }
    };

    #[cfg(unix)]
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    res
}

/// Returns true when the app should quit.
fn handle_event(app: &mut App, ev: Event) -> bool {
    match ev {
        Event::Key(key) => {
            // With REPORT_EVENT_TYPES active the terminal reports key releases
            // too; act only on presses (and auto-repeat), never on the release.
            if key.kind == KeyEventKind::Release {
                return false;
            }
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
                        app.copy_prompt_or_block();
                        return false;
                    }
                    // Plain Ctrl+C quits.
                    return true;
                }
            }
            if app.path_prompt.is_some() {
                app.handle_path_prompt_key(key);
                return false;
            }
            if app.session_pick.is_some() {
                app.handle_session_pick_key(key);
                return false;
            }
            // Ctrl-x is a prefix (emacs): the next key picks the command.
            if app.ctrl_x {
                app.ctrl_x = false;
                match key.code {
                    KeyCode::Char('b') => app.switch_session(),
                    KeyCode::Char('s') => app.save_session_prompt(),
                    KeyCode::Char('f') => app.load_session_prompt(),
                    KeyCode::Char('k') => app.kill_session(),
                    KeyCode::Char('w') => app.fork_session(),
                    _ => {}
                }
                return false;
            }
            if app.pick.is_some() {
                app.handle_pick_key(key);
                return false;
            }
            if app.mcp_view.is_some() {
                app.handle_mcp_key(key);
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
            // Ctrl-X opens a prefix chord: C-x C-b/s/f/w for session commands.
            if key.code == KeyCode::Char('x')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                app.ctrl_x = true;
                return false;
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
            // Emacs-style kill/yank on the prompt editor: C-y pastes the
            // system clipboard at the cursor; C-k kills to the end of the
            // line (or the selection, when one is active), joining lines at
            // the end of a line. Copy is M-w / Ctrl+Shift+C.
            if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.paste_clipboard();
                return false;
            }
            if key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.kill_line();
                return false;
            }
            // Emacs-style chat navigation. Plain Ctrl+p/n move block to block;
            // Ctrl+Shift and Alt variants (P/N) jump between user messages.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && let KeyCode::Char(ch) = key.code
            {
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
            if key.modifiers.contains(KeyModifiers::ALT)
                && let KeyCode::Char(ch) = key.code
            {
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
                if ch.eq_ignore_ascii_case(&'w') {
                    // M-w: Emacs-style "copy". Every terminal forwards
                    // Alt+W, so this works even where Ctrl+Shift+C is
                    // claimed by the terminal emulator itself.
                    app.copy_prompt_or_block();
                    return false;
                }
                if ch.eq_ignore_ascii_case(&'f') {
                    // M-f: toggle focus mode.
                    app.toggle_focus_mode();
                    return false;
                }
                if ch.eq_ignore_ascii_case(&'c') {
                    // M-c: compact the context (summarise and replace it).
                    app.compact_context();
                    return false;
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
                KeyCode::Enter if ctrl => {
                    // Ctrl+Enter while a run is active queues the prompt for
                    // the NEXT run; when idle it submits like plain Enter.
                    if app.running {
                        app.queue_prompt();
                    } else {
                        app.submit_prompt();
                    }
                }
                KeyCode::Enter => {
                    if shift {
                        // Shift+Enter inserts a newline instead of submitting.
                        app.input.insert('\n');
                    } else {
                        // Enter submits the prompt: it starts a run when idle,
                        // and steers the running agent/delegate while a run is
                        // in flight.
                        app.submit_prompt();
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
                    if let (false, Some(keys)) = (overlay_open, cmd.keys()) {
                        // Stay open in hint mode: the row tells the user
                        // how to run the command directly next time.
                        app.mx = Some(Mx {
                            query: String::new(),
                            matches: Vec::new(),
                            sel: 0,
                            done: Some(format!("you can run this command with {keys}")),
                        });
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
    if matches!(app.dialogs.first().unwrap().prompt, UserPrompt::Form(_)) {
        return handle_form_key(app, code);
    }
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

/// Drive a form dialog: navigate fields, edit the focused component, submit
/// (`enter`) or cancel (`esc`).
fn handle_form_key(app: &mut App, code: KeyCode) -> bool {
    enum Act {
        None,
        Deny,
        Submit,
    }
    let act = {
        let Some(d) = app.dialogs.first_mut() else {
            return true;
        };
        let Dialog { prompt, form, .. } = d;
        let UserPrompt::Form(spec) = &*prompt else {
            return true;
        };
        let Some(form) = form.as_mut() else {
            return true;
        };
        match code {
            KeyCode::Esc => Act::Deny,
            KeyCode::Enter => Act::Submit,
            KeyCode::Up | KeyCode::BackTab => {
                form.focus(false);
                Act::None
            }
            KeyCode::Down | KeyCode::Tab => {
                form.focus(true);
                Act::None
            }
            KeyCode::Left => {
                form.adjust(spec, -1);
                Act::None
            }
            KeyCode::Right => {
                form.adjust(spec, 1);
                Act::None
            }
            KeyCode::Char(c) => {
                form.input(spec, c);
                Act::None
            }
            KeyCode::Backspace => {
                form.backspace();
                Act::None
            }
            _ => Act::None,
        }
    };
    match act {
        Act::Deny => app.answer_top(UserReply::Denied),
        Act::Submit => app.submit_form(),
        Act::None => {}
    }
    true
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

/// Whether a message produces any row in focus mode: the spoken conversation
/// (user turn, model reply, delegate advisory), the model reasoning and the
/// questions the agent asked (ask_form). Tool cards, failure blocks and grey
/// status notes are dropped; a folded run digest survives only as the
/// reasoning inside it.
fn focus_visible(msg: &Msg) -> bool {
    match msg.kind {
        MsgKind::User
        | MsgKind::Assistant
        | MsgKind::Delegate
        | MsgKind::Reasoning
        | MsgKind::Question => true,
        MsgKind::Run => msg.children.iter().any(|c| c.kind == MsgKind::Reasoning),
        MsgKind::Tool | MsgKind::Failure | MsgKind::Meta => false,
    }
}

/// Whether a chat message is currently exposed: a user-turn heading always is
/// (it is its own section's header); every other message shows only while its
/// section is expanded. Content before the first user turn is exposed. In focus
/// mode only `focus_visible` messages are exposed at all.
fn chat_visible(chat: &[Msg], collapsed: &[bool], idx: usize, focus: bool) -> bool {
    let Some(msg) = chat.get(idx) else {
        return false;
    };
    if focus && !focus_visible(msg) {
        return false;
    }
    match msg.kind {
        MsgKind::User => true,
        _ => section_of_msg(chat, idx).is_none_or(|(o, _)| !collapsed_at(collapsed, o)),
    }
}

/// Next/previous visible chat block from the current selection, skipping the
/// messages hidden inside a collapsed section. Returns None at the ends.
fn step_visible(
    chat: &[Msg],
    collapsed: &[bool],
    sel: Option<usize>,
    dir: isize,
    focus: bool,
) -> Option<usize> {
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
        if chat_visible(chat, collapsed, cur as usize, focus) {
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
            "\n{}\n{}\n{}\n{}",
            t.name,
            t.args,
            t.justification.as_deref().unwrap_or(""),
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
// pom_run_tests output parsing
// ---------------------------------------------------------------------------
/// Parse a `pom_run_tests` summary: counts + duration + one (name, detail) per
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
    if let Some(home) = home
        && let Ok(rel) = root.strip_prefix(home)
    {
        if rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rel.to_string_lossy());
    }
    root.to_string_lossy().into_owned()
}

/// Status of open session `i` for the mode line and the switcher:
/// `Some("waiting")` when its run is waiting on a user dialog,
/// `Some("running")` when a run is in flight (and not waiting), else None.
fn session_status_marker(app: &App, i: usize) -> Option<&'static str> {
    let s = app.open_sessions.get(i)?;
    if app.dialogs.iter().any(|d| d.session == s.id) {
        return Some("waiting");
    }
    let running = if i == app.active {
        app.running
    } else {
        s.live.as_ref().is_some_and(|l| l.running)
    };
    running.then_some("running")
}

/// Right-aligned mode-line label counting open sessions by run status, e.g.
/// "2 running, 1 waiting, 1 idle". Statuses with no sessions are omitted.
fn session_counts_label(app: &App) -> String {
    let total = app.open_sessions.len();
    let mut running = 0usize;
    let mut blocked = 0usize;
    for i in 0..total {
        match session_status_marker(app, i) {
            Some("waiting") => blocked += 1,
            Some("running") => running += 1,
            _ => {}
        }
    }
    let idle = total.saturating_sub(running + blocked);
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if blocked > 0 {
        parts.push(format!("{blocked} waiting"));
    }
    if idle > 0 {
        parts.push(format!("{idle} idle"));
    }
    parts.join(", ")
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

    // The bar spans the whole row; a right-aligned segment holds the per-status
    // session counts next to the app name. Both drop out together on narrow
    // terminals so the left-hand mode line keeps its room.
    let counts_text = session_counts_label(app);
    let counts_w = counts_text.chars().count() as u16;
    let tag_w = (APP_TAG.chars().count() as u16).min(rows[2].width.saturating_sub(60));
    let gap_w = if tag_w > 0 && counts_w > 0 { 2 } else { 0 };
    let right_w = (counts_w + gap_w + tag_w).min(rows[2].width.saturating_sub(40));
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_w)])
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
        let right = vec![
            Span::styled(counts_text, bar_style),
            Span::raw("  "),
            Span::styled(APP_TAG, bar_style.add_modifier(Modifier::BOLD)),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(right))
                .style(bar_style)
                .alignment(Alignment::Right),
            bottom[1],
        );
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Percentage(30)])
        .split(rows[0]);
    draw_chat(app, frame, cols[0]);
    // The model panel shows 2 fixed rows (label+gauge/usage merged, then delegates)
    // plus one row per wrapped delegate line; grow it so delegate text is never
    // clipped out of view. The plan panel takes whatever is left.
    // (MODEL_PANEL_FIXED_ROWS + 2 = 2 fixed rows + 2 borders; + delegate_block rows)
    let delegate_rows = delegate_panel_rows(
        &app.cfg.delegates,
        &app.model_colors,
        usize::from(cols[1].width.saturating_sub(2)).max(1),
    );
    let delegate_block = delegate_rows.as_ref().map_or(0, |r| r.len() as u16);
    let stats_h = (MODEL_PANEL_FIXED_ROWS + 2 + delegate_block).min(rows[0].height);
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
    } else if let Some(p) = &app.path_prompt {
        // The save/load session path minibuffer replaces the prompt line.
        let label = match p.intent {
            PathIntent::Save => " save session to ",
            PathIntent::Load => " load session from ",
        };
        let line = Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(p.input.clone()),
            Span::styled("_", Style::default().fg(Color::Cyan)),
            Span::styled(
                "  enter:confirm  esc:cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), rows[1]);
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
        draw_model_pick(p, &app.model_colors, frame);
    }
    if let Some(p) = &app.session_pick {
        draw_session_pick(p, app, frame);
    }
    if let Some(v) = &app.mcp_view {
        draw_mcp_servers(v, frame);
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
    sub: Option<Color>,
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
    let mut spans: Vec<Span> = Vec::with_capacity(r.spans.len() + 2);
    if sel {
        spans.push(Span::styled(
            "> ",
            prefix_style(band, Some(Color::Yellow)).add_modifier(Modifier::BOLD),
        ));
    } else if let Some(color) = sub {
        // Delegate sub-chat: its own color as a left rule plus a 2-column
        // indent that sets the delegate's conversation apart from the parent's.
        spans.push(Span::styled(
            "| ",
            prefix_style(band, Some(color)).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("  ", prefix_style(band, None)));
    } else {
        match r.rule {
            Some(color) => spans.push(Span::styled(
                "| ",
                prefix_style(band, Some(color)).add_modifier(Modifier::BOLD),
            )),
            None => spans.push(Span::styled("  ", prefix_style(band, None))),
        }
    }
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

    // While the agent works, a focus-mode chat shows only the conversation, so
    // a silent tool run (no reasoning to stream) would look frozen. Reserve the
    // last chat row for a bottom activity spinner so there is always a visible
    // sign of life.
    let spinner = inner.height >= 2 && activity_spinner_visible(app);
    let view_h = inner.height.saturating_sub(u16::from(spinner));
    let rows_rect = Rect {
        height: view_h,
        ..inner
    };

    let width = inner.width.saturating_sub(2) as usize; // prefix column + spacing
    // Reuse the row layout of `chat` across frames while nothing chat-affecting
    // changed (typing, scrolling, search, most streaming deltas), instead of
    // re-tokenising and re-wrapping every message body on each draw.
    let cache = chat_cache(
        &mut app.chat_rows_cache,
        app.chat_epoch,
        width,
        app.focus_mode,
        &app.chat,
        &app.section_collapsed,
        &app.cfg.delegates,
        &app.model_colors,
    );

    // The live streaming preview is transient: laid out fresh every frame.
    let mut preview: Vec<Vec<Span<'static>>> = if app.stream.is_empty() {
        Vec::new()
    } else {
        let visible = strip_react_scaffolding(&app.stream);
        md_to_lines(&visible, width)
    };

    let total_rows = cache.rows.len() + preview.len();
    let max = total_rows.saturating_sub(view_h as usize);
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
    app.chat_rect = rows_rect;
    app.row_targets = cache.rows.iter().map(|r| r.tool_header).collect();
    app.row_targets.resize(total_rows, None);
    app.row_msg = cache.owner.clone();
    app.row_msg.resize(total_rows, None);
    app.msg_ranges = cache.ranges.clone();
    app.view_rows = view_h as usize;

    let sel_start = app.sel.and_then(|i| app.msg_ranges.get(i)).map(|&(s, _)| s);
    // Row span of the currently selected search match, if any.
    let search_hl = app
        .search
        .as_ref()
        .and_then(|s| s.matches.get(s.cur))
        .and_then(|&idx| app.msg_ranges.get(idx).copied());

    // Build Lines only for the rows actually on screen.
    let height = view_h as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(height.min(total_rows));
    let chat_rows = cache.rows.len();
    for row in offset..total_rows.min(offset + height) {
        if row < chat_rows {
            let r = &cache.rows[row];
            let in_match = search_hl.is_some_and(|(start, len)| row >= start && row < start + len);
            // Band + sub-chat tint by the row's owning message: user turns get
            // the lifted user band; delegate rows are tinted in their agent's
            // dim color and indented under the parent.
            let owner = cache.owner[row].and_then(|i| app.chat.get(i));
            // Band + sub-chat tint by the row's owning message: user turns get
            // the lifted user band; reasoning and delegate rows are tinted in
            // their model's dim color (delegate rows also indent under the
            // parent).
            let sub = owner
                .and_then(|m| subchat_model(m.author.as_deref(), &app.cfg.delegates))
                .map(|a| app.model_colors.name_color(a));
            let band = row_band(owner, &app.model_colors, &app.cfg.delegates, app.focus_mode);
            let row_width = if sub.is_some() {
                width.saturating_sub(2)
            } else {
                width
            };
            lines.push(render_row_line(
                r,
                Some(row) == sel_start,
                in_match,
                band,
                row_width,
                sub,
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

    frame.render_widget(Paragraph::new(lines).scroll((0, 0)), rows_rect);
    if spinner {
        let row = Rect {
            y: inner.y + view_h,
            height: 1,
            ..inner
        };
        frame.render_widget(activity_line(app, now_ms()), row);
    }
}

/// Pure row layout for a chat transcript. Org-style sections: each user turn is
/// a heading ("prompt echo") and the exchange under it — every message up to
/// the next user turn — is its body. A collapsed section renders only its
/// heading plus a "… N more" marker, so the exchange folds to one tinted block.
/// Return shape of [`layout_chat_rows`]: the render rows plus, per row, the
/// owning chat-message index and that message's `(start row, height)` span.
type ChatRowLayout = (Vec<RenderRow>, Vec<Option<usize>>, Vec<(usize, usize)>);

fn layout_chat_rows(
    chat: &[Msg],
    collapsed: &[bool],
    stream: &str,
    width: usize,
    delegates: &[DelegateCfg],
    colors: &ModelColors,
    focus: bool,
) -> ChatRowLayout {
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
        // Rows authored by a delegate form its sub-chat: laid out two columns
        // narrower than the parent so the render prefix can indent them, and
        // tinted in the delegate's color.
        let sub = subchat_model(msg.author.as_deref(), delegates);
        let w = if sub.is_some() {
            width.saturating_sub(2)
        } else {
            width
        };
        if focus && !focus_visible(msg) {
            ranges.push((out.len(), 0));
            i += 1;
            continue;
        }
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
            MsgKind::Run => {
                if focus {
                    // Focus mode drops the digest's summary row but keeps the reasoning
                    // folded inside it, rendered as normal reasoning blocks.
                    for child in &msg.children {
                        if child.kind == MsgKind::Reasoning {
                            layout_reasoning(
                                &mut out,
                                child.text.as_str(),
                                child.author.as_deref().unwrap_or("model"),
                                colors,
                                child.open,
                                width,
                            );
                        }
                    }
                } else {
                    layout_run(&mut out, i, &msg.children);
                }
            }
            MsgKind::Failure => layout_failure(&mut out, i, msg.fail.as_ref().unwrap(), width),
            MsgKind::Tool => layout_tool(&mut out, i, msg.tool.as_ref().unwrap(), w),
            MsgKind::Reasoning => layout_reasoning(
                &mut out,
                msg.text.as_str(),
                msg.author.as_deref().unwrap_or("model"),
                colors,
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
            MsgKind::Question => {
                // The agent asked the human something (ask_form): a yellow entry
                // that stays visible in focus mode, unlike the grey Meta notes.
                let style = Style::default().fg(Color::Yellow);
                for s in plain_wrap(&msg.text, width) {
                    out.push(RenderRow {
                        rule: None,
                        spans: vec![Span::styled(s, style)],
                        tool_header: None,
                    });
                }
            }
            MsgKind::Assistant => {
                // The model's spoken answer: a one-line header in the model's
                // colour over a markdown body, banded with the model's dim
                // colour — the same shape as the reasoning block, so answers,
                // thinking and delegate replies read as one integrated chat.
                let author = msg.author.as_deref().unwrap_or("assistant");
                author_header(&mut out, author, colors.name_color(author), width);
                for spans in md_to_lines(&msg.text, width) {
                    out.push(RenderRow {
                        rule: None,
                        spans,
                        tool_header: None,
                    });
                }
            }
            MsgKind::Delegate => {
                let author = msg.author.as_deref().unwrap_or("delegate");
                let color = sub.map(|m| colors.name_color(m)).unwrap_or(Color::Magenta);
                author_header(&mut out, author, color, w);
                let rule = Some(color);
                for spans in md_to_lines(&msg.text, w) {
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
    let is_run = matches!(chat.get(idx).map(|m| m.kind), Some(MsgKind::Run));
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
        "fs_read_file"
            | "fs_read_ranges"
            | "fs_list_dir"
            | "fs_list_files"
            | "fs_rgrep"
            | "ts_list_symbols"
            | "ts_find_symbol"
            | "ts_read_symbol"
            | "ts_structural_map"
            | "ts_find_references"
            | "pom_model"
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
        "fs_edit" => ("±", Color::Cyan),
        "fs_write_file" => ("✎", Color::Cyan),
        "pom_run_tests" => ("▶", Color::Yellow),
        "pom_run_task" => ("▸", Color::Yellow),
        "shell" => ("$", Color::Green),
        "git_status" | "git_diff" | "git_show" | "git_log" | "git_commit" => ("↗", Color::Magenta),
        "delegate" | "ask_advise" => ("⇄", Color::Magenta),
        "fs_read_file" | "fs_read_ranges" => ("≡", Color::Blue),
        "fs_list_dir" | "fs_list_files" | "fs_rgrep" | "ts_list_symbols" | "ts_find_symbol"
        | "ts_read_symbol" | "ts_structural_map" | "ts_find_references" | "pom_model" => {
            ("›", Color::DarkGray)
        }
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
        "fs_read_file" | "fs_read_ranges" | "fs_write_file" => pick(&["path", "file"]),
        "fs_list_dir" | "fs_list_files" => pick(&["path", "dir", "glob"]),
        "fs_rgrep" => pick(&["pattern", "glob", "query"]),
        "ts_find_symbol" | "search_symbols" => pick(&["query", "symbol"]),
        "ts_read_symbol" | "ts_rename" | "ts_find_references" => pick(&["symbol", "query"]),
        "ts_structural_map" | "ts_list_symbols" => pick(&["path", "kinds"]),
        "web_search" => pick(&["query"]),
        "pom_run_task" | "pom_run_tests" => pick(&["task", "command"]),
        "shell" => pick(&["command", "dir"]),
        "delegate" | "ask_advise" => {
            // Which delegate is engaged (ad-hoc), or which plan step (step);
            // for advice, fall back to the question itself.
            if let Some(m) = map
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(cap(m, 40))
            } else if let Some(q) = map
                .get("question")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(cap(q, 80))
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

/// Parse the `ask_advise` tool's success output into (delegate name, advice):
/// `advice from <name> (<display>):\n<advice>`.
fn parse_advice_reply(output: &str) -> Option<(String, String)> {
    let rest = output.strip_prefix("advice from ")?;
    let open = rest.find(" (")?;
    let model = rest[..open].trim();
    if model.is_empty() {
        return None;
    }
    let after = &rest[open + 2..];
    let marker = "):\n";
    let end = after.find(marker)?;
    let advice = after[end + marker.len()..].trim_end();
    Some((model.to_string(), advice.to_string()))
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
            return vec![cap(args.trim(), 300)];
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

/// The delegate model behind `author`, when `author` is one of the configured
/// delegates: rows authored by a delegate form its sub-chat (indented, tinted
/// in the delegate's color) in the chat window.
fn subchat_model<'a>(author: Option<&'a str>, delegates: &[DelegateCfg]) -> Option<&'a str> {
    author.filter(|a| delegates.iter().any(|d| d.name == *a))
}

/// The dimmed background band for a chat row, chosen from the message the row
/// belongs to: user turns get the lifted user band; the model's reasoning, its
/// final answer and a delegate's reply are tinted with their model's band, so
/// the spoken blocks read as one integrated chat rather than tool noise. When
/// `focus` is set a folded digest renders only the reasoning it holds, so those
/// rows take the thinking model's band too.
fn row_band(
    owner: Option<&Msg>,
    colors: &ModelColors,
    delegates: &[DelegateCfg],
    focus: bool,
) -> Option<Color> {
    let msg = owner?;
    match msg.kind {
        MsgKind::User => Some(user_band_bg()),
        MsgKind::Reasoning | MsgKind::Assistant => {
            Some(colors.band_color(msg.author.as_deref().unwrap_or("")))
        }
        MsgKind::Run if focus => msg
            .children
            .iter()
            .find(|c| c.kind == MsgKind::Reasoning)
            .map(|c| colors.band_color(c.author.as_deref().unwrap_or(""))),
        _ => subchat_model(msg.author.as_deref(), delegates).map(|a| colors.band_color(a)),
    }
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

/// The model's visible thinking between actions, rendered like the assistant
/// final-answer block: a one-line brain header in the model's colour, then the
/// reasoning text as markdown. No card arrow or author tag - the block reads as
/// the model speaking, and `draw_chat` tints its rows with the model's dimmed
/// background band (the same band a delegate's reply uses), so who is thinking
/// is carried by the colour.
fn layout_reasoning(
    out: &mut Vec<RenderRow>,
    text: &str,
    author: &str,
    colors: &ModelColors,
    open: bool,
    width: usize,
) {
    // Header: the brain glyph plus the model's name, in the model's colour — the
    // spoken-block look shared with the assistant final answer (ADR 19), with the
    // name made explicit so thinking is attributed to its model at a glance.
    let label = cap(&format!("🧠 {author}"), width.saturating_sub(2));
    out.push(RenderRow {
        rule: None,
        spans: vec![Span::styled(
            label,
            Style::default()
                .fg(colors.name_color(author))
                .add_modifier(Modifier::BOLD),
        )],
        tool_header: None,
    });
    if !open {
        return;
    }
    if !text.trim().is_empty() {
        for spans in md_to_lines(text, width) {
            out.push(RenderRow {
                rule: None,
                spans,
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
        if ms.is_multiple_of(1_000) {
            return format!("{s}s");
        }
        return format!("{s}.{}s", ms % 1_000 / 100);
    }
    let total_s = ms / 1_000;
    if total_s.is_multiple_of(60) {
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
        .filter(|t| t.name != "delegate" && t.name != "ask_advise")
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

/// Styled parts of one parsed code-location line: (text, style, space_before).
/// `space_before` says whether a separating space precedes this part, so
/// `crates/a.rs`, `:` and `42` stay glued while a snippet part is spaced off.
type StyledPart = (String, Style, bool);

/// Split a `path:line[:col] snippet` line into (path, line, snippet), or None
/// when the line is not a code location. The path must look like a file (carry
/// a '/' or '.') so prose such as `note: 5` is never mistaken for a location.
fn split_path_line(line: &str) -> Option<(&str, &str, &str)> {
    let colon = line.find(':')?;
    let path = &line[..colon];
    if path.is_empty() || path.contains(' ') || !(path.contains('/') || path.contains('.')) {
        return None;
    }
    let after = &line[colon + 1..];
    let nl = after.bytes().take_while(|b| b.is_ascii_digit()).count();
    if nl == 0 {
        return None;
    }
    let num = &after[..nl];
    let mut rest = &after[nl..];
    if let Some(r) = rest.strip_prefix(':') {
        let cl = r.bytes().take_while(|b| b.is_ascii_digit()).count();
        if cl > 0 {
            rest = &r[cl..];
        }
    }
    if !(rest.is_empty() || rest.starts_with(':') || rest.starts_with(' ')) {
        return None;
    }
    Some((path, num, rest.trim_start_matches([':', ' ', '\t'])))
}

/// Parse a tool-result line that names a code location into styled parts.
/// Handles the shapes Comrade tools emit:
///   * `path:line: snippet` / `path:line:col  snippet` (fs_rgrep, ts_find_references)
///   * `kind name | signature @ line` (ts_find_symbol, ts_list_symbols, ...)
fn loc_parts(line: &str) -> Option<Vec<StyledPart>> {
    let bold = |c: Color| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    if let Some((path, num, snippet)) = split_path_line(line) {
        let mut parts: Vec<StyledPart> = vec![
            (path.to_string(), bold(Color::Cyan), false),
            (":".to_string(), dim, false),
            (num.to_string(), bold(Color::Yellow), false),
        ];
        if !snippet.is_empty() {
            parts.push((snippet.to_string(), Style::default().fg(Color::White), true));
        }
        return Some(parts);
    }
    // `... @ line` — the line number sits at the very end.
    if let Some(idx) = line.rfind(" @ ") {
        let after = &line[idx + 3..];
        let dn = after.bytes().take_while(|b| b.is_ascii_digit()).count();
        if dn > 0 && after[dn..].trim().is_empty() {
            let head = line[..idx].trim_start();
            let mut parts: Vec<StyledPart> = Vec::new();
            if !head.is_empty() {
                parts.push((head.to_string(), Style::default().fg(Color::White), false));
            }
            parts.push(("@".to_string(), dim, true));
            parts.push((after[..dn].to_string(), bold(Color::Yellow), true));
            return Some(parts);
        }
    }
    None
}

/// Wrap styled parts into rows of at most `width` columns, breaking on spaces
/// and inserting a single space wherever a part asked for one. Parts that do not
/// (`crates/a.rs`, `:`, `42`) stay glued together.
fn wrap_styled(parts: &[StyledPart], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_len = 0usize;
    for (text, style, space_before) in parts {
        for (k, word) in text.split(' ').enumerate() {
            if word.is_empty() {
                continue;
            }
            let before = if k == 0 { *space_before } else { true };
            if !cur.is_empty() && cur_len + usize::from(before) + word.chars().count() > width {
                rows.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            if !cur.is_empty() && before {
                cur.push(Span::styled(" ".to_string(), Style::default()));
                cur_len += 1;
            }
            cur.push(Span::styled(word.to_string(), *style));
            cur_len += word.chars().count();
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

/// Render a tool result as styled chat rows: code-location lines get their file
/// name and line number emphasised (bold cyan path, bold yellow number) while
/// every other line keeps the legacy single-colour wrapped rendering.
fn result_rows(result: &str, ok: bool, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let fallback = if ok { Color::Green } else { Color::Red };
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    for line in result.split('\n') {
        if let Some(name) = line
            .trim_end()
            .strip_prefix("== ")
            .and_then(|s| s.strip_suffix(" =="))
        {
            rows.push(vec![Span::styled(
                format!("== {name} =="),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]);
        } else if let Some(parts) = loc_parts(line) {
            rows.extend(wrap_styled(&parts, width));
        } else {
            for s in plain_wrap(line, width) {
                rows.push(vec![Span::styled(s, Style::default().fg(fallback))]);
            }
        }
    }
    rows
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
    if card.open
        && let Some(tok) = card.tokens
    {
        spans.push(Span::styled(
            format!("  · {} tok", fmt_tokens(tok)),
            Style::default().fg(Color::DarkGray),
        ));
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
    let diff_sides = extract_diff_sides(&card.name, &card.args, card.result.as_deref());
    if let Some((old, new)) = &diff_sides {
        out.push(RenderRow {
            rule: None,
            spans: vec![Span::styled(
                edit_diff_label(&card.name, &card.args, card.result.as_deref()),
                Style::default().fg(Color::DarkGray),
            )],
            tool_header: None,
        });
        const MAX_DIFF_ROWS: usize = 200;
        let pairs = lcs_pairs(old, new);
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
    // For `git_diff` the side-by-side view above already renders the tool's
    // whole output, so echoing the raw `result:` block below it is pure
    // duplication. Every other card (edits, reads, test runs, failures, ...)
    // still shows its result text.
    let result_is_diff = card.name == "git_diff" && diff_sides.is_some();
    if !result_is_diff && let Some(result) = &card.result {
        let color = if card.ok { Color::Green } else { Color::Red };
        out.push(RenderRow {
            rule: None,
            spans: vec![Span::styled("result:", Style::default().fg(color))],
            tool_header: None,
        });
        for row in result_rows(result, card.ok, width) {
            out.push(RenderRow {
                rule: None,
                spans: row,
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
fn delegate_panel_rows(
    delegates: &[DelegateCfg],
    colors: &ModelColors,
    width: usize,
) -> Option<Vec<Line<'static>>> {
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
        let name = if let Some(ctx_window) = d.llm.context_window {
            format!("{} ({})", d.name, ctx_window)
        } else if d.name == d.llm.model {
            d.name.clone()
        } else {
            format!("{} ({})", d.name, d.llm.model)
        };
        // The delegate's name carries its assigned agent color, so the panel
        // matches the color used for its sub-chat and author tags. A disabled
        // delegate (`enabled = false`) is dimmed and marked so the human can
        // see it is configured but off.
        let name_style = if d.enabled {
            Style::default()
                .fg(colors.name_color(&d.name))
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let mut toks = vec![tok(flat(&name), name_style)];
        if !d.description.trim().is_empty() {
            toks.push(tok(format!(" — {}", flat(d.description.trim())), dim));
        }
        if !d.enabled {
            toks.push(tok(" (disabled)".to_string(), dim));
        }
        for wrapped in wrap_toks(&toks, body_width) {
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

const MODEL_PANEL_FIXED_ROWS: u16 = 2;

fn short_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        let k = n as f64 / 1_000.0;
        if k >= 10.0 {
            format!("{k:.0}k")
        } else {
            format!("{k:.1}k")
        }
    } else {
        n.to_string()
    }
}

fn gauge_line(tokens: usize, budget: usize, estimated: bool, width: usize) -> Line<'static> {
    let budget = budget.max(1);
    let ratio = (tokens as f64 / budget as f64).clamp(0.0, 1.0);
    let pct = (ratio * 100.0).round() as usize;
    let bar_color = if ratio < 0.7 {
        Color::Green
    } else if ratio < 0.9 {
        Color::Yellow
    } else {
        Color::Red
    };

    let used_short = short_tokens(tokens);
    let budget_short = short_tokens(budget);
    let core = format!("{pct}%  {used_short}/{budget_short}");
    let marker = if estimated { " est" } else { "" };

    let core_len = core.chars().count();
    let marker_len = marker.chars().count();
    let bar_w = width.saturating_sub(core_len + marker_len + 1);

    if bar_w >= 4 {
        let filled = (ratio * bar_w as f64).floor() as usize;
        let mut spans = Vec::new();
        spans.push(Span::styled(
            "#".repeat(filled),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            "-".repeat(bar_w.saturating_sub(filled)),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{pct}% "),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{used_short}/{budget_short}"),
            Style::default().fg(Color::White),
        ));
        if estimated {
            spans.push(Span::styled(" est", Style::default().fg(Color::DarkGray)));
        }
        Line::from(spans)
    } else {
        // Narrow panel: drop the bar, just show core text with color
        let mut spans = Vec::new();
        spans.push(Span::styled(
            format!("{pct}% "),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("{used_short}/{budget_short}"),
            Style::default().fg(Color::White),
        ));
        if estimated {
            spans.push(Span::styled(" est", Style::default().fg(Color::DarkGray)));
        }
        Line::from(spans)
    }
}

/// One line for the model panel: the main model's name (in its assigned agent
/// colour) left-aligned with the account balance right-aligned when it fits.
fn label_line(
    model_label: String,
    balance: Option<&str>,
    width: usize,
    color: Color,
) -> Line<'static> {
    let model_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let balance_style = Style::default().fg(Color::DarkGray);

    if let Some(bal) = balance
        && !bal.is_empty()
    {
        let label_len = model_label.chars().count();
        let bal_len = bal.chars().count();
        if label_len + bal_len < width {
            let pad = width - label_len - bal_len;
            let spans = vec![
                Span::styled(model_label, model_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(bal.to_string(), balance_style),
            ];
            return Line::from(spans);
        }
    }

    // Truncate model_label to width if needed
    let truncated = if model_label.chars().count() > width {
        model_label.chars().take(width).collect()
    } else {
        model_label
    };
    Line::from(Span::styled(truncated, model_style))
}

fn draw_stats(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" model ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let model_label = if let Some(v) = &app.cfg.llm.model_version {
        if v.is_empty() {
            app.cfg.llm.model.clone()
        } else {
            format!("{} {}", app.cfg.llm.model, v)
        }
    } else {
        app.cfg.llm.model.clone()
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(label_line(
            model_label,
            app.balance.as_deref(),
            width,
            app.model_colors.name_color(&app.cfg.llm.display()),
        )),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(gauge_line(
            app.ctx_tokens,
            app.ctx_budget,
            app.ctx_estimated,
            width,
        )),
        rows[1],
    );
    // Configured delegates ([[delegates]]) shown in the leftover space under
    // the model gauge; word-wrapped to the panel width so long names or
    // descriptions never spill past the right edge.
    if let Some(delegate_rows) = delegate_panel_rows(&app.cfg.delegates, &app.model_colors, width) {
        frame.render_widget(Paragraph::new(delegate_rows), rows[2]);
    }
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
            PlanStatus::Ready => Color::Blue,
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
                format!("verify: {}", flat(verify)),
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

/// The rotating Braille spinner frame for the current time. Shared by the plan
/// checklist glyphs and the chat's live activity line.
fn spinner_glyph(now_ms: u128) -> &'static str {
    let frame = ((now_ms / SPINNER_FRAME_MS) as usize) % SPINNER_FRAMES.len();
    SPINNER_FRAMES[frame]
}

/// The status glyph shown before a plan step: "-" for pending, "●" for a step
/// whose delegate confirmed it is ready to pick up, a rotating spinner for
/// in-progress, a tick for done and a cross for failed/blocked.
fn plan_glyph(s: &PlanStatus, now_ms: u128) -> &'static str {
    match s {
        PlanStatus::Pending => "-",
        PlanStatus::Ready => "●",
        PlanStatus::InProgress => spinner_glyph(now_ms),
        PlanStatus::Done => "✓",
        PlanStatus::Blocked => "✗",
    }
}

/// True while the chat should show its bottom activity spinner: a run is in
/// flight in focus mode (where tool cards are hidden, so a run that produces no
/// reasoning would otherwise look frozen) and it is not paused on a user dialog.
fn activity_spinner_visible(app: &App) -> bool {
    app.focus_mode && session_status_marker(app, app.active) == Some("running")
}

/// The one-line live activity indicator pinned to the bottom of the chat while
/// the agent works: a rotating spinner plus the tool currently running, or a
/// generic "working…" while the model is thinking (Claude-Code style).
fn activity_line(app: &App, now_ms: u128) -> Line<'static> {
    let what = match &app.activity {
        Some(name) => format!("running {name}"),
        None => "working…".to_string(),
    };
    Line::from(vec![
        Span::styled(
            format!("  {} ", spinner_glyph(now_ms)),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            what,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
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

/// The Ctrl-x C-b session switcher overlay: pick an open session to switch to.
fn draw_session_pick(pick: &SessionPick, app: &App, frame: &mut Frame) {
    let area = frame.area();
    let w = area.width.saturating_sub(2).min(92);
    let h = (app.open_sessions.len() as u16 + 3).min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line> = Vec::new();
    for (i, s) in app.open_sessions.iter().enumerate() {
        let selected = i == pick.sel;
        let marker = if i == app.active { "*" } else { " " };
        let status = session_status_marker(app, i);
        let file = s
            .file
            .as_ref()
            .map(|f| format!("  {}", f.display()))
            .unwrap_or_default();
        let mut text = format!(
            "{marker} {} {}{}",
            if selected { ">" } else { " " },
            s.title,
            match status {
                Some("waiting") => "  [waiting]",
                Some("running") => "  [running]",
                Some(_) => unreachable!(),
                None => "",
            }
        );
        text.push_str(&file);
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" sessions (enter:switch  esc:cancel) ");
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The Ctrl-A "assign a model" overlay: choose a plan step (pending/blocked
/// only), then choose which model runs it.
fn draw_model_pick(pick: &ModelPick, colors: &ModelColors, frame: &mut Frame) {
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
            let mut st = Style::default().fg(if selected {
                Color::White
            } else {
                colors.name_color(base_model_name(m))
            });
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

fn draw_mcp_servers(view: &McpServersView, frame: &mut Frame) {
    let area = frame.area();
    let w = area.width.saturating_sub(2).min(92);
    let max_h = area.height.saturating_sub(2);

    // Snapshot the live on/off set once per frame for the [x]/[ ] markers.
    let disabled: HashSet<String> = match view.disabled.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };

    let rows = mcp_visible_rows(view);
    let sel = mcp_clamp(view.sel, rows.len());
    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let selected = i == sel;
        let cursor = if selected {
            Span::styled("> ", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("  ", Style::default())
        };
        let mut spans = vec![cursor];
        match row {
            RowRef::Header(g) => {
                let group = &view.groups[*g];
                let collapsed = view.collapsed.contains(g);
                spans.push(Span::styled(
                    format!("{} ", if collapsed { "+" } else { "-" }),
                    Style::default().fg(Color::DarkGray),
                ));
                let name_style = if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                };
                spans.push(Span::styled(group.server.clone(), name_style));
                let tail = if group.tools.is_empty() {
                    "  (no tools connected)".to_string()
                } else {
                    let on = group
                        .tools
                        .iter()
                        .filter(|t| !disabled.contains(&t.name))
                        .count();
                    format!("  · {on}/{} on", group.tools.len())
                };
                spans.push(Span::styled(tail, Style::default().fg(Color::DarkGray)));
            }
            RowRef::Tool(g, t) => {
                let Some(entry) = view.groups.get(*g).and_then(|grp| grp.tools.get(*t)) else {
                    continue;
                };
                let enabled = !disabled.contains(&entry.name);
                spans.push(Span::styled(
                    if enabled { "[x]" } else { "[ ]" },
                    Style::default().fg(if enabled {
                        Color::Green
                    } else {
                        Color::DarkGray
                    }),
                ));
                spans.push(Span::styled(" ", Style::default()));
                let name_style = if selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else if enabled {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                spans.push(Span::styled(entry.name.clone(), name_style));
                if !entry.desc.is_empty() {
                    spans.push(Span::styled(
                        format!("  ·  {}", entry.desc),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no matching tools)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let hint = "typing filters by server or tool name · ↑/↓ or ctrl-p/n select · enter toggles · tab collapses a group · esc closes";

    let content_h = lines.len() as u16;
    let h = (content_h + 4).clamp(6, max_h.max(6));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" MCP tools ")
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    // Filter line: label + the query. Typing is always active, so the query is
    // always highlighted with a trailing cursor.
    let mut fspans = vec![
        Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            view.filter.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    fspans.push(Span::styled(
        "|",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(Line::from(fspans)), rows[0]);

    // Content list: selection-driven scrolling so the cursor stays visible.
    let viewport = usize::from(rows[1].height).max(1);
    let scroll = sel.saturating_sub(viewport.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), rows[1]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[2],
    );
}

/// A compact transcript description of a form question: its title (or
/// "Question" when untitled) followed by one line per field (`label (id)`), so
/// the answers recorded later can be read against what was asked.
fn form_question_text(spec: &FormSpec) -> String {
    let mut s = if spec.title.trim().is_empty() {
        "Question".to_string()
    } else {
        spec.title.trim().to_string()
    };
    for f in &spec.fields {
        s.push_str(&format!("\n  {} ({})", f.label, f.id));
    }
    s
}

/// One line of a form dialog: focus arrow, label, required marker and the
/// component's current rendering (text box, spinner, date, select, checkbox).
fn form_field_line(field: &comrade_tool::FormField, value: &str, focused: bool) -> Line<'static> {
    let arrow = if focused { "▶ " } else { "  " };
    let label_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![
        Span::styled(arrow.to_string(), Style::default().fg(Color::Yellow)),
        Span::styled(field.label.clone(), label_style),
    ];
    if field.required {
        spans.push(Span::styled(" *", Style::default().fg(Color::Red)));
    }
    spans.push(Span::raw(": "));
    let (widget, style) = match &field.kind {
        FieldKind::Text { placeholder } => {
            if value.is_empty() {
                (
                    placeholder.clone().unwrap_or_default(),
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                (value.to_string(), Style::default().fg(Color::Green))
            }
        }
        FieldKind::Date => (
            format!(
                "‹ {} ›",
                if value.is_empty() {
                    "YYYY-MM-DD"
                } else {
                    value
                }
            ),
            Style::default().fg(Color::Green),
        ),
        FieldKind::Number { .. } | FieldKind::Select { .. } | FieldKind::DiffChoice { .. } => {
            (format!("‹ {value} ›"), Style::default().fg(Color::Green))
        }
        FieldKind::Checkbox => (
            if comrade_tool::truthy(value) {
                "[x]".to_string()
            } else {
                "[ ]".to_string()
            },
            Style::default().fg(Color::Green),
        ),
    };
    spans.push(Span::styled(widget, style));
    if focused
        && matches!(
            field.kind,
            FieldKind::Text { .. } | FieldKind::Number { .. } | FieldKind::Date
        )
    {
        spans.push(Span::styled("_", Style::default().fg(Color::Green)));
    }
    if field.has_recommended() {
        spans.push(Span::styled(
            " (recommended)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// The reply auto-accept mode gives a prompt without asking the human, or
/// `None` when the prompt must still be shown (e.g. a form whose required
/// fields the recommended values do not satisfy).
fn auto_reply(prompt: &UserPrompt) -> Option<UserReply> {
    match prompt {
        UserPrompt::Confirm { .. } => Some(UserReply::Answer("yes".into())),
        UserPrompt::Form(spec) => {
            let values = spec.initial_values();
            spec.is_complete(&values).then_some(UserReply::Form(values))
        }
    }
}

/// Meta line describing what auto-accept did for `prompt` (the counterpart of
/// [`auto_reply`], which decided *that* it could be auto-answered).
fn auto_reply_label(prompt: &UserPrompt) -> String {
    match prompt {
        UserPrompt::Confirm { title, .. } => {
            let action = one_line(title, 80);
            if action.is_empty() {
                "auto-accept: approved".to_string()
            } else {
                format!("auto-accept → approved: {action}")
            }
        }
        UserPrompt::Form(_) => "auto-accept → form submitted".to_string(),
    }
}

fn draw_dialog(app: &App, dialog: &Dialog, frame: &mut Frame) {
    let area = frame.area();

    // Size the popup first so the body can wrap to its real inner width (the
    // block borders shave two columns off the popup).
    let w = area.width.saturating_sub(2).clamp(20, 100);
    let inner_w = w.saturating_sub(2).max(10) as usize;
    let is_form = matches!(dialog.prompt, UserPrompt::Form(_));

    // Build the body lines for the kind of prompt, wrapped to the popup width.
    let (kind_label, mut body) = match &dialog.prompt {
        UserPrompt::Confirm { title, diff } => {
            let mut lines = Vec::new();
            // The actual action (e.g. the shell command) goes on top so the
            // human always sees exactly what they are approving.
            lines.extend(preview_lines(title, inner_w));
            if let Some(d) = diff
                && !d.trim().is_empty()
            {
                lines.push(Line::from(""));
                lines.extend(preview_lines(d, inner_w));
            }
            if !app.dialog_conv.is_empty() {
                lines.push(Line::from(""));
                let conv = app.dialog_conv.join("\n");
                lines.extend(preview_lines(&conv, inner_w));
            }
            (" confirm  [y/n] ", lines)
        }
        UserPrompt::Form(spec) => {
            let mut lines = Vec::new();
            if !spec.title.trim().is_empty() {
                lines.extend(preview_lines(&spec.title, inner_w));
            }
            if let Some(desc) = &spec.description
                && !desc.trim().is_empty()
            {
                lines.push(Line::from(""));
                lines.extend(preview_lines(desc, inner_w));
            }
            let edit = dialog.form.as_ref();
            for (i, f) in spec.fields.iter().enumerate() {
                let focused = edit.is_some_and(|e| e.sel == i);
                let value = edit
                    .and_then(|e| e.values.get(i))
                    .cloned()
                    .unwrap_or_else(|| f.initial_value());
                lines.push(form_field_line(f, &value, focused));
                // A diff-choice field shows the diff of its currently-selected
                // option so the human can read the patch before deciding.
                if let FieldKind::DiffChoice { options } = &f.kind
                    && let Some(opt) = options.iter().find(|o| o.label == value)
                {
                    lines.push(Line::from(""));
                    lines.extend(preview_lines(&opt.diff, inner_w));
                }
            }
            (" form ", lines)
        }
    };
    if body.is_empty() {
        body.push(Line::from(""));
    }

    // Fit the popup to the whole body: 2 border rows + body + input + hint.
    // The body is anchored at the top, so growing the popup is what keeps the
    // question visible instead of scrolling its first lines out of view.
    let max_h = area.height.saturating_sub(2).max(5);
    let needed = body.len() as u16 + 4;
    let h = needed.clamp(5, max_h);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);

    let mut kind_label = kind_label;
    let mut border_color = if is_form { Color::Cyan } else { Color::Yellow };
    if app.dialog_ask {
        kind_label = " confirm  [ask the model] ";
        border_color = Color::Magenta;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(kind_label)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let row_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Body, anchored at the top so the question/action is always visible even
    // when it is taller than the popup.
    frame.render_widget(Paragraph::new(body), row_layout[0]);

    // Input row (forms edit inline in the body, so it stays blank there).
    let input = if is_form {
        Line::from("")
    } else {
        let input_prompt = if app.dialog_ask { "? " } else { "> " };
        Line::from(vec![
            Span::styled(input_prompt, Style::default().fg(Color::Green)),
            Span::raw(dialog.buf.clone()),
            Span::styled("_", Style::default().fg(Color::Green)),
        ])
    };
    frame.render_widget(Paragraph::new(input), row_layout[1]);

    // Hint row.
    let hint = if is_form {
        "up/down: field    left/right: adjust    space: toggle    enter: submit    esc: cancel"
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
}

/// Pretty-print a free-form preview (approval diff bodies). Highlights
/// Justification labels and edit-style `--- remove ---` / `+++ insert
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

// ---------------------------------------------------------------------------
// markdown tables (GFM pipe tables rendered as aligned text lines)
// ---------------------------------------------------------------------------

/// Column alignment from a separator cell: `:---:` centre, `---:` right, else
/// left.
#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
    Centre,
}

/// Split one pipe-table row into cells. Accepts rows with or without the outer
/// pipes (`| a | b |` and `a | b`); surrounding empties from outer pipes are
/// dropped, interior empties (`| a || c |`) are kept as blank cells.
fn table_row_cells(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let inner = if trimmed.starts_with('|') {
        trimmed.trim_matches('|')
    } else {
        trimmed
    };
    let cells: Vec<String> = inner.split('|').map(|c| c.trim().to_string()).collect();
    if cells.is_empty() { None } else { Some(cells) }
}

/// True when `cells` look like a table separator row: every cell is at least
/// three chars, all dashes/colons, with at least one dash (`---`, `:--:`).
fn is_table_sep(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let t = c.trim();
            t.chars().count() >= 3 && t.contains('-') && t.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn sep_alignment(cell: &str) -> Align {
    let t = cell.trim();
    match (t.starts_with(':'), t.ends_with(':')) {
        (true, true) => Align::Centre,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

/// Whether `lines[at]` begins a pipe table: a row whose next line is a
/// separator row.
fn is_table_start(lines: &[&str], at: usize) -> bool {
    lines
        .get(at)
        .and_then(|l| table_row_cells(l))
        .is_some_and(|cells| {
            lines
                .get(at + 1)
                .and_then(|l| table_row_cells(l))
                .is_some_and(|sep| is_table_sep(&sep))
                && !cells.is_empty()
        })
}

/// The rendered text of a cell: strip inline markers so the measured width
/// equals what the reader sees (cell content is styled as a whole - bold
/// header, plain body - rather than per-run).
fn cell_plain(cell: &str) -> String {
    inline_toks(cell, base_style())
        .into_iter()
        .map(|t| t.text)
        .collect()
}

/// Wrap one cell's text into chunks of at most `w` chars (word-aware, with
/// hard cuts for overlong words).
fn cell_chunks(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for line in plain_wrap(text, w) {
        out.extend(hard_cut(&line, w));
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Pad `text` to width `w` per alignment.
fn pad_to(text: &str, w: usize, a: Align) -> String {
    let len = text.chars().count();
    if len >= w {
        return text.to_string();
    }
    let pad = w - len;
    match a {
        Align::Left => format!("{text}{}", " ".repeat(pad)),
        Align::Right => format!("{}{text}", " ".repeat(pad)),
        Align::Centre => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
        }
    }
}

/// Fit per-column widths into `avail` total cells by shrinking the widest
/// column(s), never below 1.
fn fit_widths(widths: &mut [usize], avail: usize) {
    loop {
        let total: usize = widths.iter().sum();
        if total <= avail {
            return;
        }
        let widest = widths
            .iter()
            .enumerate()
            .filter(|&(_, w)| *w > 1)
            .max_by_key(|&(_, w)| *w);
        let Some((i, _)) = widest else { return };
        widths[i] -= 1;
    }
}

/// Render a GFM pipe table into aligned, styled text lines no wider than
/// `width`. Header is bold; a `-`/`+` rule separates it from the body; body
/// rows that wrap keep their columns aligned.
fn table_block_toks(
    header: &[String],
    sep: &[String],
    body: &[Vec<String>],
    width: usize,
) -> Vec<Vec<Tok>> {
    let n = sep.len();
    if n == 0 {
        return Vec::new();
    }
    let truncate = |cells: &[String]| -> Vec<String> {
        let mut v: Vec<String> = cells.iter().take(n).cloned().collect();
        while v.len() < n {
            v.push(String::new());
        }
        v
    };

    // Natural column widths from content.
    let mut widths = vec![1usize; n];
    let mut widen = |cells: &[String]| {
        for (i, c) in cells.iter().take(n).enumerate() {
            widths[i] = widths[i].max(cell_plain(c).chars().count());
        }
    };
    widen(header);
    for row in body {
        widen(row);
    }
    // Each column occupies `w_i` content cells, then a fixed ` | ` separator
    // (3 cells): the bar of every row sits at the same offset and content
    // starts line up, whatever a cell's length. The header rule's `+` is
    // offset the same way so it lands under each bar.
    let overhead = 3usize * n.saturating_sub(1);
    fit_widths(&mut widths, width.saturating_sub(overhead).max(n));

    let aligns: Vec<Align> = sep.iter().map(|c| sep_alignment(c)).collect();
    let joiner = tok(" | ", Style::default().fg(Color::DarkGray));
    let mut out: Vec<Vec<Tok>> = Vec::new();

    // Emit one logical row as one or more aligned lines (wrapping cells).
    let emit_row = |row: &[String], style: Style, out: &mut Vec<Vec<Tok>>| {
        let cells = truncate(row);
        let chunks: Vec<Vec<String>> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| cell_chunks(&cell_plain(c), widths[i]))
            .collect();
        let depth = chunks.iter().map(|c| c.len()).max().unwrap_or(1);
        for d in 0..depth {
            let mut line: Vec<Tok> = Vec::new();
            for i in 0..n {
                if i > 0 {
                    line.push(joiner.clone());
                }
                let chunk = chunks[i].get(d).cloned().unwrap_or_else(String::new);
                line.push(tok(pad_to(&chunk, widths[i], aligns[i]), style));
            }
            out.push(line);
        }
    };

    emit_row(
        header,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        &mut out,
    );
    // Rule line under the header: dashes cover the content region plus the
    // separator's leading space, so each `+` sits directly under a row's `|`.
    let rule = widths
        .iter()
        .map(|w| "-".repeat(*w + 1))
        .collect::<Vec<_>>()
        .join("+");
    out.push(vec![tok(rule, Style::default().fg(Color::DarkGray))]);
    for row in body {
        emit_row(row, base_style(), &mut out);
    }
    out
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
        // GFM pipe table: a row whose next line is a separator row.
        if is_table_start(&lines, i) {
            let header = table_row_cells(lines[i]).expect("table start has a header");
            let sep = table_row_cells(lines[i + 1]).expect("table start has a separator");
            i += 2;
            let mut body_rows: Vec<Vec<String>> = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if t.is_empty() {
                    break;
                }
                let Some(cells) = table_row_cells(t) else {
                    break;
                };
                if is_table_sep(&cells) {
                    break; // a second separator ends the table
                }
                body_rows.push(cells);
                i += 1;
            }
            out.extend(table_block_toks(&header, &sep, &body_rows, width));
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
                || is_table_start(&lines, i)
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
    if (1..=6).contains(&level) && s.len() > level && s.as_bytes()[level] == b' ' {
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

    fn form_spec(v: serde_json::Value) -> FormSpec {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn form_edit_seeds_and_answers_in_order() {
        let spec = form_spec(serde_json::json!({
            "fields": [
                { "id": "guests", "label": "Guests", "kind": "number", "min": 1 },
                { "id": "room", "label": "Room", "kind": "select", "options": ["single", "double"] },
                { "id": "notes", "label": "Notes", "kind": "text" }
            ]
        }));
        let mut edit = FormEdit::new(&spec);
        assert_eq!(edit.values, vec!["1", "single", ""]);
        edit.adjust(&spec, 1);
        assert_eq!(edit.values[0], "2");
        edit.focus(true);
        edit.adjust(&spec, 1);
        assert_eq!(edit.values[1], "double");
        edit.focus(true);
        edit.input(&spec, 'h');
        edit.input(&spec, 'i');
        let answers = edit.answers(&spec);
        assert_eq!(answers["guests"], "2");
        assert_eq!(answers["room"], "double");
        assert_eq!(answers["notes"], "hi");
    }

    #[test]
    fn form_edit_cycles_diff_choice_and_ignores_typing() {
        let spec = form_spec(serde_json::json!({
            "fields": [
                { "id": "pick", "label": "Pick a patch", "kind": "diff_choice",
                  "options": [
                      { "label": "A", "diff": "-old\n+new" },
                      { "label": "B", "diff": "-old\n+other" }
                  ] }
            ]
        }));
        let mut edit = FormEdit::new(&spec);
        assert_eq!(edit.values[0], "A"); // the first option seeds the field
        edit.adjust(&spec, 1);
        assert_eq!(edit.values[0], "B");
        edit.adjust(&spec, 1);
        assert_eq!(edit.values[0], "A"); // wraps around
        edit.input(&spec, 'x');
        assert_eq!(edit.values[0], "A"); // typing is ignored
        assert_eq!(edit.answers(&spec)["pick"], "A");
    }

    #[test]
    fn form_edit_clamps_numbers_cycles_selects_and_toggles() {
        let spec = form_spec(serde_json::json!({
            "fields": [
                { "id": "n", "label": "N", "kind": "number", "min": 0, "max": 2 },
                { "id": "d", "label": "D", "kind": "select", "options": ["a", "b"] },
                { "id": "c", "label": "C", "kind": "checkbox" }
            ]
        }));
        let mut edit = FormEdit::new(&spec);
        edit.adjust(&spec, -1);
        assert_eq!(edit.values[0], "0"); // clamped at min
        edit.adjust(&spec, 1);
        edit.adjust(&spec, 1);
        edit.adjust(&spec, 1);
        assert_eq!(edit.values[0], "2"); // clamped at max
        edit.focus(true);
        edit.adjust(&spec, -1);
        assert_eq!(edit.values[1], "b"); // wraps backwards
        edit.focus(true);
        edit.input(&spec, ' ');
        assert_eq!(edit.values[2], "true");
        edit.toggle(&spec);
        assert_eq!(edit.values[2], "false");
    }

    #[test]
    fn date_shift_crosses_month_boundary() {
        assert_eq!(shift_date("2026-01-31", 1), "2026-02-01");
        assert_eq!(shift_date("2026-03-01", -1), "2026-02-28");
        assert_eq!(shift_date("", 0), "1970-01-01");
    }

    #[test]
    fn auto_reply_uses_recommended_and_skips_incomplete_forms() {
        // Confirm is always approved.
        assert!(matches!(
            auto_reply(&UserPrompt::Confirm {
                title: "x".into(),
                diff: None
            }),
            Some(UserReply::Answer(a)) if a == "yes"
        ));
        // A form whose required field has a recommended value auto-submits.
        let ok = form_spec(serde_json::json!({
            "fields": [{ "id": "n", "label": "N", "kind": "text", "required": true, "recommended": "hi" }]
        }));
        assert!(matches!(
            auto_reply(&UserPrompt::Form(ok)),
            Some(UserReply::Form(_))
        ));
        // A form with an unfilled required field is shown to the human instead.
        let pending = form_spec(serde_json::json!({
            "fields": [{ "id": "n", "label": "N", "kind": "text", "required": true }]
        }));
        assert!(auto_reply(&UserPrompt::Form(pending)).is_none());
    }

    #[test]
    fn auto_reply_label_names_the_action() {
        let f = form_spec(
            serde_json::json!({ "fields": [{ "id": "n", "label": "N", "kind": "text" }] }),
        );
        assert_eq!(
            auto_reply_label(&UserPrompt::Form(f)),
            "auto-accept → form submitted"
        );
    }

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
    fn mx_all_is_sorted_by_name() {
        let names: Vec<&str> = MxCommand::ALL.iter().map(|c| c.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "MxCommand::ALL must stay sorted by name");
    }

    #[test]
    fn mx_list_mcp_servers_metadata() {
        let cmd = MxCommand::ListMcpServers;
        assert_eq!(cmd.name(), "list-mcp-servers");
        assert_eq!(cmd.keys(), None, "list-mcp-servers is palette-only");
        assert!(!cmd.desc().is_empty());
        assert!(MxCommand::ALL.contains(&cmd));
    }

    #[test]
    fn mcp_tool_groups_assigns_tools_by_server_prefix() {
        use async_trait::async_trait;
        use comrade_core::McpTransport;

        struct Stub {
            spec: comrade_tool::ToolSpec,
        }
        #[async_trait]
        impl comrade_tool::Tool for Stub {
            fn spec(&self) -> &comrade_tool::ToolSpec {
                &self.spec
            }
            async fn invoke(
                &self,
                _ctx: &comrade_tool::ToolContext,
                _args: serde_json::Value,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
        }
        let stub = |name: &str, desc: &str| -> Box<dyn comrade_tool::Tool> {
            Box::new(Stub {
                spec: comrade_tool::ToolSpec {
                    name: name.to_string(),
                    description: desc.to_string(),
                    json_schema: serde_json::json!({}),
                },
            })
        };
        let mut reg = comrade_tool::ToolRegistry::new();
        reg.register(stub("mcp_files_read", "[MCP server `files`] Reads a file."));
        reg.register(stub(
            "mcp_files_write",
            "[MCP server `files`] Writes a file.",
        ));
        reg.register(stub(
            "mcp_search_query",
            "[MCP server `search`] Runs a search.",
        ));
        // A non-MCP tool must never land in a group.
        reg.register(stub("write_file", "plain built-in"));

        let server = |name: &str| comrade_core::McpServerCfg {
            name: name.into(),
            transport: McpTransport::Http {
                url: "https://x/mcp".into(),
            },
            auth: None,
        };
        let groups = mcp_tool_groups(&[server("files"), server("search"), server("ghost")], &reg);
        assert_eq!(
            groups.iter().map(|g| g.server.as_str()).collect::<Vec<_>>(),
            vec!["files", "search", "ghost"],
            "config order preserved"
        );
        let names =
            |g: &McpToolGroup| -> Vec<String> { g.tools.iter().map(|t| t.name.clone()).collect() };
        assert_eq!(names(&groups[0]), vec!["mcp_files_read", "mcp_files_write"]);
        assert_eq!(names(&groups[1]), vec!["mcp_search_query"]);
        assert!(groups[2].tools.is_empty(), "server with no tools is empty");
        // The "[MCP server `files`] " description prefix is stripped.
        assert_eq!(groups[0].tools[0].desc, "Reads a file.");
    }

    #[test]
    fn mcp_visible_rows_orders_and_filters() {
        let group = |server: &str, tools: &[&str]| McpToolGroup {
            server: server.to_string(),
            tools: tools
                .iter()
                .map(|n| McpToolEntry {
                    name: n.to_string(),
                    desc: String::new(),
                })
                .collect(),
        };
        let view = |groups: Vec<McpToolGroup>, filter: &str, collapsed: &[usize]| McpServersView {
            groups,
            disabled: comrade_tool::ToolRegistry::new().disabled_handle(),
            filter: filter.to_string(),
            collapsed: collapsed.iter().copied().collect(),
            sel: 0,
        };

        let groups = vec![
            group("files", &["mcp_files_read", "mcp_files_write"]),
            group("search", &["mcp_search_query"]),
        ];
        // No filter: every header then every tool row, in order.
        assert_eq!(
            mcp_visible_rows(&view(groups.clone(), "", &[])),
            vec![
                RowRef::Header(0),
                RowRef::Tool(0, 0),
                RowRef::Tool(0, 1),
                RowRef::Header(1),
                RowRef::Tool(1, 0),
            ]
        );
        // A collapsed group keeps only its header.
        assert_eq!(
            mcp_visible_rows(&view(groups.clone(), "", &[1])),
            vec![
                RowRef::Header(0),
                RowRef::Tool(0, 0),
                RowRef::Tool(0, 1),
                RowRef::Header(1),
            ]
        );
        // Filter matching the SERVER name shows its whole group, others drop.
        assert_eq!(
            mcp_visible_rows(&view(groups.clone(), "files", &[])),
            vec![RowRef::Header(0), RowRef::Tool(0, 0), RowRef::Tool(0, 1)]
        );
        // Filter matching one TOOL shows that header + row only.
        assert_eq!(
            mcp_visible_rows(&view(groups.clone(), "write", &[])),
            vec![RowRef::Header(0), RowRef::Tool(0, 1)]
        );
        // Case-insensitive.
        assert_eq!(
            mcp_visible_rows(&view(groups.clone(), "SEARCH", &[])),
            vec![RowRef::Header(1), RowRef::Tool(1, 0)]
        );
        // A collapsed group matching only via its tools still shows its header.
        assert_eq!(
            mcp_visible_rows(&view(groups, "query", &[1])),
            vec![RowRef::Header(1)]
        );
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
    fn renders_a_pipe_table_as_aligned_columns() {
        let md = "| name | role |\n| --- | --- |\n| ana | dev |\n| bob | pm |\n";
        let lines = md_text(md, 40);
        assert_eq!(
            lines,
            vec!["name | role", "-----+-----", "ana  | dev ", "bob  | pm  ",],
            "{lines:?}"
        );
        // column content starts and the bar column line up across header/body
        for (i, l) in lines.iter().enumerate() {
            assert_eq!(l.chars().count(), 11, "row {i}: {l:?}");
        }
    }

    #[test]
    fn narrow_table_wraps_cells_within_width() {
        let md = "| tool | purpose |\n| --- | --- |\n| read_file | read a text file from disk, optionally windowed |\n";
        for w in [20usize, 30] {
            let lines = md_text(md, w);
            for l in &lines {
                assert!(
                    l.chars().count() <= w,
                    "line exceeds {w}: {l:?} (len {})",
                    l.chars().count()
                );
            }
            // wrapping keeps every word of the long cell visible
            let flat = lines.join("\n");
            for word in ["read", "a", "text", "file", "windowed"] {
                assert!(flat.contains(word), "missing {word:?} in {flat}");
            }
        }
    }

    #[test]
    fn pipe_text_without_a_separator_is_not_a_table() {
        let md = "use | for bitwise or\nin a plain sentence";
        let lines = md_text(md, 40);
        // treated as ordinary paragraph text, not aligned columns
        let flat = lines.join("\n");
        assert!(flat.contains("use | for bitwise or"), "{flat}");
        assert!(flat.contains("in a plain sentence"), "{flat}");
        assert_eq!(lines.len(), 1, "{flat}");
    }

    #[test]
    fn right_aligned_column_pads_cells() {
        let md = "| item | count |\n| :--- | ---: |\n| x | 42 |\n";
        let lines = md_text(md, 40);
        assert_eq!(lines[0], "item | count");
        assert_eq!(lines[1], "-----+------");
        // right-aligned column: value flush to the right edge of its column
        assert_eq!(lines[2], "x    |    42");
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
        let line = render_row_line(&row, false, false, Some(Color::Rgb(1, 2, 3)), 10, None);
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // prefix "| " + "hi" + pad to the span-area width (10): 12 cells total,
        // which equals the chat area width for rows with a 2-cell gutter.
        assert_eq!(flat, "| hi        ", "{flat}");
        assert_eq!(flat.chars().count(), 12);
    }

    #[test]
    fn subchat_row_indents_and_uses_agent_rule() {
        let row = RenderRow {
            rule: None,
            spans: vec![Span::raw("hi")],
            tool_header: None,
        };
        let line = render_row_line(
            &row,
            false,
            false,
            Some(Color::Rgb(9, 9, 9)),
            8,
            Some(Color::Rgb(1, 2, 3)),
        );
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // "| " rule + a 2-column indent + "hi" + pad to width 8 = 12 cells.
        assert!(flat.starts_with("|   hi"), "{flat}");
        assert_eq!(flat.chars().count(), 12);
        // The rule span is drawn in the delegate's agent color.
        assert_eq!(line.spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn unbanded_row_is_not_padded() {
        let row = RenderRow {
            rule: None,
            spans: vec![Span::raw("hi")],
            tool_header: None,
        };
        let line = render_row_line(&row, false, false, None, 10, None);
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
        let line = render_row_line(&row, true, false, None, 10, None);
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
        assert_eq!(step_visible(&chat, none, None, 1, false), Some(0));
        assert_eq!(step_visible(&chat, none, None, -1, false), Some(3));
        // Past an edge there is no visible block: no move.
        assert_eq!(step_visible(&chat, none, Some(0), -1, false), None);
        assert_eq!(step_visible(&chat, none, Some(3), 1, false), None);
        assert_eq!(step_visible(&chat, none, Some(1), 1, false), Some(2));
        assert_eq!(step_visible(&[], &[], Some(0), 1, false), None);
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
        assert_eq!(step_visible(&chat, &collapsed, Some(0), 1, false), Some(2));
        assert_eq!(step_visible(&chat, &collapsed, Some(2), -1, false), Some(0));
        assert_eq!(step_visible(&chat, &collapsed, Some(1), -1, false), Some(0));
        // chat_visible: headings always exposed; interiors only when expanded.
        assert!(chat_visible(&chat, &collapsed, 0, false));
        assert!(!chat_visible(&chat, &collapsed, 1, false));
        assert!(chat_visible(&chat, &collapsed, 2, false));
        assert!(chat_visible(&chat, &collapsed, 3, false));
        assert!(!chat_visible(&chat, &collapsed, 99, false));
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

    // --- focus mode -----------------------------------------------------------

    fn card() -> Msg {
        tool_card("read_file", r#"{"path":"src/x.rs"}"#, true, false)
    }

    #[test]
    fn focus_visible_shows_conversation_and_reasoning() {
        // User/Assistant/Delegate/Reasoning are visible
        assert!(focus_visible(&Msg::authored(MsgKind::User, "you", "hi")));
        assert!(focus_visible(&Msg::authored(
            MsgKind::Assistant,
            "model",
            "reply"
        )));
        assert!(focus_visible(&Msg::authored(
            MsgKind::Delegate,
            "delegate",
            "advice"
        )));
        assert!(focus_visible(&Msg::authored(
            MsgKind::Reasoning,
            "model",
            "thinking"
        )));
        // A question the agent asked (ask_form) survives focus mode.
        assert!(focus_visible(&Msg::text(MsgKind::Question, "Pick a patch")));
        // Tool/Failure/Meta are hidden
        assert!(!focus_visible(&card()));
        assert!(!focus_visible(&Msg::failure(
            "test".into(),
            "failed".into()
        )));
        assert!(!focus_visible(&Msg::text(MsgKind::Meta, "status")));
    }

    #[test]
    fn form_question_text_lists_title_and_fields() {
        let spec = form_spec(serde_json::json!({
            "title": "Pick a patch",
            "fields": [
                { "id": "pick", "label": "Which patch?", "kind": "text" }
            ]
        }));
        assert_eq!(
            form_question_text(&spec),
            "Pick a patch\n  Which patch? (pick)"
        );
        // An untitled form falls back to a plain "Question" heading.
        let untitled = form_spec(serde_json::json!({
            "fields": [{ "id": "n", "label": "N", "kind": "number" }]
        }));
        assert_eq!(form_question_text(&untitled), "Question\n  N (n)");
    }

    #[test]
    fn focus_visible_run_with_reasoning_child_is_visible() {
        // Run with a Reasoning child: visible
        let run_with_reasoning = Msg::run(vec![Msg::reasoning("model", "thinking"), card()]);
        assert!(focus_visible(&run_with_reasoning));
        // Run with only Tool children: not visible
        let run_only_tools = Msg::run(vec![card(), card()]);
        assert!(!focus_visible(&run_only_tools));
    }

    #[test]
    fn layout_chat_rows_focus_filters_messages() {
        let chat = vec![
            Msg::authored(MsgKind::User, "you", "do it"),
            card(),
            Msg::authored(MsgKind::Reasoning, "model", "thinking"),
            Msg::authored(MsgKind::Assistant, "model", "done"),
            Msg::text(MsgKind::Meta, "status"),
            Msg::failure("test".into(), "failed".into()),
        ];
        let collapsed = &[false];

        // With focus=true, only User/Reasoning/Assistant should produce rows
        let (_rows_focus, owners_focus, _) =
            layout_chat_rows(&chat, collapsed, "", 80, &[], &ModelColors::default(), true);
        // Only indices 0 (User), 2 (Reasoning), 3 (Assistant) should have rows
        // Tool (idx 1), Meta (idx 4), Failure (idx 5) should be hidden
        for idx in owners_focus.iter().flatten() {
            let kind = chat[*idx].kind;
            assert_ne!(
                kind,
                MsgKind::Tool,
                "Tool message should be hidden in focus mode"
            );
            assert_ne!(
                kind,
                MsgKind::Meta,
                "Meta message should be hidden in focus mode"
            );
            assert_ne!(
                kind,
                MsgKind::Failure,
                "Failure message should be hidden in focus mode"
            );
        }
        // The visible messages (User, Reasoning, Assistant) should have rows
        let visible_indices: std::collections::HashSet<_> = owners_focus.iter().flatten().collect();
        assert!(
            visible_indices.contains(&0),
            "User message should be visible"
        );
        assert!(
            visible_indices.contains(&2),
            "Reasoning message should be visible"
        );
        assert!(
            visible_indices.contains(&3),
            "Assistant message should be visible"
        );
        assert!(
            !visible_indices.contains(&1),
            "Tool message should be hidden"
        );
        assert!(
            !visible_indices.contains(&4),
            "Meta message should be hidden"
        );
        assert!(
            !visible_indices.contains(&5),
            "Failure message should be hidden"
        );
    }

    #[tokio::test]
    async fn activity_spinner_shows_only_for_a_running_focus_mode_chat() {
        let mut app = test_app();
        assert!(!activity_spinner_visible(&app), "idle: no spinner");
        app.running = true;
        assert!(
            !activity_spinner_visible(&app),
            "running but not focus mode: no spinner"
        );
        app.focus_mode = true;
        assert!(activity_spinner_visible(&app), "running in focus mode");
        // Paused on a user dialog: the spinner hides (the dialog is the signal).
        let (tx, _rx) = oneshot::channel();
        app.dialogs.push(Dialog {
            session: app.active_id(),
            prompt: UserPrompt::Confirm {
                title: "T".to_string(),
                diff: None,
            },
            buf: String::new(),
            reply: tx,
            form: None,
        });
        assert!(!activity_spinner_visible(&app), "waiting on the user");
    }

    #[tokio::test]
    async fn activity_line_names_the_running_tool_then_the_model() {
        let mut app = test_app();
        app.running = true;
        app.focus_mode = true;
        app.activity = Some("fs_read_file".to_string());
        let joined = |line: Line<'static>| -> String {
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        };
        let text = joined(activity_line(&app, 0));
        assert!(text.contains("running fs_read_file"), "{text:?}");
        assert!(text.contains(spinner_glyph(0)), "{text:?}");
        // With no tool in flight the model is thinking.
        app.activity = None;
        let text = joined(activity_line(&app, 0));
        assert!(text.contains("working"), "{text:?}");
    }

    #[test]
    fn layout_chat_rows_focus_run_with_reasoning_shows_reasoning() {
        let run_with_reasoning = Msg::run(vec![
            Msg::reasoning("model", "let me think about this"),
            card(),
        ]);
        let chat = vec![
            Msg::authored(MsgKind::User, "you", "go"),
            run_with_reasoning,
            Msg::authored(MsgKind::Assistant, "model", "done"),
        ];
        let collapsed = &[false];

        // With focus=true, the Run digest's summary row is hidden but reasoning text appears
        let (rows, owners, _) =
            layout_chat_rows(&chat, collapsed, "", 80, &[], &ModelColors::default(), true);

        // The digest has no summary row of its own, but its reasoning child
        // renders as a normal block (header + markdown body), so search the
        // row spans for the text with whitespace normalised (markdown splits
        // prose into several spans).
        let reasoning_text = "let me think about this";
        let has_reasoning = rows.iter().any(|row| {
            let joined: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            let joined = joined.split_whitespace().collect::<Vec<_>>().join(" ");
            joined.contains(reasoning_text)
        });
        assert!(
            has_reasoning,
            "Reasoning text should appear in focus mode for Run with Reasoning child"
        );

        // The digest has no summary row of its own, but the reasoning it keeps
        // is attributed to the digest so it stays selectable as one block.
        assert!(
            owners.contains(&Some(1)),
            "the folded reasoning should be owned by the digest index"
        );
    }

    #[test]
    fn chat_visible_and_step_visible_with_focus_skip_hidden() {
        let chat = vec![
            Msg::authored(MsgKind::User, "you", "a"),
            card(),
            Msg::authored(MsgKind::Assistant, "model", "b"),
        ];
        let none = &[false];

        // With focus=true, Tool message (idx 1) is hidden
        assert!(chat_visible(&chat, none, 0, true)); // User visible
        assert!(!chat_visible(&chat, none, 1, true)); // Tool hidden
        assert!(chat_visible(&chat, none, 2, true)); // Assistant visible

        // step_visible with focus=true skips the hidden Tool message
        // Starting from User (idx 0), stepping down should go to Assistant (idx 2)
        assert_eq!(step_visible(&chat, none, Some(0), 1, true), Some(2));
        // Starting from Assistant (idx 2), stepping up should go to User (idx 0)
        assert_eq!(step_visible(&chat, none, Some(2), -1, true), Some(0));
        // Tool message (idx 1) is not visible, so stepping from it should find next visible
        assert_eq!(step_visible(&chat, none, Some(1), 1, true), Some(2));
        assert_eq!(step_visible(&chat, none, Some(1), -1, true), Some(0));
    }

    // --- run folding ------------------------------------------------------

    fn tool_card(name: &str, args: &str, ok: bool, open: bool) -> Msg {
        Msg::tool(ToolCard {
            name: name.into(),
            author: Some("model".into()),
            args: args.into(),
            justification: None,
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
    fn reasoning_block_renders_as_a_brained_spoken_block() {
        let mut out = Vec::new();
        layout_reasoning(
            &mut out,
            "let me check **this**",
            "model",
            &ModelColors::default(),
            true,
            40,
        );
        // The header is the brain glyph plus the model's name in its colour, not
        // a collapsible card arrow.
        assert_eq!(out[0].spans.len(), 1);
        assert_eq!(out[0].spans[0].content, "\u{1f9e0} model");
        assert!(out[0].tool_header.is_none(), "header is not a tool card");
        // The body follows as a normal markdown block, no rule, no card.
        assert!(out.len() > 1);
        let joined: String = out[1..]
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("this"), "body must render: {joined:?}");
        for r in &out {
            assert!(r.rule.is_none() && r.tool_header.is_none());
        }
        // Collapsed: only the header line.
        let mut closed = Vec::new();
        layout_reasoning(
            &mut closed,
            "hidden",
            "model",
            &ModelColors::default(),
            false,
            40,
        );
        assert_eq!(closed.len(), 1);
    }

    #[test]
    fn reasoning_body_text_is_bright_like_the_answer() {
        // Reasoning is a chat bubble like the final answer, so its prose must
        // render in the same bright base colour - never dimmed grey.
        let mut out = Vec::new();
        layout_reasoning(
            &mut out,
            "plain reasoning prose",
            "model",
            &ModelColors::default(),
            true,
            40,
        );
        let body = &out[1];
        assert!(
            body.spans.iter().any(|s| s.style.fg == Some(Color::White)),
            "reasoning prose must use the bright base fg: {:?}",
            body.spans
        );
        assert!(
            body.spans
                .iter()
                .all(|s| s.style.fg != Some(Color::DarkGray)),
            "reasoning prose must never be dimmed: {:?}",
            body.spans
        );
    }

    #[test]
    fn spoken_blocks_are_banded_by_their_model() {
        let mut colors = ModelColors::new();
        colors.assign(&["main".to_string(), "dev".to_string()]);
        let dev = DelegateCfg {
            name: "dev".into(),
            ..Default::default()
        };
        let delegates = vec![dev];
        // Reasoning is banded with its model's dimmed colour.
        let r = Msg::reasoning("main", "think");
        assert_eq!(
            row_band(Some(&r), &colors, &delegates, false),
            Some(colors.band_color("main"))
        );
        // The final answer is banded with the model's colour too (integration).
        let a = Msg::authored(MsgKind::Assistant, "main", "done");
        assert_eq!(
            row_band(Some(&a), &colors, &delegates, false),
            Some(colors.band_color("main"))
        );
        // A delegate reply keeps its own band.
        let d = Msg::authored(MsgKind::Delegate, "dev", "hi");
        assert_eq!(
            row_band(Some(&d), &colors, &delegates, false),
            Some(colors.band_color("dev"))
        );
        // A user turn takes the lifted user band.
        let u = Msg::authored(MsgKind::User, "you", "go");
        assert_eq!(
            row_band(Some(&u), &colors, &delegates, false),
            Some(user_band_bg())
        );
        // A folded digest bands its reasoning only in focus mode; an unowned
        // row is never banded.
        let run = Msg::run(vec![Msg::reasoning("main", "hmm")]);
        assert_eq!(
            row_band(Some(&run), &colors, &delegates, true),
            Some(colors.band_color("main"))
        );
        assert_eq!(row_band(Some(&run), &colors, &delegates, false), None);
        assert_eq!(row_band(None, &colors, &delegates, false), None);
    }

    #[test]
    fn assistant_header_uses_the_model_colour_and_is_banded() {
        // The final-answer block shares the reasoning block's shape: a header in
        // the model's colour (not the old fixed Green) and a model-coloured
        // band on its rows.
        let mut colors = ModelColors::new();
        colors.assign(&["main".to_string()]);
        let chat = vec![Msg::authored(MsgKind::Assistant, "main", "the answer")];
        let (rows, owners, _) = layout_chat_rows(&chat, &[false], "", 60, &[], &colors, false);
        assert!(owners.iter().all(|o| *o == Some(0)));
        let header = &rows[0];
        assert_eq!(header.spans[0].style.fg, Some(colors.name_color("main")));
        assert_ne!(header.spans[0].style.fg, Some(Color::Green));
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
    fn loc_parts_styles_path_and_line() {
        let parts = loc_parts("crates/a.rs:42: let x = 1;").unwrap();
        // path, ':', number, snippet
        assert_eq!(parts[0].0, "crates/a.rs");
        assert_eq!(parts[0].1.fg, Some(Color::Cyan));
        assert_eq!(parts[1].0, ":");
        assert_eq!(parts[2].0, "42");
        assert_eq!(parts[2].1.fg, Some(Color::Yellow));
        assert_eq!(parts[3].0, "let x = 1;");
    }

    #[test]
    fn loc_parts_handles_column_and_double_space() {
        // ts_find_references: `file:line:col  context`
        let parts = loc_parts("crates/a.rs:12:7  let y = 2;").unwrap();
        assert_eq!(parts[0].0, "crates/a.rs");
        assert_eq!(parts[2].0, "12");
        assert_eq!(parts[3].0, "let y = 2;");
    }

    #[test]
    fn loc_parts_handles_at_line_form() {
        let parts = loc_parts("fn build | pub fn build(x: u32) @ 42").unwrap();
        assert_eq!(parts[parts.len() - 1].0, "42");
        assert_eq!(parts[parts.len() - 1].1.fg, Some(Color::Yellow));
    }

    #[test]
    fn loc_parts_rejects_prose() {
        assert!(loc_parts("just some text").is_none());
        assert!(loc_parts("note: 5").is_none());
        assert!(loc_parts("test result: ok. 5 passed; 0 failed").is_none());
    }

    #[test]
    fn result_rows_glues_location_and_styles_snippet() {
        let rows = result_rows("crates/a.rs:42: let x = 1;", true, 80);
        let flat: String = rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        assert!(flat.contains("crates/a.rs:42"));
        assert!(rows[0].iter().any(|s| s.style.fg == Some(Color::Cyan)));
        assert!(rows[0].iter().any(|s| s.style.fg == Some(Color::Yellow)));
    }

    #[test]
    fn result_rows_falls_back_to_single_colour() {
        let rows = result_rows("ok\nnothing here", true, 80);
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|r| r.iter().all(|s| s.style.fg == Some(Color::Green)))
        );
        let failed = result_rows("boom", false, 80);
        assert!(
            failed
                .iter()
                .all(|r| r.iter().all(|s| s.style.fg == Some(Color::Red)))
        );
    }

    #[test]
    fn result_rows_styles_file_header() {
        let rows = result_rows("== crates/a.rs ==", false, 80);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].content.as_ref(), "== crates/a.rs ==");
        assert_eq!(rows[0][0].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn delegate_failure_output_is_not_a_reply() {
        assert!(parse_delegate_reply("ERROR: delegate mistral failed").is_none());
        assert!(parse_delegate_reply("").is_none());
        assert!(parse_delegate_reply("delegate replied:\nno name").is_none());
    }

    #[test]
    fn advice_reply_is_parsed_into_delegate_and_text() {
        let out = "advice from mistral (ollama/mistral:7b):\nSplit the task into three steps.";
        let (model, advice) = parse_advice_reply(out).unwrap();
        assert_eq!(model, "mistral");
        assert_eq!(advice, "Split the task into three steps.");
        // Failure output is not advice.
        assert!(parse_advice_reply("ERROR: delegate mistral failed").is_none());
        assert!(parse_advice_reply("advice from :\nnothing").is_none());
    }

    #[test]
    fn delegate_panel_rows_wrap_inside_the_panel_width() {
        // A delegate whose label + model + description is far wider than the
        // model panel must be wrapped, never left to spill past the edge.
        let mut d = DelegateCfg {
            name: "mistral".into(),
            description:
                "Cheap and fast, good for basic coding tasks and summarising long outputs.".into(),
            ..Default::default()
        };
        d.llm.model = "ollama/mistral:7b".into();
        let rows = delegate_panel_rows(&[d], &ModelColors::new(), 24)
            .expect("rows for a configured delegate");
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
                row.width() <= 24,
                "delegate row wider than the panel: {:?}",
                row
            );
        }
        // No delegate configured → the panel keeps its legacy fixed height.
        assert!(delegate_panel_rows(&[], &ModelColors::new(), 24).is_none());
    }

    #[test]
    fn disabled_delegate_is_flagged_in_the_panel() {
        let mut d = DelegateCfg {
            name: "off".into(),
            enabled: false,
            ..Default::default()
        };
        d.llm.model = "off".into();
        let rows = delegate_panel_rows(&[d], &ModelColors::new(), 24)
            .expect("rows for a configured delegate");
        let joined: String = rows
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("off"), "{joined:?}");
        assert!(joined.contains("(disabled)"), "{joined:?}");
    }

    #[test]
    fn short_tokens_compacts_counts() {
        assert_eq!(short_tokens(0), "0");
        assert_eq!(short_tokens(900), "900");
        assert_eq!(short_tokens(9_500), "9.5k");
        assert_eq!(short_tokens(85_000), "85k");
        assert_eq!(short_tokens(128_000), "128k");
        assert_eq!(short_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn gauge_line_shows_pct_and_counts() {
        let l = gauge_line(25_000, 100_000, false, 40);
        let flattened: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(flattened.contains("25%"), "missing pct in {flattened}");
        assert!(
            flattened.contains("25k/100k"),
            "missing counts in {flattened}"
        );
        assert!(flattened.contains('#'), "missing bar in {flattened}");
    }

    #[test]
    fn gauge_line_drops_bar_on_narrow_panel() {
        let l = gauge_line(25_000, 100_000, true, 8);
        let flattened: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !flattened.contains('#'),
            "bar should be dropped in {flattened}"
        );
        assert!(flattened.contains("25%"), "missing pct in {flattened}");
        assert!(flattened.contains("100k"), "missing budget in {flattened}");
        assert!(
            flattened.contains("est"),
            "missing est marker in {flattened}"
        );
    }

    /// A minimal App wired to offline defaults, for session tests.
    fn test_app() -> App {
        let cfg = Arc::new(
            comrade_core::Config::load(None)
                .expect("default config")
                .config,
        );
        let client = Arc::new(comrade_core::LlmClient::new(&cfg.llm).expect("llm client"));
        let tools = Arc::new(comrade_tool::ToolRegistry::new());
        let deps = Deps {
            cfg,
            client,
            tools,
            root: std::env::temp_dir(),
            balance: None,
            config_source: None,
            auto_forced: false,
        };
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<TaggedEvent>();
        let run_tx = spawn_tagged_relay(0, events_tx.clone());
        let (asks_tx, asks_rx) = mpsc::channel::<PendingAsk>(4);
        let (git_tx, git_rx) = mpsc::channel::<GitBarInfo>(4);
        build_app(
            &deps, events_tx, events_rx, run_tx, asks_tx, asks_rx, git_tx, git_rx,
        )
    }

    #[tokio::test]
    async fn session_counts_label_counts_running_and_idle() {
        let mut app = test_app();
        // Single idle session.
        assert_eq!(session_counts_label(&app), "1 idle");
        // Two sessions, the first parked with a run in flight, the active one idle.
        app.running = true;
        app.new_session();
        assert_eq!(session_counts_label(&app), "1 running, 1 idle");
        // Second (active) session now running too.
        app.running = true;
        assert_eq!(session_counts_label(&app), "2 running");
        // All idle.
        app.running = false;
        if let Some(s) = app.open_sessions.iter_mut().find(|s| s.live.is_some()) {
            s.live.as_mut().unwrap().running = false;
        }
        assert_eq!(session_counts_label(&app), "2 idle");
    }

    #[tokio::test]
    async fn blocked_session_counts_as_blocked_and_shows_waiting() {
        let mut app = test_app();
        assert_eq!(session_counts_label(&app), "1 idle");
        assert_eq!(session_status_marker(&app, 0), None);
        let (tx, _rx) = oneshot::channel();
        app.dialogs.push(Dialog {
            session: app.active_id(),
            prompt: UserPrompt::Confirm {
                title: "Test".to_string(),
                diff: None,
            },
            buf: String::new(),
            reply: tx,
            form: None,
        });
        assert_eq!(session_status_marker(&app, 0), Some("waiting"));
        assert_eq!(session_counts_label(&app), "1 waiting");
        app.new_session();
        assert_eq!(session_counts_label(&app), "1 waiting, 1 idle");
    }

    #[tokio::test]
    async fn new_session_parks_a_running_session_and_routes_its_events() {
        let mut app = test_app();
        let first_id = app.active_id();
        // A run is in flight on the first session when the user opens a new one.
        app.running = true;
        app.new_session();
        assert_eq!(app.open_sessions.len(), 2);
        assert_ne!(app.active_id(), first_id);
        assert!(!app.running);
        let parked_running = |app: &App| {
            app.open_sessions
                .iter()
                .find(|s| s.id == first_id)
                .and_then(|s| s.live.as_ref())
                .is_some_and(|l| l.running)
        };
        assert!(parked_running(&app), "the parked session keeps its run");
        let active_chat = app.chat.len();
        // An event tagged with the parked session updates ITS chat, not the active one.
        app.on_agent_event_for(first_id, AgentEvent::RunEnd);
        assert!(!parked_running(&app), "RunEnd lands on the parked session");
        assert_eq!(app.chat.len(), active_chat, "active session chat untouched");
    }

    #[tokio::test]
    async fn switching_back_restores_the_parked_session_chat() {
        let mut app = test_app();
        let first_id = app.active_id();
        app.push_meta("marker-one");
        let first_len = app.chat.len();
        app.new_session();
        let second_id = app.active_id();
        assert_ne!(first_id, second_id);
        assert_eq!(app.chat.len(), 1); // just the "opened a new session" note
        let idx = app.session_index(first_id).unwrap();
        app.activate(idx);
        assert_eq!(app.active_id(), first_id);
        assert_eq!(app.chat.len(), first_len);
        assert!(app.chat.iter().any(|m| m.text == "marker-one"));
        let idx = app.session_index(second_id).unwrap();
        app.activate(idx);
        assert_eq!(app.active_id(), second_id);
        assert_eq!(app.chat.len(), 1);
    }

    #[tokio::test]
    async fn kill_closes_the_active_session_and_activates_a_neighbour() {
        let mut app = test_app();
        let first_id = app.active_id();
        app.new_session();
        let second_id = app.active_id();
        app.kill_session();
        assert_eq!(app.open_sessions.len(), 1);
        assert_eq!(app.active_id(), first_id);
        assert!(app.session_index(second_id).is_none());
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
/// line numbers. For `fs_edit` literal mode the numbers come from the tool
/// result (which reports `(@@ -a,b +c,d @@)`); for `fs_edit` patch mode they
/// are parsed from its own `@@` header. Falls back to plain `diff:` when
/// nothing is derivable.
fn edit_diff_label(name: &str, args_json: &str, result: Option<&str>) -> String {
    let value: Option<serde_json::Value> = serde_json::from_str(args_json).ok();
    let pick_path = |keys: &[&str]| -> Option<String> {
        let map = value.as_ref()?.as_object()?;
        for k in keys {
            if let Some(s) = map.get(*k).and_then(serde_json::Value::as_str)
                && !s.trim().is_empty()
            {
                return Some(s.trim().to_string());
            }
        }
        None
    };
    let (rel, hunk) = match name {
        "fs_edit" if value.as_ref().and_then(|v| v.get("diff")).is_some() => {
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
        "fs_edit" => (pick_path(&["path", "file"]), result.and_then(hunk_token)),
        "git_diff" => {
            let diff = result.unwrap_or("");
            let files: Vec<&str> = diff
                .lines()
                .filter_map(|l| l.strip_prefix("+++ b/"))
                .map(str::trim)
                .collect();
            let rel = match files.as_slice() {
                [f] => Some((*f).to_string()),
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
fn extract_diff_sides(
    name: &str,
    args_json: &str,
    result: Option<&str>,
) -> Option<(Vec<String>, Vec<String>)> {
    let value: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    match name {
        "fs_edit" if value.get("diff").is_some() => {
            let diff = value.get("diff")?.as_str()?;
            for line in diff.lines() {
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    continue;
                } else if let Some(stripped) = line.strip_prefix('+') {
                    added.push(stripped.to_string());
                } else if let Some(stripped) = line.strip_prefix('-') {
                    removed.push(stripped.to_string());
                }
            }
        }
        "fs_edit" => {
            let old = value.get("old")?.as_str()?;
            let new = value.get("new")?.as_str()?;
            removed.extend(old.lines().map(str::to_string));
            added.extend(new.lines().map(str::to_string));
        }
        "git_diff" => {
            let diff = result?;
            for line in diff.lines() {
                if line.starts_with("diff --git")
                    || line.starts_with("index ")
                    || line.starts_with("new file mode")
                    || line.starts_with("deleted file mode")
                    || line.starts_with("old mode")
                    || line.starts_with("new mode")
                    || line.starts_with("similarity index")
                    || line.starts_with("rename from")
                    || line.starts_with("rename to")
                    || line.starts_with("---")
                    || line.starts_with("+++")
                    || line.starts_with("@@")
                    || line.starts_with("\\ No newline")
                {
                    continue;
                } else if let Some(rest) = line.strip_prefix('+') {
                    added.push(rest.to_string());
                } else if let Some(rest) = line.strip_prefix('-') {
                    removed.push(rest.to_string());
                }
            }
            if removed.is_empty() && added.is_empty() {
                return None;
            }
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
    fn fs_edit_patch_mode_extracts_sides() {
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n";
        let args = serde_json::json!({ "diff": diff }).to_string();
        let (old, new) = extract_diff_sides("fs_edit", &args, None).unwrap();
        assert_eq!(old, vec!["fn old() {}"]);
        assert_eq!(new, vec!["fn new() {}"]);
    }

    #[test]
    fn edit_diff_label_shows_file_and_hunk_numbers() {
        // fs_edit patch mode: file + @@ come from its own diff text.
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n-fn old() {}\n+fn new() {}\n";
        let args = serde_json::json!({ "diff": diff }).to_string();
        assert_eq!(
            edit_diff_label("fs_edit", &args, None),
            "diff  a.rs  @@ -1,3 +1,3 @@"
        );
        // fs_edit literal mode: file from args, numbers from the tool result token.
        let args = serde_json::json!({ "path": "src/lib.rs", "old": "a", "new": "b" }).to_string();
        assert_eq!(
            edit_diff_label(
                "fs_edit",
                &args,
                Some("Edited src/lib.rs: replaced 1 exact block (@@ -12,2 +12,3 @@).")
            ),
            "diff  src/lib.rs  @@ -12,2 +12,3 @@"
        );
        // No result yet: file only.
        assert_eq!(edit_diff_label("fs_edit", &args, None), "diff  src/lib.rs");
        // Nothing derivable falls back to the plain label.
        assert_eq!(edit_diff_label("rgrep", "{}", None), "diff:");
    }

    #[test]
    fn hunk_token_finds_last_terminator() {
        assert_eq!(hunk_token("@@ -1 +1 @@"), Some("@@ -1 +1 @@".to_string()));
        assert_eq!(hunk_token("no hunk here"), None);
    }

    #[test]
    fn fs_edit_literal_mode_uses_old_new() {
        let old = "a\nb\n";
        let new = "a\nc\n";
        let args = format!(
            r#"{{"old":{},"new":{}}}"#,
            serde_json::to_string(old).unwrap(),
            serde_json::to_string(new).unwrap()
        );
        let (removed, added) = extract_diff_sides("fs_edit", &args, None).unwrap();
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

    #[test]
    fn git_diff_result_extracts_sides() {
        let result = "diff --git a/a.rs b/a.rs\nindex 111..222 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n fn keep() {}\n-fn old() {}\n+fn new() {}\n";
        let (removed, added) = extract_diff_sides("git_diff", "{}", Some(result)).unwrap();
        assert_eq!(removed, vec!["fn old() {}"]);
        assert_eq!(added, vec!["fn new() {}"]);
    }

    #[test]
    fn git_diff_without_result_is_none() {
        assert!(extract_diff_sides("git_diff", "{}", None).is_none());
    }

    #[test]
    fn git_diff_no_changes_is_none() {
        assert!(extract_diff_sides("git_diff", "{}", Some("(no diff)")).is_none());
    }

    #[test]
    fn git_diff_label_shows_file_and_first_hunk() {
        let result = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -12,2 +12,3 @@\n-a\n+b\n";
        assert_eq!(
            edit_diff_label("git_diff", "{}", Some(result)),
            "diff  src/lib.rs  @@ -12,2 +12,3 @@"
        );
    }

    #[test]
    fn git_diff_multi_file_label_omits_hunk() {
        let result = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
                      diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -5 +5 @@\n-p\n+q\n";
        assert_eq!(edit_diff_label("git_diff", "{}", Some(result)), "diff:");
    }

    #[test]
    fn git_diff_card_shows_only_the_side_by_side_diff() {
        let result = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n";
        let card = ToolCard {
            name: "git_diff".into(),
            author: Some("model".into()),
            args: "{}".into(),
            justification: None,
            result: Some(result.into()),
            ok: true,
            open: true,
            started: Some(Instant::now()),
            taken_ms: Some(5),
            tokens: None,
        };
        let mut out = Vec::new();
        layout_tool(&mut out, 1, &card, 60);
        let body: String = out
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // The side-by-side diff rows are rendered...
        assert!(body.contains("fn old() {}"), "{body}");
        assert!(body.contains("fn new() {}"), "{body}");
        // ...and the raw git-diff output is NOT echoed below them.
        assert!(!body.contains("result:"), "{body}");
        assert!(!body.contains("diff --git"), "{body}");
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
        let (rows, owner, ranges) = layout_chat_rows(
            &two_exchanges(),
            &[],
            "",
            60,
            &[],
            &ModelColors::new(),
            false,
        );
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
        let (rows, owner, ranges) = layout_chat_rows(
            &two_exchanges(),
            &[true, false],
            "",
            60,
            &[],
            &ModelColors::new(),
            false,
        );
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
        assert!(!owner.contains(&Some(1)), "{owner:?}");
        assert_eq!(owner[0], Some(0));
        assert_eq!(owner[1], Some(0));
        assert_eq!(owner[2], Some(2));
        // Echo and marker rows are both clickable section headers.
        assert_eq!(rows.iter().filter(|r| r.tool_header == Some(0)).count(), 2);
    }

    #[test]
    fn expanding_a_section_restores_its_body() {
        let (rows, _, _) = layout_chat_rows(
            &two_exchanges(),
            &[true, false],
            "",
            60,
            &[],
            &ModelColors::new(),
            false,
        );
        let all: String = rows.iter().map(row_text).collect::<Vec<_>>().join("|");
        assert!(!all.contains("secret reply 0"), "{all}");
        let (rows, _, _) = layout_chat_rows(
            &two_exchanges(),
            &[false, false],
            "",
            60,
            &[],
            &ModelColors::new(),
            false,
        );
        let all: String = rows.iter().map(row_text).collect::<Vec<_>>().join("|");
        assert!(all.contains("secret reply 0"), "{all}");
        assert!(!all.contains("··· 1 more"), "{all}");
    }

    #[test]
    fn wrapped_user_echo_indents_continuation_rows() {
        let text = "abcdefghijkl mnopqrstuvwxyz 1234567890";
        let chat = vec![Msg::authored(MsgKind::User, "you", text)];
        let (rows, _, ranges) =
            layout_chat_rows(&chat, &[false], "", 12, &[], &ModelColors::new(), false);
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
        let c = chat_cache(
            &mut cache,
            7,
            60,
            false,
            &chat,
            &[],
            &[],
            &ModelColors::new(),
        );
        let ranges = c.ranges.clone();
        let owner = c.owner.clone();
        let rows = c.rows.len();
        let c2 = chat_cache(
            &mut cache,
            7,
            60,
            false,
            &chat,
            &[],
            &[],
            &ModelColors::new(),
        );
        assert_eq!(c2.ranges, ranges);
        assert_eq!(c2.owner, owner);
        assert_eq!(c2.rows.len(), rows);
    }

    #[test]
    fn chat_cache_relayouts_on_width_change_and_epoch_bump() {
        let mut cache: Option<ChatRowsCache> = None;
        let chat = two_exchanges();
        chat_cache(
            &mut cache,
            1,
            60,
            false,
            &chat,
            &[],
            &[],
            &ModelColors::new(),
        );
        // A narrower terminal width re-wraps text: the row count must change.
        let narrow = chat_cache(
            &mut cache,
            1,
            12,
            false,
            &chat,
            &[],
            &[],
            &ModelColors::new(),
        );
        let narrow_rows = narrow.rows.len();
        // An epoch bump (any chat/collapse mutation) must also rebuild.
        let collapsed = chat_cache(
            &mut cache,
            2,
            12,
            false,
            &chat,
            &[true, false],
            &[],
            &ModelColors::new(),
        );
        assert!(
            collapsed.rows.len() < narrow_rows,
            "collapsing exchange 0 must shrink the layout"
        );
        // The rebuilt cache equals a fresh pure layout of the same inputs.
        let (rows, owner, ranges) = layout_chat_rows(
            &chat,
            &[true, false],
            "",
            12,
            &[],
            &ModelColors::new(),
            false,
        );
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
        assert_eq!(plan_glyph(&PlanStatus::Ready, 0), "●");
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

    #[test]
    fn session_mx_commands() {
        // All four session commands are present in MxCommand::ALL.
        assert!(MxCommand::ALL.contains(&MxCommand::SaveSession));
        assert!(MxCommand::ALL.contains(&MxCommand::LoadSession));
        assert!(MxCommand::ALL.contains(&MxCommand::SwitchSession));
        assert!(MxCommand::ALL.contains(&MxCommand::ForkSession));
        assert!(MxCommand::ALL.contains(&MxCommand::KillSession));

        // Names match the expected Emacs-style identifiers.
        assert_eq!(MxCommand::SaveSession.name(), "save-session");
        assert_eq!(MxCommand::LoadSession.name(), "load-session");
        assert_eq!(MxCommand::SwitchSession.name(), "switch-session");
        assert_eq!(MxCommand::ForkSession.name(), "fork-session");
        assert_eq!(MxCommand::KillSession.name(), "kill-session");

        // Keybindings are as configured.
        assert_eq!(MxCommand::SaveSession.keys(), Some("C-x C-s"));
        assert_eq!(MxCommand::LoadSession.keys(), Some("C-x C-f"));
        assert_eq!(MxCommand::SwitchSession.keys(), Some("C-x C-b"));
        assert_eq!(MxCommand::ForkSession.keys(), Some("C-x C-w"));
        assert_eq!(MxCommand::KillSession.keys(), Some("C-x C-k"));

        // Descriptions are non-empty.
        assert!(!MxCommand::SaveSession.desc().is_empty());
        assert!(!MxCommand::LoadSession.desc().is_empty());
        assert!(!MxCommand::SwitchSession.desc().is_empty());
        assert!(!MxCommand::ForkSession.desc().is_empty());
        assert!(!MxCommand::KillSession.desc().is_empty());
    }
}
