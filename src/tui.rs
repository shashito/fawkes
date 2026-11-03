//! `fawkes tui`: a full-screen client of the daemon sockets, one screen for every supervised app.
//!
//! This file is the connection, state, action and key layer. Each app has a reader thread holding one `subscribe`
//! connection; every action is an ordinary one-shot request on its own thread, so the screen never blocks.
//! All drawing lives in `draw.rs`, which only reads `App`.

mod draw;

use crate::state::now_secs;
use crate::text;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(1);
const RESCAN: Duration = Duration::from_secs(3);
const BACKLOG: usize = 100;
const MAX_EVENTS: usize = 500;

/// Which daemons the screen talks to.
pub enum Target {
    /// Exactly one daemon, named by `--socket`, `--manifest` or `FAWKES_SOCKET`.
    One(PathBuf),
    /// Every daemon that linked its socket as `<dir>/<name>.sock`.
    Discover(PathBuf),
}

/// `$FAWKES_HOME/run`, with `~/.fawkes` as the default home: where daemons link their sockets.
pub fn run_dir() -> PathBuf {
    let home = std::env::var_os("FAWKES_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fawkes")))
        .unwrap_or_else(|| PathBuf::from(".fawkes"));
    home.join("run")
}

/// Everything the UI thread hears about. `String`s named `app` are the app's key (see `AppView::key`).
enum Msg {
    Key(KeyEvent),
    /// A daemon answered: the app exists from now on.
    Up(String, PathBuf),
    /// The events before we subscribed.
    Backlog(String, Vec<Value>),
    /// A line of the `subscribe` stream: `status`, `history` or `event`.
    Line(String, Value),
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
    /// The ref being typed for `s`.
    Stage(String),
    /// A `y/N` question; `y` sends the request.
    Ask(Action),
}

/// One request to one daemon.
#[derive(Debug, PartialEq, Clone)]
struct Action {
    label: String,
    socket: PathBuf,
    request: Value,
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
    status: Option<Value>,
    status_at: Instant,
    /// Oldest first, as the daemon sends it.
    history: Vec<Value>,
    events: VecDeque<Value>,
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
            selected: None,
        }
    }

    fn name(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s["name"].as_str())
            .unwrap_or(&self.key)
            .to_string()
    }

    fn push_event(&mut self, event: Value) {
        self.events.push_back(event);
        if self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    fn line(&mut self, line: &Value) {
        if let Some(status) = line.get("status") {
            self.status = Some(status.clone());
            self.status_at = Instant::now();
        } else if let Some(history) = line["history"]["generations"].as_array() {
            self.history = history.clone();
        } else if let Some(event) = line.get("event") {
            self.push_event(event.clone());
        }
    }

    /// The history, newest first: the order of the list.
    fn rows(&self) -> Vec<&Value> {
        self.history.iter().rev().collect()
    }

    fn selected_index(&self) -> usize {
        let rows = self.rows();
        self.selected
            .as_ref()
            .and_then(|id| rows.iter().position(|g| g["id"] == json!(id)))
            .unwrap_or(0)
    }

    fn selected_id(&self) -> Option<String> {
        self.rows()
            .get(self.selected_index())
            .and_then(|g| g["id"].as_str().map(String::from))
    }

    fn select(&mut self, delta: isize) {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        let index = self.selected_index().saturating_add_signed(delta).min(last);
        self.selected = rows
            .get(index)
            .and_then(|g| g["id"].as_str().map(String::from));
    }

    fn generation(&self, id: &Value) -> Option<&Value> {
        self.history.iter().find(|g| g["id"] == *id)
    }

    /// Seconds until the pending switch is rolled back, if one awaits confirm.
    fn countdown(&self, now: u64) -> Option<u64> {
        let deadline = self.status.as_ref()?["pending"]["deadline"].as_str()?;
        let deadline = crate::state::parse_time(deadline).unwrap_or(0);
        Some(deadline.saturating_sub(now))
    }
}

struct App {
    /// Sorted by key.
    apps: Vec<AppView>,
    /// The key of the app on the right; the first app while nothing was chosen.
    current: Option<String>,
    mode: Mode,
    /// The footer's last word on an action; `true` for an error.
    notice: Option<(String, bool)>,
}

impl App {
    fn new(target: &Target) -> App {
        let mut app = App {
            apps: Vec::new(),
            current: None,
            mode: Mode::Normal,
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

    fn current_index(&self) -> usize {
        self.current
            .as_ref()
            .and_then(|key| self.apps.iter().position(|a| a.key == *key))
            .unwrap_or(0)
    }

    fn current(&self) -> Option<&AppView> {
        self.apps.get(self.current_index())
    }

    fn choose(&mut self, index: usize) {
        self.current = self.apps.get(index).map(|a| a.key.clone());
    }

    fn step_app(&mut self, delta: isize) {
        if !self.apps.is_empty() {
            let len = self.apps.len() as isize;
            let index = (self.current_index() as isize + delta).rem_euclid(len);
            self.choose(index as usize);
        }
    }

    fn apply(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Up(key, socket) => {
                if self.view_mut(&key).is_none() {
                    self.apps.push(AppView::new(key.clone(), socket));
                    self.apps.sort_by(|a, b| a.key.cmp(&b.key));
                }
                if let Some(view) = self.view_mut(&key) {
                    view.link = Link::Up;
                    view.events.clear();
                }
            }
            Msg::Down(key, why) => {
                if let Some(view) = self.view_mut(&key) {
                    view.link = Link::Down(why);
                }
            }
            Msg::Gone(key) => {
                let keep = self.current().map(|a| a.key.clone());
                self.apps.retain(|a| a.key != key);
                self.current = keep.filter(|k| *k != key);
            }
            Msg::Backlog(key, events) => {
                if let Some(view) = self.view_mut(&key) {
                    events.into_iter().for_each(|e| view.push_event(e));
                }
            }
            Msg::Line(key, line) => {
                if let Some(view) = self.view_mut(&key) {
                    view.line(&line);
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
                        let request = json!({ "command": "stage", "args": { "ref": git_ref } });
                        return self.send(format!("stage {git_ref}"), request, false);
                    }
                    KeyCode::Esc => {}
                    KeyCode::Backspace => {
                        git_ref.pop();
                        self.mode = Mode::Stage(git_ref);
                    }
                    KeyCode::Char(c) => {
                        git_ref.push(c);
                        self.mode = Mode::Stage(git_ref);
                    }
                    _ => self.mode = Mode::Stage(git_ref),
                }
                None
            }
            Mode::Ask(action) => (key.code == KeyCode::Char('y')).then_some(Cmd::Send(action)),
            Mode::Normal => {
                self.notice = None;
                self.normal_key(key.code)
            }
        }
    }

    fn normal_key(&mut self, code: KeyCode) -> Option<Cmd> {
        let plain = |name: &str| json!({ "command": name, "args": {} });
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(Cmd::Quit),
            KeyCode::Tab => self.step_app(1),
            KeyCode::BackTab => self.step_app(-1),
            KeyCode::Char(d @ '1'..='9') => self.choose(d as usize - '1' as usize),
            KeyCode::Up | KeyCode::Char('k') => self.select(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select(1),
            KeyCode::Char('s') => {
                if self.current().is_some() {
                    self.mode = Mode::Stage(String::new());
                } else {
                    self.notice = Some(("no app to stage for".into(), true));
                }
            }
            KeyCode::Char('t') => return self.send_selected("test", false),
            KeyCode::Char('w') => return self.send_selected("switch", true),
            KeyCode::Char('c') => return self.send("confirm".into(), plain("confirm"), false),
            KeyCode::Char('r') => return self.send("rollback".into(), plain("rollback"), true),
            KeyCode::Char('S') => return self.send("start".into(), plain("start"), false),
            KeyCode::Char('X') => return self.send("stop".into(), plain("stop"), true),
            KeyCode::Char('R') => return self.send("restart".into(), plain("restart"), true),
            _ => {}
        }
        None
    }

    fn select(&mut self, delta: isize) {
        let index = self.current_index();
        if let Some(view) = self.apps.get_mut(index) {
            view.select(delta);
        }
    }

    /// `test` or `switch` on the selected generation of the current app.
    fn send_selected(&mut self, name: &str, ask: bool) -> Option<Cmd> {
        let Some(id) = self.current().and_then(AppView::selected_id) else {
            self.notice = Some(("no generation selected".into(), true));
            return None;
        };
        let request = json!({ "command": name, "args": { "generation": id } });
        self.send(format!("{name} {id}"), request, ask)
    }

    /// Aim a request at the current app: send now, or ask `y/N` first.
    fn send(&mut self, label: String, request: Value, ask: bool) -> Option<Cmd> {
        let Some(app) = self.current() else {
            self.notice = Some(("no app selected".into(), true));
            return None;
        };
        let action = Action {
            label,
            socket: app.socket.clone(),
            request,
        };
        if ask {
            self.mode = Mode::Ask(action);
            None
        } else {
            Some(Cmd::Send(action))
        }
    }
}

// ------------------------------------------------------------ threads

/// The keys of the apps that have a reader thread.
type Active = Arc<Mutex<HashSet<String>>>;

/// Follow one daemon. A single target is retried for as long as the screen lives; a discovered one is dropped from
/// the list when it stops answering, and the scanner starts a fresh reader if it comes back.
fn reader(key: &str, socket: &Path, active: Option<&Active>, tx: &Sender<Msg>) {
    loop {
        let (up, why) = follow(key, socket, tx);
        let sent = match active {
            None => tx.send(Msg::Down(key.into(), why)),
            Some(_) if up => tx.send(Msg::Gone(key.into())),
            Some(_) => Ok(()),
        };
        if sent.is_err() {
            return;
        }
        if let Some(active) = active {
            active.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
            return;
        }
        std::thread::sleep(RETRY);
    }
}

/// Fetch the recent events, then follow `subscribe`. The error says whether the app had answered.
fn follow(key: &str, socket: &Path, tx: &Sender<Msg>) -> (bool, String) {
    let backlog = match request(
        socket,
        &json!({ "command": "log", "args": { "limit": BACKLOG } }),
    ) {
        Ok(backlog) => backlog,
        Err(e) => return (false, e),
    };
    let events = backlog["events"].as_array().cloned().unwrap_or_default();
    let sent = tx.send(Msg::Up(key.into(), socket.into())).is_ok()
        && tx.send(Msg::Backlog(key.into(), events)).is_ok();
    if !sent {
        return (true, "the screen is gone".into());
    }
    let stream = match open(socket, &json!({ "command": "subscribe" })) {
        Ok(stream) => stream,
        Err(e) => return (true, e),
    };
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if let Ok(value) = serde_json::from_str(&line)
            && tx.send(Msg::Line(key.into(), value)).is_err()
        {
            return (true, "the screen is gone".into());
        }
    }
    (true, "connection lost".into())
}

/// Every few seconds, start a reader for each `<name>.sock` link in `dir` that has none.
fn scan(dir: &Path, active: &Active, tx: &Sender<Msg>) {
    loop {
        let links = std::fs::read_dir(dir).into_iter().flatten().flatten();
        for link in links {
            let path = link.path();
            let Some(key) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".sock"))
                .map(String::from)
            else {
                continue;
            };
            if active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone())
            {
                let (active, tx) = (active.clone(), tx.clone());
                std::thread::spawn(move || reader(&key, &path, Some(&active), &tx));
            }
        }
        std::thread::sleep(RESCAN);
    }
}

fn open(socket: &Path, request: &Value) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    writeln!(stream, "{request}").map_err(|e| e.to_string())?;
    Ok(stream)
}

/// One request: progress lines are skipped (they reach the screen through `subscribe`), the answer is the result.
fn request(socket: &Path, request: &Value) -> Result<Value, String> {
    let stream = open(socket, request)?;
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["ok"] == json!(true) {
            return Ok(message["result"].clone());
        } else if message.get("ok").is_some() {
            return Err(text(&message["error"]["message"]));
        }
    }
    Err("the daemon closed the connection without an answer".into())
}

// ------------------------------------------------------------ the loop

pub fn run(target: Target) -> u8 {
    let (tx, rx) = mpsc::channel();
    match &target {
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
    let result = event_loop(&mut terminal, App::new(&target), &rx, &tx);
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
                        let result = request(&action.socket, &action.request).map(|_| ());
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
    use crate::state::format_time;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn gen_record(id: &str, git_ref: &str, commit: &str, state: &str) -> Value {
        json!({ "id": id, "ref": git_ref, "commit": format!("{commit}0000000000"), "state": state,
                "test": null, "rollback_reason": null })
    }

    /// Two apps: `sample` with a switch awaiting confirm until `deadline`, and a quiet `blog`.
    fn fixture(deadline: u64) -> App {
        let mut app = App::new(&Target::Discover("/tmp/fk/run".into()));
        let mut g8 = gen_record("g8", "feat", "91aa2c01", "tested");
        g8["test"] = json!({ "port": 4791, "passed": true, "seconds": 2.1, "message": "healthy" });
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
                         "uptime": 65.2, "restarts": 1,
                         "last_exit": { "at": "x", "code": null, "signal": 9, "requested": true } }
        });
        let quiet = json!({
            "name": "blog", "daemon_pid": 813, "permanent": "g1", "current": "g1", "busy": null,
            "pending": null,
            "process": { "state": "stopped", "generation": "g1", "pid": null, "port": null,
                         "uptime": null, "restarts": 0, "last_exit": null }
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
            ));
            app.apply(Msg::Line(key.into(), json!({ "status": status })));
            app.apply(Msg::Line(
                key.into(),
                json!({ "history": { "generations": history } }),
            ));
        }
        for (at, kind, g, message) in [
            (
                "2026-11-03T14:02:01Z",
                "progress",
                "g7",
                "switch g7 (deadline 120s)",
            ),
            (
                "2026-11-03T14:02:40Z",
                "progress",
                "g8",
                "g8 test passed on :4791",
            ),
        ] {
            let event = json!({ "at": at, "kind": kind, "generation": g, "message": message });
            app.apply(Msg::Line("sample".into(), json!({ "event": event })));
        }
        app
    }

    fn render(app: &App, now: u64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
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

    #[test]
    fn the_screen_lists_every_app_and_shows_the_selected_ones_switch() {
        let now = 1_000_000;
        let mut app = fixture(now + 102);
        app.choose(1);
        let screen = render(&app, now);
        for expected in [
            "blog",
            "sample",
            "g7",
            "main@238c447f",
            "CONFIRM",
            "01:42",
            "9001",
            "4747",
            "SIGKILL (requested)",
            "tested ✓ 2.1s on :4791",
            "g5",
            "rolled_back",
            "g8 test passed on :4791",
            "stage",
            "quit",
        ] {
            assert!(
                screen.contains(expected),
                "missing `{expected}` in\n{screen}"
            );
        }
    }

    #[test]
    fn keys_pick_an_app_and_a_generation_ask_before_destroying_and_prompt_for_a_ref() {
        let mut app = fixture(0);
        let key = |c| Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let press = |code| Msg::Key(KeyEvent::new(code, KeyModifiers::NONE));
        // Apps are sorted by name: `blog` first, `sample` after Tab.
        assert_eq!(app.current().map(|a| a.name()).as_deref(), Some("blog"));
        app.apply(press(KeyCode::Tab));
        assert_eq!(app.current().map(|a| a.name()).as_deref(), Some("sample"));
        // Newest first: g8 is selected, `down` moves to g7.
        assert_eq!(
            app.current().and_then(AppView::selected_id).as_deref(),
            Some("g8")
        );
        app.apply(press(KeyCode::Down));
        // `w` asks; anything but `y` cancels; `y` aims the request at the current app's socket.
        assert_eq!(app.apply(key('w')), None);
        assert_eq!(app.apply(key('n')), None);
        assert_eq!(app.apply(key('w')), None);
        let Some(Cmd::Send(action)) = app.apply(key('y')) else {
            panic!("y should send the switch");
        };
        assert_eq!(action.label, "switch g7");
        assert_eq!(action.request["args"]["generation"], "g7");
        assert_eq!(action.socket, PathBuf::from("/tmp/fk/run/sample.sock"));
        // `s` types a ref inline.
        app.apply(key('s'));
        "main".chars().for_each(|c| {
            app.apply(key(c));
        });
        let Some(Cmd::Send(action)) = app.apply(press(KeyCode::Enter)) else {
            panic!("enter should send the stage");
        };
        assert_eq!(
            action.request,
            json!({ "command": "stage", "args": { "ref": "main" } })
        );
        // `t` needs no question; a lost discovered app leaves the list.
        assert!(matches!(app.apply(key('t')), Some(Cmd::Send(_))));
        app.apply(Msg::Gone("sample".into()));
        assert_eq!(app.apps.len(), 1);
    }
}
