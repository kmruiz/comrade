//! Cockpit TUI: markdown chat + plan checklist + prompt bar + modal dialogs.
//!
//! Chat layout:
//! - no emojis
//! - user messages carry a cyan rule on the left of every wrapped line
//! - agent tool calls render as a compact card (tool + justification); clicking
//!   (or the mouse wheel) opens the details
//! - assistant/user text is rendered as markdown

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use arboard::Clipboard;
use async_trait::async_trait;
use comrade_core::{AgentEvent, AgentSession, ChatMessage, MemoryUndo, Role, run_agent};
use comrade_tool::{PlanStatus, SessionControl, ToolContext, UserIo, UserPrompt, UserReply};
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

#[derive(Clone, Copy, PartialEq)]
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
}

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
}

/// One failed test: name + captured failure detail.
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
        }
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

/// Active incremental search over the chat history (Ctrl-F).
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

struct App {
    cfg: Arc<comrade_core::Config>,
    client: Arc<comrade_core::LlmClient>,
    tools: Arc<comrade_tool::ToolRegistry>,
    root: std::path::PathBuf,

    session: Arc<AgentSession>,
    undo: Arc<MemoryUndo>,
    ctx_base: ToolContext,

    events_tx: mpsc::Sender<AgentEvent>,
    events_rx: mpsc::Receiver<AgentEvent>,
    asks_rx: mpsc::Receiver<PendingAsk>,

    stop: Option<CancellationToken>,
    running: bool,
    /// Auto-accept mode: approvals are answered "yes" without prompting.
    auto_accept: bool,
    chat: Vec<Msg>,
    /// Raw current model output (not yet committed to a message).
    stream: String,
    input: Editor,
    /// Active Ctrl-F search over chat history (None when closed).
    search: Option<Search>,
    dialogs: Vec<Dialog>,
    /// True while the open dialog buffers a follow-up question to the model.
    dialog_ask: bool,
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
            let author = self.actor_label();
            self.push_msg(Msg::authored(MsgKind::Reasoning, author, visible));
        }
        self.stream.clear();
    }

    fn push_msg(&mut self, msg: Msg) {
        if self.chat.len() >= 400 {
            self.chat.remove(0);
        }
        self.chat.push(msg);
    }

    fn push_meta(&mut self, text: impl Into<String>) {
        self.push_msg(Msg::text(MsgKind::Meta, text));
    }

    fn last_tool_mut(&mut self, name: &str) -> Option<&mut ToolCard> {
        self.chat.iter_mut().rev().find_map(|m| match &mut m.tool {
            Some(c) if c.name == name || name.is_empty() => Some(c),
            _ => None,
        })
    }

    fn toggle_tool(&mut self, idx: usize) {
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

    /// Move to the next (`+1`) or previous (`-1`) chat block.
    fn move_block(&mut self, dir: isize) {
        if let Some(next) = step_block(self.sel, self.chat.len(), dir) {
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

    // --- Ctrl-F search over chat history ----------------------------------

    /// Recompute match indices from the current query, then jump to the first.
    fn refresh_search(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };
        let ql = query.to_lowercase();
        let matches: Vec<usize> = self
            .chat
            .iter()
            .enumerate()
            .filter(|(_, m)| msg_matches(m, &ql))
            .map(|(i, _)| i)
            .collect();
        if let Some(s) = &mut self.search {
            s.matches = matches;
            s.cur = 0;
        }
        self.goto_search_match();
    }

    /// Scroll the current search match into view; expand collapsed cards so the
    /// matched content is actually visible.
    fn goto_search_match(&mut self) {
        let Some(idx) = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.cur).copied())
        else {
            return;
        };
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
        let tx = self.events_tx.clone();
        let balance_tx = self.events_tx.clone();
        let stop = CancellationToken::new();
        self.stop = Some(stop.clone());
        self.running = true;
        self.follow = true;
        self.sel = None;
        tokio::spawn(async move {
            let _ = run_agent(&cfg, &client, ctx, &tools, prompt, tx, stop).await;
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
                self.push_meta("run finished");
            }
            AgentEvent::User(u) => {
                self.stream.clear();
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
                    let author = self.actor_label();
                    self.push_msg(Msg::authored(MsgKind::Reasoning, author, t.to_string()));
                }
            }
            AgentEvent::FinalAnswer(a) => {
                self.stream.clear();
                self.activity = None;
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
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

pub async fn run(deps: &Deps) -> Result<()> {
    let (asks_tx, asks_rx) = mpsc::channel::<PendingAsk>(16);
    let (dialog_ans_tx, mut dialog_ans_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let user = Arc::new(TuiUserIo { tx: asks_tx });
    let (bundle, events_tx, events_rx) = new_session(deps, user);

    let mut app = App {
        cfg: deps.cfg.clone(),
        client: deps.client.clone(),
        tools: deps.tools.clone(),
        root: deps.root.clone(),
        session: bundle.session.clone(),
        undo: bundle.undo.clone(),
        ctx_base: bundle.ctx_base.clone(),
        events_tx,
        events_rx,
        asks_rx,
        stop: None,
        running: false,
        auto_accept: false,
        chat: Vec::new(),
        stream: String::new(),
        input: Editor::new(),
        search: None,
        dialogs: Vec::new(),
        dialog_ask: false,
        dialog_ask_tx: None,
        dialog_conv: Vec::new(),
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
        }
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
                app.auto_accept = !app.auto_accept;
                app.push_msg(Msg::text(
                    MsgKind::Meta,
                    if app.auto_accept {
                        "auto-accept ON: approvals will be accepted automatically (ctrl-space to disable)"
                            .to_string()
                    } else {
                        "auto-accept off".to_string()
                    },
                ));
                if app.auto_accept {
                    // Accept any approval already waiting, then run with it.
                    app.accept_top_confirm();
                }
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
            if app.search.is_some() {
                return handle_search_key(app, key);
            }
            if !app.dialogs.is_empty() {
                return handle_dialog_key(app, key.code);
            }
            // Ctrl-F opens the chat-history search.
            if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.search = Some(Search::new());
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

/// Keys while the Ctrl-F search bar is active. Returns true when the app should quit.
fn handle_search_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        // Close the search bar (Esc or Ctrl-F).
        KeyCode::Esc => app.search = None,
        KeyCode::Char('f') if ctrl => app.search = None,
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

/// Next/previous chat block index given the current selection.
fn step_block(sel: Option<usize>, len: usize, dir: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match sel {
        Some(i) => (i as isize + dir).clamp(0, len as isize - 1) as usize,
        None => {
            if dir < 0 {
                len - 1
            } else {
                0
            }
        }
    })
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

fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Pre-wrap the prompt text so the row reserved for it can grow with the
    // content (search mode replaces the prompt with a fixed single row).
    let (prompt_rows, prompt_win, prompt_cur) = prompt_view(app, area.width);
    let prompt_h = if app.search.is_some() {
        1
    } else {
        prompt_rows.len().clamp(1, PROMPT_MAX_ROWS)
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(prompt_h as u16),
            Constraint::Length(1),
        ])
        .split(area);

    let run_state = if app.running { "RUNNING" } else { "IDLE" };
    let undo_count = app.undo.entry_count();

    let header = Line::from(vec![
        Span::styled(
            format!(" comrade | {} ", app.session.title()),
            Style::default()
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            app.root.to_string_lossy().into_owned(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled("undo:", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{undo_count}")),
    ]);
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
    let footer = Line::from(vec![
        Span::styled(
            status_msg,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::raw("  "),
        Span::styled(
            run_state,
            Style::default()
                .fg(if app.running {
                    Color::Green
                } else {
                    Color::Yellow
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "enter:run shift-enter:newline alt-backspace:word esc:cancel ctrl-c:quit ctrl-f:search ctrl-p/n:block alt-p/n:user tab:toggle",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), rows[0]);
    frame.render_widget(Paragraph::new(footer).alignment(Alignment::Right), rows[3]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Percentage(30)])
        .split(rows[1]);
    draw_chat(app, frame, cols[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(0)])
        .split(cols[1]);
    draw_stats(app, frame, right[0]);
    draw_plan(app, frame, right[1]);

    if let Some(s) = &app.search {
        // Search bar replaces the prompt line while Ctrl-F is active.
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
        frame.render_widget(Paragraph::new(search_line), rows[2]);
    } else {
        draw_prompt(app, frame, rows[2], &prompt_rows, prompt_win, prompt_cur);
    }

    if let Some(d) = app.dialogs.first() {
        draw_dialog(app, d, frame);
    }
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
    let mut lines = Vec::with_capacity(rows.len().min(PROMPT_MAX_ROWS));
    for (i, r) in rows.iter().enumerate().skip(win).take(PROMPT_MAX_ROWS) {
        let prefix = if i == 0 {
            Span::styled("> ", Style::default().fg(Color::Green))
        } else {
            Span::styled("  ", Style::default().fg(Color::DarkGray))
        };
        let mut spans = vec![prefix];
        spans.extend(selection_spans(&text, *r, sel));
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

fn draw_chat(app: &mut App, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" chat ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(2) as usize; // prefix column + spacing
    let (rows, row_msg, ranges) = layout_messages(app, width);

    app.chat_rect = inner;
    app.row_targets = rows.iter().map(|r| r.tool_header).collect();
    app.row_msg = row_msg;
    app.msg_ranges = ranges;
    app.view_rows = inner.height as usize;
    let max = rows.len().saturating_sub(inner.height as usize);
    // Autoscroll: stay pinned to the bottom while following a run or while the
    // user is already at the bottom of the chat.
    if app.follow || app.was_at_bottom {
        app.scroll_top = max;
    }
    app.scroll_top = app.scroll_top.min(max);
    let offset = app.scroll_top;
    app.was_at_bottom = app.scroll_top >= max;

    let sel_start = app.sel.and_then(|i| app.msg_ranges.get(i)).map(|&(s, _)| s);
    // Row span of the currently selected search match, if any.
    let search_hl = app
        .search
        .as_ref()
        .and_then(|s| s.matches.get(s.cur))
        .and_then(|&idx| app.msg_ranges.get(idx).copied());
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(row, r)| {
            let in_match = search_hl.is_some_and(|(start, len)| row >= start && row < start + len);
            let prefix = if Some(row) == sel_start {
                Line::from(vec![Span::styled(
                    "> ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )])
            } else {
                match r.rule {
                    Some(color) => Line::from(vec![Span::styled(
                        "| ",
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    )]),
                    None => Line::from("  "),
                }
            };
            let mut line = prefix;
            for s in &r.spans {
                let span = if in_match {
                    s.clone().patch_style(Style::default().bg(Color::Yellow))
                } else {
                    s.clone()
                };
                line.push_span(span);
            }
            line
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), inner);
}

/// Lay the chat out into rows. Returns (rows, msg-owner per row, per-message
/// row spans). The live streaming preview is appended without an owner.
fn layout_messages(
    app: &App,
    width: usize,
) -> (Vec<RenderRow>, Vec<Option<usize>>, Vec<(usize, usize)>) {
    let mut out = Vec::new();
    let mut owner: Vec<Option<usize>> = Vec::new();
    let mut ranges = Vec::new();
    for (i, msg) in app.chat.iter().enumerate() {
        let start = out.len();
        match msg.kind {
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
                        spans: vec![Span::styled(s, Style::default().fg(Color::DarkGray))],
                        tool_header: None,
                    });
                }
            }
            MsgKind::User => {
                author_header(&mut out, "you", Color::Cyan, width);
                let rule = Some(Color::Cyan);
                for spans in md_to_lines(&msg.text, width) {
                    out.push(RenderRow {
                        rule,
                        spans,
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
    }
    // Live streaming preview (scaffolding hidden), never committed.
    if !app.stream.is_empty() {
        let visible = strip_react_scaffolding(&app.stream);
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

fn layout_tool(out: &mut Vec<RenderRow>, msg_idx: usize, card: &ToolCard, width: usize) {
    let (icon, accent) = tool_icon(&card.name);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            if card.open { "v " } else { "> " },
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            icon,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", card.name),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(a) = card.author.as_deref() {
        spans.push(Span::styled(
            format!(" · {a}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(h) = tool_headline(&card.name, &card.args) {
        spans.push(Span::styled(
            format!("  {h}"),
            Style::default().fg(Color::White),
        ));
    }
    if let Some(j) = card.justification.as_deref() {
        spans.push(Span::styled(
            format!("  · {j}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Status: pass/fail mark, plus (when collapsed) a one-line result tail so
    // the row shows what happened, never just the task's name.
    if let Some(result) = card.result.as_deref() {
        let ok = card.ok;
        spans.push(Span::styled(
            format!("  {}", if ok { "✓" } else { "✗" }),
            Style::default()
                .fg(if ok { Color::Green } else { Color::Red })
                .add_modifier(Modifier::BOLD),
        ));
        if !card.open {
            let tail = one_line(result, 56);
            spans.push(Span::styled(
                format!("  {tail}"),
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
            spans: vec![Span::styled("diff:", Style::default().fg(Color::DarkGray))],
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
    // the model gauge; clipped naturally when the panel is short.
    if !app.cfg.delegates.is_empty() {
        let dim = Style::default().fg(Color::DarkGray);
        let mut delegate_lines = vec![Line::from(Span::styled(
            "delegates:",
            dim.add_modifier(Modifier::BOLD),
        ))];
        for d in &app.cfg.delegates {
            let label = if d.name == d.llm.model {
                d.name.clone()
            } else {
                format!("{} ({})", d.name, d.llm.model)
            };
            delegate_lines.push(Line::from(Span::styled(format!("  {label}"), dim)));
        }
        frame.render_widget(Paragraph::new(delegate_lines), rows[3]);
    }
    let _ = rows;
}

fn draw_plan(app: &App, frame: &mut Frame, area: Rect) {
    let finished = app.session.finished_summary().is_some();
    let block =
        Block::default()
            .borders(Borders::ALL)
            .title(if finished { " plan ok " } else { " plan " });
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
        let text_color = if step.status == PlanStatus::Done {
            Color::DarkGray
        } else {
            Color::White
        };
        let mut toks = vec![
            tok(
                format!("{} ", plan_prefix(&step.status)),
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

fn plan_prefix(s: &PlanStatus) -> &'static str {
    match s {
        PlanStatus::Pending => "-",
        PlanStatus::InProgress => "o",
        PlanStatus::Done => "+",
        PlanStatus::Blocked => "x",
    }
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
    fn wraps_long_paragraph() {
        let text = "word ".repeat(50);
        let lines = md_text(&text, 20);
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(l.chars().count() <= 20, "line too wide: {l:?}");
        }
    }

    #[test]
    fn block_navigation_wraps_at_edges() {
        // blocks: user, tool, assistant
        assert_eq!(step_block(None, 3, 1), Some(0));
        assert_eq!(step_block(None, 3, -1), Some(2));
        assert_eq!(step_block(Some(0), 3, -1), Some(0));
        assert_eq!(step_block(Some(2), 3, 1), Some(2));
        assert_eq!(step_block(Some(1), 3, 1), Some(2));
        assert_eq!(step_block(Some(0), 0, 1), None);
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
        });
        assert!(msg_matches(&card, "run_task"));
        assert!(msg_matches(&card, "verify"));
        assert!(msg_matches(&card, "passed"));

        let fail = Msg::failure("flaky_test".into(), "assert left == right".into());
        assert!(msg_matches(&fail, "flaky"));
        assert!(msg_matches(&fail, "left == right"));
    }
}
