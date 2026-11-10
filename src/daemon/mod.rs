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
//! A subscribed client's thread looks every `CLOSE_CHECK` whether its client has gone.
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
use crate::protocol::{
    DaemonAction, Error, ErrorCode, Event, EventKind, Facts, LastExit, Out, Pending, R,
    ReloadResult, RollbackCause, err,
};
use crate::state::{EventLog, Paths, State, Store, Txn, Unchanged, now, parse_time, with_suffix};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{FlockOperation, flock};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
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

    /// The app's exit status once it has exited, None while it runs: the one answer to "has the live app exited?".
    /// `try_wait` keeps the status it reaps, so the supervisor's `reap` still sees an exit found here first. An error
    /// (which a child of ours never gives) counts as running: only `reap` takes an app out, so an exit counted
    /// without a status would be one nothing ever handles, while a pending generation that counts as running still
    /// rolls back on a failed health check or at its deadline.
    fn exit_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
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

    /// `stop` and `restart` act on what is running. An app that has already exited stays in place and the request is
    /// `busy`: the exit is a crash for the supervisor's `reap` to decide (a pending generation gets no retries), not a
    /// stop that was asked for.
    fn refuse_exited(&mut self) -> R<()> {
        if let Some(app) = self.running_mut()
            && app.exit_status().is_some()
        {
            return Err(err(
                ErrorCode::Busy,
                format!(
                    "{} exited; the supervisor is handling it, try again in a moment",
                    app.generation
                ),
            ));
        }
        Ok(())
    }

    /// The slots a step of kind `slot` needs, free or not: the check `begin` makes before it takes them, and the one
    /// `status` makes to leave out the commands that would wait.
    fn check_slot(&self, slot: Slot) -> R<()> {
        let taken = self
            .busy
            .as_ref()
            .filter(|_| slot.step())
            .or_else(|| self.moving.as_ref().filter(|_| slot.live()));
        match taken {
            Some(other) => Err(err(
                ErrorCode::Busy,
                format!("{other} is in progress; try again when it has finished"),
            )),
            None => Ok(()),
        }
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

/// How large a generation's `logs/app.log` grows before the watcher moves it to `app.log.1`, and `daemon.log`, when it
/// is the daemon's stderr, before `Echo` moves it to `daemon.log.1`. Small under test, so a test fills it with a few
/// bytes.
#[cfg(not(test))]
const LOG_BYTES: u64 = 10 << 20;
#[cfg(test)]
const LOG_BYTES: u64 = 64;

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
///
/// When stderr is `<state_dir>/daemon.log` (a launchd service's, or a redirect to it), the thread also keeps that file
/// under `LOG_BYTES`, as the supervisor does `app.log`. It is the file's only writer, so a line is never lost to the
/// cap. Whoever started the daemon decides where stderr goes; the daemon only looks at what it is.
struct Echo(SyncSender<String>);

impl Echo {
    /// Start the thread that writes to `to`, capping it when it is the file `log`. It ends once the `Echo` is gone and
    /// what was queued is written, which disconnects the returned receiver.
    fn start(mut to: impl Write + AsFd + Send + 'static, log: PathBuf) -> (Echo, Receiver<()>) {
        let (lines, queued) = mpsc::sync_channel::<String>(ECHO_QUEUE);
        let (finished, ended) = mpsc::channel();
        thread::spawn(move || {
            let _finished = finished;
            for line in queued {
                // A failed write is no reason to stop: a terminal on hold takes lines again once it is released.
                let _ = writeln!(to, "{line}");
                // Best effort like the line itself: a log that cannot be capped goes on growing, and the next line
                // tries again.
                let _ = cap_daemon_log(&to, &log);
            }
        });
        (Echo(lines), ended)
    }

    fn line(&self, line: String) {
        // Full only while stderr is stuck; the line is in `events.jsonl`.
        let _ = self.0.try_send(line);
    }
}

/// When `to` is the file at `log` and has grown past `LOG_BYTES`, copy it to `daemon.log.1` (replacing the one before)
/// and empty it. The seek puts the next line at the start whether or not the descriptor appends. A duplicate of the
/// descriptor shares its offset, so it truncates and seeks `to` itself.
fn cap_daemon_log(to: &impl AsFd, log: &Path) -> std::io::Result<()> {
    let mut to = File::from(to.as_fd().try_clone_to_owned()?);
    let open = to.metadata()?;
    if !open.is_file() || open.len() <= LOG_BYTES {
        return Ok(());
    }
    let named = std::fs::metadata(log)?;
    if (named.dev(), named.ino()) != (open.dev(), open.ino()) {
        return Ok(());
    }
    std::fs::copy(log, with_suffix(log, ".1"))?;
    to.set_len(0)?;
    to.seek(SeekFrom::Start(0))?;
    Ok(())
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

/// How long the accept loop pauses after a failed `accept` or `poll`, so an error that persists does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

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
        // Best effort: a leftover socket file is replaced by the next daemon, a dead link retried quietly by `watch`.
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
        // `status` names it, and a reload reads it again whatever the working directory is by then.
        let manifest_path = std::path::absolute(manifest_path).map_err(|e| e.to_string())?;
        let manifest = Manifest::load(&manifest_path)?;
        let owner = rustix::process::geteuid().as_raw();
        let mut paths = manifest.paths();
        paths.root = private_state_dir(&paths.root, owner)?;
        owned_dir(&manifest.home, "home", owner)?;
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
        let (echo, echo_finished) = Echo::start(std::io::stderr(), paths.daemon_log());

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
        daemon.event_with(
            EventKind::Daemon,
            None,
            format!(
                "started, pid {}, socket {}",
                std::process::id(),
                daemon.socket.display()
            ),
            Facts {
                action: Some(DaemonAction::Started),
                pid: Some(std::process::id()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                ..Facts::default()
            },
        );
        if let Some(warning) = recovered {
            daemon.event(EventKind::Error, None, warning);
        }

        // A crashed daemon leaves its app, and the install, test or hook it was running, behind. The state lock proves
        // that daemon is gone, so what it started has no other supervisor; `leftover` makes sure the pid still names
        // it. The steps go before the sweep removes the directory they run in, and a hook before the rollback below
        // runs the restore again.
        for (what, pid_file) in [
            ("the app", daemon.paths.app_pid()),
            (
                "the install, test or test_copy hook",
                daemon.paths.step_pid(),
            ),
            ("the snapshot or restore hook", daemon.paths.hook_pid()),
        ] {
            if let Some(pid) = process::leftover(&pid_file) {
                process::stop_group(pid, daemon.manifest().stop_timeout, None);
                daemon.event_with(
                    EventKind::Daemon,
                    None,
                    format!("stopped {what} (pid {pid}) left by an earlier daemon"),
                    Facts {
                        action: Some(DaemonAction::CleanedUp),
                        pid: Some(pid),
                        ..Facts::default()
                    },
                );
            }
        }
        daemon.forget_step_pid_file();
        // Best effort, as for `step.pid`: `leftover` never mistakes a stale file for a live hook.
        let _ = std::fs::remove_file(daemon.paths.hook_pid());
        daemon.sweep_generations();

        // What the last daemon was doing when it ended, from the step it saved before each side effect. A
        // pending switch is never resumed: it rolls back, and so does a rollback that was under way.
        let txn = daemon.inner().store.state().txn.clone();
        match txn {
            None => daemon.start_permanent(),
            Some(Txn::Switching { generation, .. }) => {
                daemon.event_with(
                    EventKind::Daemon,
                    Some(&generation),
                    format!(
                        "the switch to {generation} was cut short by the end of the last daemon; it stays tested"
                    ),
                    Facts {
                        action: Some(DaemonAction::SwitchCutShort),
                        ..Facts::default()
                    },
                );
                daemon.update(&mut daemon.inner().store, |state| state.txn = None);
                daemon.start_permanent();
            }
            Some(Txn::Pending(_)) => {
                daemon.roll_back_at_start("daemon_restart", Some(RollbackCause::DaemonRestart))?;
            }
            Some(Txn::RollingBack { reason, cause, .. }) => {
                daemon.roll_back_at_start(&reason, cause)?;
            }
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
                // Logged by `reload_manifest`; a signal has no one to answer.
                let _ = self.reload_manifest();
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
                sleep(ACCEPT_RETRY);
            }
        }
    }

    /// Carry out the rollback that startup found pending or under way; `reason` and `cause` apply when it was not
    /// decided yet.
    fn roll_back_at_start(
        self: &Arc<Self>,
        reason: &str,
        cause: Option<RollbackCause>,
    ) -> Result<(), String> {
        let _busy = self.begin("rollback", Slot::Both).map_err(|e| e.message)?;
        // `busy` is a rollback that ended while the daemon shuts down: `start_or_give_up` logged what it did not start.
        if let Err(e) = self.rollback_pending(&mut Out::none(), reason, cause)
            && e.code != ErrorCode::Busy
        {
            self.error_event(&e);
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

    /// `fawkes reload` and SIGHUP: apply what may change while the daemon runs (`Manifest::reload`), or keep the old
    /// manifest whole. Either way the outcome is logged; the answer is for the client that asked, if any.
    fn reload_manifest(&self) -> R<ReloadResult> {
        let refuse = |code, message: String| {
            self.event_with(
                EventKind::Error,
                None,
                format!("manifest not reloaded: {message}"),
                Facts {
                    error_code: Some(code),
                    ..Facts::default()
                },
            );
            Err(err(code, message))
        };
        let new = match Manifest::load(&self.manifest_path) {
            Ok(new) => new,
            Err(e) => return refuse(ErrorCode::Usage, e),
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
        let changed = match reloaded {
            Ok(changed) => changed.into_iter().map(String::from).collect::<Vec<_>>(),
            Err(e) => return refuse(ErrorCode::InvalidState, e),
        };
        let message = if changed.is_empty() {
            "manifest reloaded, nothing changed".to_string()
        } else {
            *lock(&self.manifest) = Arc::new(new);
            format!("manifest reloaded, changed: {}", changed.join(", "))
        };
        self.event_with(
            EventKind::Daemon,
            None,
            message,
            Facts {
                action: Some(DaemonAction::Reloaded),
                changed: Some(changed.clone()),
                ..Facts::default()
            },
        );
        Ok(ReloadResult { changed })
    }

    /// Stop the live app and any test and clean up; the generations keep their state.
    fn shutdown(&self) {
        // The supervisor and every other waiter on `wake` see the flag now.
        self.wake.notify();
        // A switch, rollback or restart in flight finishes first (start_live refuses from now on), so no app it
        // starts outlives the daemon. Its end frees the live slot, which moves `wake`.
        let give_up = Instant::now() + self.manifest().shutdown_wait();
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
        self.event_with(
            EventKind::Daemon,
            None,
            "stopped",
            Facts {
                action: Some(DaemonAction::Stopped),
                ..Facts::default()
            },
        );
    }
}

/// The state directory as `Paths.root`: created (with missing parents) readable by its owner only, or an existing one
/// tightened to that, since it holds the app's logs and copies of its storage. Refused when it is a symlink or not
/// `owner`'s (`owned_dir`), before anything changes its mode. The result is the resolved path, so every later use
/// reaches this directory even if a component of the manifest's path is swapped for a symlink meanwhile.
fn private_state_dir(dir: &Path, owner: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let opened = owned_dir(dir, "state_dir", owner)?;
    opened
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot make {} private: {e}", dir.display()))?;
    let root =
        std::fs::canonicalize(dir).map_err(|e| format!("cannot resolve {}: {e}", dir.display()))?;
    // The directory checked and chmodded above, not one put in its place since.
    let inspect = |e: std::io::Error| format!("cannot inspect state_dir {}: {e}", dir.display());
    let (checked, resolved) = (
        opened.metadata().map_err(inspect)?,
        std::fs::metadata(&root).map_err(inspect)?,
    );
    if (checked.dev(), checked.ino()) != (resolved.dev(), resolved.ino()) {
        return Err(format!(
            "state_dir {} was replaced while the daemon checked it; start the daemon again",
            dir.display()
        ));
    }
    Ok(root)
}

/// Create `dir` (with missing parents) as `0700` when it is missing, and open it, refusing a symlink or a directory
/// another user owns: whoever owns it, or can point the link elsewhere, could run code as the daemon's user from it or
/// read the app's storage. `what` names it in the messages (`state_dir`, `home`).
fn owned_dir(dir: &Path, what: &str, owner: u32) -> Result<File, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "{what} {} is a symbolic link, which fawkes never follows; set {what} to the directory it points at",
            dir.display()
        ));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // O_NOFOLLOW: the directory checked is the one opened, even if a symlink took its place since the check above.
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(dir)
        .map_err(|e| format!("cannot open {what} {}: {e}", dir.display()))?;
    let found = opened
        .metadata()
        .map_err(|e| format!("cannot inspect {what} {}: {e}", dir.display()))?
        .uid();
    if found != owner {
        return Err(format!(
            "{what} {} belongs to uid {found}, not to this daemon's user (uid {owner}); set {what} to a directory of \
             this user's, or remove that one",
            dir.display()
        ));
    }
    Ok(opened)
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
            Event::new(
                EventKind::Error,
                None,
                format!("panic: {info}; the daemon aborts, and its next start recovers"),
                Facts::default(),
            ),
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
        self.event_with(kind, generation, message, Facts::default())
    }

    /// `event` with the typed facts its message tells. Built by the caller before any lock is taken.
    fn event_with(
        &self,
        kind: EventKind,
        generation: Option<&str>,
        message: impl Into<String>,
        facts: Facts,
    ) -> Event {
        let event = Event::new(kind, generation, message, facts);
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

    /// An `error` event for a failure that has an `ErrorCode`, which it carries as `error_code`.
    fn error_event(&self, error: &Error) {
        self.event_with(
            EventKind::Error,
            None,
            &error.message,
            Facts {
                error_code: Some(error.code),
                ..Facts::default()
            },
        );
    }

    /// A progress line: logged, and sent to the client waiting for this step.
    fn say(&self, out: &mut Out, generation: &str, message: impl Into<String>) {
        let event = self.event(EventKind::Progress, Some(generation), message);
        out.event(&event);
    }

    fn begin(&self, step: &str, slot: Slot) -> R<Busy<'_>> {
        let mut inner = self.inner();
        inner.check_slot(slot)?;
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
            snapshot: None,
            test_home: None,
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
    use crate::protocol::{GenState, Generation};
    use rustix::process::{Signal, WaitOptions, kill_process, waitpid};
    use std::sync::mpsc::RecvTimeoutError;

    /// Kills and reaps a child a test spawned when it goes out of scope, so a failed assertion leaves no process
    /// running. For children the test does not wait for itself: a reaped pid may belong to someone else by then.
    pub(super) struct KillOnDrop(pub(super) u32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let pid = i32::try_from(self.0)
                .ok()
                .and_then(rustix::process::Pid::from_raw);
            if let Some(pid) = pid {
                let _ = kill_process(pid, Signal::KILL);
                let _ = waitpid(Some(pid), WaitOptions::empty());
            }
        }
    }

    /// A daemon over a scratch state directory, with no socket and no supervision thread.
    pub(super) fn scratch_daemon(dir: &Path) -> Daemon {
        scratch_daemon_keeping(dir, None)
    }

    /// `scratch_daemon` whose manifest has `keep = N` (without it `prune` has nothing to do).
    fn scratch_daemon_keeping(dir: &Path, keep: Option<u32>) -> Daemon {
        let manifest_path = dir.join("fawkes.toml");
        let keep = keep.map_or_else(String::new, |keep| format!("keep = {keep}\n"));
        std::fs::write(
            &manifest_path,
            format!(
                "name = \"app\"\nrepo = \"repo\"\nstate_dir = \"state\"\nhome = \"home\"\nport = 4790\n\
                 spare_ports = [4791, 4795]\nstart = \"node app.js\"\n{keep}[health]\nurl = \"http://127.0.0.1:{{port}}/\"\n"
            ),
        )
        .unwrap();
        let manifest = Manifest::load(&manifest_path).unwrap();
        let paths = Paths {
            root: dir.join("state"),
        };
        std::fs::create_dir_all(&paths.root).unwrap();
        let (store, _) = Store::load(paths.state_json()).unwrap();
        Daemon {
            shutdown: Arc::new(AtomicBool::new(false)),
            reload: Arc::new(AtomicBool::new(false)),
            manifest_path,
            manifest: Mutex::new(Arc::new(manifest)),
            socket: paths.socket(),
            bin: PathBuf::new(),
            inner: Mutex::new(Inner::new(store)),
            log: Mutex::new(EventLog::open(paths.events()).unwrap()),
            paths,
            hub: Hub(Mutex::new(Vec::new())),
            wake: Wake::new(),
            step_pgids: Mutex::new(Vec::new()),
            echo: Mutex::new(None),
        }
    }

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

    /// Echo `lines` to `to` and wait until the writer has ended.
    fn echo_into(to: File, log: &Path, lines: &[&str]) {
        let (echo, finished) = Echo::start(to, log.to_path_buf());
        for line in lines {
            echo.line((*line).to_string());
        }
        drop(echo);
        assert!(matches!(
            finished.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    /// `daemon.log` as the daemon's stderr moves to `daemon.log.1` past the cap, whole, and starts over, whether its
    /// descriptor appends (launchd's) or not (`2> daemon.log`); a stderr that is another file is left alone.
    #[test]
    fn a_daemon_log_over_the_cap_moves_to_daemon_log_1_and_starts_over() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("daemon.log");
        let old = with_suffix(&log, ".1");
        let line = "x".repeat(usize::try_from(LOG_BYTES).unwrap() / 3);
        let full = format!("{line}\n{line}\n{line}\n");
        for append in [true, false] {
            let _ = std::fs::remove_file(&log);
            let to = OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .open(&log)
                .unwrap();
            echo_into(to, &log, &[&line, &line, &line, "next"]);
            assert_eq!(std::fs::read_to_string(&old).unwrap(), full, "{append}");
            assert_eq!(std::fs::read_to_string(&log).unwrap(), "next\n", "{append}");
        }

        std::fs::remove_file(&old).unwrap();
        let elsewhere = temp.path().join("terminal.log");
        echo_into(
            File::create(&elsewhere).unwrap(),
            &log,
            &[&line, &line, &line],
        );
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), full);
        assert!(!old.exists());
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
        let refused = inner.refuse_exited().expect_err("an exited app");
        assert_eq!(refused.code, ErrorCode::Busy);
        assert!(
            inner.running().is_some_and(|app| app.generation == "g1"),
            "the exited app was taken out instead of left for `reap`"
        );

        let alive = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _reaper = KillOnDrop(alive.id());
        inner.live = Live::Running(App::new(alive, "g2"));
        inner.refuse_exited().unwrap();
        let taken = inner.take_running().expect("a running app");
        assert_eq!(taken.generation, "g2");
        assert!(matches!(inner.live, Live::Stopped));
    }

    enum Process {
        Stopped,
        Running(&'static str),
        /// The app of this generation has exited and the supervisor has not reaped it yet.
        Exited(&'static str),
        Backoff,
        Failed,
    }
    enum Step {
        None,
        Switching,
        /// Seconds from now to the confirm deadline (negative: passed).
        Pending(i64),
        RollingBack {
            restore_failed: bool,
        },
    }
    struct Row {
        name: &'static str,
        generations: &'static [(&'static str, GenState)],
        process: Process,
        step: Step,
        busy: Option<&'static str>,
        moving: Option<&'static str>,
        keep: Option<u32>,
        actions: &'static [&'static str],
    }
    use GenState::{Current, Permanent, Retired, RolledBack, Staged, TestFailed, Tested};
    const IDLE: &[(&str, GenState)] = &[
        ("g1", Permanent),
        ("g2", Staged),
        ("g3", Tested),
        ("g4", TestFailed),
        ("g5", Retired),
        ("g6", RolledBack),
    ];
    const ONE: &[(&str, GenState)] = &[("g1", Permanent)];
    const PENDING: &[(&str, GenState)] = &[("g1", Permanent), ("g2", Current)];
    const PENDING_AND_TESTED: &[(&str, GenState)] =
        &[("g1", Permanent), ("g2", Current), ("g3", Tested)];
    const ROLLED_BACK: &[(&str, GenState)] =
        &[("g1", Permanent), ("g2", RolledBack), ("g3", Tested)];

    fn row(
        name: &'static str,
        generations: &'static [(&'static str, GenState)],
        process: Process,
        step: Step,
        actions: &'static [&'static str],
    ) -> Row {
        Row {
            name,
            generations,
            process,
            step,
            busy: None,
            moving: None,
            keep: None,
            actions,
        }
    }

    /// With no switch awaiting confirm, and while one is being made.
    fn idle_rows() -> Vec<Row> {
        vec![
            row(
                "an empty daemon",
                &[],
                Process::Stopped,
                Step::None,
                &["stage"],
            ),
            row(
                "permanent runs",
                IDLE,
                Process::Running("g1"),
                Step::None,
                &[
                    "stop",
                    "restart",
                    "stage",
                    "test g2",
                    "test g3",
                    "test g4",
                    "test g5",
                    "switch g3",
                ],
            ),
            row(
                "permanent stopped",
                IDLE,
                Process::Stopped,
                Step::None,
                &[
                    "start",
                    "restart",
                    "stage",
                    "test g2",
                    "test g3",
                    "test g4",
                    "test g5",
                    "switch g3",
                ],
            ),
            row(
                "crash restart scheduled",
                ONE,
                Process::Backoff,
                Step::None,
                &["start", "stop", "restart", "stage"],
            ),
            row(
                "given up (failed)",
                ONE,
                Process::Failed,
                Step::None,
                &["start", "restart", "stage"],
            ),
            Row {
                busy: Some("switch g2"),
                moving: Some("switch g2"),
                ..row(
                    "switching",
                    &[("g1", Permanent), ("g2", Tested)],
                    Process::Stopped,
                    Step::Switching,
                    &[],
                )
            },
        ]
    }

    /// With a switch pending, rolling back or failed to restore, and with other steps in progress.
    fn switch_rows() -> Vec<Row> {
        vec![
            row(
                "pending, runs, within its deadline",
                PENDING,
                Process::Running("g2"),
                Step::Pending(120),
                &["stop", "restart", "stage", "confirm g2", "rollback"],
            ),
            row(
                "pending hides the switch of a tested generation",
                PENDING_AND_TESTED,
                Process::Running("g2"),
                Step::Pending(120),
                &[
                    "stop",
                    "restart",
                    "stage",
                    "test g3",
                    "confirm g2",
                    "rollback",
                ],
            ),
            row(
                "pending, deadline passed",
                PENDING,
                Process::Running("g2"),
                Step::Pending(-5),
                &["stop", "restart", "stage", "rollback"],
            ),
            row(
                "pending, stopped",
                PENDING,
                Process::Stopped,
                Step::Pending(120),
                &["start", "restart", "stage", "rollback"],
            ),
            row(
                "pending, its app exited and is not reaped yet",
                PENDING,
                Process::Exited("g2"),
                Step::Pending(120),
                &["stage", "rollback"],
            ),
            Row {
                moving: Some("rollback"),
                ..row(
                    "rolling back (decided by the supervisor)",
                    ROLLED_BACK,
                    Process::Stopped,
                    Step::RollingBack {
                        restore_failed: false,
                    },
                    &["stage", "test g3"],
                )
            },
            row(
                "restore failed",
                ROLLED_BACK,
                Process::Failed,
                Step::RollingBack {
                    restore_failed: true,
                },
                &["stage", "test g3", "rollback", "rollback --keep-storage"],
            ),
            Row {
                busy: Some("stage"),
                ..row(
                    "a stage in progress, pending runs",
                    PENDING,
                    Process::Running("g2"),
                    Step::Pending(120),
                    &["stop", "restart", "confirm g2"],
                )
            },
            Row {
                moving: Some("stop"),
                ..row(
                    "a stop in progress",
                    &[("g1", Permanent), ("g2", Staged), ("g3", Tested)],
                    Process::Running("g1"),
                    Step::None,
                    &["stage", "test g2", "test g3"],
                )
            },
            Row {
                keep: Some(2),
                ..row(
                    "keep is set",
                    ONE,
                    Process::Running("g1"),
                    Step::None,
                    &["stop", "restart", "stage", "prune"],
                )
            },
            Row {
                keep: Some(2),
                busy: Some("test g2"),
                ..row(
                    "keep is set, a test in progress",
                    &[("g1", Permanent), ("g2", Staged)],
                    Process::Running("g1"),
                    Step::None,
                    &["stop", "restart"],
                )
            },
        ]
    }

    /// What `status.actions` holds in every situation the state machine has. The list is built from the checks the
    /// commands run, so each row also reads as the table in DESIGN.md → `status`.
    #[test]
    fn status_lists_the_commands_the_daemon_accepts_now() {
        use crate::state::{format_time, now_secs};

        let record = |id: &str, state| Generation {
            state,
            ..Generation::staging(id, "main", "0", PathBuf::new())
        };
        // Held to the end: a failed assertion must not leave a `sleep` behind.
        let mut sleepers = Vec::new();
        for row in idle_rows().into_iter().chain(switch_rows()) {
            let temp = tempfile::tempdir().unwrap();
            let daemon = scratch_daemon_keeping(temp.path(), row.keep);
            let mut inner = daemon.inner();
            inner
                .store
                .modify(|state| {
                    state.generations = row
                        .generations
                        .iter()
                        .map(|&(id, gen_state)| record(id, gen_state))
                        .collect();
                    state.permanent = (!row.generations.is_empty()).then(|| "g1".into());
                    let pending = |seconds: i64| Pending {
                        generation: "g2".into(),
                        previous: Some("g1".into()),
                        deadline: format_time(now_secs().saturating_add_signed(seconds)),
                    };
                    state.txn = match row.step {
                        Step::None => None,
                        Step::Switching => Some(Txn::Switching {
                            generation: "g2".into(),
                            previous: Some("g1".into()),
                        }),
                        Step::Pending(seconds) => Some(Txn::Pending(pending(seconds))),
                        Step::RollingBack { restore_failed } => Some(Txn::RollingBack {
                            pending: pending(120),
                            reason: "unhealthy".into(),
                            cause: Some(RollbackCause::Unhealthy),
                            restore_failed,
                        }),
                    };
                    Ok(())
                })
                .unwrap();
            inner.live = match row.process {
                Process::Stopped => Live::Stopped,
                Process::Running(generation) => {
                    let child = std::process::Command::new("sleep")
                        .arg("30")
                        .spawn()
                        .unwrap();
                    sleepers.push(KillOnDrop(child.id()));
                    Live::Running(App::new(child, generation))
                }
                Process::Exited(generation) => {
                    let mut child = std::process::Command::new("true").spawn().unwrap();
                    let give_up = Instant::now() + Duration::from_secs(5);
                    while child.try_wait().unwrap().is_none() {
                        assert!(Instant::now() < give_up, "`true` never exited");
                        sleep(Duration::from_millis(10));
                    }
                    Live::Running(App::new(child, generation))
                }
                Process::Backoff => Live::Backoff {
                    until: Instant::now() + Duration::from_secs(60),
                    at: String::new(),
                },
                Process::Failed => Live::Failed,
            };
            inner.busy = row.busy.map(String::from);
            inner.moving = row.moving.map(String::from);
            drop(inner);
            assert_eq!(daemon.status().actions, row.actions, "{}", row.name);
        }
    }

    /// `switch` and `rollback` answer `busy` while the daemon shuts down (DESIGN.md → Error codes); `start` does too.
    #[test]
    fn start_while_the_daemon_shuts_down_is_busy() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = Arc::new(scratch_daemon(temp.path()));
        daemon
            .inner()
            .store
            .modify(|state| {
                state.generations = vec![Generation {
                    state: GenState::Permanent,
                    ..Generation::staging("g1", "main", "0", PathBuf::new())
                }];
                state.permanent = Some("g1".into());
                Ok(())
            })
            .unwrap();
        daemon.shutdown.store(true, Ordering::SeqCst);

        let refused = daemon.start_command().unwrap_err();
        assert_eq!(refused.code, ErrorCode::Busy, "{}", refused.message);
        assert!(
            refused.message.contains("shutting down"),
            "{}",
            refused.message
        );
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_state_dir_is_created_or_tightened_to_0700_and_resolved() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let me = rustix::process::geteuid().as_raw();
        let fresh = temp.path().join("a/b/state");
        let root = private_state_dir(&fresh, me).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        assert_eq!(root, std::fs::canonicalize(&fresh).unwrap());

        let open = temp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_state_dir(&open, me).unwrap();
        assert_eq!(mode(&open), 0o700);

        // Reached through a symlinked parent, the root is the real path, so a later swap of that parent redirects
        // nothing.
        let parent = temp.path().join("parent");
        std::os::unix::fs::symlink(temp.path().join("a"), &parent).unwrap();
        let root = private_state_dir(&parent.join("b/state"), me).unwrap();
        assert_eq!(root, std::fs::canonicalize(&fresh).unwrap());

        let home = temp.path().join("home");
        owned_dir(&home, "home", me).unwrap();
        assert_eq!(mode(&home), 0o700);
    }

    #[test]
    fn a_symlinked_or_foreign_state_dir_is_refused_before_any_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let me = rustix::process::geteuid().as_raw();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let refused = private_state_dir(&link, me).unwrap_err();
        assert!(refused.contains("symbolic link"), "{refused}");
        assert_eq!(mode(&real), 0o755, "the symlink's target was chmodded");

        // Another user's directory, as the check sees it when the daemon's user is someone else.
        let refused = private_state_dir(&real, me + 1).unwrap_err();
        assert!(refused.contains("belongs to uid"), "{refused}");
        assert_eq!(mode(&real), 0o755, "another user's directory was chmodded");

        let refused = owned_dir(&link, "home", me).unwrap_err();
        assert!(
            refused.contains("home") && refused.contains("symbolic link"),
            "{refused}"
        );
        assert!(owned_dir(&real, "home", me + 1).is_err());
        owned_dir(&real, "home", me).unwrap();
        assert_eq!(mode(&real), 0o755, "an existing home keeps its mode");
    }
}
