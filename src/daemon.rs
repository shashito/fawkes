//! The daemon: socket server, supervision loop and the transactions (stage, test, switch, confirm, rollback).
//!
//! Locking: `inner` holds everything that changes (state.json, the live process, the busy slots). It is never held
//! across a slow step; stages, tests and stops run unlocked and take `inner` again to record their outcome.
//! Lock order is `inner` before `manifest` before `log` before `hub`, and nothing waits for another thread while
//! holding one.
//!
//! Panics abort the process (`Cargo.toml`): the init system restarts the daemon and its startup recovers a pending
//! switch, so no thread ever carries on past a broken invariant and no mutex is ever poisoned.

use crate::manifest::{Manifest, Vars};
use crate::process;
use crate::protocol::{self, Error, Out, R, Request, err};
use crate::state::{
    self, Event, EventLog, GenState, Generation, LastExit, Paths, Pending, State, TestInfo, now,
    now_secs, parse_time,
};
use crate::storage;
use rustix::fs::{FlockOperation, Mode, flock};
use serde_json::{Value, json};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::fs::{File, OpenOptions};
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

/// Lock a mutex. Under `panic = "abort"` no mutex is ever poisoned, so the error arm cannot run.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|_| std::process::abort())
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
    /// A rollback of the pending generation is decided or under way, with its reason: it can no longer be confirmed.
    /// Set in the same critical section that learns why (a crash, the deadline, a failed health check), so no
    /// confirm fits between the decision and the rollback.
    rollback: Option<String>,
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

/// The longest request line a client may send, in bytes.
const MAX_REQUEST: u64 = 64 * 1024;

/// How many messages a subscriber may fall behind before it is dropped.
const SUBSCRIBER_QUEUE: usize = 1024;

/// The subscribers' queues. Bounded: a subscriber that falls `SUBSCRIBER_QUEUE` messages behind (its client stopped
/// reading) is dropped, which ends its stream, instead of growing the daemon's memory for as long as it lives.
struct Hub(Mutex<Vec<SyncSender<Msg>>>);

impl Hub {
    fn join(&self) -> Receiver<Msg> {
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        lock(&self.0).push(tx);
        rx
    }

    fn send(&self, msg: &Msg) {
        lock(&self.0).retain(|tx| tx.try_send(msg.clone()).is_ok());
    }
}

pub struct Daemon {
    /// Set by SIGTERM and SIGINT.
    shutdown: Arc<AtomicBool>,
    /// Set by SIGHUP.
    reload: Arc<AtomicBool>,
    manifest_path: PathBuf,
    manifest: Mutex<Arc<Manifest>>,
    paths: Paths,
    socket: PathBuf,
    bin: PathBuf,
    inner: Mutex<Inner>,
    log: Mutex<EventLog>,
    hub: Hub,
    /// Process groups of running tests and installs, killed on shutdown.
    step_pgids: Mutex<Vec<u32>>,
    /// `<fawkes_home>/run/<name>.sock`, removed on shutdown.
    run_link: PathBuf,
}

// ------------------------------------------------------------- errors

fn restore_failed(id: &str) -> Error {
    invalid_state(format!(
        "{id} was rolled back but its storage could not be restored; `fawkes rollback` retries the restore"
    ))
}

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
        GenState::Staging => {
            format!("{id} is still being staged; try again when its stage has finished")
        }
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
        // First of all, so a SIGTERM during startup still ends in `shutdown`'s cleanup. signal-hook installs them
        // with sigaction and SA_RESTART; the handlers only set a flag.
        let shutdown = Arc::new(AtomicBool::new(false));
        let reload = Arc::new(AtomicBool::new(false));
        for (signal, flag) in [(SIGTERM, &shutdown), (SIGINT, &shutdown), (SIGHUP, &reload)] {
            signal_hook::flag::register(signal, flag.clone())
                .map_err(|e| format!("cannot install the signal handlers: {e}"))?;
        }
        let manifest = Manifest::load(&manifest_path)?;
        let paths = Paths {
            root: manifest.state_dir().to_path_buf(),
        };
        std::fs::create_dir_all(&paths.root)
            .map_err(|e| format!("cannot create {}: {e}", paths.root.display()))?;
        let _lock = lock_state_dir(&paths)?;
        log_panics(paths.events());
        let socket = std::path::absolute(socket.unwrap_or_else(|| paths.socket()))
            .map_err(|e| e.to_string())?;
        let listener = bind(&socket)?;
        let run_link = register(&manifest.name, &socket).inspect_err(|_| {
            let _ = std::fs::remove_file(&socket);
        })?;
        let (mut state, recovered) = State::load(&paths.state_json())?;
        let stale_app = state.app_pid.take();
        state.daemon_pid = Some(std::process::id());
        let log = EventLog::open(paths.events())
            .map_err(|e| format!("cannot open the event log: {e}"))?;
        let bin = std::env::current_exe().map_err(|e| e.to_string())?;

        let daemon = Arc::new(Daemon {
            shutdown,
            reload,
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
                rollback: None,
            }),
            log: Mutex::new(log),
            hub: Hub(Mutex::new(Vec::new())),
            step_pgids: Mutex::new(Vec::new()),
            run_link,
        });
        daemon.event(
            "daemon",
            None,
            format!(
                "started, pid {}, socket {}",
                std::process::id(),
                daemon.socket.display()
            ),
        );
        if let Some(warning) = recovered {
            daemon.event("error", None, warning);
        }

        // A crashed daemon leaves its app running (pid reuse is accepted as a prototype risk). The state lock
        // proves that daemon is gone, so the app it recorded has no other supervisor.
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
        daemon.sweep_generations();

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
            while !watcher.shutting_down() {
                sleep(Duration::from_millis(100));
                watcher.tick();
            }
        });

        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        while !daemon.shutting_down() {
            if daemon.reload.swap(false, Ordering::SeqCst) {
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

    fn shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// SIGHUP: apply what may change while the daemon runs (`Manifest::reload`), or keep the old manifest whole.
    fn reload_manifest(&self) {
        let new = match Manifest::load(&self.manifest_path) {
            Ok(new) => new,
            Err(e) => {
                self.event("error", None, format!("manifest not reloaded: {e}"));
                return;
            }
        };
        // Held across the swap, so no app starts between the check of `port` and the swap.
        let inner = self.inner();
        match self.manifest().reload(&new, inner.app.is_some()) {
            Ok(changed) if changed.is_empty() => {
                self.event("daemon", None, "manifest reloaded, nothing changed");
            }
            Ok(changed) => {
                *lock(&self.manifest) = Arc::new(new);
                self.event(
                    "daemon",
                    None,
                    format!("manifest reloaded, changed: {}", changed.join(", ")),
                );
            }
            Err(e) => {
                self.event("error", None, format!("manifest not reloaded: {e}"));
            }
        }
    }

    /// Stop the live app and any test and clean up; the generations keep their state.
    fn shutdown(&self) {
        // A switch, rollback or restart in flight finishes first (start_live refuses from now on), so no app it
        // starts outlives the daemon.
        let give_up =
            Instant::now() + Duration::from_secs_f64(2.0 * self.manifest().stop_timeout + 5.0);
        while self.inner().moving.is_some() && Instant::now() < give_up {
            sleep(Duration::from_millis(20));
        }
        let app = self.inner().app.take();
        if let Some(app) = app {
            self.stop_app(app);
        }
        for pgid in lock(&self.step_pgids).drain(..) {
            process::kill_group(pgid);
        }
        let mut inner = self.inner();
        inner.state.daemon_pid = None;
        self.save(&inner);
        drop(inner);
        self.event("daemon", None, "stopped");
        let _ = std::fs::remove_file(&self.socket);
        if std::fs::read_link(&self.run_link).is_ok_and(|target| target == self.socket) {
            let _ = std::fs::remove_file(&self.run_link);
        }
    }
}

/// Link the socket as `<fawkes_home>/run/<name>.sock`, where `fawkes tui` finds every daemon.
fn register(name: &str, socket: &Path) -> Result<PathBuf, String> {
    let dir = crate::manifest::fawkes_home()?.join("run");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let link = dir.join(format!("{name}.sock"));
    if std::fs::read_link(&link).is_ok_and(|target| target != socket)
        && UnixStream::connect(&link).is_ok()
    {
        return Err(format!(
            "another daemon named {name} already runs ({} answers); give this app another name",
            link.display()
        ));
    }
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(socket, &link)
        .map_err(|e| format!("cannot link {}: {e}", link.display()))?;
    Ok(link)
}

/// Take the exclusive `flock` on `<state_dir>/lock`, held as long as the returned file lives: one daemon per state
/// directory, decided by the kernel. It ends with the process however the process ends, so it is never stale, and
/// std opens files close-on-exec, so no app inherits it.
fn lock_state_dir(paths: &Paths) -> Result<File, String> {
    let path = paths.lock();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(format!(
            "another daemon already runs for {}; stop it first, or give this app another state_dir",
            paths.root.display()
        )),
        Err(e) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// Append an `error` event for every panic before the default hook reports it and the process aborts.
fn log_panics(events: PathBuf) {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A file of its own: the panicking thread may hold the daemon's event log.
        if let Ok(mut log) = EventLog::open(events.clone()) {
            log.append(&Event {
                at: now(),
                kind: "error".into(),
                generation: None,
                message: format!("panic: {info}; the daemon aborts, and its next start recovers"),
            });
        }
        report(info);
    }));
}

/// Bind the socket. The caller holds the state lock, so a socket file nobody answers on belongs to a dead daemon.
fn bind(socket: &Path) -> Result<UnixListener, String> {
    if socket.as_os_str().len() >= 100 {
        return Err(format!(
            "socket path {} is too long (limit about 100 bytes); use a shorter state_dir",
            socket.display()
        ));
    }
    // Only a daemon of another state directory can answer here (a `--socket` shared by two manifests): never take
    // its socket over.
    if UnixStream::connect(socket).is_ok() {
        return Err(format!(
            "another daemon already answers on {}; give this app another --socket",
            socket.display()
        ));
    }
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot remove the old socket {}: {e}",
                socket.display()
            ));
        }
    }
    // Created 0600 from the start: no moment in which the umask's permissions apply. The umask is process-wide, so
    // this must run before the daemon starts its first thread.
    let umask = rustix::process::umask(Mode::from_raw_mode(0o177));
    let bound = UnixListener::bind(socket);
    rustix::process::umask(umask);
    bound.map_err(|e| format!("cannot bind {}: {e}", socket.display()))
}

// ---------------------------------------------------- shared helpers

impl Daemon {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn manifest(&self) -> Arc<Manifest> {
        lock(&self.manifest).clone()
    }

    /// Save bookkeeping (the live pid, counters, a test result): a failure is reported and a later save repeats it.
    fn save(&self, inner: &Inner) {
        if let Err(e) = inner.state.save(&self.paths.state_json()) {
            self.event("error", None, format!("cannot write state.json: {e}"));
        }
    }

    /// Save the intent of a transaction step (a pending switch, a confirm, the end of a rollback). The step fails
    /// when this fails: a daemon restart acts on what is on disk, not on what was meant.
    fn persist(&self, state: &State) -> R<()> {
        state.save(&self.paths.state_json()).map_err(|e| {
            self.event("error", None, format!("cannot write state.json: {e}"));
            err(
                "internal",
                format!("cannot write {}: {e}", self.paths.state_json().display()),
            )
        })
    }

    /// The pending switch's deadline. An unreadable one counts as passed, so the switch rolls back, and says so.
    fn deadline_secs(&self, pending: &Pending) -> u64 {
        parse_time(&pending.deadline).unwrap_or_else(|| {
            self.event(
                "error",
                Some(&pending.generation),
                format!(
                    "the confirm deadline `{}` in state.json is unreadable; it counts as passed",
                    pending.deadline
                ),
            );
            0
        })
    }

    fn broadcast(&self, msg: Msg) {
        self.hub.send(&msg);
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
        lock(&self.log).append(&event);
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
        if self.shutting_down() {
            return Err(err("start_failed", "the daemon is shutting down"));
        }
        let m = self.manifest();
        let g = inner.state.find(id).cloned().ok_or_else(|| not_found(id))?;
        let vars = self.vars(m.port, &m.home, &g);
        let env = self.app_env(&m, &vars, "live");
        std::fs::create_dir_all(&m.home)?;
        let child = process::spawn(
            &vars.expand_command(&m.start),
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

        // Health never confirms: only `confirm` makes a generation permanent. With `rollback_on_unhealthy` a pending
        // generation that gives up is rolled back by the supervisor without waiting for its deadline.
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
                Err(e) if daemon.is_running(pid) => {
                    let event = daemon.event("unhealthy", Some(&id), e);
                    if m.rollback_on_unhealthy {
                        daemon.roll_back_unhealthy(&id, pid);
                    }
                    event
                }
                Err(_) => return,
            };
        });
        Ok(())
    }

    /// Record the rollback of the pending generation `id` whose process `pid` never became healthy, unless it has
    /// changed since (confirmed, restarted, already rolling back); the next supervision pass carries it out.
    fn roll_back_unhealthy(&self, id: &str, pid: u32) {
        let mut inner = self.inner();
        let pending = inner.state.pending.as_ref().is_some_and(|pending| {
            pending.generation == id && !inner.state.restore_failed(pending)
        });
        let running = inner.app.as_ref().is_some_and(|app| app.child.id() == pid);
        if pending && running && inner.rollback.is_none() {
            inner.rollback = Some("health check failed".into());
        }
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
        // Bounded, so a client cannot make the daemon allocate without limit.
        let read = stream.try_clone().and_then(|s| {
            use std::io::{BufRead, Read};
            std::io::BufReader::new(s.take(MAX_REQUEST)).read_line(&mut line)
        });
        let mut out = Out::new(stream);
        let result = match read {
            Ok(n) if n as u64 >= MAX_REQUEST && !line.ends_with('\n') => Err(err(
                "usage",
                format!("the request is longer than {MAX_REQUEST} bytes"),
            )),
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
            "switch" => self.switch(out, arg(args, "generation")?, protocol::deadline_arg(args)?),
            "confirm" => {
                let record = self.confirm(args.get("generation").and_then(Value::as_str))?;
                self.prune_soon();
                Ok(record)
            }
            "prune" => self.prune(out),
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
                Ok(json!({ "events": lock(&self.log).tail(limit) }))
            }
            "subscribe" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                self.stream(out, limit, true);
                Ok(Value::Null)
            }
            other => Err(err("usage", format!("unknown command `{other}`"))),
        }
    }

    /// Send events (and, for `subscribe`, status and history) to a client until it closes.
    fn stream(&self, out: &mut Out, tail: Option<usize>, snapshots: bool) {
        let rx = self.hub.join();
        let mut ok = true;
        if let Some(n) = tail {
            let events = lock(&self.log).tail(n);
            ok = events.iter().all(|e| out.event(e));
        }
        ok = ok && (!snapshots || self.send_snapshot(out));
        while ok && !self.shutting_down() {
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
    /// One supervision pass: reap the app, roll back a pending switch that is dead, overdue or unhealthy, restart a
    /// crash. Each of them moves the live process, so the pass holds the live slot like `stop` or `restart` do: no
    /// start, stop, switch or rollback runs while it cleans up after a crash. A stage or test never delays it.
    fn tick(self: &Arc<Self>) {
        let Some(step) = self.due() else { return };
        // Busy: the step holding the slot deals with the live process itself, or the next pass does.
        let Ok(_busy) = self.begin(step, Slot::Live) else {
            return;
        };
        let (stragglers, rollback) = {
            let mut inner = self.inner();
            if self.shutting_down() {
                return;
            }
            let stragglers = self.reap(&mut inner);
            (stragglers, self.decide_rollback(&mut inner))
        };
        // The leader is gone; end the rest of its group too, so nothing of it lives on unsupervised or keeps the
        // storage open while a rollback restores it.
        if let Some(pgid) = stragglers {
            process::stop_group(pgid, 1.0, None);
        }
        if let Some(reason) = rollback {
            if let Err(e) = self.rollback_pending(&mut Out::none(), &reason) {
                self.event("error", None, e.message);
            }
            return;
        }
        let mut inner = self.inner();
        if self.restart_due(&inner)
            && let Some(id) = inner.state.permanent.clone()
            && let Err(e) = self.start_live(&mut inner, &id, Why::Crash)
        {
            inner.app_failed = true;
            self.event("failed", Some(&id), e.message);
        }
    }

    /// What a supervision pass has to do, named for `busy`; None in the usual case, so an idle pass takes no slot.
    fn due(&self) -> Option<&'static str> {
        let mut inner = self.inner();
        if self.shutting_down() {
            return None;
        }
        let exited = inner
            .app
            .as_mut()
            .is_some_and(|app| matches!(app.child.try_wait(), Ok(Some(_))));
        let pending = inner
            .state
            .pending
            .as_ref()
            .filter(|pending| !inner.state.restore_failed(pending));
        let overdue = pending.is_some_and(|pending| {
            parse_time(&pending.deadline).is_none_or(|deadline| now_secs() >= deadline)
        });
        if pending.is_some() && (exited || overdue || inner.rollback.is_some()) {
            Some("rollback")
        } else if exited {
            Some("reap")
        } else if self.restart_due(&inner) {
            Some("restart")
        } else {
            None
        }
    }

    fn restart_due(&self, inner: &Inner) -> bool {
        inner.state.pending.is_none()
            && inner.app.is_none()
            && !inner.app_failed
            && inner.restart_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Whether the pending switch must roll back, and why. A reason is recorded in `inner.rollback` before the lock
    /// is released, so from here on a confirm is refused.
    fn decide_rollback(&self, inner: &mut Inner) -> Option<String> {
        let pending = inner.state.pending.as_ref()?;
        // Rolled back, but its storage restore failed: retried by `rollback` or the next daemon start.
        if inner.state.restore_failed(pending) {
            return None;
        }
        if inner.rollback.is_none() && now_secs() >= self.deadline_secs(pending) {
            inner.rollback = Some("missed confirm deadline".into());
        }
        inner.rollback.clone()
    }

    /// If the app exited without being asked to, take it out and decide what follows where the facts are: the pending
    /// generation gets no retries and is rolled back (recorded in `inner.rollback`, so no confirm slips in), any
    /// other is restarted after a backoff or, after too many failed starts, given up. Returns the process group, which
    /// the caller ends once it has released `inner`.
    fn reap(&self, inner: &mut Inner) -> Option<u32> {
        let status = inner
            .app
            .as_mut()
            .and_then(|a| a.child.try_wait().ok().flatten())?;
        let app = inner.app.take()?;
        let ran = app.started.elapsed().as_secs_f64();
        let m = self.manifest();
        let pgid = app.child.id();
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
        let mut message = format!("{} after {ran:.1}s", exit.describe());
        let gave_up = !pending && failed_starts >= m.max_failed_starts;
        if pending {
            // A reason decided earlier (a failed health check) stays the reason.
            inner
                .rollback
                .get_or_insert_with(|| format!("exited before confirm ({})", exit.describe()));
        } else if gave_up {
            inner.app_failed = true;
        } else {
            let backoff = (1u64 << failed_starts.saturating_sub(1).min(5)).min(30);
            inner.restart_at = Some(Instant::now() + Duration::from_secs(backoff));
            message += &format!("; restarting in {backoff}s");
        }
        inner.last_exit = Some(exit);
        self.event("exited", Some(&app.generation), message);
        if gave_up {
            self.event(
                "failed",
                Some(&app.generation),
                format!("{failed_starts} failed starts in a row; not restarting"),
            );
        }
        self.save(inner);
        Some(pgid)
    }
}

// -------------------------------------------------- lifecycle commands

impl Daemon {
    fn current_generation(&self, inner: &Inner) -> R<String> {
        if let Some(pending) = &inner.state.pending
            && inner.state.restore_failed(pending)
        {
            return Err(restore_failed(&pending.generation));
        }
        if let (Some(pending), Some(reason)) = (&inner.state.pending, &inner.rollback) {
            return Err(err(
                "busy",
                format!(
                    "{} is being rolled back ({reason}); try again when the rollback has finished",
                    pending.generation
                ),
            ));
        }
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
                // A ref starting with `-` is a name, never an option.
                "--end-of-options",
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
        // Recorded before the slow work, so a stage cut short by a daemon restart shows in history and the next
        // daemon start cleans it up.
        let (id, reuse_from) = {
            let mut inner = self.inner();
            inner.state.next_id += 1;
            let id = format!("g{}", inner.state.next_id);
            let reuse_from = inner
                .state
                .permanent
                .as_ref()
                .map_or(m.repo.clone(), |p| self.paths.code(p));
            inner.state.generations.push(Generation {
                id: id.clone(),
                git_ref: git_ref.into(),
                commit: commit.clone(),
                state: GenState::Staging,
                dir: self.paths.code(&id),
                staged_at: now(),
                test: None,
                switched_at: None,
                confirmed_at: None,
                rolled_back_at: None,
                rollback_reason: None,
                failed_starts: 0,
            });
            self.save(&inner);
            (id, reuse_from)
        };
        if let Err(e) = self.build(out, &m, &id, git_ref, &commit, &reuse_from) {
            // A failed install's log outlives the generation, for the agent to read.
            let _ = std::fs::rename(self.paths.install_log(&id), self.paths.failed_install_log());
            let removed = self.remove_generation(&m, &id);
            self.event(
                "error",
                Some(&id),
                format!(
                    "stage of {git_ref} failed: {}{}",
                    e.message,
                    removed.err().map_or(String::new(), |e| format!("; {e}"))
                ),
            );
            return Err(e);
        }
        let mut inner = self.inner();
        let g = inner.state.find_mut(&id).ok_or_else(|| not_found(&id))?;
        g.state = GenState::Staged;
        g.staged_at = now();
        let record = json!(g);
        self.save(&inner);
        self.event("staged", Some(&id), format!("{git_ref} staged as {id}"));
        Ok(record)
    }

    /// The slow part of `stage`: the worktree, the reused directories and the install.
    fn build(
        &self,
        out: &mut Out,
        m: &Manifest,
        id: &str,
        git_ref: &str,
        commit: &str,
        reuse_from: &Path,
    ) -> R<()> {
        let code = self.paths.code(id);
        std::fs::create_dir_all(self.paths.generation(id))?;
        self.say(
            out,
            id,
            format!(
                "checking out {} at {}",
                git_ref,
                &commit[..commit.len().min(8)]
            ),
        );
        let code_arg = code.to_string_lossy().into_owned();
        git(&m.repo, &["worktree", "add", "--detach", &code_arg, commit]).map_err(|e| {
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
                    id,
                    format!("cloning {dir} from {}", reuse_from.display()),
                );
                if let Err(e) = clone_directory(&from, &to) {
                    self.say(
                        out,
                        id,
                        format!("cloning {dir} failed ({e}); install starts without it"),
                    );
                    let _ = std::fs::remove_dir_all(&to);
                }
            }
        }

        let Some(install) = &m.install else {
            return Ok(());
        };
        let vars = Vars {
            port: m.port,
            home: m.home.clone(),
            code: code.clone(),
            generation: id.into(),
            state: self.paths.root.clone(),
        };
        let command = vars.expand_command(install);
        self.say(out, id, format!("installing: {command}"));
        self.run_install(m, &command, &code, &self.paths.install_log(id))
            .map_err(|e| {
                err(
                    "install_failed",
                    format!(
                        "install for {id} failed ({e}); see {}",
                        self.paths.failed_install_log().display()
                    ),
                )
            })
    }

    /// Run the install command in the worktree, in its own process group and with Fawkes's own environment (so
    /// package managers find their real caches), for at most `install_timeout` seconds.
    fn run_install(
        &self,
        m: &Manifest,
        command: &str,
        code: &Path,
        log: &Path,
    ) -> Result<(), String> {
        let mut child = process::spawn(command, code, &[], log, &format!("install: {command}"))
            .map_err(|e| e.to_string())?;
        let pgid = child.id();
        lock(&self.step_pgids).push(pgid);
        let give_up = Instant::now() + Duration::from_secs_f64(m.install_timeout);
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break Ok(()),
                Ok(Some(status)) => break Err(status.to_string()),
                Err(e) => break Err(e.to_string()),
                Ok(None) if Instant::now() >= give_up => {
                    break Err(format!("timed out after {}s", m.install_timeout));
                }
                Ok(None) => sleep(Duration::from_millis(100)),
            }
        };
        // A timed-out install still runs, and a finished one may have left processes in its group: nothing of it
        // outlives the stage.
        process::stop_group(pgid, m.stop_timeout, Some(&mut child));
        lock(&self.step_pgids).retain(|p| *p != pgid);
        result
    }

    /// Remove generation `id` for good: its worktree, its directory (snapshot and logs included) and its record.
    /// The caller makes sure nothing uses it.
    fn remove_generation(&self, m: &Manifest, id: &str) -> Result<(), String> {
        let code = self.paths.code(id);
        if code.exists() {
            // Best effort: the directory goes below either way, and `worktree prune` forgets a worktree whose
            // directory is gone.
            let _ = git(
                &m.repo,
                &["worktree", "remove", "--force", &code.to_string_lossy()],
            );
        }
        match std::fs::remove_dir_all(self.paths.generation(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "cannot remove {}: {e}",
                    self.paths.generation(id).display()
                ));
            }
        }
        let _ = git(&m.repo, &["worktree", "prune"]);
        let mut inner = self.inner();
        inner.state.generations.retain(|g| g.id != id);
        self.save(&inner);
        Ok(())
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
            &vars.expand_command(&m.start),
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
        lock(&self.step_pgids).push(pgid);

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
        lock(&self.step_pgids).retain(|p| *p != pgid);
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
                if inner.state.restore_failed(pending) {
                    return Err(restore_failed(&pending.generation));
                }
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

        // The request and the manifest both bound the deadline by MAX_DEADLINE, so this never saturates; saturating
        // only so that no arithmetic can panic halfway through a switch.
        let deadline =
            state::format_time(now_secs().saturating_add(1 + deadline.unwrap_or(m.deadline)));
        self.say(
            out,
            id,
            format!(
                "starting {id} on port {}; confirm before {deadline}",
                m.port
            ),
        );
        // The intent goes to disk before the side effect: once a process of the new generation exists, state.json
        // already says the switch is pending, so a daemon that dies from here on rolls it back at its next start.
        let mut inner = self.inner();
        let mut next = inner.state.clone();
        if let Some(g) = next.find_mut(id) {
            g.state = GenState::Current;
            g.switched_at = Some(now());
        }
        next.pending = Some(Pending {
            generation: id.into(),
            previous: previous.clone(),
            deadline: deadline.clone(),
        });
        if let Err(e) = self.persist(&next) {
            drop(inner);
            resume(self);
            return Err(err(
                e.code,
                format!("{}; {id} stays tested and nothing changed", e.message),
            ));
        }
        inner.state = next;
        if let Err(e) = self.start_live(&mut inner, id, Why::Fresh) {
            drop(inner);
            // The ordinary rollback undoes the recorded switch and starts the permanent generation again.
            let message = match self.rollback_pending(out, &format!("start failed: {}", e.message))
            {
                Ok(_) => format!("{}; {id} is rolled back", e.message),
                Err(rollback) => format!(
                    "{}; rolling {id} back failed too: {}",
                    e.message, rollback.message
                ),
            };
            return Err(err(e.code, message));
        }
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
        if let Some(pending) = &inner.state.pending
            && inner.state.restore_failed(pending)
        {
            return Err(restore_failed(&pending.generation));
        }
        if let (Some(pending), Some(reason)) = (&inner.state.pending, &inner.rollback) {
            return Err(invalid_state(format!(
                "{} is being rolled back ({reason}) and can no longer be confirmed",
                pending.generation
            )));
        }
        if let Some(pending) = &inner.state.pending
            && now_secs() >= self.deadline_secs(pending)
        {
            return Err(invalid_state(format!(
                "{} missed its confirm deadline ({}) and is being rolled back",
                pending.generation, pending.deadline
            )));
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
        // Only a generation that runs can vouch for itself. One whose process just exited is about to be rolled back
        // by the supervisor; making it permanent would keep crashed code.
        match inner.app.as_mut() {
            Some(app) if app.generation == pending.generation => {
                if let Ok(Some(status)) = app.child.try_wait() {
                    return Err(invalid_state(format!(
                        "{} exited ({}) before it was confirmed and is being rolled back",
                        pending.generation,
                        LastExit::new(Some(status), false).describe()
                    )));
                }
            }
            _ => {
                return Err(invalid_state(format!(
                    "{} is not running, so it cannot be confirmed; `fawkes start` it and confirm again, or `fawkes rollback`",
                    pending.generation
                )));
            }
        }
        // Applied to a copy and kept only once it is on disk: a confirm the daemon could not write is not one, and
        // the switch stays pending (a daemon restart would roll it back).
        let mut next = inner.state.clone();
        if let Some(old) = next.permanent.clone()
            && let Some(g) = next.find_mut(&old)
        {
            g.state = GenState::Retired;
        }
        let g = next
            .find_mut(&pending.generation)
            .ok_or_else(|| not_found(&pending.generation))?;
        g.state = GenState::Permanent;
        g.confirmed_at = Some(now());
        let record = json!(g);
        next.permanent = Some(pending.generation.clone());
        next.pending = None;
        self.persist(&next).map_err(|e| {
            err(
                e.code,
                format!(
                    "{}; {} is still pending, confirm again",
                    e.message, pending.generation
                ),
            )
        })?;
        inner.state = next;
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
            inner.rollback.get_or_insert_with(|| reason.into());
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
        // A retried restore keeps the first rollback's time and reason.
        if let Some(g) = inner.state.find_mut(&id)
            && g.state != GenState::RolledBack
        {
            g.state = GenState::RolledBack;
            g.rolled_back_at = Some(now());
            g.rollback_reason = Some(reason.into());
        }
        inner.rollback = None;
        let permanent = inner.state.permanent.clone();
        if let Err(e) = restored {
            // Starting the permanent generation on storage in an unknown state could do more harm. `pending` stays
            // in state.json, so `rollback` or the next daemon start retries the restore (the code is not retried:
            // the generation is rolled_back for good).
            inner.app_failed = true;
            self.save(&inner);
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
        // Until the end of the switch is on disk the app stays stopped and the switch pending, like a failed restore:
        // `rollback` or the next daemon start finishes it (restoring the same snapshot again is harmless).
        let mut next = inner.state.clone();
        next.pending = None;
        if let Err(e) = self.persist(&next) {
            inner.app_failed = true;
            self.event(
                "rolled_back",
                Some(&id),
                format!("{reason}; state.json could not be written"),
            );
            return Err(err(
                e.code,
                format!(
                    "{id} was rolled back but {}; the app stays stopped and `fawkes rollback` retries",
                    e.message
                ),
            ));
        }
        inner.state = next;
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

// ------------------------------------------------------------ pruning

impl Daemon {
    /// Remove the oldest generations so that the manifest's `keep` remain (`State::prunable` says which). It takes
    /// the step slot, so no stage, test or switch uses a generation while it goes.
    fn prune(&self, out: &mut Out) -> R<Value> {
        let _busy = self.begin("prune", Slot::Step)?;
        let m = self.manifest();
        let keep = m.keep.ok_or_else(|| {
            invalid_state(
                "pruning needs `keep = N` in the manifest; without it every generation is kept",
            )
        })?;
        let doomed = self
            .inner()
            .state
            .prunable(usize::try_from(keep).unwrap_or(usize::MAX));
        let mut removed = Vec::new();
        for id in doomed {
            self.say(out, &id, format!("removing {id}"));
            if let Err(e) = self.remove_generation(&m, &id) {
                self.event("error", Some(&id), format!("prune: {e}"));
                return Err(err(
                    "internal",
                    format!(
                        "{e}; removed before it: {}; `fawkes prune` tries again",
                        if removed.is_empty() {
                            "none".into()
                        } else {
                            removed.join(", ")
                        }
                    ),
                ));
            }
            removed.push(id);
        }
        if !removed.is_empty() {
            self.event(
                "pruned",
                None,
                format!("removed {} (keep = {keep})", removed.join(", ")),
            );
        }
        let kept: Vec<String> = self
            .inner()
            .state
            .generations
            .iter()
            .map(|g| g.id.clone())
            .collect();
        Ok(json!({ "removed": removed, "kept": kept }))
    }

    /// After a confirm, prune in the background once no stage or test holds the step slot; the confirm itself never
    /// waits for it.
    fn prune_soon(self: &Arc<Self>) {
        if self.manifest().keep.is_none() {
            return;
        }
        let daemon = self.clone();
        thread::spawn(move || {
            while !daemon.shutting_down() {
                match daemon.prune(&mut Out::none()) {
                    Err(e) if e.code == "busy" => sleep(Duration::from_secs(1)),
                    // Reported as an event already, and the next confirm or `fawkes prune` tries again.
                    _ => return,
                }
            }
        });
    }

    /// At startup: remove what a daemon that died mid-stage left behind, a `staging` record with its directory or a
    /// generation directory without a record, and the worktrees git still lists for them.
    fn sweep_generations(&self) {
        let m = self.manifest();
        let staging: Vec<String> = {
            let inner = self.inner();
            inner
                .state
                .generations
                .iter()
                .filter(|g| g.state == GenState::Staging)
                .map(|g| g.id.clone())
                .collect()
        };
        for id in staging {
            match self.remove_generation(&m, &id) {
                Ok(()) => self.event(
                    "daemon",
                    Some(&id),
                    format!("removed {id}, whose stage the end of the last daemon cut short"),
                ),
                Err(e) => self.event("error", Some(&id), format!("cannot remove {id}: {e}")),
            };
        }
        let dirs = std::fs::read_dir(self.paths.root.join("generations"))
            .into_iter()
            .flatten();
        for dir in dirs.flatten() {
            let name = dir.file_name().to_string_lossy().into_owned();
            if self.inner().state.find(&name).is_none() {
                match std::fs::remove_dir_all(dir.path()) {
                    Ok(()) => self.event(
                        "daemon",
                        None,
                        format!("removed {}, which no generation owns", dir.path().display()),
                    ),
                    Err(e) => self.event(
                        "error",
                        None,
                        format!("cannot remove {}: {e}", dir.path().display()),
                    ),
                };
            }
        }
        // Best effort: a worktree git still lists only costs a line in `git worktree list`.
        let _ = git(&m.repo, &["worktree", "prune"]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscriber_that_falls_behind_is_dropped_not_buffered_without_limit() {
        let hub = Hub(Mutex::new(Vec::new()));
        let stuck = hub.join();
        for _ in 0..SUBSCRIBER_QUEUE * 3 {
            hub.send(&Msg::Changed);
        }
        assert!(lock(&hub.0).is_empty(), "the stuck subscriber was kept");
        assert_eq!(stuck.try_iter().count(), SUBSCRIBER_QUEUE);
        assert!(matches!(
            stuck.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Disconnected)
        ));

        let reading = hub.join();
        hub.send(&Msg::Changed);
        assert!(reading.try_recv().is_ok() && lock(&hub.0).len() == 1);
    }
}
