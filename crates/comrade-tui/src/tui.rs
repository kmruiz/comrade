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
use async_trait::async_trait;
use comrade_core::{AgentEvent, AgentSession, MemoryUndo, run_agent};
use comrade_tool::{PlanStatus, SessionControl, ToolContext, UserIo, UserPrompt, UserReply};
use crossterm::event::{
    self, Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{Deps, new_session};

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
    User,
    Assistant,
    Tool,
    Meta,
}

struct ToolCard {
    name: String,
    args: String,
    justification: Option<String>,
    risk: Option<String>,
    result: Option<String>,
    ok: bool,
    open: bool,
}

struct Msg {
    kind: MsgKind,
    text: String,
    tool: Option<ToolCard>,
}

impl Msg {
    fn text(kind: MsgKind, text: impl Into<String>) -> Self {
        Msg {
            kind,
            text: text.into(),
            tool: None,
        }
    }
    fn tool(card: ToolCard) -> Self {
        Msg {
            kind: MsgKind::Tool,
            text: String::new(),
            tool: Some(card),
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
    chat: Vec<Msg>,
    /// Raw current model output (not yet committed to a message).
    stream: String,
    input: String,
    dialogs: Vec<Dialog>,

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
    /// Latest context-usage snapshot for the gauge.
    ctx_tokens: usize,
    ctx_budget: usize,
    ctx_estimated: bool,
    /// Name of the tool currently running (auto status while no agent text).
    activity: Option<String>,
}

impl App {
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
            if let Some(card) = &mut m.tool {
                card.open = !card.open;
            }
        }
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
        if let Some(&(start, _)) = self.msg_ranges.get(idx) {
            self.scroll_top = start;
        }
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
        let stop = CancellationToken::new();
        self.stop = Some(stop.clone());
        self.running = true;
        self.follow = true;
        self.sel = None;
        tokio::spawn(async move {
            let _ = run_agent(&cfg, &client, ctx, &tools, prompt, tx, stop).await;
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
                self.push_msg(Msg::text(MsgKind::User, u));
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
                // This turn produced a tool call; drop any scaffold-only prose
                // that was streaming and show a compact card instead.
                self.stream.clear();
                self.activity = Some(name.clone());
                self.push_msg(Msg::tool(ToolCard {
                    name,
                    args,
                    justification,
                    risk,
                    result: None,
                    ok: true,
                    open: false,
                }));
            }
            AgentEvent::ToolStart { .. } => {}
            AgentEvent::ToolResult { name, output, ok } => {
                self.stream.clear();
                self.activity = None;
                if let Some(card) = self.last_tool_mut(&name) {
                    card.result = Some(output);
                    card.ok = ok;
                }
            }
            AgentEvent::AssistantText(_) => {}
            AgentEvent::Thought(_) => {}
            AgentEvent::FinalAnswer(a) => {
                self.stream.clear();
                self.activity = None;
                let visible = strip_react_scaffolding(&a);
                if !visible.trim().is_empty() {
                    self.push_msg(Msg::text(MsgKind::Assistant, visible));
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
        chat: Vec::new(),
        stream: String::new(),
        input: String::new(),
        dialogs: Vec::new(),
        chat_rect: Rect::default(),
        row_targets: Vec::new(),
        row_msg: Vec::new(),
        msg_ranges: Vec::new(),
        view_rows: 0,
        sel: None,
        scroll_top: 0,
        follow: true,
        ctx_tokens: 0,
        ctx_budget: deps.cfg.context.budget_tokens,
        ctx_estimated: true,
        activity: None,
    };

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
                        app.dialogs.push(Dialog { prompt: ask.prompt, buf: String::new(), reply: ask.reply });
                        app.push_meta("waiting for your input");
                    }
                    None => break Err(anyhow::anyhow!("ask channel closed")),
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
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return true;
            }
            if !app.dialogs.is_empty() {
                return handle_dialog_key(app, key.code);
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
            match key.code {
                KeyCode::Esc => {
                    if app.running {
                        app.cancel_run();
                    }
                }
                KeyCode::Enter => {
                    let prompt = std::mem::take(&mut app.input);
                    app.start_run(prompt);
                }
                KeyCode::Tab => {
                    // Toggle the selected block (the one under the "> " marker).
                    if let Some(idx) = app.sel {
                        app.toggle_tool(idx);
                    }
                }
                KeyCode::Char(c) => app.input.push(c),
                KeyCode::Backspace => {
                    app.input.pop();
                }
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

fn handle_dialog_key(app: &mut App, code: KeyCode) -> bool {
    let is_confirm = matches!(
        app.dialogs.first().unwrap().prompt,
        UserPrompt::Confirm { .. }
    );
    match code {
        KeyCode::Esc => app.answer_top(UserReply::Denied),
        KeyCode::Char('y') if is_confirm => app.answer_top(UserReply::Answer("yes".into())),
        KeyCode::Char('n') if is_confirm => app.answer_top(UserReply::Answer("no".into())),
        KeyCode::Enter => app.answer_from_buf(),
        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
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
        }
        MouseEventKind::ScrollDown => {
            app.scroll_top += 3;
            app.follow = false;
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

// ---------------------------------------------------------------------------
// chat rendering / markdown
// ---------------------------------------------------------------------------

fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
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
            "enter:run  esc:cancel  ctrl-c:quit  ctrl-p/n:block  alt-p/n:user  tab:toggle",
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

    let input_hint = if app.running { " (running...)" } else { "" };
    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Green)),
        Span::raw(app.input.clone()),
        Span::styled("_", Style::default().fg(Color::Green)),
        Span::styled(input_hint, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(input_line), rows[2]);

    if let Some(d) = app.dialogs.first() {
        draw_dialog(app, d, frame);
    }
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
    if app.follow {
        app.scroll_top = rows.len().saturating_sub(inner.height as usize);
    }
    let offset = app
        .scroll_top
        .min(rows.len().saturating_sub(inner.height as usize));
    app.scroll_top = offset;

    let sel_start = app.sel.and_then(|i| app.msg_ranges.get(i)).map(|&(s, _)| s);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(row, r)| {
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
                line.push_span(s.clone());
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
            MsgKind::Tool => layout_tool(&mut out, i, msg.tool.as_ref().unwrap(), width),
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
                for spans in md_to_lines(&msg.text, width) {
                    out.push(RenderRow {
                        rule: None,
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

fn layout_tool(out: &mut Vec<RenderRow>, msg_idx: usize, card: &ToolCard, width: usize) {
    out.push(RenderRow {
        rule: None,
        spans: vec![
            Span::styled(
                if card.open { "v " } else { "> " },
                Style::default().fg(Color::Magenta),
            ),
            Span::styled(
                card.name.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                card.justification
                    .as_deref()
                    .map(|j| format!("  - {j}"))
                    .unwrap_or_default(),
                Style::default().fg(Color::White),
            ),
        ],
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
    out.push(RenderRow {
        rule: None,
        spans: vec![Span::styled("args:", Style::default().fg(Color::DarkGray))],
        tool_header: None,
    });
    for s in plain_wrap(&card.args, width) {
        out.push(RenderRow {
            rule: None,
            spans: vec![Span::styled(s, Style::default().fg(Color::Magenta))],
            tool_header: None,
        });
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
    let block = Block::default().borders(Borders::ALL).title(" context ");
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
    let filled = (ratio * width as f64).floor() as usize;
    let mut line1 = Vec::new();
    line1.push(Span::styled(
        "#".repeat(filled),
        Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
    ));
    line1.push(Span::styled(
        "-".repeat(width.saturating_sub(filled)),
        Style::default().fg(Color::DarkGray),
    ));

    let mut line2 = vec![Span::styled(
        format!("{pct}% used  "),
        Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
    )];
    line2.push(Span::styled(
        format!("{tokens} / {budget} tokens"),
        Style::default().fg(Color::White),
    ));
    if app.ctx_estimated {
        line2.push(Span::styled(
            " (est.)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        line2.push(Span::styled(" (api)", Style::default().fg(Color::DarkGray)));
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    frame.render_widget(Paragraph::new(Line::from(line1)), rows[0]);
    frame.render_widget(Paragraph::new(Line::from(line2)), rows[1]);
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
        let mut spans = vec![
            Span::styled(
                format!("{} ", plan_prefix(&step.status)),
                Style::default().fg(color),
            ),
            Span::styled(
                format!("{}. ", step.id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(step.goal.clone(), Style::default().fg(text_color)),
        ];
        if let Some(note) = &step.note {
            spans.push(Span::styled(
                format!("  - {note}"),
                Style::default().fg(color),
            ));
        }
        lines.push(Line::from(spans));
        // verification: how this step is proven (keeps steps isolated)
        let verify = step.verification.trim();
        if !verify.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(
                    format!("verify: {verify}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
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
    let w = area.width.min(76);
    let h = 10u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h);
    frame.render_widget(Clear, popup);

    let title = match dialog.prompt {
        UserPrompt::Question { .. } => " question ",
        UserPrompt::Confirm { .. } => " confirm ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Blue));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let (question, options) = match &dialog.prompt {
        UserPrompt::Question { prompt, options } => (prompt.clone(), options.clone()),
        UserPrompt::Confirm { title, diff } => match diff {
            Some(d) if !d.is_empty() => (format!("{title}\n\n{d}"), vec![]),
            _ => (title.clone(), vec![]),
        },
    };

    let mut text = vec![Line::from(question), Line::from("")];
    for (i, o) in options.iter().enumerate() {
        text.push(Line::from(Span::styled(
            format!("  {}. {o}", i + 1),
            Style::default().fg(Color::Cyan),
        )));
    }
    if options.is_empty() {
        text.push(Line::from(Span::styled(
            "(y/n)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Green)),
        Span::raw(dialog.buf.clone()),
        Span::styled("_", Style::default().fg(Color::Green)),
    ]));

    let hint = if matches!(dialog.prompt, UserPrompt::Question { .. }) && options.is_empty() {
        "type + enter: submit    esc: cancel"
    } else if !options.is_empty() {
        "number: pick    esc: cancel"
    } else {
        "y / n    esc: cancel"
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), rows[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
    let _ = app;
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
}
