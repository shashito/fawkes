//! The daemon: socket server, supervision loop and the transactions (stage, test, switch, confirm, rollback).
//!
//! Locking: `inner` holds everything that changes (state.json, the live process, the busy slots). It is never held
//! across a slow step; stages, tests and stops run unlocked and take `inner` again to record their outcome.
//! Lock order is `inner` before `log` before `hub`, and nothing waits for another thread while holding one.

use crate::manifest::{Manifest, Vars};
use crate::process;
use crate::protocol::{Error, Out, R, Request, err};
use crate::state::{
    self, Event, EventLog, GenState, Generation, LastExit, Paths, Pending, State, TestInfo, now,
    now_secs, parse_time,
};
use crate::storage;
use serde_json::{Value, json};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: libc::c_int) {
    let flag = if signal == libc::SIGHUP {
        &RELOAD
    } else {
        &SHUTDOWN
    };
    flag.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(signal, handler) };
    }
}

// ---------------------------------------------------------------- types

/// The app process Fawkes started and owns.
struct App {
    child: Child,
    generation: String,
    started: Instant,
    started_at: String,
}

/// Why a process is being started; decides what happens to the restart counters.
#[derive(Clone, Copy, PartialEq)]
enum Why {
    /// A requested start: `start`, a switch, a rollback, a daemon start. Clears the failed-start counter.
    Fresh,
    /// The supervisor restarting a crashed process.
    Crash,
    /// The `restart` command. Clears the failed-start counter.
    Restart,
}

struct Inner {
    state: State,
    app: Option<App>,
    /// The app crashed `max_failed_starts` times in a row and is no longer restarted.
    app_failed: bool,
    /// When the supervisor restarts a crashed process.
    restart_at: Option<Instant>,
    last_exit: Option<LastExit>,
    /// Generation of the latest start and the number of restarts since it became current.
    last_started: Option<String>,
    restarts: u32,
    /// The transaction step in progress (stage, test, switch, rollback): one at a time.
    busy: Option<String>,
    /// A step that moves the live process (switch, rollback, start, stop, restart): one at a time.
    moving: Option<String>,
    /// A rollback has taken the pending generation apart; it can no longer be confirmed.
    rolling_back: bool,
}

/// Which slots a step occupies.
#[derive(Clone, Copy)]
enum Slot {
    /// stage, test
    Step,
    /// start, stop, restart
    Live,
    /// switch, rollback
    Both,
}

/// Held while a step runs; frees its slots on drop.
struct Busy<'a> {
    daemon: &'a Daemon,
    slot: Slot,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut inner = self.daemon.inner();
        if matches!(self.slot, Slot::Step | Slot::Both) {
            inner.busy = None;
        }
        if matches!(self.slot, Slot::Live | Slot::Both) {
            inner.moving = None;
        }
        drop(inner);
        self.daemon.broadcast(Msg::Changed);
    }
}

/// What subscribers (`subscribe`, `log --follow`) receive.
#[derive(Clone)]
enum Msg {
    Event(Event),
    /// Generations or the live process changed; `subscribe` sends fresh status and history lines.
    Changed,
}

pub struct Daemon {
    manifest_path: PathBuf,
    manifest: Mutex<Arc<Manifest>>,
    paths: Paths,
    socket: PathBuf,
    bin: PathBuf,
    inner: Mutex<Inner>,
    log: Mutex<EventLog>,
    hub: Mutex<Vec<Sender<Msg>>>,
    /// Process groups of running tests, killed on shutdown.
    test_pgids: Mutex<Vec<u32>>,
}

// ------------------------------------------------------------- errors

fn invalid_state(message: impl Into<String>) -> Error {
    err("invalid_state", message)
}

fn not_found(id: &str) -> Error {
    err(
        "not_found",
        format!("there is no generation {id}; `fawkes history` lists them"),
    )
}

/// Why `action` (test, switch or confirm) is not possible for a generation in this state.
fn refusal(g: &Generation, action: &str) -> Error {
    let id = &g.id;
    let state = g.state.as_str();
    invalid_state(match g.state {
        GenState::RolledBack => format!(
            "{id} was rolled back at {} ({}); stage the ref again for a new generation",
            g.rolled_back_at.as_deref().unwrap_or("?"),
            g.rollback_reason.as_deref().unwrap_or("no reason recorded")
        ),
        GenState::Staged if action == "switch" => {
            format!("{id} is staged, not tested; run `fawkes test {id}` first")
        }
        GenState::TestFailed if action == "switch" => {
            format!(
                "{id} failed its last test; run `fawkes test {id}` again or stage the ref again"
            )
        }
        GenState::Retired if action == "switch" => {
            format!("{id} is retired; run `fawkes test {id}` first")
        }
        GenState::Current | GenState::Permanent if action != "confirm" => {
            format!("{id} is {state}, it already is the live generation")
        }
        _ => format!("{id} is {state}, not pending; there is nothing to confirm"),
    })
}

fn arg<'a>(args: &'a Value, key: &str) -> R<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| err("usage", format!("missing argument `{key}`")))
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

// ------------------------------------------------------------ startup

impl Daemon {
    /// Run the daemon until SIGTERM or SIGINT.
    pub fn run(manifest_path: PathBuf, socket: Option<PathBuf>) -> Result<(), String> {
        let manifest = Manifest::load(&manifest_path)?;
        let paths = Paths {
            root: manifest.state_dir().to_path_buf(),
        };
        std::fs::create_dir_all(&paths.root)
            .map_err(|e| format!("cannot create {}: {e}", paths.root.display()))?;
        let socket = socket.unwrap_or_else(|| paths.socket());
        let listener = bind(&socket)?;
        let mut state = State::load(&paths.state_json())?;
        let stale_app = state.app_pid.take();
        state.daemon_pid = Some(std::process::id());
        let log = EventLog::open(paths.events())
            .map_err(|e| format!("cannot open the event log: {e}"))?;
        let bin = std::env::current_exe().map_err(|e| e.to_string())?;

        let daemon = Arc::new(Daemon {
            manifest_path,
            manifest: Mutex::new(Arc::new(manifest)),
            paths,
            socket,
            bin,
            inner: Mutex::new(Inner {
                state,
                app: None,
                app_failed: false,
                restart_at: None,
                last_exit: None,
                last_started: None,
                restarts: 0,
                busy: None,
                moving: None,
                rolling_back: false,
            }),
            log: Mutex::new(log),
            hub: Mutex::new(Vec::new()),
            test_pgids: Mutex::new(Vec::new()),
        });
        install_signal_handlers();
        daemon.event(
            "daemon",
            None,
            format!(
                "started, pid {}, socket {}",
                std::process::id(),
                daemon.socket.display()
            ),
        );

        // A crashed daemon leaves its app running (pid reuse is accepted as a prototype risk).
        if let Some(pid) = stale_app
            && process::group_alive(pid)
        {
            process::stop_group(pid, daemon.manifest().stop_timeout, None);
            daemon.event(
                "daemon",
                None,
                format!("stopped the app (pid {pid}) left by an earlier daemon"),
            );
        }
        daemon.save(&daemon.inner());

        if daemon.inner().state.pending.is_some() {
            let _busy = daemon
                .begin("rollback", Slot::Both)
                .map_err(|e| e.message)?;
            if let Err(e) = daemon.rollback_pending(&mut Out::none(), "daemon_restart") {
                daemon.event("error", None, e.message);
            }
        } else {
            daemon.start_permanent();
        }

        let watcher = daemon.clone();
        thread::spawn(move || {
            while !SHUTDOWN.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(100));
                watcher.tick();
            }
        });

        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        while !SHUTDOWN.load(Ordering::SeqCst) {
            if RELOAD.swap(false, Ordering::SeqCst) {
                daemon.reload_manifest();
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let daemon = daemon.clone();
                    thread::spawn(move || daemon.handle(stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    sleep(Duration::from_millis(20))
                }
                Err(e) => {
                    daemon.event("error", None, format!("accept failed: {e}"));
                }
            }
        }
        daemon.shutdown();
        Ok(())
    }

    fn start_permanent(self: &Arc<Self>) {
        let mut inner = self.inner();
        if let Some(id) = inner.state.permanent.clone()
            && let Err(e) = self.start_live(&mut inner, &id, Why::Fresh)
        {
            inner.app_failed = true;
            self.event("failed", Some(&id), e.message);
        }
    }

    fn reload_manifest(&self) {
        match Manifest::load(&self.manifest_path) {
            Ok(m) => {
                *self.manifest.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(m);
                self.event("daemon", None, "manifest reloaded");
            }
            Err(e) => {
                self.event("error", None, format!("manifest not reloaded: {e}"));
            }
        }
    }

    /// Stop the live app and any test and clean up; the generations keep their state.
    fn shutdown(&self) {
        let app = self.inner().app.take();
        if let Some(app) = app {
            self.stop_app(app);
        }
        for pgid in self
            .test_pgids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            process::kill_group(pgid);
        }
        let mut inner = self.inner();
        inner.state.daemon_pid = None;
        self.save(&inner);
        drop(inner);
        self.event("daemon", None, "stopped");
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Bind the socket; refuse when another daemon answers, replace a stale file.
fn bind(socket: &Path) -> Result<UnixListener, String> {
    if socket.as_os_str().len() >= 100 {
        return Err(format!(
            "socket path {} is too long (limit about 100 bytes); use a shorter state_dir",
            socket.display()
        ));
    }
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(format!(
                "another daemon already answers on {}",
                socket.display()
            ));
        }
        std::fs::remove_file(socket)
            .map_err(|e| format!("cannot remove stale socket {}: {e}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("cannot bind {}: {e}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    Ok(listener)
}

// ---------------------------------------------------- shared helpers

impl Daemon {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn manifest(&self) -> Arc<Manifest> {
        self.manifest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn save(&self, inner: &Inner) {
        if let Err(e) = inner.state.save(&self.paths.state_json()) {
            self.event("error", None, format!("cannot write state.json: {e}"));
        }
    }

    fn broadcast(&self, msg: Msg) {
        self.hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|tx| tx.send(msg.clone()).is_ok());
    }

    /// Record an event: stderr, events.jsonl, every subscriber. Anything but progress also changes the picture.
    fn event(&self, kind: &str, generation: Option<&str>, message: impl Into<String>) -> Event {
        let event = Event {
            at: now(),
            kind: kind.into(),
            generation: generation.map(String::from),
            message: message.into(),
        };
        eprintln!(
            "{} {} {} {}",
            event.at,
            event.kind,
            event.generation.as_deref().unwrap_or("-"),
            event.message
        );
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(&event);
        self.broadcast(Msg::Event(event.clone()));
        if kind != "progress" {
            self.broadcast(Msg::Changed);
        }
        event
    }

    /// A progress line: logged, and sent to the client waiting for this step.
    fn say(&self, out: &mut Out, generation: &str, message: impl Into<String>) {
        let event = self.event("progress", Some(generation), message);
        out.event(&event);
    }

    fn begin(&self, step: &str, slot: Slot) -> R<Busy<'_>> {
        let mut inner = self.inner();
        let wants_step = matches!(slot, Slot::Step | Slot::Both);
        let wants_live = matches!(slot, Slot::Live | Slot::Both);
        let taken = if wants_step { inner.busy.clone() } else { None }.or(if wants_live {
            inner.moving.clone()
        } else {
            None
        });
        if let Some(other) = taken {
            return Err(err(
                "busy",
                format!("{other} is in progress; try again when it has finished"),
            ));
        }
        if wants_step {
            inner.busy = Some(step.into());
        }
        if wants_live {
            inner.moving = Some(step.into());
        }
        drop(inner);
        self.broadcast(Msg::Changed);
        Ok(Busy { daemon: self, slot })
    }

    fn vars(&self, port: u16, home: &Path, g: &Generation) -> Vars {
        Vars {
            port,
            home: home.to_path_buf(),
            code: g.dir.clone(),
            generation: g.id.clone(),
            state: self.paths.root.clone(),
        }
    }

    /// Environment of `start` and `health.command`: the manifest's `[env]` plus the FAWKES_* variables.
    fn app_env(&self, m: &Manifest, vars: &Vars, phase: &str) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = m
            .env
            .iter()
            .map(|(k, v)| (k.clone(), vars.expand(v)))
            .collect();
        env.push(("FAWKES_BIN".into(), self.bin.to_string_lossy().into()));
        env.push(("FAWKES_SOCKET".into(), self.socket.to_string_lossy().into()));
        env.push(("FAWKES_GENERATION".into(), vars.generation.clone()));
        env.push(("FAWKES_PHASE".into(), phase.into()));
        env
    }

    fn is_running(&self, pid: u32) -> bool {
        self.inner()
            .app
            .as_ref()
            .is_some_and(|a| a.child.id() == pid)
    }

    /// Start generation `id` on the live port. The caller holds `inner`, so nothing sees a half-started app.
    fn start_live(self: &Arc<Self>, inner: &mut Inner, id: &str, why: Why) -> R<()> {
        let m = self.manifest();
        let g = inner.state.find(id).cloned().ok_or_else(|| not_found(id))?;
        let vars = self.vars(m.port, &m.home, &g);
        let env = self.app_env(&m, &vars, "live");
        std::fs::create_dir_all(&m.home)?;
        let child = process::spawn(
            &vars.expand(&m.start),
            &g.dir,
            &env,
            &self.paths.app_log(id),
            &format!("start {id} live port {}", m.port),
        )
        .map_err(|e| err("start_failed", format!("cannot start {id}: {e}")))?;
        let pid = child.id();

        if why == Why::Fresh {
            if inner.last_started.as_deref() != Some(id) {
                inner.restarts = 0;
            }
        } else {
            inner.restarts += 1;
        }
        if why != Why::Crash
            && let Some(g) = inner.state.find_mut(id)
        {
            g.failed_starts = 0;
        }
        inner.last_started = Some(id.into());
        inner.app = Some(App {
            child,
            generation: id.into(),
            started: Instant::now(),
            started_at: now(),
        });
        inner.state.app_pid = Some(pid);
        inner.app_failed = false;
        inner.restart_at = None;
        self.save(inner);
        self.point_current_link(&g.dir);
        self.event("started", Some(id), format!("pid {pid} on port {}", m.port));

        // Health is reported, never acted on: only `confirm` makes a generation permanent.
        let daemon = self.clone();
        let id = id.to_string();
        thread::spawn(move || {
            let check = m.health_check(&vars);
            let result = process::wait_healthy(
                &check,
                &vars.code,
                &env,
                m.health.interval,
                m.health.timeout,
                || daemon.is_running(pid),
            );
            match result {
                Ok(()) => daemon.event("healthy", Some(&id), "health check passed"),
                Err(e) if daemon.is_running(pid) => daemon.event("unhealthy", Some(&id), e),
                Err(_) => return,
            };
        });
        Ok(())
    }

    /// `<state_dir>/current` -> the live worktree, replaced atomically.
    fn point_current_link(&self, code: &Path) {
        let tmp = self.paths.root.join("current.tmp");
        let _ = std::fs::remove_file(&tmp);
        if std::os::unix::fs::symlink(code, &tmp).is_ok() {
            let _ = std::fs::rename(&tmp, self.paths.current_link());
        }
    }

    /// Stop an app Fawkes owns and record that Fawkes asked for it.
    fn stop_app(&self, mut app: App) {
        let m = self.manifest();
        let status = process::stop_group(app.child.id(), m.stop_timeout, Some(&mut app.child));
        let exit = LastExit::new(status, true);
        let mut inner = self.inner();
        inner.state.app_pid = None;
        inner.last_exit = Some(exit);
        self.save(&inner);
    }

    fn process_value(&self, inner: &Inner) -> Value {
        let (state, generation, pid, started_at, uptime) = match &inner.app {
            Some(app) => (
                "running",
                Some(app.generation.clone()),
                Some(app.child.id()),
                Some(app.started_at.clone()),
                Some((app.started.elapsed().as_secs_f64() * 10.0).round() / 10.0),
            ),
            None => (
                if inner.app_failed {
                    "failed"
                } else {
                    "stopped"
                },
                inner.state.current().map(String::from),
                None,
                None,
                None,
            ),
        };
        json!({
            "state": state, "generation": generation, "pid": pid, "port": self.manifest().port,
            "started_at": started_at, "uptime": uptime, "restarts": inner.restarts, "last_exit": inner.last_exit,
        })
    }

    fn status(&self) -> Value {
        let inner = self.inner();
        json!({
            "name": self.manifest().name,
            "daemon_pid": std::process::id(),
            "permanent": inner.state.permanent,
            "current": inner.state.current(),
            "pending": inner.state.pending,
            "busy": inner.busy.as_ref().or(inner.moving.as_ref()),
            "process": self.process_value(&inner),
        })
    }

    fn history(&self) -> Value {
        json!({ "generations": self.inner().state.generations })
    }
}

// ----------------------------------------------------- request handling

impl Daemon {
    fn handle(self: Arc<Self>, stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let read = stream.try_clone().and_then(|s| {
            use std::io::BufRead;
            std::io::BufReader::new(s).read_line(&mut line)
        });
        let mut out = Out::new(stream);
        let result = match read {
            Ok(n) if n > 0 => match serde_json::from_str::<Request>(&line) {
                Ok(request) => self.dispatch(&mut out, request),
                Err(e) => Err(err("usage", format!("malformed request: {e}"))),
            },
            _ => Err(err("usage", "no request received")),
        };
        out.finish(result);
    }

    fn dispatch(self: &Arc<Self>, out: &mut Out, request: Request) -> R<Value> {
        let args = &request.args;
        match request.command.as_str() {
            "start" => self.start_command(),
            "stop" => self.stop_command(out),
            "restart" => self.restart_command(out),
            "stage" => self.stage(out, arg(args, "ref")?),
            "test" => self.test(out, arg(args, "generation")?),
            "switch" => self.switch(
                out,
                arg(args, "generation")?,
                args.get("deadline").and_then(Value::as_u64),
            ),
            "confirm" => self.confirm(args.get("generation").and_then(Value::as_str)),
            "rollback" => {
                let _busy = self.begin("rollback", Slot::Both)?;
                self.rollback_pending(out, "rollback requested")
            }
            "status" => Ok(self.status()),
            "history" => Ok(self.history()),
            "log" => {
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                if args.get("follow").and_then(Value::as_bool) == Some(true) {
                    self.stream(out, Some(limit), false);
                    return Ok(Value::Null);
                }
                Ok(
                    json!({ "events": self.log.lock().unwrap_or_else(|e| e.into_inner()).tail(limit) }),
                )
            }
            "subscribe" => {
                self.stream(out, None, true);
                Ok(Value::Null)
            }
            other => Err(err("usage", format!("unknown command `{other}`"))),
        }
    }

    /// Send events (and, for `subscribe`, status and history) to a client until it closes.
    fn stream(&self, out: &mut Out, tail: Option<usize>, snapshots: bool) {
        let (tx, rx) = mpsc::channel();
        self.hub.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        let mut ok = true;
        if let Some(n) = tail {
            let events = self.log.lock().unwrap_or_else(|e| e.into_inner()).tail(n);
            ok = events.iter().all(|e| out.event(e));
        }
        ok = ok && (!snapshots || self.send_snapshot(out));
        while ok && !SHUTDOWN.load(Ordering::SeqCst) {
            let first = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    ok = !out.closed();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let mut changed = false;
            for msg in std::iter::once(first).chain(rx.try_iter()) {
                match msg {
                    Msg::Event(event) => ok = ok && out.event(&event),
                    Msg::Changed => changed = true,
                }
            }
            ok = ok && (!snapshots || !changed || self.send_snapshot(out));
        }
    }

    fn send_snapshot(&self, out: &mut Out) -> bool {
        out.line(&json!({ "status": self.status() }))
            && out.line(&json!({ "history": self.history() }))
    }
}

// ------------------------------------------------------ the supervisor

impl Daemon {
    /// One supervision pass: reap the app, roll back a pending switch that is dead or overdue, restart a crash.
    fn tick(self: &Arc<Self>) {
        let rollback_reason = {
            let mut inner = self.inner();
            if SHUTDOWN.load(Ordering::SeqCst) {
                return;
            }
            self.reap(&mut inner);
            match &inner.state.pending {
                Some(pending) if now_secs() >= parse_time(&pending.deadline).unwrap_or(0) => {
                    Some("missed confirm deadline".to_string())
                }
                Some(_) if inner.app.is_none() => inner
                    .last_exit
                    .as_ref()
                    .filter(|x| !x.requested)
                    .map(|x| format!("exited before confirm ({})", x.describe())),
                Some(_) => None,
                None => {
                    let due = inner.restart_at.is_some_and(|at| Instant::now() >= at);
                    if due
                        && inner.app.is_none()
                        && !inner.app_failed
                        && inner.moving.is_none()
                        && let Some(id) = inner.state.permanent.clone()
                        && let Err(e) = self.start_live(&mut inner, &id, Why::Crash)
                    {
                        inner.app_failed = true;
                        self.event("failed", Some(&id), e.message);
                    }
                    None
                }
            }
        };
        if let Some(reason) = rollback_reason
            && let Ok(_busy) = self.begin("rollback", Slot::Both)
        {
            match self.rollback_pending(&mut Out::none(), &reason) {
                Ok(_) => {}
                Err(e) if e.code == "invalid_state" => {}
                Err(e) => {
                    self.event("error", None, e.message);
                }
            }
        }
    }

    /// If the app exited without being asked to, count it and schedule its restart.
    fn reap(&self, inner: &mut Inner) {
        let Some(status) = inner
            .app
            .as_mut()
            .and_then(|a| a.child.try_wait().ok().flatten())
        else {
            return;
        };
        let app = inner.app.take().expect("checked above");
        let m = self.manifest();
        let ran = app.started.elapsed().as_secs_f64();
        let exit = LastExit::new(Some(status), false);
        inner.state.app_pid = None;
        let pending = inner
            .state
            .pending
            .as_ref()
            .is_some_and(|p| p.generation == app.generation);
        let failed_starts = inner.state.find_mut(&app.generation).map_or(0, |g| {
            g.failed_starts = if ran < m.min_uptime {
                g.failed_starts + 1
            } else {
                0
            };
            g.failed_starts
        });
        let message = format!("{} after {ran:.1}s", exit.describe());
        inner.last_exit = Some(exit);
        if pending {
            // A pending generation gets no retries; tick rolls it back.
        } else if failed_starts >= m.max_failed_starts {
            inner.app_failed = true;
            self.event("exited", Some(&app.generation), message);
            self.event(
                "failed",
                Some(&app.generation),
                format!("{failed_starts} failed starts in a row; not restarting"),
            );
            self.save(inner);
            return;
        } else {
            let backoff = (1u64 << failed_starts.saturating_sub(1).min(5)).min(30);
            inner.restart_at = Some(Instant::now() + Duration::from_secs(backoff));
            self.event(
                "exited",
                Some(&app.generation),
                format!("{message}; restarting in {backoff}s"),
            );
            self.save(inner);
            return;
        }
        self.event("exited", Some(&app.generation), message);
        self.save(inner);
    }
}

// -------------------------------------------------- lifecycle commands

impl Daemon {
    fn current_generation(&self, inner: &Inner) -> R<String> {
        inner.state.current().map(String::from).ok_or_else(|| {
            invalid_state(
                "there is no generation to run yet; `fawkes stage`, `test` and `switch` one first",
            )
        })
    }

    fn start_command(self: &Arc<Self>) -> R<Value> {
        let _busy = self.begin("start", Slot::Live)?;
        let mut inner = self.inner();
        let id = self.current_generation(&inner)?;
        if let Some(app) = &inner.app {
            return Err(invalid_state(format!(
                "{id} is already running (pid {})",
                app.child.id()
            )));
        }
        self.start_live(&mut inner, &id, Why::Fresh)?;
        Ok(self.process_value(&inner))
    }

    fn stop_command(self: &Arc<Self>, out: &mut Out) -> R<Value> {
        let _busy = self.begin("stop", Slot::Live)?;
        let app = {
            let mut inner = self.inner();
            let id = self.current_generation(&inner)?;
            inner.restart_at = None;
            inner.app.take().ok_or_else(|| {
                invalid_state(format!("{id} is not running; `fawkes start` starts it"))
            })?
        };
        self.say(out, &app.generation, format!("stopping {}", app.generation));
        let id = app.generation.clone();
        self.stop_app(app);
        self.event("stopped", Some(&id), "stopped on request");
        Ok(self.process_value(&self.inner()))
    }

    fn restart_command(self: &Arc<Self>, out: &mut Out) -> R<Value> {
        let _busy = self.begin("restart", Slot::Live)?;
        let (id, app) = {
            let mut inner = self.inner();
            inner.restart_at = None;
            (self.current_generation(&inner)?, inner.app.take())
        };
        if let Some(app) = app {
            self.say(out, &id, format!("stopping {id}"));
            self.stop_app(app);
        }
        let mut inner = self.inner();
        self.start_live(&mut inner, &id, Why::Restart)?;
        Ok(self.process_value(&inner))
    }
}

// -------------------------------------------------------- transactions

impl Daemon {
    fn stage(self: &Arc<Self>, out: &mut Out, git_ref: &str) -> R<Value> {
        let _busy = self.begin("stage", Slot::Step)?;
        let m = self.manifest();
        let commit = git(
            &m.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{git_ref}^{{commit}}"),
            ],
        )
        .map_err(|e| {
            err(
                "git_failed",
                format!(
                    "`{git_ref}` is not a commit, branch or tag in {}{}",
                    m.repo.display(),
                    if e.is_empty() {
                        String::new()
                    } else {
                        format!(" ({e})")
                    }
                ),
            )
        })?;
        let (id, reuse_from) = {
            let mut inner = self.inner();
            inner.state.next_id += 1;
            let id = format!("g{}", inner.state.next_id);
            let reuse_from = inner
                .state
                .permanent
                .as_ref()
                .map_or(m.repo.clone(), |p| self.paths.code(p));
            self.save(&inner);
            (id, reuse_from)
        };
        let code = self.paths.code(&id);
        std::fs::create_dir_all(self.paths.generation(&id))?;

        self.say(
            out,
            &id,
            format!(
                "checking out {} at {}",
                git_ref,
                &commit[..commit.len().min(8)]
            ),
        );
        let code_arg = code.to_string_lossy().into_owned();
        git(
            &m.repo,
            &["worktree", "add", "--detach", &code_arg, &commit],
        )
        .map_err(|e| {
            err(
                "git_failed",
                format!("cannot create the worktree for {id}: {e}"),
            )
        })?;

        for dir in &m.reuse {
            let (from, to) = (reuse_from.join(dir), code.join(dir));
            if from.exists() && !to.exists() {
                self.say(
                    out,
                    &id,
                    format!("cloning {dir} from {}", reuse_from.display()),
                );
                if let Err(e) = clone_directory(&from, &to) {
                    self.say(
                        out,
                        &id,
                        format!("cloning {dir} failed ({e}); install starts without it"),
                    );
                    let _ = std::fs::remove_dir_all(&to);
                }
            }
        }

        if let Some(install) = &m.install {
            let vars = Vars {
                port: m.port,
                home: m.home.clone(),
                code: code.clone(),
                generation: id.clone(),
                state: self.paths.root.clone(),
            };
            let log = self.paths.install_log(&id);
            self.say(out, &id, format!("installing: {}", vars.expand(install)));
            let result = run_install(&vars.expand(install), &code, &log);
            if let Err(e) = result {
                let _ = git(&m.repo, &["worktree", "remove", "--force", &code_arg]);
                return Err(err(
                    "install_failed",
                    format!("install for {id} failed ({e}); see {}", log.display()),
                ));
            }
        }

        let generation = Generation {
            id: id.clone(),
            git_ref: git_ref.into(),
            commit,
            state: GenState::Staged,
            dir: code,
            staged_at: now(),
            test: None,
            switched_at: None,
            confirmed_at: None,
            rolled_back_at: None,
            rollback_reason: None,
            failed_starts: 0,
        };
        let record = json!(generation);
        let mut inner = self.inner();
        inner.state.generations.push(generation);
        self.save(&inner);
        self.event("staged", Some(&id), format!("{git_ref} staged as {id}"));
        Ok(record)
    }

    fn test(self: &Arc<Self>, out: &mut Out, id: &str) -> R<Value> {
        let _busy = self.begin(&format!("test {id}"), Slot::Step)?;
        let m = self.manifest();
        let g = {
            let inner = self.inner();
            let g = inner.state.find(id).ok_or_else(|| not_found(id))?;
            if !matches!(
                g.state,
                GenState::Staged | GenState::TestFailed | GenState::Tested | GenState::Retired
            ) {
                return Err(refusal(g, "test"));
            }
            g.clone()
        };
        let port = (m.spare_ports[0]..=m.spare_ports[1])
            .find(|&p| p != m.port && TcpListener::bind(("127.0.0.1", p)).is_ok())
            .ok_or_else(|| {
                err(
                    "start_failed",
                    format!(
                        "no free spare port in {}..{}",
                        m.spare_ports[0], m.spare_ports[1]
                    ),
                )
            })?;
        let test_home = self.paths.test_home(id);
        let _ = std::fs::remove_dir_all(&test_home);
        std::fs::create_dir_all(&test_home)?;
        storage::copy(&m.home, &m.storage, &test_home).map_err(|e| err("storage_failed", e))?;

        self.say(
            out,
            id,
            format!("starting {id} on spare port {port} with a copy of the storage"),
        );
        let started = Instant::now();
        let vars = self.vars(port, &test_home, &g);
        let env = self.app_env(&m, &vars, "test");
        let log = self.paths.app_log(id);
        let mut child = process::spawn(
            &vars.expand(&m.start),
            &g.dir,
            &env,
            &log,
            &format!("start {id} test port {port}"),
        )
        .map_err(|e| {
            err(
                "start_failed",
                format!("cannot start {id} for its test: {e}"),
            )
        })?;
        let pgid = child.id();
        self.test_pgids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pgid);

        let check = m.health_check(&vars);
        let result = process::wait_healthy(
            &check,
            &g.dir,
            &env,
            m.health.interval,
            m.health.timeout,
            || matches!(child.try_wait(), Ok(None)),
        );
        process::stop_group(pgid, m.stop_timeout, Some(&mut child));
        self.test_pgids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|p| *p != pgid);
        let _ = std::fs::remove_dir_all(&test_home);

        let passed = result.is_ok();
        let info = TestInfo {
            port,
            passed,
            at: now(),
            seconds: (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            message: result
                .as_ref()
                .map_or_else(Clone::clone, |_| "healthy".into()),
        };
        let mut inner = self.inner();
        let g = inner
            .state
            .find_mut(id)
            .expect("generations are never removed");
        g.state = if passed {
            GenState::Tested
        } else {
            GenState::TestFailed
        };
        g.test = Some(info.clone());
        let record = json!(g);
        self.save(&inner);
        match result {
            Ok(()) => {
                self.event(
                    "tested",
                    Some(id),
                    format!("test passed on port {port} in {}s", info.seconds),
                );
                Ok(record)
            }
            Err(e) => {
                self.event("test_failed", Some(id), &e);
                Err(err(
                    "health_failed",
                    format!(
                        "{id} failed its test: {e}; see {}; fix the ref and stage it again, or `fawkes test {id}` to retry",
                        log.display()
                    ),
                ))
            }
        }
    }

    fn switch(self: &Arc<Self>, out: &mut Out, id: &str, deadline: Option<u64>) -> R<Value> {
        let _busy = self.begin(&format!("switch {id}"), Slot::Both)?;
        let m = self.manifest();
        let previous = {
            let inner = self.inner();
            if let Some(pending) = &inner.state.pending {
                return Err(invalid_state(format!(
                    "{} is still waiting for confirm; `fawkes confirm` or `fawkes rollback` first",
                    pending.generation
                )));
            }
            let g = inner.state.find(id).ok_or_else(|| not_found(id))?;
            if g.state != GenState::Tested {
                return Err(refusal(g, "switch"));
            }
            inner.state.permanent.clone()
        };

        // Stop first: no write of the old generation can land after the snapshot.
        let old = {
            let mut inner = self.inner();
            inner.restart_at = None;
            inner.app.take()
        };
        let had_app = old.is_some();
        if let Some(app) = old {
            self.say(out, id, format!("stopping {}", app.generation));
            self.stop_app(app);
        }
        let resume = |daemon: &Arc<Self>| {
            if let (true, Some(previous)) = (had_app, &previous) {
                let mut inner = daemon.inner();
                if let Err(e) = daemon.start_live(&mut inner, previous, Why::Fresh) {
                    inner.app_failed = true;
                    daemon.event("failed", Some(previous), e.message);
                }
            }
        };

        self.say(out, id, "snapshotting storage");
        if let Err(e) = storage::snapshot(&m.home, &m.storage, &self.paths.snapshot(id)) {
            resume(self);
            return Err(err(
                "storage_failed",
                format!("{e}; {id} stays tested and nothing changed"),
            ));
        }

        let deadline = state::format_time(state::now_secs() + 1 + deadline.unwrap_or(m.deadline));
        self.say(
            out,
            id,
            format!(
                "starting {id} on port {}; confirm before {deadline}",
                m.port
            ),
        );
        let mut inner = self.inner();
        if let Err(e) = self.start_live(&mut inner, id, Why::Fresh) {
            drop(inner);
            resume(self);
            return Err(e);
        }
        let g = inner.state.find_mut(id).expect("checked above");
        g.state = GenState::Current;
        g.switched_at = Some(now());
        inner.state.pending = Some(Pending {
            generation: id.into(),
            previous: previous.clone(),
            deadline: deadline.clone(),
        });
        self.save(&inner);
        drop(inner);
        self.event(
            "switched",
            Some(id),
            format!(
                "switched from {}; confirm before {deadline}",
                previous.as_deref().unwrap_or("nothing")
            ),
        );
        Ok(json!({ "generation": id, "previous": previous, "deadline": deadline }))
    }

    fn confirm(&self, id: Option<&str>) -> R<Value> {
        let mut inner = self.inner();
        if inner.rolling_back {
            return Err(invalid_state(
                "the pending switch is being rolled back and can no longer be confirmed",
            ));
        }
        let Some(pending) = inner.state.pending.clone() else {
            return Err(match id {
                Some(id) => inner
                    .state
                    .find(id)
                    .map_or_else(|| not_found(id), |g| refusal(g, "confirm")),
                None => invalid_state("no switch is pending; there is nothing to confirm"),
            });
        };
        if let Some(id) = id
            && id != pending.generation
        {
            return Err(match inner.state.find(id) {
                Some(g) if g.state == GenState::RolledBack => refusal(g, "confirm"),
                Some(g) => invalid_state(format!(
                    "{id} is {}, not pending; {} is the one waiting for confirm",
                    g.state.as_str(),
                    pending.generation
                )),
                None => not_found(id),
            });
        }
        if let Some(old) = inner.state.permanent.clone()
            && let Some(g) = inner.state.find_mut(&old)
        {
            g.state = GenState::Retired;
        }
        let g = inner
            .state
            .find_mut(&pending.generation)
            .expect("pending generations exist");
        g.state = GenState::Permanent;
        g.confirmed_at = Some(now());
        let record = json!(g);
        inner.state.permanent = Some(pending.generation.clone());
        inner.state.pending = None;
        self.save(&inner);
        self.event(
            "confirmed",
            Some(&pending.generation),
            format!("{} is permanent", pending.generation),
        );
        Ok(record)
    }

    /// Roll the pending generation back (the caller holds the rollback slots).
    fn rollback_pending(self: &Arc<Self>, out: &mut Out, reason: &str) -> R<Value> {
        let m = self.manifest();
        let (pending, app) = {
            let mut inner = self.inner();
            let pending = inner.state.pending.clone().ok_or_else(|| {
                invalid_state("no switch is pending, so there is nothing to roll back; to go back past a confirm, `test` and `switch` to a retired generation")
            })?;
            inner.rolling_back = true;
            inner.restart_at = None;
            (pending, inner.app.take())
        };
        let id = pending.generation;
        self.say(out, &id, format!("rolling back {id}: {reason}"));
        if let Some(app) = app {
            self.stop_app(app);
        }
        let restored = storage::restore(&m.home, &self.paths.snapshot(&id));

        let mut inner = self.inner();
        if let Some(g) = inner.state.find_mut(&id) {
            g.state = GenState::RolledBack;
            g.rolled_back_at = Some(now());
            g.rollback_reason = Some(reason.into());
        }
        inner.state.pending = None;
        inner.rolling_back = false;
        let permanent = inner.state.permanent.clone();
        self.save(&inner);
        if let Err(e) = restored {
            // Starting the permanent generation on storage in an unknown state could do more harm.
            inner.app_failed = true;
            self.event(
                "rolled_back",
                Some(&id),
                format!("{reason}; storage restore failed"),
            );
            return Err(err(
                "storage_failed",
                format!(
                    "{id} was rolled back but its storage could not be restored: {e}; the app stays stopped"
                ),
            ));
        }
        self.event(
            "rolled_back",
            Some(&id),
            format!(
                "{reason}; back to {}",
                permanent.as_deref().unwrap_or("nothing")
            ),
        );
        if let Some(permanent) = &permanent
            && let Err(e) = self.start_live(&mut inner, permanent, Why::Fresh)
        {
            inner.app_failed = true;
            self.event("failed", Some(permanent), e.message);
        }
        Ok(json!({ "from": id, "to": permanent, "reason": reason }))
    }
}

// ------------------------------------------------------ small helpers

/// Copy-on-write clone of a directory (APFS `clonefile` on macOS, reflinks elsewhere).
fn clone_directory(from: &Path, to: &Path) -> Result<(), String> {
    let flags: &[&str] = if cfg!(target_os = "macos") {
        &["-cR"]
    } else {
        &["-R", "--reflink=auto"]
    };
    let status = Command::new("cp")
        .args(flags)
        .arg(from)
        .arg(to)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cp {status}"))
    }
}

/// Run the install command in the worktree with Fawkes's own environment.
fn run_install(command: &str, code: &Path, log: &Path) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| e.to_string())?;
    let _ = writeln!(file, "=== {} install: {command} ===", now());
    let status = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(code)
        .stdin(Stdio::null())
        .stdout(file.try_clone().map_err(|e| e.to_string())?)
        .stderr(file)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(status.to_string())
    }
}
