//! The daemon: socket server, supervision loop and the transactions (stage, test, switch, confirm, rollback).
//!
//! Locking: `inner` holds everything that changes (state.json, the live process, the busy slots). It is never held
//! across a slow step; stages, tests and stops run unlocked and take `inner` again to record their outcome.
//! Lock order is `inner` before `manifest` before `log` before `hub` and `echo` (`step_pgids` is only ever taken
//! alone, and `wake` last, with nothing taken while it is held), and nothing waits for another thread while holding
//! one. Events go to the hub and to stderr's queue while `log` is held, so every subscriber sees them once and in
//! `seq` order. No thread that may hold a lock writes stderr: a thread of its own does (`Echo`).
//!
//! Neither the accept loop nor the supervisor polls: the accept loop sleeps in `poll(2)` on the socket and a pipe the
//! signal handlers write to, and the supervisor sleeps on `wake` until something changes or its next timer runs out.
//! A subscribed client's thread looks every 500 ms whether its client has gone.
//!
//! Panics abort the process (`Cargo.toml`): the init system restarts the daemon and its startup recovers a pending
//! switch, so no thread ever carries on past a broken invariant and no mutex is ever poisoned.

mod lifecycle;
mod server;
mod supervisor;
mod transactions;

use server::{Admission, bind, refuse, register};

use crate::manifest::{Manifest, Vars};
use crate::process;
use crate::protocol::{ErrorCode, Event, EventKind, LastExit, Out, Pending, R, err};
use crate::state::{EventLog, Paths, State, Store, Txn, Unchanged, now, parse_time};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{FlockOperation, flock};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

/// Lock a mutex. Under `panic = "abort"` no mutex is ever poisoned, so the error arm cannot run.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|_| std::process::abort())
}

/// Seconds, rounded to tenths: an uptime or a test's duration as the wire carries it.
fn tenths(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10.0).round() / 10.0
}

// ---------------------------------------------------------------- types

/// The app process Fawkes started and owns.
struct App {
    child: Child,
    generation: String,
    started: Instant,
    started_at: String,
}

impl App {
    fn new(child: Child, generation: &str) -> App {
        App {
            child,
            generation: generation.into(),
            started: Instant::now(),
            started_at: now(),
        }
    }
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

/// What the live process is doing. Exactly one of these holds, so an app that runs is never also given up on or
/// scheduled for a restart.
enum Live {
    /// Not running and not scheduled: never started, stopped on request, or stopped for a switch or rollback.
    Stopped,
    Running(App),
    /// Crashed; the supervisor restarts the permanent generation at `until`. `at` is the same moment as wall time,
    /// computed once when the backoff is scheduled, for `restart_at`.
    Backoff {
        until: Instant,
        at: String,
    },
    /// Given up: `max_failed_starts` crashes in a row, a start that failed, or a rollback whose restore failed.
    /// Cleared by `start`, `restart`, the next switch or a daemon start.
    Failed,
}

struct Inner {
    store: Store,
    live: Live,
    last_exit: Option<LastExit>,
    /// Generation of the latest start and the number of restarts since it became current.
    last_started: Option<String>,
    restarts: u32,
    /// The transaction step in progress (stage, test, switch, rollback): one at a time.
    busy: Option<String>,
    /// A step that moves the live process (switch, rollback, start, stop, restart): one at a time.
    moving: Option<String>,
}

impl Inner {
    /// Nothing running and no step in flight, over the state `store` holds.
    fn new(store: Store) -> Inner {
        Inner {
            store,
            live: Live::Stopped,
            last_exit: None,
            last_started: None,
            restarts: 0,
            busy: None,
            moving: None,
        }
    }

    fn running(&self) -> Option<&App> {
        match &self.live {
            Live::Running(app) => Some(app),
            _ => None,
        }
    }

    fn running_mut(&mut self) -> Option<&mut App> {
        match &mut self.live {
            Live::Running(app) => Some(app),
            _ => None,
        }
    }

    /// Take the app out for a step that moves the live process. A scheduled crash restart is cancelled; a process
    /// given up on stays given up until something starts it.
    fn take_running(&mut self) -> Option<App> {
        match std::mem::replace(&mut self.live, Live::Stopped) {
            Live::Running(app) => Some(app),
            Live::Failed => {
                self.live = Live::Failed;
                None
            }
            Live::Stopped | Live::Backoff { .. } => None,
        }
    }

    /// `take_running` for `stop` and `restart`, which act on what is running. An app that has already exited stays
    /// in place and the request is `busy`: the exit is a crash for the supervisor's `reap` to decide (a pending
    /// generation gets no retries), not a stop that was asked for.
    fn take_running_alive(&mut self) -> R<Option<App>> {
        if let Some(app) = self.running_mut()
            && matches!(app.child.try_wait(), Ok(Some(_)))
        {
            return Err(err(
                ErrorCode::Busy,
                format!(
                    "{} exited; the supervisor is handling it, try again in a moment",
                    app.generation
                ),
            ));
        }
        Ok(self.take_running())
    }
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

impl Slot {
    /// Takes the transaction step slot, `busy`.
    fn step(self) -> bool {
        matches!(self, Slot::Step | Slot::Both)
    }

    /// Takes the live process slot, `moving`.
    fn live(self) -> bool {
        matches!(self, Slot::Live | Slot::Both)
    }
}

/// Held while a step runs; frees its slots on drop.
struct Busy<'a> {
    daemon: &'a Daemon,
    slot: Slot,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut inner = self.daemon.inner();
        if self.slot.step() {
            inner.busy = None;
        }
        if self.slot.live() {
            inner.moving = None;
        }
        drop(inner);
        self.daemon.changed();
    }
}

/// What subscribers (`subscribe`, `log --follow`) receive.
#[derive(Clone)]
enum Msg {
    Event(Event),
    /// Generations or the live process changed; `subscribe` sends fresh status and history lines.
    Changed,
}

/// How large a generation's `logs/app.log` grows before the watcher moves it to `app.log.1`.
const APP_LOG_BYTES: u64 = 10 << 20;

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

/// How many lines stderr may fall behind before new ones are dropped.
const ECHO_QUEUE: usize = 1024;

/// The daemon's stderr: a copy of the event log for whoever watches the terminal, written by a thread of its own. An
/// event's caller may hold `inner`, and a stderr that blocks (a terminal on hold) must not stall the daemon, nor one
/// that fails (a closed terminal, a `| tee` that died) end it. Best effort: a line that cannot be written, or finds
/// the queue full, is dropped, since `events.jsonl` is the record.
struct Echo(SyncSender<String>);

impl Echo {
    /// Start the thread that writes to `to`. It ends once the `Echo` is gone and what was queued is written, which
    /// disconnects the returned receiver.
    fn start(mut to: impl Write + Send + 'static) -> (Echo, Receiver<()>) {
        let (lines, queued) = mpsc::sync_channel::<String>(ECHO_QUEUE);
        let (finished, ended) = mpsc::channel();
        thread::spawn(move || {
            let _finished = finished;
            for line in queued {
                // A failed write is no reason to stop: a terminal on hold takes lines again once it is released.
                let _ = writeln!(to, "{line}");
            }
        });
        (Echo(lines), ended)
    }

    fn line(&self, line: String) {
        // Full only while stderr is stuck; the line is in `events.jsonl`.
        let _ = self.0.try_send(line);
    }
}

/// What the supervisor sleeps on: a count of changes, and a condvar to wait for the next one. Every change
/// subscribers hear of (`Daemon::changed`) moves it, since whatever changes the picture may be for the supervisor to
/// act on, and so do an exit of the live app and the start of shutdown. A waiter reads the count before it looks at
/// the state, so a change made after the look ends the wait, and none is lost in between.
struct Wake {
    count: Mutex<u64>,
    moved: Condvar,
}

impl Wake {
    fn new() -> Wake {
        Wake {
            count: Mutex::new(0),
            moved: Condvar::new(),
        }
    }

    fn seen(&self) -> u64 {
        *lock(&self.count)
    }

    fn notify(&self) {
        *lock(&self.count) += 1;
        self.moved.notify_all();
    }

    /// Sleep until the count has moved past `seen`, or `timeout` has passed (without one, for as long as it takes).
    fn wait(&self, seen: u64, timeout: Option<Duration>) {
        let count = lock(&self.count);
        let unmoved = |count: &mut u64| *count == seen;
        // Under `panic = "abort"` the mutex is never poisoned.
        let poisoned = match timeout {
            Some(timeout) => self
                .moved
                .wait_timeout_while(count, timeout, unmoved)
                .is_err(),
            None => self.moved.wait_while(count, unmoved).is_err(),
        };
        if poisoned {
            std::process::abort();
        }
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
    wake: Wake,
    /// Process groups of running tests and installs, killed on shutdown.
    step_pgids: Mutex<Vec<u32>>,
    /// Taken at the end of `run` (`Flush`), so stderr's writer finishes what is queued and ends.
    echo: Mutex<Option<Echo>>,
}

/// How long a daemon on its way out waits for stderr to take its last lines.
const FLUSH_FOR: Duration = Duration::from_secs(1);

/// Lets stderr's writer finish the queued lines (the last of them `stopped`, or why startup failed) when `run`
/// returns however it returns, for at most `FLUSH_FOR`: a stderr that does not read must not keep the state lock, and
/// with it the next daemon, waiting.
struct Flush {
    daemon: Arc<Daemon>,
    finished: Receiver<()>,
}

impl Drop for Flush {
    fn drop(&mut self) {
        lock(&self.daemon.echo).take();
        // Disconnected once the writer has ended; a timeout leaves the rest unwritten, as events.jsonl has it.
        let _ = self.finished.recv_timeout(FLUSH_FOR);
    }
}

/// The daemon's socket and its `<fawkes_home>/run/<name>.sock` link, removed when `run` returns however it returns,
/// so a failed startup leaves neither behind. Both are only ever ours: `bind` refuses a socket another daemon answers
/// on, and the link goes only while it still points at our socket.
struct Published {
    socket: PathBuf,
    run_link: Option<PathBuf>,
}

impl Drop for Published {
    fn drop(&mut self) {
        // Best effort: a leftover socket file is replaced by the next daemon, a dead link skipped by the TUI.
        let _ = std::fs::remove_file(&self.socket);
        if let Some(link) = &self.run_link
            && std::fs::read_link(link).is_ok_and(|target| target == self.socket)
        {
            let _ = std::fs::remove_file(link);
        }
    }
}

// ------------------------------------------------------------ startup

impl Daemon {
    /// Run the daemon until SIGTERM or SIGINT.
    pub fn run(manifest_path: PathBuf, socket: Option<PathBuf>) -> Result<(), String> {
        // First of all, so a SIGTERM during startup still ends in `shutdown`'s cleanup. signal-hook installs them
        // with sigaction and SA_RESTART; the handlers set a flag, then write a byte to `signals` to wake the accept
        // loop. Actions run in the order they were registered, so a woken loop that has drained the pipe finds the
        // flag set.
        let shutdown = Arc::new(AtomicBool::new(false));
        let reload = Arc::new(AtomicBool::new(false));
        let (signals, signalled) =
            UnixStream::pair().map_err(|e| format!("cannot create the signal pipe: {e}"))?;
        for (signal, flag) in [(SIGTERM, &shutdown), (SIGINT, &shutdown), (SIGHUP, &reload)] {
            signal_hook::flag::register(signal, flag.clone())
                .and_then(|_| signalled.try_clone())
                .and_then(|pipe| signal_hook::low_level::pipe::register(signal, pipe))
                .map_err(|e| format!("cannot install the signal handlers: {e}"))?;
        }
        let manifest = Manifest::load(&manifest_path)?;
        let paths = manifest.paths();
        create_private_dir(&paths.root)?;
        let _lock = lock_state_dir(&paths)?;
        let log = EventLog::open(paths.events())
            .map_err(|e| format!("cannot open the event log: {e}"))?;
        log_panics(paths.events(), log.sequence());
        let socket = std::path::absolute(socket.unwrap_or_else(|| paths.socket()))
            .map_err(|e| e.to_string())?;
        let listener = bind(&socket)?;
        // From here on every way out of `run`, a failed startup included, removes the socket and the run link.
        let mut published = Published {
            socket: socket.clone(),
            run_link: None,
        };
        published.run_link = Some(register(&manifest.name, &socket)?);
        let (store, recovered) = Store::load(paths.state_json())?;
        let bin = std::env::current_exe().map_err(|e| e.to_string())?;
        let (echo, echo_finished) = Echo::start(std::io::stderr());

        let daemon = Arc::new(Daemon {
            shutdown,
            reload,
            manifest_path,
            manifest: Mutex::new(Arc::new(manifest)),
            paths,
            socket,
            bin,
            inner: Mutex::new(Inner::new(store)),
            log: Mutex::new(log),
            hub: Hub(Mutex::new(Vec::new())),
            wake: Wake::new(),
            step_pgids: Mutex::new(Vec::new()),
            echo: Mutex::new(Some(echo)),
        });
        let _flush = Flush {
            daemon: daemon.clone(),
            finished: echo_finished,
        };
        daemon.event(
            EventKind::Daemon,
            None,
            format!(
                "started, pid {}, socket {}",
                std::process::id(),
                daemon.socket.display()
            ),
        );
        if let Some(warning) = recovered {
            daemon.event(EventKind::Error, None, warning);
        }

        // A crashed daemon leaves its app, and the install or test it was running, behind. The state lock proves that
        // daemon is gone, so what it started has no other supervisor; `leftover` makes sure the pid still names it.
        // The step goes before the sweep removes the directory it runs in.
        for (what, pid_file) in [
            ("the app", daemon.paths.app_pid()),
            ("the install or test", daemon.paths.step_pid()),
        ] {
            if let Some(pid) = process::leftover(&pid_file) {
                process::stop_group(pid, daemon.manifest().stop_timeout, None);
                daemon.event(
                    EventKind::Daemon,
                    None,
                    format!("stopped {what} (pid {pid}) left by an earlier daemon"),
                );
            }
        }
        daemon.forget_step_pid_file();
        daemon.update(&mut daemon.inner().store, |state| {
            state.daemon_pid = Some(std::process::id());
        });
        daemon.sweep_generations();

        // What the last daemon was doing when it ended, from the step it saved before each side effect. A
        // pending switch is never resumed: it rolls back, and so does a rollback that was under way.
        let txn = daemon.inner().store.state().txn.clone();
        match txn {
            None => daemon.start_permanent(),
            Some(Txn::Switching { generation, .. }) => {
                daemon.event(
                    EventKind::Daemon,
                    Some(&generation),
                    format!(
                        "the switch to {generation} was cut short by the end of the last daemon; it stays tested"
                    ),
                );
                daemon.update(&mut daemon.inner().store, |state| state.txn = None);
                daemon.start_permanent();
            }
            Some(Txn::Pending(_)) => daemon.roll_back_at_start("daemon_restart")?,
            Some(Txn::RollingBack { reason, .. }) => daemon.roll_back_at_start(&reason)?,
        }

        let watcher = daemon.clone();
        thread::spawn(move || watcher.watch());

        daemon.serve(&listener, signals)?;
        daemon.shutdown();
        Ok(())
    }

    /// Accept clients and act on SIGHUP until SIGTERM or SIGINT, asleep in `poll` in between.
    fn serve(
        self: &Arc<Self>,
        listener: &UnixListener,
        mut signals: UnixStream,
    ) -> Result<(), String> {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        signals.set_nonblocking(true).map_err(|e| e.to_string())?;
        let open = Arc::new(AtomicUsize::new(0));
        loop {
            // Drained before the flags are read: a signal that comes after the drain leaves a byte that ends the
            // next `poll` at once.
            while signals.read(&mut [0; 64]).is_ok_and(|n| n > 0) {}
            if self.shutting_down() {
                return Ok(());
            }
            if self.reload.swap(false, Ordering::SeqCst) {
                self.reload_manifest();
            }
            let failed = match listener.accept() {
                Ok((stream, _)) => {
                    // Taken before anything is read, so a client that sends nothing still holds its place.
                    match Admission::take(&open) {
                        Some(admitted) => {
                            let daemon = self.clone();
                            thread::spawn(move || daemon.handle(stream, admitted));
                        }
                        None => refuse(stream),
                    }
                    None
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut ready = [
                        PollFd::new(listener, PollFlags::IN),
                        PollFd::new(&signals, PollFlags::IN),
                    ];
                    match poll(&mut ready, None) {
                        Ok(_) | Err(rustix::io::Errno::INTR) => None,
                        Err(e) => Some(format!("cannot wait for clients: {e}")),
                    }
                }
                Err(e) => Some(format!("accept failed: {e}")),
            };
            if let Some(e) = failed {
                self.event(EventKind::Error, None, e);
                // An error that persists (out of file descriptors) would otherwise repeat as fast as it fails.
                sleep(Duration::from_millis(100));
            }
        }
    }

    /// Carry out the rollback that startup found pending or under way; `reason` applies when it was not decided yet.
    fn roll_back_at_start(self: &Arc<Self>, reason: &str) -> Result<(), String> {
        let _busy = self.begin("rollback", Slot::Both).map_err(|e| e.message)?;
        // `busy` is a rollback that ended while the daemon shuts down: `start_or_give_up` logged what it did not start.
        if let Err(e) = self.rollback_pending(&mut Out::none(), reason)
            && e.code != ErrorCode::Busy
        {
            self.event(EventKind::Error, None, e.message);
        }
        Ok(())
    }

    fn start_permanent(self: &Arc<Self>) {
        let mut inner = self.inner();
        if let Some(id) = inner.store.state().permanent.clone() {
            self.start_or_give_up(&mut inner, &id, Why::Fresh);
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
                self.event(
                    EventKind::Error,
                    None,
                    format!("manifest not reloaded: {e}"),
                );
                return;
            }
        };
        // Held across the swap, so no app starts between the check of `port` and the swap.
        let inner = self.inner();
        let current = self.manifest();
        let reloaded = current.reload(&new, inner.running().is_some());
        // A step that holds the live slot has no app in `live` between stopping the old process and starting the
        // new one, yet it announces the port it reads from the manifest and must start on the same one.
        let reloaded = match &inner.moving {
            Some(step) if current.port != new.port && inner.running().is_none() => {
                let moving = format!(
                    "port cannot change while {step} is in progress: reload again when it has finished"
                );
                Err(match reloaded {
                    Err(problems) => format!("{problems}; {moving}"),
                    Ok(_) => moving,
                })
            }
            _ => reloaded,
        };
        match reloaded {
            Ok(changed) if changed.is_empty() => {
                self.event(
                    EventKind::Daemon,
                    None,
                    "manifest reloaded, nothing changed",
                );
            }
            Ok(changed) => {
                *lock(&self.manifest) = Arc::new(new);
                self.event(
                    EventKind::Daemon,
                    None,
                    format!("manifest reloaded, changed: {}", changed.join(", ")),
                );
            }
            Err(e) => {
                self.event(
                    EventKind::Error,
                    None,
                    format!("manifest not reloaded: {e}"),
                );
            }
        }
    }

    /// Stop the live app and any test and clean up; the generations keep their state.
    fn shutdown(&self) {
        // The supervisor and every other waiter on `wake` see the flag now.
        self.wake.notify();
        // A switch, rollback or restart in flight finishes first (start_live refuses from now on), so no app it
        // starts outlives the daemon. Its end frees the live slot, which moves `wake`.
        let give_up =
            Instant::now() + Duration::from_secs_f64(2.0 * self.manifest().stop_timeout + 5.0);
        loop {
            let seen = self.wake.seen();
            let left = give_up.saturating_duration_since(Instant::now());
            if self.inner().moving.is_none() || left.is_zero() {
                break;
            }
            self.wake.wait(seen, Some(left));
        }
        let app = self.inner().take_running();
        if let Some(app) = app {
            self.stop_app(app);
        }
        for pgid in lock(&self.step_pgids).drain(..) {
            process::kill_group(pgid);
        }
        self.update(&mut self.inner().store, |state| state.daemon_pid = None);
        self.event(EventKind::Daemon, None, "stopped");
    }
}

/// Create the state directory (and missing parents) readable by its owner only, and tighten one that exists: it holds
/// the app's logs and copies of its storage.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
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
fn log_panics(events: PathBuf, sequence: Arc<AtomicU64>) {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A file handle of its own: the panicking thread may hold the daemon's event log.
        EventLog::append_once(
            &events,
            &sequence,
            Event {
                seq: 0,
                at: now(),
                kind: EventKind::Error,
                generation: None,
                message: format!("panic: {info}; the daemon aborts, and its next start recovers"),
            },
        );
        report(info);
    }));
}

// ---------------------------------------------------- shared helpers

impl Daemon {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn manifest(&self) -> Arc<Manifest> {
        lock(&self.manifest).clone()
    }

    /// Change bookkeeping (the live pid, counters, a test result, a decided rollback) and save it. A failed save is
    /// reported and the change stands in memory: a later save repeats it.
    fn update<T>(&self, store: &mut Store, change: impl FnOnce(&mut State) -> T) -> T {
        let (value, failed) = store.update(change);
        if let Some(e) = failed {
            self.event(
                EventKind::Error,
                None,
                format!("saving state.json failed: {e}"),
            );
        }
        value
    }

    /// Change the state by the intent of a transaction step (a pending switch, a confirm, the end of a rollback).
    /// The step fails when state.json cannot be written: a daemon restart acts on what is on disk, not on what was
    /// meant. Once it is written the step stands, and a later failure of the save (its copy) is only reported.
    fn modify<T>(&self, store: &mut Store, change: impl FnOnce(&mut State) -> R<T>) -> R<T> {
        match store.modify(change) {
            Ok((value, late)) => {
                if let Some(e) = late {
                    self.event(
                        EventKind::Error,
                        None,
                        format!(
                            "cannot write state.json completely: {e}; state.json itself is saved"
                        ),
                    );
                }
                Ok(value)
            }
            Err(Unchanged::Refused(e)) => Err(e),
            Err(Unchanged::Unsaved(e)) => {
                self.event(
                    EventKind::Error,
                    None,
                    format!("cannot write state.json: {e}"),
                );
                Err(err(
                    ErrorCode::Internal,
                    format!("cannot write {}: {e}", self.paths.state_json().display()),
                ))
            }
        }
    }

    /// The pending switch's deadline. An unreadable one counts as passed, so the switch rolls back, and says so.
    fn deadline_secs(&self, pending: &Pending) -> u64 {
        parse_time(&pending.deadline).unwrap_or_else(|| {
            self.event(
                EventKind::Error,
                Some(&pending.generation),
                format!(
                    "the confirm deadline `{}` in state.json is unreadable; it counts as passed",
                    pending.deadline
                ),
            );
            0
        })
    }

    fn broadcast(&self, msg: &Msg) {
        self.hub.send(msg);
    }

    /// Generations, the live process or a slot changed: subscribers get fresh status, and the supervisor looks again.
    fn changed(&self) {
        self.broadcast(&Msg::Changed);
        self.wake.notify();
    }

    /// Record an event: events.jsonl, every subscriber, stderr. Anything but progress also changes the picture.
    fn event(
        &self,
        kind: EventKind,
        generation: Option<&str>,
        message: impl Into<String>,
    ) -> Event {
        let event = Event {
            seq: 0,
            at: now(),
            kind,
            generation: generation.map(String::from),
            message: message.into(),
        };
        // Subscribers get the event while the log is still held: `stream` joins the hub and reads its replay under
        // the same lock, so an event is in the replay or on the channel, never both and never neither, and the
        // channel carries events in `seq` order. `hub.send` and `Echo::line` only `try_send`, so holding the log
        // across them is cheap.
        let event = {
            let mut log = lock(&self.log);
            let event = log.append(event);
            self.broadcast(&Msg::Event(event.clone()));
            if let Some(echo) = lock(&self.echo).as_ref() {
                echo.line(format!(
                    "{} {} {} {}",
                    event.at,
                    event.kind.as_str(),
                    event.generation.as_deref().unwrap_or("-"),
                    event.message
                ));
            }
            event
        };
        if kind != EventKind::Progress {
            self.changed();
        }
        event
    }

    /// A progress line: logged, and sent to the client waiting for this step.
    fn say(&self, out: &mut Out, generation: &str, message: impl Into<String>) {
        let event = self.event(EventKind::Progress, Some(generation), message);
        out.event(&event);
    }

    fn begin(&self, step: &str, slot: Slot) -> R<Busy<'_>> {
        let mut inner = self.inner();
        let taken = inner
            .busy
            .as_ref()
            .filter(|_| slot.step())
            .or_else(|| inner.moving.as_ref().filter(|_| slot.live()));
        if let Some(other) = taken {
            return Err(err(
                ErrorCode::Busy,
                format!("{other} is in progress; try again when it has finished"),
            ));
        }
        if slot.step() {
            inner.busy = Some(step.into());
        }
        if slot.live() {
            inner.moving = Some(step.into());
        }
        drop(inner);
        self.changed();
        Ok(Busy { daemon: self, slot })
    }

    /// What `{port}`, `{home}`, `{code}`, `{generation}` and `{state}` expand to for generation `id` checked out at
    /// `code`.
    fn vars(&self, port: u16, home: &Path, code: &Path, id: &str) -> Vars {
        Vars {
            port,
            home: home.to_path_buf(),
            code: code.to_path_buf(),
            generation: id.into(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

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

    #[test]
    fn taking_the_app_for_stop_or_restart_leaves_one_that_already_exited_to_the_supervisor() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let (store, _) = Store::load(dir.join("state.json")).unwrap();
        let mut inner = Inner::new(store);

        let mut crashed = std::process::Command::new("true").spawn().unwrap();
        // `try_wait` keeps the status it reaps, so the call under test sees the exit too.
        let give_up = Instant::now() + Duration::from_secs(5);
        while crashed.try_wait().unwrap().is_none() {
            assert!(Instant::now() < give_up, "`true` never exited");
            sleep(Duration::from_millis(10));
        }
        inner.live = Live::Running(App::new(crashed, "g1"));
        let refused = inner.take_running_alive().err().expect("an exited app");
        assert_eq!(refused.code, ErrorCode::Busy);
        assert!(
            inner.running().is_some_and(|app| app.generation == "g1"),
            "the exited app was taken out instead of left for `reap`"
        );

        let alive = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        inner.live = Live::Running(App::new(alive, "g2"));
        let mut taken = inner.take_running_alive().unwrap().expect("a running app");
        assert_eq!(taken.generation, "g2");
        assert!(matches!(inner.live, Live::Stopped));
        taken.child.kill().unwrap();
        taken.child.wait().unwrap();
    }
}
