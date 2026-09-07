//! Minimal cockpit TUI: transcript + plan checklist + prompt bar + modal
//! dialogs for questions/approvals. Deliberately small; the UX will be
//! redesigned later.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_core::{AgentEvent, AgentSession, MemoryUndo, run_agent};
use comrade_tool::{PlanStatus, SessionControl, ToolContext, UserIo, UserPrompt, UserReply};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
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
// UI state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tag {
    User,
    Thought,
    Tool,
    Ok,
    Err,
    Answer,
    Meta,
}

struct LogLine {
    tag: Tag,
    text: String,
}

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
    lines: Vec<LogLine>,
    input: String,
    dialogs: Vec<Dialog>,
}

impl App {
    fn push(&mut self, tag: Tag, text: impl Into<String>) {
        let text = text.into();
        for l in text.lines() {
            self.lines.push(LogLine {
                tag,
                text: l.to_string(),
            });
        }
        if self.lines.len() > 2000 {
            self.lines.drain(..self.lines.len() - 2000);
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
        self.push(Tag::User, format!("🧑 {prompt}"));
        tokio::spawn(async move {
            let _ = run_agent(&cfg, &client, ctx, &tools, prompt, tx, stop).await;
        });
    }

    fn cancel_run(&mut self) {
        if let Some(stop) = &self.stop {
            stop.cancel();
        }
        self.push(Tag::Meta, "(cancelling…)");
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::RunStart => self.push(Tag::Meta, "— run started —"),
            AgentEvent::RunEnd => {
                self.running = false;
                self.stop = None;
                self.push(Tag::Meta, "— run finished —");
            }
            AgentEvent::User(u) => self.push(Tag::User, format!("🧑 {u}")),
            AgentEvent::Thought(t) => self.push(Tag::Thought, format!("🧠 {t}")),
            AgentEvent::AssistantText(t) => {
                if !t.contains("Tool:") && !t.contains("Thought:") {
                    self.push(Tag::Thought, t);
                }
            }
            AgentEvent::ToolStart { name, args } => {
                self.push(Tag::Tool, format!("🔧 {name} {args}"));
            }
            AgentEvent::ToolResult { name, output, ok } => {
                let tag = if ok { Tag::Ok } else { Tag::Err };
                self.push(tag, format!("   └ {name}: {output}"));
            }
            AgentEvent::FinalAnswer(a) => self.push(Tag::Answer, format!("✅ {a}")),
            AgentEvent::Error(e) => self.push(Tag::Err, format!("❌ {e}")),
            AgentEvent::TitleChanged | AgentEvent::StatusChanged | AgentEvent::PlanChanged => {}
            AgentEvent::PlanFinished(s) => match s {
                Some(s) => self.push(Tag::Meta, format!("— plan finished: {s} —")),
                None => self.push(Tag::Meta, "— plan finished —"),
            },
        }
    }

    /// Resolve the top dialog with an answer and pop it.
    fn answer_top(&mut self, reply: UserReply) {
        if !self.dialogs.is_empty() {
            let text = match &reply {
                UserReply::Answer(a) => format!("↳ answer: {a}"),
                UserReply::Denied => "↳ dismissed".to_string(),
            };
            self.push(Tag::User, text);
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
        lines: Vec::new(),
        input: String::new(),
        dialogs: Vec::new(),
    };

    let mut terminal = ratatui::init();

    // Key events arrive on a background thread.
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

    let res = loop {
        tokio::select! {
            ev = kev_rx.recv() => {
                match ev {
                    Some(ev) => if handle_key(&mut app, ev) { break Ok(()); },
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
                        app.push(Tag::Meta, "— waiting for your input —");
                    }
                    None => break Err(anyhow::anyhow!("ask channel closed")),
                }
            }
        }
        let _ = terminal.draw(|f| draw(&app, f));
    };

    ratatui::restore();
    res
}

/// Returns true when the app should quit.
fn handle_key(app: &mut App, ev: Event) -> bool {
    let Event::Key(key) = ev else { return false };
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }

    // Dialog mode takes over all input.
    if !app.dialogs.is_empty() {
        let is_confirm = matches!(
            app.dialogs.first().unwrap().prompt,
            UserPrompt::Confirm { .. }
        );
        match key.code {
            KeyCode::Esc => {
                app.answer_top(UserReply::Denied);
            }
            KeyCode::Char('y') if is_confirm => app.answer_top(UserReply::Answer("yes".into())),
            KeyCode::Char('n') if is_confirm => app.answer_top(UserReply::Answer("no".into())),
            KeyCode::Enter => app.answer_from_buf(),
            KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                let handled = app
                    .dialogs
                    .first()
                    .is_some_and(|d| matches!(&d.prompt, UserPrompt::Question { options, .. } if n >= 1 && n <= options.len()));
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
        return false;
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
        KeyCode::Char(c) => app.input.push(c),
        KeyCode::Backspace => {
            app.input.pop();
        }
        _ => {}
    }
    false
}

// ---------------------------------------------------------------------------
// drawing
// ---------------------------------------------------------------------------

fn status_style(tag: Tag) -> Style {
    match tag {
        Tag::User => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        Tag::Thought => Style::default().fg(Color::White),
        Tag::Tool => Style::default().fg(Color::Magenta),
        Tag::Ok => Style::default().fg(Color::Green),
        Tag::Err => Style::default().fg(Color::Red),
        Tag::Answer => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        Tag::Meta => Style::default().fg(Color::DarkGray),
    }
}

fn plan_prefix(s: &PlanStatus) -> &'static str {
    match s {
        PlanStatus::Pending => "○",
        PlanStatus::InProgress => "◐",
        PlanStatus::Done => "●",
        PlanStatus::Blocked => "⊗",
    }
}

fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let run_state = if app.running { "RUNNING" } else { "IDLE" };
    let undo_count = app.undo.entry_count();

    let header = Line::from(vec![
        Span::styled(
            format!(" comrade · {} ", app.session.title()),
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
    let footer = Line::from(vec![
        Span::styled(app.session.status(), Style::default().fg(Color::DarkGray)),
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
            "enter:run esc:cancel ctrl-c:quit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), rows[0]);
    frame.render_widget(Paragraph::new(footer).alignment(Alignment::Right), rows[2]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Percentage(34)])
        .split(rows[1]);
    draw_log(app, frame, cols[0]);
    draw_plan(app, frame, cols[1]);

    if let Some(d) = app.dialogs.first() {
        draw_dialog(app, d, frame);
    }
}

fn draw_log(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" run ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines: Vec<Line> = app
        .lines
        .iter()
        .map(|l| Line::from(Span::styled(l.text.clone(), status_style(l.tag))))
        .collect();
    let scroll = lines.len().saturating_sub(inner.height as usize) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

fn draw_plan(app: &App, frame: &mut Frame, area: Rect) {
    let finished = app.session.finished_summary().is_some();
    let block = Block::default().borders(Borders::ALL).title(if finished {
        " plan ✓ "
    } else {
        " plan "
    });
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
            Span::styled(step.description.clone(), Style::default().fg(text_color)),
        ];
        if let Some(note) = &step.note {
            spans.push(Span::styled(
                format!("  — {note}"),
                Style::default().fg(color),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);
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
        Span::styled("▎", Style::default().fg(Color::Green)),
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
