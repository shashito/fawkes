//! All drawing of the TUI. It only reads `App`; restyle the screen by changing this file alone.
//!
//! No boxes around panes: a one-line header, the apps table, one-line title rules (`─ name ───`), the selected app's
//! detail, its generations and its events, and a one-line footer. Dialogs and the help overlay are the only boxes.

use super::{App, AppView, Focus, Link, Mode};
use crate::or_dash;
use crate::protocol::{Event, EventKind, GenState, Generation, LastExit, ProcessState};
use crate::state::parse_time;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState};

/// A countdown under this many seconds is red.
const URGENT_SECS: u64 = 15;
/// Columns are this far apart.
const GAP: u16 = 2;
/// The most rows the apps table and the generations list take before they scroll.
const MAX_APP_ROWS: usize = 6;
const MAX_GEN_ROWS: usize = 6;
/// The events pane keeps at least its rule and this many lines while the lists above it have room.
const MIN_EVENT_ROWS: u16 = 3;

/// A length as terminal rows or columns, which no terminal reaches the `u16` limit of.
fn clamp_u16(len: usize) -> u16 {
    u16::try_from(len).unwrap_or(u16::MAX)
}

pub fn draw(frame: &mut Frame, app: &App, now: u64) {
    let area = frame.area();
    let mut left = area.height;
    let mut take = |want: u16| {
        let got = want.min(left);
        left -= got;
        got
    };
    let footer = take(1);
    let header = take(1);
    let (mut table, mut detail, mut gens, mut events) = (0, 0, 0, 0);
    let view = app.current();
    let lines = view.map(|v| detail_lines(v, now)).unwrap_or_default();
    if app.zoom {
        events = take(u16::MAX);
    } else if let Some(view) = view {
        table = take(clamp_u16(app.apps.len().min(MAX_APP_ROWS)) + 1);
        detail = take(1 + clamp_u16(lines.len()));
        let reserve = take(MIN_EVENT_ROWS);
        gens = take(clamp_u16(view.history.len().min(MAX_GEN_ROWS)) + 1);
        events = reserve + take(u16::MAX);
    } else {
        table = take(u16::MAX);
    }
    let [header, table, detail, gens, events, footer] = Layout::vertical([
        Constraint::Length(header),
        Constraint::Length(table),
        Constraint::Length(detail),
        Constraint::Length(gens),
        Constraint::Length(events),
        Constraint::Length(footer),
    ])
    .areas(area);
    draw_header(frame, app, now, header);
    if app.zoom {
        if let Some(view) = view {
            draw_events(frame, app, view, events);
        }
    } else if let Some(view) = view {
        draw_apps(frame, app, now, table);
        draw_detail(frame, view, lines, detail);
        draw_generations(frame, app, view, now, gens);
        draw_events(frame, app, view, events);
    } else {
        let waiting = Paragraph::new(" waiting for a daemon…").style(dim());
        frame.render_widget(waiting, table);
    }
    draw_footer(frame, app, footer);
    match &app.mode {
        Mode::Ask(_, question) => draw_dialog(frame, question),
        Mode::Help => draw_help(frame),
        _ => {}
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn state_color(state: GenState) -> Color {
    match state {
        GenState::Current | GenState::Tested => Color::Green,
        GenState::Permanent => Color::Cyan,
        GenState::RolledBack | GenState::TestFailed => Color::Red,
        GenState::Staging | GenState::Staged | GenState::Retired => Color::Reset,
    }
}

fn countdown_style(left: u64) -> Style {
    if left < URGENT_SECS {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Yellow)
    }
}

/// `HH:MM:SS` of an RFC 3339 time, in the local time zone: people read the screen, not the log files.
fn clock(at: &str) -> String {
    parse_time(at).map_or_else(|| "--:--:--".to_string(), local_clock)
}

pub fn local_clock(secs: u64) -> String {
    match i64::try_from(secs).map(Timestamp::from_second) {
        Ok(Ok(t)) => t
            .to_zoned(TimeZone::system())
            .strftime("%H:%M:%S")
            .to_string(),
        _ => "--:--:--".into(),
    }
}

fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a float-to-integer cast saturates, so a negative or absurd uptime shows as 0s or the largest count"
)]
fn duration(secs: f64) -> String {
    let secs = secs as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..86400 => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d", secs / 86400),
    }
}

/// How long ago, in the largest unit: `45s`, `3m`, `5h`, `2d`.
fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        3 => "SIGQUIT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        11 => "SIGSEGV".into(),
        13 => "SIGPIPE".into(),
        15 => "SIGTERM".into(),
        n => format!("signal {n}"),
    }
}

fn last_exit(exit: &LastExit) -> String {
    let how = match (exit.code, exit.signal) {
        (Some(code), _) => format!("exit {code}"),
        (_, Some(signal)) => signal_name(signal),
        _ => "unknown".into(),
    };
    let requested = if exit.requested { " (requested)" } else { "" };
    format!("{how}{requested}")
}

/// Terminal columns `text` takes, the measure ratatui itself lays out with: `chars().count()` is wrong for wide and
/// zero-width characters.
fn width_of(text: &str) -> u16 {
    clamp_u16(Span::raw(text).width())
}

/// `text` padded with spaces to `width` terminal columns.
fn pad_to(text: &str, width: u16) -> String {
    let fill = width.saturating_sub(width_of(text)) as usize;
    format!("{text}{}", " ".repeat(fill))
}

/// `─ title  extras ─────────`, one line over the full width.
fn rule(title: &str, extras: Vec<Span<'static>>, width: u16, focused: bool) -> Line<'static> {
    let title_style = if focused {
        bold().fg(Color::Cyan)
    } else {
        bold()
    };
    let mut spans = vec![
        Span::styled("─ ", dim()),
        Span::styled(title.to_string(), title_style),
    ];
    for extra in extras {
        spans.push(Span::raw("  "));
        spans.push(extra);
    }
    spans.push(Span::raw(" "));
    let used: u16 = spans.iter().map(|s| width_of(&s.content)).sum();
    spans.push(Span::styled(
        "─".repeat(width.saturating_sub(used) as usize),
        dim(),
    ));
    Line::from(spans)
}

// ------------------------------------------------------------ the header

fn draw_header(frame: &mut Frame, app: &App, now: u64, area: Rect) {
    let count = app.apps.len();
    let mut spans = vec![
        Span::styled(" fawkes", bold()),
        Span::styled(
            format!("  {count} app{}", if count == 1 { "" } else { "s" }),
            dim(),
        ),
    ];
    let down = app
        .apps
        .iter()
        .filter(|a| matches!(a.link, Link::Down(_)))
        .count();
    if down > 0 {
        spans.push(Span::styled(
            format!("  {down} disconnected"),
            Style::new().fg(Color::Red).bold(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let clock = format!("{} ", local_clock(now));
    frame.render_widget(
        Paragraph::new(Span::styled(clock, dim())).alignment(Alignment::Right),
        area,
    );
}

// ------------------------------------------------------------ tables

/// One column of a table: hidden, lowest `rank` first... highest rank is dropped first; rank 0 is never dropped.
struct Col {
    head: &'static str,
    width: u16,
    rank: u8,
    /// Takes the width that is left over.
    fill: bool,
}

impl Col {
    fn new<'a>(head: &'static str, cells: impl Iterator<Item = &'a str>, rank: u8) -> Col {
        let widest = cells.map(width_of).max().unwrap_or(0);
        Col {
            head,
            width: widest.max(width_of(head)),
            rank,
            fill: false,
        }
    }

    fn fill(mut self) -> Col {
        self.fill = true;
        self.width = self.width.min(12);
        self
    }
}

/// Which columns fit in `avail` columns: the highest-ranked ones go first.
fn visible(cols: &[Col], avail: u16) -> Vec<bool> {
    let mut shown = vec![true; cols.len()];
    loop {
        let needed: u16 = cols
            .iter()
            .zip(&shown)
            .filter(|(_, s)| **s)
            .map(|(c, _)| c.width + GAP)
            .sum();
        let drop = (0..cols.len())
            .filter(|&i| shown[i] && cols[i].rank > 0)
            .max_by_key(|&i| cols[i].rank);
        match drop {
            Some(i) if needed > avail + GAP => shown[i] = false,
            _ => return shown,
        }
    }
}

struct Body {
    cells: Vec<Cell<'static>>,
    style: Style,
}

/// A table with a header row and a full-width selection bar marked at both edges.
fn draw_table(
    frame: &mut Frame,
    area: Rect,
    cols: &[Col],
    rows: Vec<Body>,
    selected: Option<usize>,
    focused: bool,
) {
    if area.height == 0 || area.width < 3 {
        return;
    }
    // The bar's markers take a column at each edge.
    let shown = visible(cols, area.width.saturating_sub(2));
    let pick = |cells: Vec<Cell<'static>>| -> Vec<Cell<'static>> {
        cells
            .into_iter()
            .zip(&shown)
            .filter(|(_, s)| **s)
            .map(|(c, _)| c)
            .collect()
    };
    let widths: Vec<Constraint> = cols
        .iter()
        .zip(&shown)
        .filter(|(_, s)| **s)
        .map(|(c, _)| {
            if c.fill {
                Constraint::Fill(1)
            } else {
                Constraint::Length(c.width)
            }
        })
        .collect();
    let head_style = if focused {
        bold().fg(Color::Cyan)
    } else {
        dim()
    };
    let header = Row::new(pick(cols.iter().map(|c| Cell::new(c.head)).collect())).style(head_style);
    let rows: Vec<Row> = rows
        .into_iter()
        .map(|b| Row::new(pick(b.cells)).style(b.style))
        .collect();
    let bar = if focused {
        Modifier::REVERSED
    } else {
        Modifier::REVERSED | Modifier::DIM
    };
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(GAP)
        .highlight_symbol(" ")
        .highlight_spacing(HighlightSpacing::Always)
        .row_highlight_style(Style::new().add_modifier(bar));
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
    let Some(selected) = selected else { return };
    let y = area.y + 1 + clamp_u16(selected.saturating_sub(state.offset()));
    if y < area.bottom() {
        let edge = Style::new().fg(Color::Cyan);
        let buffer = frame.buffer_mut();
        buffer[(area.x, y)].set_symbol("▌").set_style(edge);
        buffer[(area.right() - 1, y)]
            .set_symbol("▐")
            .set_style(edge);
    }
}

// ------------------------------------------------------------ the apps table

/// The state cell: symbol and word, and its colour.
fn app_state(view: &AppView) -> (String, Color) {
    if matches!(view.link, Link::Down(_)) {
        return ("◌ disconnected".into(), Color::Red);
    }
    let Some(status) = &view.status else {
        return ("◌ connecting".into(), Color::Gray);
    };
    if let Some(step) = &status.busy {
        return (format!("⟳ {step}"), Color::Yellow);
    }
    match status.process.state {
        ProcessState::Running => ("● running".into(), Color::Green),
        ProcessState::Stopped => ("○ stopped".into(), Color::Gray),
        ProcessState::Failed => ("✘ failed".into(), Color::Red),
    }
}

/// The uptime the daemon reported, plus the time since it did, so the screen keeps counting between updates.
fn uptime(view: &AppView, reported: Option<f64>) -> String {
    reported.map_or("-".to_string(), |u| {
        duration(u + view.status_at.elapsed().as_secs_f64())
    })
}

/// The newest generation that waits to be switched to.
fn staged(view: &AppView) -> Option<&Generation> {
    view.history.iter().rev().find(|g| {
        matches!(
            g.state,
            GenState::Staged | GenState::Tested | GenState::TestFailed
        )
    })
}

fn draw_apps(frame: &mut Frame, app: &App, now: u64, area: Rect) {
    struct Row6 {
        cells: [String; 10],
        confirm: Option<u64>,
        state_color: Color,
        style: Style,
    }
    let rows: Vec<Row6> = app
        .apps
        .iter()
        .map(|view| {
            let (state, state_color) = app_state(view);
            let status = view.status.as_ref();
            let process = status.map(|s| &s.process);
            let running = process.is_some_and(|p| p.state == ProcessState::Running);
            let fact = |value: String| if running { value } else { "-".to_string() };
            let confirm = view.countdown(now);
            let style = if matches!(view.link, Link::Down(_)) {
                dim()
            } else if process.is_some_and(|p| p.state == ProcessState::Failed) {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Row6 {
                cells: [
                    state,
                    view.name(),
                    or_dash(status.and_then(|s| s.current.as_ref())),
                    or_dash(status.and_then(|s| s.permanent.as_ref())),
                    or_dash(staged(view).map(|g| &g.id)),
                    confirm.map_or("-".into(), |left| format!("⏱ {}", mmss(left))),
                    fact(or_dash(process.and_then(|p| p.pid))),
                    fact(or_dash(process.map(|p| p.port))),
                    fact(uptime(view, process.and_then(|p| p.uptime))),
                    or_dash(process.map(|p| p.restarts)),
                ],
                confirm,
                state_color,
                style,
            }
        })
        .collect();
    // Drop order when narrow: PERM, RST, STAGED, then UP, PID, PORT, LIVE.
    let heads = [
        ("ST", 0),
        ("APP", 0),
        ("LIVE", 3),
        ("PERM", 9),
        ("STAGED", 7),
        ("CONFIRM", 0),
        ("PID", 5),
        ("PORT", 4),
        ("UP", 6),
        ("RST", 8),
    ];
    let cols: Vec<Col> = heads
        .iter()
        .enumerate()
        .map(|(i, (head, rank))| {
            let mut col = Col::new(head, rows.iter().map(|r| r.cells[i].as_str()), *rank);
            if *head == "APP" {
                col.width = col.width.min(24);
            }
            col
        })
        .collect();
    let bodies: Vec<Body> = rows
        .into_iter()
        .map(|r| {
            let cells = r.cells.into_iter().enumerate().map(|(i, text)| {
                let cell = Cell::new(text);
                match i {
                    0 => cell.style(Style::new().fg(r.state_color)),
                    5 => match r.confirm {
                        Some(left) => cell.style(countdown_style(left)),
                        None => cell.style(dim()),
                    },
                    _ => cell,
                }
            });
            Body {
                cells: cells.collect(),
                style: r.style,
            }
        })
        .collect();
    let selected = app.current_index();
    draw_table(
        frame,
        area,
        &cols,
        bodies,
        selected,
        app.focus == Focus::Apps,
    );
}

// ------------------------------------------------------------ the selected app

fn draw_detail(frame: &mut Frame, view: &AppView, lines: Vec<Line<'static>>, area: Rect) {
    if area.height == 0 {
        return;
    }
    let extras = match &view.link {
        Link::Up => vec![],
        Link::Connecting => vec!["connecting…".yellow()],
        Link::Down(why) => vec![format!("disconnected, retrying: {why}").red().bold()],
    };
    let title = rule(&view.name(), extras, area.width, false);
    let [title_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(title), title_area);
    // What a lost connection still shows is the last known picture, so it is dimmed.
    let paragraph = Paragraph::new(lines);
    let stale = matches!(view.link, Link::Down(_));
    frame.render_widget(if stale { paragraph.dim() } else { paragraph }, body);
}

fn detail_lines(view: &AppView, now: u64) -> Vec<Line<'static>> {
    let Some(status) = &view.status else {
        return vec![Line::from(Span::styled(
            format!(" waiting for {}", view.socket.display()),
            dim(),
        ))];
    };
    let process = &status.process;
    let (state, color) = app_state(view);
    let mut first = vec![Span::styled(
        format!(" {state}"),
        Style::new().fg(color).bold(),
    )];
    if let Some(g) = process
        .generation
        .as_deref()
        .and_then(|id| view.generation(id))
    {
        first.push(Span::raw(format!("  {} {}", g.id, g.label())));
    }
    if process.state == ProcessState::Running {
        first.push(Span::raw(format!(
            "  pid {}  :{}  up {}  restarts {}",
            or_dash(process.pid),
            process.port,
            uptime(view, process.uptime),
            process.restarts
        )));
    } else {
        first.push(Span::raw(format!("  restarts {}", process.restarts)));
    }
    if let Some(at) = process.restart_at.as_deref().and_then(parse_time) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a countdown of seconds is far below 2^52"
        )]
        let remaining = at.saturating_sub(now) as f64;
        first.push(Span::styled(
            format!("  restarting in {}", duration(remaining)),
            Style::new().fg(Color::Yellow),
        ));
    }
    let mut second = vec![Span::styled("   last exit ", dim())];
    second.push(Span::styled(
        process.last_exit.as_ref().map_or("-".into(), last_exit),
        dim(),
    ));
    let health = view.events.iter().rev().find(|e| {
        matches!(e.kind, EventKind::Healthy | EventKind::Unhealthy)
            && e.generation == process.generation
    });
    if let Some(event) = health {
        let (symbol, color) = if event.kind == EventKind::Healthy {
            ("✔", Color::Green)
        } else {
            ("✖", Color::Red)
        };
        second.push(Span::styled(" · health ", dim()));
        second.push(Span::styled(
            format!("{symbol} {}", clock(&event.at)),
            Style::new().fg(color),
        ));
    }
    let mut lines = vec![Line::from(first), Line::from(second)];
    if let Some(left) = view.countdown(now)
        && let Some(pending) = &status.pending
    {
        let rollback = match &pending.previous {
            Some(previous) => format!("rolls back to {previous} (code + storage)"),
            None => "is rolled back".to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" ⏱ confirm {} within ", pending.generation),
                countdown_style(left),
            ),
            Span::styled(mmss(left), countdown_style(left).bold()),
            Span::styled(format!(", or it {rollback}"), countdown_style(left)),
        ]));
    }
    lines
}

/// Seconds since the generation last changed state, from the newest time it recorded.
fn generation_age(g: &Generation, now: u64) -> String {
    [
        g.rolled_back_at.as_deref(),
        g.confirmed_at.as_deref(),
        g.switched_at.as_deref(),
        Some(g.staged_at.as_str()),
    ]
    .into_iter()
    .find_map(|at| at.and_then(parse_time))
    .map_or("-".into(), |at| age(now.saturating_sub(at)))
}

/// The INFO column and the colour it takes, for one generation.
fn generation_info(view: &AppView, g: &Generation) -> (String, Color) {
    let status = view.status.as_ref();
    let pending = status.and_then(|s| s.pending.as_ref());
    let test = g.test.as_ref();
    match g.state {
        GenState::Tested => (
            format!(
                "✔ test {:.1}s on :{}",
                test.map_or(0.0, |t| t.seconds),
                or_dash(test.map(|t| t.port))
            ),
            Color::Green,
        ),
        GenState::TestFailed => (
            format!(
                "✖ test failed: {}",
                or_dash(test.map(|t| t.message.as_str()))
            ),
            Color::Red,
        ),
        GenState::Staging => ("checking out and installing".into(), Color::Yellow),
        GenState::Staged => ("not tested".into(), Color::Reset),
        GenState::Current if pending.is_some_and(|p| p.generation == g.id) => {
            ("live · awaiting confirm".into(), Color::Yellow)
        }
        GenState::Current => ("live".into(), Color::Green),
        GenState::Permanent if pending.is_some_and(|p| p.previous.as_ref() == Some(&g.id)) => {
            ("rollback target".into(), Color::Cyan)
        }
        GenState::Permanent if status.is_some_and(|s| s.current.as_ref() == Some(&g.id)) => {
            ("live".into(), Color::Green)
        }
        GenState::RolledBack => (
            format!(
                "✖ {}",
                g.rollback_reason.as_deref().unwrap_or("rolled back")
            ),
            Color::Red,
        ),
        GenState::Permanent | GenState::Retired => (String::new(), Color::Reset),
    }
}

fn draw_generations(frame: &mut Frame, app: &App, view: &AppView, now: u64, area: Rect) {
    let rows: Vec<(&Generation, [String; 5], Color)> = view
        .rows()
        .into_iter()
        .map(|g| {
            let (info, info_color) = generation_info(view, g);
            let cells = [
                g.id.clone(),
                g.label(),
                g.state.as_str().to_string(),
                generation_age(g, now),
                info,
            ];
            (g, cells, info_color)
        })
        .collect();
    let heads = [
        ("GEN", 0),
        ("REF", 0),
        ("STATE", 0),
        ("AGE", 2),
        ("INFO", 0),
    ];
    let cols: Vec<Col> = heads
        .iter()
        .enumerate()
        .map(|(i, (head, rank))| {
            let col = Col::new(head, rows.iter().map(|r| r.1[i].as_str()), *rank);
            if *head == "INFO" { col.fill() } else { col }
        })
        .collect();
    let bodies: Vec<Body> = rows
        .into_iter()
        .map(|(g, cells, info_color)| {
            let red = matches!(g.state, GenState::RolledBack | GenState::TestFailed);
            let cells = cells.into_iter().enumerate().map(|(i, text)| {
                let cell = Cell::new(text);
                match i {
                    2 => cell.style(Style::new().fg(state_color(g.state))),
                    3 => cell.style(dim()),
                    4 => cell.style(Style::new().fg(info_color)),
                    _ => cell,
                }
            });
            Body {
                cells: cells.collect(),
                style: if red {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new()
                },
            }
        })
        .collect();
    let selected = (!bodies.is_empty()).then(|| view.selected_index());
    draw_table(
        frame,
        area,
        &cols,
        bodies,
        selected,
        app.focus == Focus::Generations,
    );
}

// ------------------------------------------------------------ events

fn event_color(kind: EventKind, message: &str) -> Color {
    match kind {
        EventKind::Switched => Color::Yellow,
        EventKind::RolledBack
        | EventKind::Failed
        | EventKind::Error
        | EventKind::TestFailed
        | EventKind::Unhealthy => Color::Red,
        // A crash; an exit Fawkes asked for is routine.
        EventKind::Exited if !message.contains("(requested)") => Color::Red,
        EventKind::Healthy | EventKind::Confirmed | EventKind::Tested => Color::Green,
        EventKind::Progress => Color::DarkGray,
        _ => Color::Reset,
    }
}

fn draw_events(frame: &mut Frame, app: &App, view: &AppView, area: Rect) {
    if area.height == 0 {
        return;
    }
    let mut extras = Vec::new();
    if !app.filter.is_empty() || matches!(app.mode, Mode::Search) {
        extras.push(Span::styled(
            format!("/{}", app.filter),
            Style::new().fg(Color::Cyan),
        ));
    }
    if !app.follow {
        extras.push(Span::styled(
            "paused",
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    if app.zoom {
        extras.push(Span::styled(format!("{} · z closes", view.name()), dim()));
    }
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(rule("events", extras, area.width, false)),
        title,
    );

    let shown = if app.follow {
        view.events.len()
    } else {
        view.frozen.unwrap_or(view.events.len())
    };
    let needle = app.filter.to_lowercase();
    // The newest that fit, found from the end so a long backlog is not filtered whole on every frame.
    let mut events: Vec<&Event> = view
        .events
        .iter()
        .take(shown)
        .rev()
        .filter(|e| needle.is_empty() || event_text(e).to_lowercase().contains(&needle))
        .take(body.height as usize)
        .collect();
    events.reverse();
    let kind_width = events
        .iter()
        .map(|e| width_of(e.kind.as_str()))
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = events
        .iter()
        .map(|e| {
            let message = e.message.clone();
            let color = event_color(e.kind, &message);
            let generation = e.generation.as_deref().unwrap_or("");
            Line::from(vec![
                Span::styled(format!(" {} ", clock(&e.at)), dim()),
                Span::styled(
                    format!("{} ", pad_to(e.kind.as_str(), kind_width)),
                    Style::new().fg(color),
                ),
                Span::styled(format!("{generation:<3} "), dim()),
                Span::styled(
                    message,
                    if color == Color::Reset {
                        Style::new()
                    } else {
                        Style::new().fg(color)
                    },
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body);
}

/// What `/` searches in one event.
fn event_text(e: &Event) -> String {
    format!(
        "{} {} {} {}",
        clock(&e.at),
        e.kind.as_str(),
        e.generation.as_deref().unwrap_or(""),
        e.message
    )
}

// ------------------------------------------------------------ footer and overlays

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let prompt = match &app.mode {
        Mode::Stage(git_ref) => Some(Line::from(vec![
            " stage ref: ".bold(),
            Span::raw(git_ref.clone()),
            "█".into(),
            Span::styled("   Enter stage · Esc cancel", dim()),
        ])),
        Mode::Search => Some(Line::from(vec![
            " /".cyan().bold(),
            Span::raw(app.filter.clone()),
            "█".into(),
            Span::styled("   Enter keep · Esc clear", dim()),
        ])),
        _ => None,
    };
    if let Some(prompt) = prompt {
        frame.render_widget(Paragraph::new(prompt), area);
        return;
    }
    // The last word on an action sits at the right edge; the key hints take the room that is left.
    let notice = app.notice.as_ref().map(|(message, error)| {
        let style = if *error {
            Style::new().fg(Color::Red).bold()
        } else {
            Style::new().fg(Color::Green)
        };
        Span::styled(format!("{message} "), style)
    });
    let room = area
        .width
        .saturating_sub(notice.as_ref().map_or(0, |n| width_of(&n.content) + 1));
    frame.render_widget(Paragraph::new(keys_line(app, room)), area);
    if let Some(notice) = notice {
        frame.render_widget(Paragraph::new(notice).alignment(Alignment::Right), area);
    }
}

/// The key hints. A switch awaiting confirm puts `c confirm` first, highlighted. Whole items drop from the end when
/// the terminal is narrow; a label is never cut.
fn keys_line(app: &App, width: u16) -> Line<'static> {
    let pending = app
        .current()
        .and_then(|a| a.status.as_ref())
        .is_some_and(|s| s.pending.is_some());
    let mut items = vec![
        ("?", "help"),
        ("c", "confirm"),
        ("b", "rollback"),
        ("n", "stage"),
        ("t", "test"),
        ("w", "switch"),
        ("s/x/r", "start/stop/restart"),
    ];
    if pending {
        let confirm = items.remove(1);
        items.insert(0, confirm);
    }
    // Items are `key label`, two spaces apart when all of them fit that way, else one. The highlighted first item
    // has a space of padding on each side.
    let size = |(key, what): &(&str, &str)| width_of(key) + width_of(what) + 1;
    let all: u16 = 1 + items.iter().map(size).sum::<u16>() + 2 * (clamp_u16(items.len()) - 1);
    let gap = if all <= width { "  " } else { " " };
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, item) in items.iter().enumerate() {
        let need = size(item) + if i > 0 { clamp_u16(gap.len()) } else { 0 };
        if used + need > width {
            break;
        }
        used += need;
        if i > 0 {
            spans.push(Span::raw(gap));
        }
        let (key, what) = item;
        if pending && i == 0 {
            let style = Style::new().fg(Color::Black).bg(Color::Yellow).bold();
            spans.push(Span::styled(format!("{key} {what}"), style));
        } else {
            spans.push(Span::styled(key.to_string(), bold()));
            spans.push(Span::styled(format!(" {what}"), dim()));
        }
    }
    Line::from(spans)
}

/// A box of `width` x `height` in the middle of the screen, cut to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (w, h) = (width.min(area.width), height.min(area.height));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn draw_dialog(frame: &mut Frame, question: &str) {
    let text = format!("{question} y/N");
    let area = centered(frame.area(), width_of(&text) + 4, 3);
    let block = Block::bordered().border_style(Style::new().fg(Color::Yellow));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Span::styled(text, bold()))
            .alignment(Alignment::Center)
            .block(block),
        area,
    );
}

fn draw_help(frame: &mut Frame) {
    let keys = [
        ("↑/↓ j/k", "select an app (a generation when it has focus)"),
        ("Tab", "move focus: apps ⇄ generations"),
        ("s", "start the app"),
        ("x", "stop the app (asks)"),
        ("r", "restart the app (asks)"),
        ("n", "stage a ref (type it, Enter)"),
        ("t", "test the selected generation"),
        ("w", "switch to the selected generation (asks)"),
        ("c", "confirm the pending generation"),
        ("b", "roll back the pending generation (asks)"),
        ("p", "prune old generations beyond `keep` (asks)"),
        ("f", "pause / follow the events pane"),
        ("z", "zoom the events pane"),
        ("/", "filter events (Esc clears)"),
        ("?", "this help"),
        ("Esc", "close a prompt, dialog, help or filter"),
        ("q Ctrl-C", "quit (the apps keep running)"),
    ];
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<9}"), bold().fg(Color::Cyan)),
                Span::raw(*what),
            ])
        })
        .collect();
    let area = centered(frame.area(), 62, clamp_u16(keys.len()) + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys ")),
        area,
    );
}
