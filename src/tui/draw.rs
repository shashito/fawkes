//! All drawing of the TUI. It only reads `App`; restyle the screen by changing this file alone.

use super::{App, AppView, Link, Mode};
use crate::{short, text};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListState, Paragraph, Row, Table, TableState};
use serde_json::{Value, json};

/// A countdown under this many seconds is red.
const URGENT_SECS: u64 = 15;

pub fn draw(frame: &mut Frame, app: &App, now: u64) {
    let table_height = (app.apps.len().max(1) + 3).min(10) as u16;
    let [table, detail, middle, footer] = Layout::vertical([
        Constraint::Length(table_height),
        Constraint::Length(5),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    draw_apps(frame, app, now, table);
    match app.current() {
        Some(view) => {
            draw_detail(frame, view, detail);
            let [generations, log] =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .areas(middle);
            draw_generations(frame, view, generations);
            draw_log(frame, view, log);
        }
        None => {
            let waiting = Paragraph::new("waiting for a daemon…").block(Block::bordered());
            frame.render_widget(waiting, detail.union(middle));
        }
    }
    draw_footer(frame, app, footer);
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn state_color(state: &str) -> Color {
    match state {
        "current" | "running" => Color::Green,
        "permanent" => Color::Cyan,
        "tested" => Color::Yellow,
        "rolled_back" | "test_failed" | "failed" => Color::Red,
        _ => Color::Reset,
    }
}

fn countdown_style(left: u64) -> Style {
    if left < URGENT_SECS {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Yellow)
    }
}

/// `ref@commit8`
fn label(g: &Value) -> String {
    format!("{}@{}", text(&g["ref"]), short(&g["commit"]))
}

fn clock(at: &Value) -> String {
    at.as_str()
        .and_then(|s| s.get(11..19))
        .unwrap_or("--:--:--")
        .to_string()
}

fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn duration(secs: f64) -> String {
    let secs = secs as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn signal_name(signal: i64) -> String {
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

fn last_exit(exit: &Value) -> String {
    let how = match (exit["code"].as_i64(), exit["signal"].as_i64()) {
        (Some(code), _) => format!("exit {code}"),
        (_, Some(signal)) => signal_name(signal),
        _ => "unknown".into(),
    };
    let requested = if exit["requested"] == json!(true) {
        " (requested)"
    } else {
        ""
    };
    format!("{how}{requested}")
}

// ------------------------------------------------------------ the app table

fn draw_apps(frame: &mut Frame, app: &App, now: u64, area: Rect) {
    let rows: Vec<Row> = app.apps.iter().map(|view| app_row(view, now)).collect();
    let widths = [
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(24),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(9),
    ];
    let header = Row::new([
        "APP", "STATE", "LIVE", "CONFIRM", "PID", "PORT", "UP", "RESTARTS",
    ])
    .style(dim());
    let mut state =
        TableState::default().with_selected((!rows.is_empty()).then(|| app.current_index()));
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(" fawkes "))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(table, area, &mut state);
}

fn app_row(view: &AppView, now: u64) -> Row<'static> {
    let Some(status) = &view.status else {
        let why = match &view.link {
            Link::Down(_) => "disconnected",
            _ => "connecting…",
        };
        return Row::new([view.name(), why.to_string()]).style(dim());
    };
    let process = &status["process"];
    let state = if matches!(view.link, Link::Down(_)) {
        "disconnected".to_string()
    } else if let Some(step) = status["busy"].as_str() {
        format!("busy: {step}")
    } else {
        text(&process["state"])
    };
    let color = if matches!(view.link, Link::Down(_)) {
        Color::Red
    } else {
        state_color(&text(&process["state"]))
    };
    let live = view
        .generation(&status["current"])
        .map_or("-".to_string(), |g| {
            format!("{} {}", text(&g["id"]), label(g))
        });
    let running = process["state"] == "running";
    let fact = |value: String| if running { value } else { "-".to_string() };
    let uptime = process["uptime"].as_f64().map_or("-".to_string(), |u| {
        duration(u + view.status_at.elapsed().as_secs_f64())
    });
    let confirm = match view.countdown(now) {
        Some(left) => Cell::new(mmss(left)).style(countdown_style(left)),
        None => Cell::new("-").style(dim()),
    };
    Row::new([
        Cell::new(view.name()),
        Cell::new(state).style(Style::new().fg(color)),
        Cell::new(live),
        confirm,
        Cell::new(fact(text(&process["pid"]))),
        Cell::new(fact(text(&process["port"]))),
        Cell::new(fact(uptime)),
        Cell::new(text(&process["restarts"])),
    ])
}

// ------------------------------------------------------------ the selected app

fn draw_detail(frame: &mut Frame, view: &AppView, area: Rect) {
    let mut block = Block::bordered().title(format!(" {} ", view.name()));
    match &view.link {
        Link::Up => {}
        Link::Connecting => {
            block = block.title(Line::from(" connecting… ".yellow()).right_aligned());
        }
        Link::Down(why) => {
            let title = format!(" disconnected, retrying: {why} ");
            block = block.title(Line::from(title.red().bold()).right_aligned());
        }
    }
    let lines = match &view.status {
        Some(status) => detail_lines(view, status),
        None => vec![Line::from(format!("waiting for {}", view.socket.display()))],
    };
    // What a lost connection still shows is the last known picture, so it is dimmed.
    let stale = matches!(view.link, Link::Down(_));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(if stale { paragraph.dim() } else { paragraph }, area);
}

fn detail_lines(view: &AppView, status: &Value) -> Vec<Line<'static>> {
    let generation = |name: &str, id: &Value| -> Line<'static> {
        let mut spans = vec![Span::styled(format!("{name:<8}"), dim())];
        match view.generation(id) {
            Some(g) => {
                let state = text(&g["state"]);
                spans.push(Span::raw(format!("{:<4}{:<18}", text(id), label(g))));
                spans.push(Span::styled(
                    state.clone(),
                    Style::new().fg(state_color(&state)),
                ));
            }
            None => spans.push(Span::styled("none", dim())),
        }
        Line::from(spans)
    };

    let staged = view.history.iter().rev().find(|g| {
        matches!(
            g["state"].as_str(),
            Some("staged" | "tested" | "test_failed")
        )
    });
    let staged_line = match staged {
        Some(g) => {
            let state = text(&g["state"]);
            let result = match (state.as_str(), g["test"].is_null()) {
                ("tested", false) => format!(
                    "tested ✓ {:.1}s on :{}",
                    g["test"]["seconds"].as_f64().unwrap_or(0.0),
                    text(&g["test"]["port"])
                ),
                ("test_failed", false) => format!("test failed ✗ {}", text(&g["test"]["message"])),
                _ => format!("{state}, not tested"),
            };
            Line::from(vec![
                Span::styled("staged  ", dim()),
                Span::raw(format!("{:<4}{:<18}", text(&g["id"]), label(g))),
                Span::styled(result, Style::new().fg(state_color(&state))),
            ])
        }
        None => Line::from(vec![Span::styled("staged  none", dim())]),
    };

    let exit = &status["process"]["last_exit"];
    let exit_line = if exit.is_null() {
        Line::from(Span::styled("last    -", dim()))
    } else {
        Line::from(Span::styled(format!("last    {}", last_exit(exit)), dim()))
    };
    vec![
        generation("perm.", &status["permanent"]),
        staged_line,
        exit_line,
    ]
}

fn draw_generations(frame: &mut Frame, view: &AppView, area: Rect) {
    let items: Vec<Line> = view
        .rows()
        .into_iter()
        .map(|g| {
            let state = text(&g["state"]);
            let mut spans = vec![
                Span::raw(format!("{:<4}", text(&g["id"]))),
                Span::styled(format!("{state:<13}"), Style::new().fg(state_color(&state))),
                Span::raw(label(g)),
            ];
            if let Some(reason) = g["rollback_reason"].as_str() {
                spans.push(Span::styled(format!("  {reason}"), dim()));
            }
            Line::from(spans)
        })
        .collect();
    let mut state =
        ListState::default().with_selected((!items.is_empty()).then(|| view.selected_index()));
    let list = List::new(items)
        .block(Block::bordered().title(" generations "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_log(frame: &mut Frame, view: &AppView, area: Rect) {
    let block = Block::bordered().title(" log ");
    let height = block.inner(area).height as usize;
    let lines: Vec<Line> = view
        .events
        .iter()
        .skip(view.events.len().saturating_sub(height))
        .map(|e| {
            let kind = text(&e["kind"]);
            let style = match kind.as_str() {
                "progress" => dim(),
                "failed" | "error" | "test_failed" | "rollback" | "rolled_back" => {
                    Style::new().fg(Color::Red)
                }
                _ => Style::new(),
            };
            let message = text(&e["message"]);
            let gen_id = e["generation"]
                .as_str()
                .filter(|g| !message.contains(g))
                .map_or(String::new(), |g| format!("{g} "));
            Line::styled(format!("{} {gen_id}{message}", clock(&e["at"])), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match (&app.mode, &app.notice) {
        (Mode::Stage(git_ref), _) => Line::from(vec![
            "stage ref: ".bold(),
            Span::raw(git_ref.clone()),
            "█".into(),
            Span::styled("   Enter stage, Esc cancel", dim()),
        ]),
        (Mode::Ask(action), _) => {
            let on = app
                .current()
                .map_or(String::new(), |a| format!(" on {}", a.name()));
            Line::from(format!("{}{on}? y/N", action.label).yellow().bold())
        }
        (Mode::Normal, Some((message, true))) => Line::from(message.clone().red().bold()),
        (Mode::Normal, Some((message, false))) => Line::from(message.clone().green()),
        (Mode::Normal, None) => {
            let keys = [
                ("tab", "app"),
                ("s", "stage"),
                ("t", "test"),
                ("w", "switch"),
                ("c", "confirm"),
                ("r", "rollback"),
                ("S", "start"),
                ("X", "stop"),
                ("R", "restart"),
                ("q", "quit"),
            ];
            let mut spans = Vec::new();
            for (key, what) in keys {
                spans.push(Span::styled(key, Style::new().add_modifier(Modifier::BOLD)));
                spans.push(Span::styled(format!(" {what} "), dim()));
            }
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line).block(Block::bordered()), area);
}
