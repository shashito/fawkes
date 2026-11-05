//! `fawkes tui`: a full-screen client of the daemon sockets, one screen for every supervised app.
//!
//! This file is the state, action and key layer and the event loop. Each app has a reader thread holding one
//! `subscribe` connection; every action is an ordinary one-shot request on its own thread, so the screen never
//! blocks. Those threads live in `net.rs`. All drawing lives in `draw.rs`, which only reads `App`.

mod draw;
mod net;

use self::net::{Active, reader, request, scan};
use crate::protocol::{self, Generation, Line, Request, Status};
use crate::state::now_secs;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(250);
const MAX_EVENTS: usize = 500;

/// Which daemons the screen talks to.
pub enum Target {
    /// Exactly one daemon, named by `--socket`, `--manifest` or `FAWKES_SOCKET`.
    One(PathBuf),
    /// Every daemon that linked its socket as `<dir>/<name>.sock`.
    Discover(PathBuf),
}

/// Everything the UI thread hears about. `String`s named `app` are the app's key (see `AppView::key`).
enum Msg {
    Key(KeyEvent),
    /// A daemon answered: the app exists from now on. The flag says the connection resumed after the events the
    /// screen already has.
    Up(String, PathBuf, bool),
    /// A line of the `subscribe` stream: `status`, `history` or `event`.
    Line(String, Line),
    /// The connection dropped; it will be retried.
    Down(String, String),
    /// A discovered app stopped answering; it leaves the list until it answers again.
    Gone(String),
    /// An action finished: what it was, and its error message if it failed.
    Done(String, Result<(), String>),
}

#[derive(PartialEq)]
enum Link {
    Connecting,
    Up,
    Down(String),
}

/// What the user is in the middle of typing or answering.
enum Mode {
    Normal,
    /// The ref being typed for `n`.
    Stage(String),
    /// A centered `y/N` dialog with its question; `y` sends the request.
    Ask(Action, String),
    /// Typing the event filter (`App::filter`).
    Search,
    /// The help overlay.
    Help,
}

/// Which list the arrow keys move, and `t`/`w` read from.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Apps,
    Generations,
}

/// One request to one daemon.
#[derive(Debug, PartialEq, Clone)]
struct Action {
    label: String,
    socket: PathBuf,
    request: Request,
}

/// What a key asks the event loop to do.
#[derive(Debug, PartialEq)]
enum Cmd {
    Quit,
    Send(Action),
}

/// What one daemon has told us.
struct AppView {
    /// The link's name (`<name>.sock`), or the socket path for a single target.
    key: String,
    socket: PathBuf,
    link: Link,
    status: Option<Status>,
    status_at: Instant,
    /// Oldest first, as the daemon sends it.
    history: Vec<Generation>,
    events: VecDeque<protocol::Event>,
    /// While the events pane is paused: how many events it shows.
    frozen: Option<usize>,
    /// The chosen generation; the newest while nothing was chosen.
    selected: Option<String>,
}

impl AppView {
    fn new(key: String, socket: PathBuf) -> AppView {
        AppView {
            key,
            socket,
            link: Link::Connecting,
            status: None,
            status_at: Instant::now(),
            history: Vec::new(),
            events: VecDeque::new(),
            frozen: None,
            selected: None,
        }
    }

    fn name(&self) -> String {
        self.status
            .as_ref()
            .map_or(self.key.as_str(), |s| s.name.as_str())
            .to_string()
    }

    fn push_event(&mut self, event: protocol::Event) {
        self.events.push_back(event);
        if self.events.len() > MAX_EVENTS {
            self.events.pop_front();
            self.frozen = self.frozen.map(|n| n.saturating_sub(1));
        }
    }

    fn line(&mut self, line: Line) {
        match line {
            Line::Status { status } => {
                self.status = Some(*status);
                self.status_at = Instant::now();
            }
            Line::History { history } => self.history = history.generations,
            Line::Event { event } => self.push_event(event),
            Line::Done { .. } | Line::Failed { .. } => {}
        }
    }

    /// The history, newest first: the order of the list.
    fn rows(&self) -> Vec<&Generation> {
        self.history.iter().rev().collect()
    }

    fn selected_index(&self) -> usize {
        let rows = self.rows();
        self.selected
            .as_ref()
            .and_then(|id| rows.iter().position(|g| g.id == *id))
            .unwrap_or(0)
    }

    fn selected_id(&self) -> Option<String> {
        self.rows().get(self.selected_index()).map(|g| g.id.clone())
    }

    fn select(&mut self, delta: isize) {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        let index = self.selected_index().saturating_add_signed(delta).min(last);
        self.selected = rows.get(index).map(|g| g.id.clone());
    }

    fn generation(&self, id: &str) -> Option<&Generation> {
        self.history.iter().find(|g| g.id == id)
    }

    /// Seconds until the pending switch is rolled back, if one awaits confirm.
    fn countdown(&self, now: u64) -> Option<u64> {
        let pending = self.status.as_ref()?.awaiting_confirm()?;
        let deadline = crate::state::parse_time(&pending.deadline).unwrap_or(0);
        Some(deadline.saturating_sub(now))
    }
}

struct App {
    /// Sorted by key.
    apps: Vec<AppView>,
    /// The key of the app on the right; the first app while nothing was chosen.
    current: Option<String>,
    mode: Mode,
    focus: Focus,
    /// The events pane follows new events; `f` pauses it.
    follow: bool,
    /// The events pane fills the screen (`z`).
    zoom: bool,
    /// Only events containing this text (any case) are shown.
    filter: String,
    /// The footer's last word on an action; `true` for an error.
    notice: Option<(String, bool)>,
}

impl App {
    fn new(target: &Target) -> App {
        let mut app = App {
            apps: Vec::new(),
            current: None,
            mode: Mode::Normal,
            focus: Focus::Apps,
            follow: true,
            zoom: false,
            filter: String::new(),
            notice: None,
        };
        if let Target::One(socket) = target {
            app.apps
                .push(AppView::new(socket.display().to_string(), socket.clone()));
        }
        app
    }

    fn view_mut(&mut self, key: &str) -> Option<&mut AppView> {
        self.apps.iter_mut().find(|a| a.key == key)
    }

    /// The selected app's row. An app that left the list stays selected (nothing is), so an action never falls
    /// through to another app.
    fn current_index(&self) -> Option<usize> {
        match &self.current {
            Some(key) => self.apps.iter().position(|a| a.key == *key),
            None => (!self.apps.is_empty()).then_some(0),
        }
    }

    fn current(&self) -> Option<&AppView> {
        self.current_index().and_then(|i| self.apps.get(i))
    }

    fn choose(&mut self, index: usize) {
        self.current = self.apps.get(index).map(|a| a.key.clone());
    }

    fn step_app(&mut self, delta: isize) {
        let last = self.apps.len().saturating_sub(1);
        let index = self.current_index().unwrap_or(0);
        self.choose(index.saturating_add_signed(delta).min(last));
    }

    fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        for view in &mut self.apps {
            view.frozen = (!self.follow).then_some(view.events.len());
        }
    }

    fn apply(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Up(key, socket, resumed) => {
                // Pin what is on screen, so an app that sorts before it does not take the selection.
                self.current = self
                    .current
                    .take()
                    .or_else(|| self.apps.first().map(|a| a.key.clone()))
                    .or_else(|| Some(key.clone()));
                if self.view_mut(&key).is_none() {
                    self.apps.push(AppView::new(key.clone(), socket));
                    self.apps.sort_by(|a, b| a.key.cmp(&b.key));
                }
                let follow = self.follow;
                if let Some(view) = self.view_mut(&key) {
                    view.link = Link::Up;
                    // A resumed connection sends only what came after these events. A fresh one replays the
                    // backlog, and a paused pane stays paused while it arrives.
                    if !resumed {
                        view.events.clear();
                        view.frozen = (!follow).then_some(0);
                    }
                }
            }
            Msg::Down(key, why) => {
                if let Some(view) = self.view_mut(&key) {
                    view.link = Link::Down(why);
                }
            }
            Msg::Gone(key) => {
                self.apps.retain(|a| a.key != key);
            }
            Msg::Line(key, line) => {
                if let Some(view) = self.view_mut(&key) {
                    view.line(line);
                }
            }
            Msg::Done(label, Ok(())) => self.notice = Some((format!("✓ {label}"), false)),
            Msg::Done(label, Err(message)) => {
                self.notice = Some((format!("{label}: {message}"), true));
            }
        }
        None
    }

    fn key(&mut self, key: KeyEvent) -> Option<Cmd> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Cmd::Quit);
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Stage(mut git_ref) => {
                match key.code {
                    KeyCode::Enter if !git_ref.trim().is_empty() => {
                        let git_ref = git_ref.trim().to_string();
                        let request = Request::Stage {
                            git_ref: git_ref.clone(),
                        };
                        return self.send(format!("stage {git_ref}"), request, None);
                    }
                    KeyCode::Esc => return None,
                    KeyCode::Backspace => {
                        git_ref.pop();
                    }
                    KeyCode::Char(c) => git_ref.push(c),
                    _ => {}
                }
                self.mode = Mode::Stage(git_ref);
                None
            }
            // Anything but `y` cancels: a stray key must not destroy anything.
            Mode::Ask(action, _) => (key.code == KeyCode::Char('y')).then_some(Cmd::Send(action)),
            Mode::Search => {
                match key.code {
                    KeyCode::Enter => return None,
                    KeyCode::Esc => {
                        self.filter.clear();
                        return None;
                    }
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Char(c) => self.filter.push(c),
                    _ => {}
                }
                self.mode = Mode::Search;
                None
            }
            Mode::Help => {
                if !matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.mode = Mode::Help;
                }
                None
            }
            Mode::Normal => {
                self.notice = None;
                self.normal_key(key.code)
            }
        }
    }

    fn normal_key(&mut self, code: KeyCode) -> Option<Cmd> {
        match code {
            KeyCode::Char('q') => return Some(Cmd::Quit),
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Apps => Focus::Generations,
                    Focus::Generations => Focus::Apps,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char('f') => self.toggle_follow(),
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Char('n') => {
                if self.current().is_some() {
                    self.mode = Mode::Stage(String::new());
                } else {
                    self.notice = Some(("no app to stage for".into(), true));
                }
            }
            KeyCode::Char('t') => return self.send_selected(false),
            KeyCode::Char('w') => return self.send_selected(true),
            KeyCode::Char('c') => {
                let generation = self.current().and_then(|a| {
                    let pending = a.status.as_ref()?.awaiting_confirm()?;
                    Some(pending.generation.clone())
                });
                return self.send("confirm".into(), Request::Confirm { generation }, None);
            }
            KeyCode::Char('b') => {
                let question = match self.current() {
                    Some(app) => {
                        let previous = app
                            .status
                            .as_ref()
                            .and_then(|s| s.pending.as_ref()?.previous.as_ref());
                        match previous {
                            Some(to) => format!("Roll back {} to {to}?", app.name()),
                            None => format!("Roll back {}?", app.name()),
                        }
                    }
                    None => String::new(),
                };
                return self.send(
                    "rollback".into(),
                    Request::Rollback {
                        keep_storage: false,
                    },
                    Some(question),
                );
            }
            KeyCode::Char('s') => return self.send("start".into(), Request::Start, None),
            KeyCode::Char('x') => return self.ask_app("stop", "Stop", Request::Stop),
            KeyCode::Char('r') => return self.ask_app("restart", "Restart", Request::Restart),
            KeyCode::Char('p') => {
                return self.ask_app("prune", "Prune old generations of", Request::Prune);
            }
            _ => {}
        }
        None
    }

    /// `stop` or `restart` of the current app, after a dialog.
    fn ask_app(&mut self, label: &str, verb: &str, request: Request) -> Option<Cmd> {
        let question = self
            .current()
            .map(|a| format!("{verb} {}?", a.name()))
            .unwrap_or_default();
        self.send(label.into(), request, Some(question))
    }

    /// The arrow keys move the app or the generation, whichever has focus.
    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Apps => self.step_app(delta),
            Focus::Generations => self.select(delta),
        }
    }

    fn select(&mut self, delta: isize) {
        if let Some(view) = self.current_index().and_then(|i| self.apps.get_mut(i)) {
            view.select(delta);
        }
    }

    /// `test` or `switch` on the selected generation of the current app; a switch asks first.
    fn send_selected(&mut self, switch: bool) -> Option<Cmd> {
        let Some(id) = self.current().and_then(AppView::selected_id) else {
            self.notice = Some(("no generation selected".into(), true));
            return None;
        };
        let (request, question) = if switch {
            let app = self.current().map(AppView::name).unwrap_or_default();
            let request = Request::Switch {
                generation: id.clone(),
                deadline: None,
            };
            (request, Some(format!("Switch {app} to {id}?")))
        } else {
            let request = Request::Test {
                generation: id.clone(),
            };
            (request, None)
        };
        self.send(format!("{} {id}", request.command()), request, question)
    }

    /// Aim a request at the current app: send now, or ask `y/N` first.
    fn send(&mut self, label: String, request: Request, ask: Option<String>) -> Option<Cmd> {
        let Some(app) = self.current() else {
            self.notice = Some(("no app selected".into(), true));
            return None;
        };
        let action = Action {
            label,
            socket: app.socket.clone(),
            request,
        };
        match ask {
            Some(question) => {
                self.mode = Mode::Ask(action, question);
                None
            }
            None => Some(Cmd::Send(action)),
        }
    }
}

// ------------------------------------------------------------ the loop

pub fn run(target: &Target) -> u8 {
    let (tx, rx) = mpsc::channel();
    match target {
        Target::One(socket) => {
            let (tx, socket) = (tx.clone(), socket.clone());
            let key = socket.display().to_string();
            std::thread::spawn(move || reader(&key, &socket, None, &tx));
        }
        Target::Discover(dir) => {
            let (tx, dir) = (tx.clone(), dir.clone());
            let active = Active::default();
            std::thread::spawn(move || scan(&dir, &active, &tx));
        }
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if let Event::Key(key) = event
                    && tx.send(Msg::Key(key)).is_err()
                {
                    return;
                }
            }
        });
    }
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("error: cannot start the terminal screen: {e}");
            return 1;
        }
    };
    let result = event_loop(&mut terminal, App::new(target), &rx, &tx);
    ratatui::restore();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    mut app: App,
    rx: &mpsc::Receiver<Msg>,
    tx: &Sender<Msg>,
) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| draw::draw(frame, &app, now_secs()))?;
        let mut msgs = Vec::new();
        match rx.recv_timeout(TICK) {
            Ok(msg) => msgs.push(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        msgs.extend(rx.try_iter());
        for msg in msgs {
            match app.apply(msg) {
                Some(Cmd::Quit) => return Ok(()),
                Some(Cmd::Send(action)) => {
                    app.notice = Some((format!("{}…", action.label), false));
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let result = request(&action.socket, &action.request);
                        let _ = tx.send(Msg::Done(action.label, result));
                    });
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::History;
    use crate::state::format_time;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::{Value, json};

    /// A full record as the daemon sends it, which also checks that `Generation` reads that shape.
    fn gen_record(id: &str, git_ref: &str, commit: &str, state: &str) -> Value {
        json!({ "id": id, "ref": git_ref, "commit": format!("{commit}0000000000"), "state": state,
                "dir": format!("/tmp/fk/generations/{id}/code"), "staged_at": format_time(1_000_000 - 180),
                "test": null, "switched_at": null, "confirmed_at": null, "rolled_back_at": null,
                "rollback_reason": null, "failed_starts": 0 })
    }

    fn typed<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    /// Two apps: `sample` with a switch awaiting confirm until `deadline`, and a quiet `blog`.
    fn fixture(deadline: u64) -> App {
        let mut app = App::new(&Target::Discover("/tmp/fk/run".into()));
        let mut g8 = gen_record("g8", "feat", "91aa2c01", "tested");
        g8["test"] = json!({ "port": 4791, "passed": true, "at": "x", "seconds": 2.1, "message": "healthy" });
        let mut g5 = gen_record("g5", "main", "0c1d2e3f", "rolled_back");
        g5["rollback_reason"] = json!("missed confirm deadline");
        let history = [
            g5,
            gen_record("g6", "main", "bf0ad442", "permanent"),
            gen_record("g7", "main", "238c447f", "current"),
            g8,
        ];
        let status = json!({
            "name": "sample", "daemon_pid": 812, "permanent": "g6", "current": "g7", "busy": null,
            "pending": { "generation": "g7", "previous": "g6", "deadline": format_time(deadline) },
            "process": { "state": "running", "generation": "g7", "pid": 9001, "port": 4747,
                         "started_at": "x", "uptime": 65.2, "restarts": 1,
                         "last_exit": { "at": "x", "code": null, "signal": 9, "requested": true } }
        });
        let quiet = json!({
            "name": "blog", "daemon_pid": 813, "permanent": "g1", "current": "g1", "busy": null,
            "pending": null,
            "process": { "state": "stopped", "generation": "g1", "pid": null, "port": 3000,
                         "started_at": null, "uptime": null, "restarts": 0, "last_exit": null }
        });
        for (key, status, history) in [
            (
                "blog",
                quiet,
                vec![gen_record("g1", "main", "11111111", "permanent")],
            ),
            ("sample", status, history.to_vec()),
        ] {
            app.apply(Msg::Up(
                key.into(),
                format!("/tmp/fk/run/{key}.sock").into(),
                false,
            ));
            app.apply(Msg::Line(
                key.into(),
                Line::Status {
                    status: Box::new(typed(status)),
                },
            ));
            app.apply(Msg::Line(
                key.into(),
                Line::History {
                    history: History {
                        generations: typed(Value::Array(history)),
                    },
                },
            ));
        }
        for (at, kind, g, message) in [
            (
                "2026-11-03T14:02:01Z",
                "switched",
                "g7",
                "switched from g6; confirm before 14:04:01",
            ),
            (
                "2026-11-03T14:02:03Z",
                "healthy",
                "g7",
                "health check passed",
            ),
            (
                "2026-11-03T14:02:40Z",
                "tested",
                "g8",
                "test passed on port 4791 in 2.1s",
            ),
        ] {
            let event = json!({ "at": at, "kind": kind, "generation": g, "message": message });
            app.apply(Msg::Line(
                "sample".into(),
                Line::Event {
                    event: typed(event),
                },
            ));
        }
        app
    }

    fn render(app: &App, now: u64) -> String {
        render_at(app, now, 100, 24)
    }

    fn render_at(app: &App, now: u64, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw::draw(frame, app, now)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The screen shows local times.
    fn local(at: &str) -> String {
        draw::local_clock(crate::state::parse_time(at).unwrap())
    }

    #[test]
    fn the_screen_lists_every_app_and_shows_the_selected_ones_switch() {
        let now = 1_000_000;
        let mut app = fixture(now + 102);
        app.choose(1);
        let screen = render(&app, now);
        for expected in [
            "2 apps",
            "○ stopped",
            "● running",
            "blog",
            "sample",
            "CONFIRM",
            "⏱ 01:42",
            "9001",
            "4747",
            "SIGKILL (requested)",
            format!("health ✔ {}", local("2026-11-03T14:02:03Z")).as_str(),
            "⏱ confirm g7 within 01:42, or it rolls back to g6 (code + storage)",
            "g8   feat@91aa2c01  tested",
            "✔ test 2.1s on :4791",
            "live · awaiting confirm",
            "rollback target",
            "g5   main@0c1d2e3f  rolled_back",
            "✖ missed confirm deadline",
            format!("{} tested", local("2026-11-03T14:02:40Z")).as_str(),
        ] {
            assert!(
                screen.contains(expected),
                "missing `{expected}` in\n{screen}"
            );
        }
        // Newest generation first, and while a switch is pending `c confirm` leads the footer.
        assert!(screen.find("g8 ") < screen.find("g5 "), "{screen}");
        let footer = screen.lines().last().unwrap();
        assert!(footer.trim_start().starts_with("c confirm"), "{footer}");
        assert!(footer.find("c confirm") < footer.find("? help"), "{footer}");
        // Without a switch the help hint leads.
        let quiet = render(&fixture(0), now);
        let footer = quiet.lines().last().unwrap();
        assert!(footer.trim_start().starts_with("? help"), "{footer}");
    }

    /// `pending` stays set while the switch is rolled back, so the screen reads `step`: no countdown, no `c confirm`.
    #[test]
    fn a_rollback_shows_no_confirm_countdown() {
        let now = 1_000_000;
        let with_step = |step: Option<&str>| {
            let mut app = fixture(now + 102);
            app.choose(1);
            let AppView {
                status: Some(status),
                ..
            } = &mut app.apps[1]
            else {
                panic!("the fixture has a status");
            };
            status.step = step.map(|s| typed(json!(s)));
            render(&app, now)
        };
        let countdown = ["⏱", "awaiting confirm", "rollback target"];

        // An older daemon sends no step, and its pending switch awaits its confirm.
        let older = with_step(None);
        let pending = with_step(Some("pending"));
        for screen in [&older, &pending] {
            for expected in countdown {
                assert!(
                    screen.contains(expected),
                    "missing `{expected}` in\n{screen}"
                );
            }
        }

        let rolling_back = with_step(Some("rolling_back"));
        let failed = with_step(Some("restore_failed"));
        for screen in [&rolling_back, &failed] {
            for unexpected in countdown {
                assert!(!screen.contains(unexpected), "`{unexpected}` in\n{screen}");
            }
            let footer = screen.lines().last().unwrap();
            assert!(footer.trim_start().starts_with("? help"), "{footer}");
        }
        assert!(rolling_back.contains("rolling back g7"), "{rolling_back}");
        assert!(failed.contains("✘ restore failed, b retries"), "{failed}");
    }

    #[test]
    fn values_from_a_newer_daemon_are_kept_and_shown_neutrally() {
        let now = 1_000_000;
        let mut app = fixture(now + 102);
        app.choose(1);
        let events = app.apps[1].events.len();
        app.apply(Msg::Line(
            "sample".into(),
            Line::Event {
                event: typed(
                    json!({ "seq": 9, "at": "2026-11-03T14:03:00Z", "kind": "from_the_future",
                                     "generation": "g7", "message": "something new" }),
                ),
            },
        ));
        assert_eq!(app.apps[1].events.len(), events + 1);
        let AppView {
            status: Some(status),
            history,
            ..
        } = &mut app.apps[1]
        else {
            panic!("the fixture has a status");
        };
        status.process.state = typed(json!("paused"));
        history[0].state = typed(json!("quarantined"));
        let screen = render(&app, now);
        for expected in [
            "? unknown",
            "a state this fawkes does not know",
            "something new",
        ] {
            assert!(
                screen.contains(expected),
                "missing `{expected}` in\n{screen}"
            );
        }
    }

    #[test]
    fn a_narrow_terminal_drops_columns_and_footer_items_and_never_panics() {
        let now = 1_000_000;
        let mut app = fixture(now + 10);
        app.choose(1);
        let screen = render_at(&app, now, 50, 14);
        assert!(
            screen.contains("CONFIRM") && screen.contains("⏱ 00:10"),
            "{screen}"
        );
        for dropped in ["PERM", "STAGED", "RST"] {
            assert!(
                !screen.contains(dropped),
                "{dropped} should be hidden in\n{screen}"
            );
        }
        let footer = screen.lines().last().unwrap();
        assert!(
            footer.contains("c confirm") && !footer.contains("start/stop"),
            "{footer}"
        );
        // Whole items only: the list ends at an item boundary, never inside a label.
        assert!(
            [
                "c confirm",
                "? help",
                "b rollback",
                "n stage",
                "t test",
                "w switch"
            ]
            .iter()
            .any(|item| footer.trim().ends_with(item)),
            "{footer}"
        );
        app.mode = Mode::Help;
        for (w, h) in [(40, 10), (20, 5), (1, 1), (80, 1), (3, 30), (0, 0)] {
            render_at(&app, now, w, h);
            app.zoom = !app.zoom;
            render_at(&app, now, w, h);
        }
    }

    fn key(c: char) -> Msg {
        press(KeyCode::Char(c))
    }

    fn press(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn name_of_current(app: &App) -> Option<String> {
        app.current().map(AppView::name)
    }

    fn selected_generation(app: &App) -> Option<String> {
        app.current().and_then(AppView::selected_id)
    }

    /// The fixture with `sample` (the second app, sorted by name) chosen.
    fn on_sample() -> App {
        let mut app = fixture(0);
        app.apply(press(KeyCode::Down));
        assert_eq!(name_of_current(&app).as_deref(), Some("sample"));
        app
    }

    fn sent(cmd: Option<Cmd>) -> Action {
        let Some(Cmd::Send(action)) = cmd else {
            panic!("expected a request, got {cmd:?}");
        };
        action
    }

    #[test]
    fn the_arrows_move_the_app_while_the_apps_have_focus_and_the_generation_after_tab() {
        let mut app = fixture(0);
        // Apps are sorted by name: `blog` first.
        assert_eq!(name_of_current(&app).as_deref(), Some("blog"));
        app.apply(press(KeyCode::Down));
        assert_eq!(name_of_current(&app).as_deref(), Some("sample"));
        // Newest first: g8 is selected. Tab moves focus, so `j` now moves the generation to g7.
        assert_eq!(selected_generation(&app).as_deref(), Some("g8"));
        app.apply(press(KeyCode::Tab));
        assert!(app.focus == Focus::Generations);
        app.apply(key('j'));
        assert_eq!(name_of_current(&app).as_deref(), Some("sample"));
        assert_eq!(selected_generation(&app).as_deref(), Some("g7"));
        app.apply(key('k'));
        assert_eq!(selected_generation(&app).as_deref(), Some("g8"));
    }

    #[test]
    fn a_switch_asks_naming_the_target_and_only_y_sends_it_to_the_current_app() {
        let mut app = on_sample();
        app.apply(press(KeyCode::Tab));
        app.apply(key('j'));
        assert_eq!(app.apply(key('w')), None);
        assert!(matches!(&app.mode, Mode::Ask(_, q) if q == "Switch sample to g7?"));
        assert_eq!(app.apply(key('n')), None, "anything but y cancels");
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.apply(key('w')), None);
        let action = sent(app.apply(key('y')));
        assert_eq!(action.label, "switch g7");
        assert_eq!(
            action.request,
            Request::Switch {
                generation: "g7".into(),
                deadline: None
            }
        );
        assert_eq!(action.socket, PathBuf::from("/tmp/fk/run/sample.sock"));
    }

    #[test]
    fn rollback_stop_and_restart_ask_and_escape_closes_the_dialog() {
        let mut app = on_sample();
        for (k, question) in [
            ('b', "Roll back sample to g6?"),
            ('x', "Stop sample?"),
            ('r', "Restart sample?"),
        ] {
            assert_eq!(app.apply(key(k)), None);
            assert!(matches!(&app.mode, Mode::Ask(_, q) if q == question));
            assert_eq!(app.apply(press(KeyCode::Esc)), None);
            assert!(matches!(app.mode, Mode::Normal));
        }
    }

    #[test]
    fn n_types_a_ref_inline_and_enter_stages_it() {
        let mut app = on_sample();
        app.apply(key('n'));
        "main".chars().for_each(|c| {
            app.apply(key(c));
        });
        app.apply(press(KeyCode::Backspace));
        app.apply(key('n'));
        let action = sent(app.apply(press(KeyCode::Enter)));
        assert_eq!(action.label, "stage main");
        assert_eq!(
            action.request,
            Request::Stage {
                git_ref: "main".into()
            }
        );
        // Esc abandons the prompt, and an empty ref is not sent.
        app.apply(key('n'));
        assert_eq!(app.apply(press(KeyCode::Enter)), None);
        assert!(matches!(&app.mode, Mode::Stage(r) if r.is_empty()));
        assert_eq!(app.apply(press(KeyCode::Esc)), None);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn test_start_and_confirm_send_at_once_and_confirm_names_the_pending_generation() {
        let mut app = on_sample();
        let action = sent(app.apply(key('t')));
        assert_eq!(
            action.request,
            Request::Test {
                generation: "g8".into()
            }
        );
        assert_eq!(sent(app.apply(key('s'))).request, Request::Start);
        assert_eq!(
            sent(app.apply(key('c'))).request,
            Request::Confirm {
                generation: Some("g7".into())
            }
        );
    }

    #[test]
    fn q_quits_and_the_retired_bindings_do_nothing() {
        let mut app = on_sample();
        for k in ['S', 'X', 'R', '2'] {
            assert_eq!(app.apply(key(k)), None);
            assert!(matches!(app.mode, Mode::Normal));
        }
        assert_eq!(app.apply(key('q')), Some(Cmd::Quit));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.apply(Msg::Key(ctrl_c)), Some(Cmd::Quit));
    }

    #[test]
    fn f_pauses_the_events_pane_slash_filters_it_and_escape_clears_the_filter() {
        let mut app = on_sample();
        app.apply(key('f'));
        assert!(!app.follow);
        app.apply(key('/'));
        app.apply(key('t'));
        app.apply(press(KeyCode::Enter));
        assert_eq!(app.filter, "t");
        let screen = render(&app, 0);
        assert!(
            screen.contains("paused") && screen.contains("/t"),
            "{screen}"
        );
        app.apply(press(KeyCode::Esc));
        assert_eq!(app.filter, "");
    }

    /// The selection stays on what is shown instead of following the sort order, so an app appearing cannot move it
    /// and send an unasked `c` to another daemon.
    #[test]
    fn an_app_appearing_does_not_move_the_selection() {
        let mut app = fixture(0);
        app.current = None;
        assert_eq!(app.current().map(|a| a.key.as_str()), Some("blog"));
        app.apply(Msg::Up("aaa".into(), "/tmp/fk/run/aaa.sock".into(), false));
        assert_eq!(app.current().map(|a| a.key.as_str()), Some("blog"));
        assert_eq!(app.apps.len(), 3);
    }

    /// A selected app that leaves selects nothing, so no action falls through to another app. The body of the screen
    /// is not asserted: it hides the other apps until an arrow key picks one.
    #[test]
    fn a_selected_app_that_leaves_selects_nothing_and_an_arrow_picks_another() {
        let now = 1_000_000;
        let mut app = fixture(now + 102);
        app.choose(1);
        app.apply(Msg::Gone("sample".into()));
        assert!(app.current().is_none());
        assert!(render(&app, now).contains(" fawkes  1 app"));
        for k in ['c', 's', 't', 'n'] {
            assert_eq!(app.apply(key(k)), None, "{k} reached an app");
            assert!(matches!(app.mode, Mode::Normal));
        }
        assert!(app.notice.as_ref().is_some_and(|(_, error)| *error));

        app.apply(press(KeyCode::Down));
        assert_eq!(name_of_current(&app).as_deref(), Some("blog"));
        assert!(render(&app, now).contains("blog"));
    }

    #[test]
    fn another_app_leaving_changes_nothing_on_the_screen_but_the_count() {
        let now = 1_000_000;
        let mut app = fixture(now + 102);
        app.choose(1);
        app.apply(Msg::Gone("blog".into()));
        app.apply(Msg::Gone("never-listed".into()));
        assert_eq!(name_of_current(&app).as_deref(), Some("sample"));
        let screen = render(&app, now);
        assert!(
            screen.contains(" fawkes  1 app") && screen.contains("CONFIRM"),
            "{screen}"
        );
    }

    #[test]
    fn p_asks_before_pruning_and_only_a_yes_sends_it_to_the_selected_app() {
        // `blog` comes first; `sample` is chosen so the request has to follow the selection, not the first app.
        let mut app = on_sample();
        assert_eq!(app.apply(key('p')), None, "p only asks");
        assert!(matches!(&app.mode, Mode::Ask(_, q) if q == "Prune old generations of sample?"));
        assert_eq!(app.apply(key('n')), None, "declining sends nothing");
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.apply(key('p')), None);
        let action = sent(app.apply(key('y')));
        assert_eq!(action.label, "prune");
        assert_eq!(action.request, Request::Prune);
        assert_eq!(action.socket, PathBuf::from("/tmp/fk/run/sample.sock"));
        assert!(matches!(app.mode, Mode::Normal));
    }

    fn event_numbered(seq: u64) -> protocol::Event {
        typed(
            json!({ "seq": seq, "at": "2026-11-03T14:02:01Z", "kind": "progress",
                      "generation": null, "message": format!("event {seq}") }),
        )
    }

    fn seqs(view: &AppView) -> Vec<u64> {
        view.events.iter().map(|e| e.seq).collect()
    }

    fn up(app: &mut App, resumed: bool) {
        app.apply(Msg::Up(
            "sample".into(),
            "/tmp/fk/run/sample.sock".into(),
            resumed,
        ));
    }

    #[test]
    fn a_resumed_connection_keeps_the_events_and_a_fresh_one_starts_the_pane_over() {
        let mut app = fixture(0);
        let sample = |app: &App| app.apps[1].events.len();
        assert_eq!(sample(&app), 3);

        app.apply(Msg::Down("sample".into(), "connection lost".into()));
        assert!(matches!(&app.apps[1].link, Link::Down(why) if why == "connection lost"));
        up(&mut app, true);
        assert!(app.apps[1].link == Link::Up);
        assert_eq!(
            sample(&app),
            3,
            "a resumed connection sends only what is new"
        );
        // The replay that follows is appended after what was already there.
        app.apply(Msg::Line(
            "sample".into(),
            Line::Event {
                event: event_numbered(4),
            },
        ));
        assert_eq!(sample(&app), 4);

        // A replay with a gap, or a daemon whose log started over, replaces the pane.
        up(&mut app, false);
        assert_eq!(sample(&app), 0);
        assert_eq!(app.apps[1].frozen, None);
    }

    #[test]
    fn a_paused_pane_stays_paused_across_a_reconnect() {
        let mut app = fixture(0);
        app.apply(key('f'));
        assert_eq!(app.apps[1].frozen, Some(3));
        up(&mut app, true);
        assert_eq!(app.apps[1].frozen, Some(3), "the events on screen are kept");
        // A fresh replay starts the paused pane empty while the backlog arrives.
        up(&mut app, false);
        assert_eq!(app.apps[1].frozen, Some(0));
        assert!(app.apps[1].events.is_empty());
    }

    #[test]
    fn the_event_list_is_capped_and_a_paused_view_keeps_showing_the_same_events() {
        let mut view = AppView::new("sample".into(), "/tmp/fk/run/sample.sock".into());
        for seq in 1..=MAX_EVENTS as u64 {
            view.push_event(event_numbered(seq));
        }
        assert_eq!(view.events.len(), MAX_EVENTS);
        // Paused on the oldest three.
        view.frozen = Some(3);
        view.push_event(event_numbered(MAX_EVENTS as u64 + 1));
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert_eq!(
            view.events.front().map(|e| e.seq),
            Some(2),
            "oldest dropped"
        );
        assert_eq!(view.frozen, Some(2));
        let shown: Vec<u64> = view.events.iter().take(2).map(|e| e.seq).collect();
        assert_eq!(shown, [2, 3], "the still-shown events are the same ones");
        // Everything paused on drops out: the pane is empty, never negative.
        for seq in 0..5 {
            view.push_event(event_numbered(MAX_EVENTS as u64 + 2 + seq));
        }
        assert_eq!(view.frozen, Some(0));
        assert_eq!(seqs(&view).len(), MAX_EVENTS);
        // Following: the cap alone.
        view.frozen = None;
        view.push_event(event_numbered(10_000));
        assert_eq!(view.frozen, None);
        assert_eq!(view.events.len(), MAX_EVENTS);
    }
}
