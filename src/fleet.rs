//! The supervisor of every app's daemon: `fawkes up --all` runs the daemon of each manifest in `$FAWKES_HOME` as its
//! child, starts it again when it fails, and answers `status`, `start`, `down` and `reexec` on
//! `<fawkes_home>/supervisor.sock` (DESIGN.md, "Supervisor"). Each daemon stays the one `fawkes up` runs for one app,
//! with its own state lock, socket and run link: the supervisor starts it, waits for it and signals it, and leaves
//! alone a daemon whose state lock another process holds (`attached`).
//!
//! Locking: `apps` holds the table of apps, their down marks and the requests at work. It is never held across a wait
//! for a daemon (its socket answering, its exit). A daemon's spawn and the exec of `reexec` happen while it is held, so
//! no daemon inherits the descriptors that `reexec` keeps open across its exec; the one other spawn, the installed
//! binary's version check, comes before those are made inheritable, and one `reexec` at a time (`reexecing`).
//! `wake` is taken last, with nothing taken while it is held. Lines for stderr go through `Echo`'s thread, so whoever
//! holds `apps` never blocks on a terminal.
//!
//! The daemons outlive the supervisor: each runs in its own process group, so a Ctrl-C reaches the supervisor alone,
//! and one that dies (SIGKILL, a panic, which aborts) leaves them running, for the next to find attached.

use crate::daemon::server::{REQUEST_DEADLINE, bind, read_request};
use crate::daemon::{Echo, MAX_REQUEST, Wake, lock};
use crate::discovery;
use crate::manifest::{self, Manifest};
use crate::process::{self, ChildPid, Waitable};
use crate::protocol::{
    AppState, DownResult, Error, ErrorCode, FleetApp, FleetDownResult, FleetRequest, FleetStatus,
    Line, Out, R, ReexecResult, Reply, Request, StartResult, Status, WRITE_TIMEOUT, err,
    invalid_state, read_line,
};
use crate::reexec::{exec_with_handover, installed_version, take_listener, take_lock};
use crate::state::{Paths, format_time, now, now_secs, write_durably};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{FlockOperation, flock};
use rustix::process::{Signal, kill_process};
use serde::{Deserialize, Serialize};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the supervisor reads `<fawkes_home>` again and looks at attached daemons' locks.
const TICK: Duration = Duration::from_secs(2);

/// The longest backoff between the starts of a daemon that keeps failing.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// A daemon that ran this long before it failed starts again after the shortest backoff.
const STABLE_RUN: Duration = Duration::from_secs(60);

/// How long `start` waits for the daemon it started to answer `status`.
const START_WAIT: Duration = Duration::from_secs(30);

/// How often `start` looks again meanwhile, and how often it looks whether a daemon `down` ended has exited.
const POLL: Duration = Duration::from_millis(100);

/// How long a daemon has to answer a `status` the supervisor asks, and the supervisor or a daemon one that
/// `fawkes status --all` asks: an answer it waits longer for would hold up the whole fleet's screen.
pub const ANSWER_WAIT: Duration = Duration::from_secs(2);

/// What the supervisor adds to a daemon's own bound (`Manifest::daemon_stop_wait`) when it waits for it to end: its
/// answer, and its exit after it. launchd's `ExitTimeOut` leaves more (`fawkes service`).
const STOP_MARGIN: Duration = Duration::from_secs(5);

/// How long the accept loop pauses after a failed `accept` or `poll`, so an error that persists does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// How often `status` looks again whether a `reexec` at work has executed or given up.
const REEXEC_LOOK: Duration = Duration::from_millis(50);

/// The variables a daemon must not inherit from the supervisor's environment: they would name another daemon, or say
/// the daemon runs inside an app.
const NOT_INHERITED: [&str; 4] = [
    "FAWKES_SOCKET",
    "FAWKES_MANIFEST",
    "FAWKES_GENERATION",
    "FAWKES_PHASE",
];

/// `<fawkes_home>/supervisor.sock`, where the supervisor answers.
pub fn socket(home: &Path) -> PathBuf {
    home.join("supervisor.sock")
}

/// The name the supervisor knows the app of the manifest at `path` by: its file stem, when it is
/// `<home>/<name>.toml` (the same file, however either path is spelt); none for a manifest elsewhere.
pub fn app_in_home(path: &Path, home: &Path) -> Option<String> {
    if path.extension()? != "toml" {
        return None;
    }
    let name = path.file_stem()?.to_str()?;
    let in_home = home.join(format!("{name}.toml")).canonicalize().ok()?;
    (path.canonicalize().ok()? == in_home).then(|| name.to_string())
}

// ------------------------------------------------------------- policy

/// What the supervisor does once an app's daemon has exited.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// It ended by itself, as `fawkes down` towards the daemon ends it: it stays down until `fawkes up`.
    MarkDown,
    /// Its manifest is invalid (exit 2): it starts again once the manifest changes.
    Broken,
    /// It failed: start it again after `after`.
    Restart { after: Duration },
    /// The supervisor asked for the exit (its own shutdown, or `down`), which marks nothing.
    Leave,
}

/// The failures counted and what comes next when a daemon that ran for `ran`, with `failures` failed exits before it,
/// exits with `code` (none for a signal). `asked` is an exit the supervisor asked for. Pure: the caller keeps the
/// table, the lines and the clock.
fn after_exit(code: Option<i32>, failures: u32, ran: Duration, asked: bool) -> (u32, Next) {
    if asked {
        return (failures, Next::Leave);
    }
    match code {
        Some(0) => (0, Next::MarkDown),
        Some(2) => (failures.saturating_add(1), Next::Broken),
        _ => {
            // A daemon that ran for a while failed afresh, not again.
            let failures = if ran >= STABLE_RUN {
                1
            } else {
                failures.saturating_add(1)
            };
            (
                failures,
                Next::Restart {
                    after: backoff(failures),
                },
            )
        }
    }
}

/// The wait before the next start after `failures` failed exits in a row: 1 s, doubling to `BACKOFF_CAP`.
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(5);
    Duration::from_secs(1 << doublings).min(BACKOFF_CAP)
}

// ---------------------------------------------------------------- types

/// One app of `<fawkes_home>`.
struct App {
    manifest: PathBuf,
    /// Its manifest is still in `<fawkes_home>`; one that went is forgotten once its daemon has exited.
    present: bool,
    /// Its daemon, when it runs as the supervisor's child.
    child: Option<Child>,
    /// Another process holds its state lock: a daemon that runs, but not as the supervisor's child.
    attached: bool,
    wait: Wait,
    /// Failed exits in a row (`after_exit`).
    failures: u32,
    /// The pid of its last daemon that exited and how, for the `start` that waited for it.
    ended: Option<(u32, String)>,
}

impl App {
    fn new(manifest: PathBuf) -> App {
        App {
            manifest,
            present: true,
            child: None,
            attached: false,
            wait: Wait::None,
            failures: 0,
            ended: None,
        }
    }
}

/// Why an app without a daemon is not started yet.
enum Wait {
    None,
    /// Its daemon failed; it starts at `until`, which is `at` (Unix seconds) on the wall clock.
    Backoff {
        until: Instant,
        at: u64,
    },
    /// Its manifest is invalid; it starts once the manifest's modification time is no longer `mtime`.
    Broken {
        mtime: Option<SystemTime>,
    },
}

impl Wait {
    fn backoff(after: Duration) -> Wait {
        Wait::Backoff {
            until: Instant::now() + after,
            at: now_secs() + after.as_secs(),
        }
    }
}

/// A daemon the supervisor started.
struct Child {
    process: ChildPid,
    started: Instant,
    /// `down` asked it to end, so its exit marks nothing and `start` waits for it.
    stopping: bool,
    /// The longest it takes to end after SIGTERM (`Manifest::daemon_stop_wait`).
    stop_wait: Duration,
}

/// What `apps` guards.
struct Table {
    apps: BTreeMap<String, App>,
    /// The apps that stay down, as `supervisor.json` lists them.
    down: BTreeSet<String>,
    /// `start` and `down` requests at work, which `reexec` waits for.
    busy: usize,
    /// A `reexec` is at work: `start`, `down` and another `reexec` are refused until it has executed or given up.
    reexecing: bool,
}

impl Table {
    /// Refused `busy` while a `reexec` is at work, whose exec would cut this request's answer off.
    fn refuse_reexecing(&self) -> R<()> {
        if self.reexecing {
            return Err(err(
                ErrorCode::Busy,
                "the supervisor is re-executing; ask again once it answers `status`",
            ));
        }
        Ok(())
    }
}

/// `supervisor.json`.
#[derive(Serialize, Deserialize, Default)]
struct Marks {
    #[serde(default)]
    down: BTreeSet<String>,
}

pub struct Fleet {
    home: PathBuf,
    bin: PathBuf,
    socket: PathBuf,
    /// Set by SIGTERM and SIGINT.
    shutdown: Arc<AtomicBool>,
    apps: Mutex<Table>,
    /// Moved by the exit of a daemon and by the start of shutdown.
    wake: Wake,
    echo: Echo,
    /// Holds the `flock` on `supervisor.lock` for the process's life, across a `reexec` too.
    lock_file: File,
    listener: UnixListener,
}

/// A `start` or `down` at work, counted in `Table::busy` until it is dropped.
struct Busy<'a>(&'a Fleet);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        lock(&self.0.apps).busy -= 1;
    }
}

/// How `launch` went.
enum Launch {
    Spawned {
        pid: u32,
        paths: Paths,
    },
    /// Another process holds the state lock.
    Attached {
        paths: Paths,
    },
    /// Logged, and the app is in backoff or broken.
    Failed(Error),
}

/// The socket file, removed when `run` returns however it returns. An exec (`reexec`) runs no destructor, so the
/// program after it keeps serving on it.
struct Published(PathBuf);

impl Drop for Published {
    fn drop(&mut self) {
        // Best effort: a socket file nobody answers on is taken over by the next supervisor's `bind`.
        let _ = std::fs::remove_file(&self.0);
    }
}

// ------------------------------------------------------------ startup

/// Run the supervisor until SIGTERM or SIGINT. `handover` is the JSON an earlier program of this process passed
/// before it re-executed (`reexec`): its lock, socket and daemons are taken over instead of taken anew.
pub fn run(handover: Option<&str>) -> Result<(), String> {
    // First of all, as in the daemon: the handlers set the flag, then write a byte that ends the accept loop's poll.
    let shutdown = Arc::new(AtomicBool::new(false));
    let (signals, signalled) =
        UnixStream::pair().map_err(|e| format!("cannot create the signal pipe: {e}"))?;
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, shutdown.clone())
            .and_then(|_| signalled.try_clone())
            .and_then(|pipe| signal_hook::low_level::pipe::register(signal, pipe))
            .map_err(|e| format!("cannot install the signal handlers: {e}"))?;
    }
    let handover = handover
        .map(|json| {
            serde_json::from_str::<Handover>(json)
                .map_err(|e| format!("unreadable --handover: {e}"))
        })
        .transpose()?;
    let home = manifest::fawkes_home()?;
    // 0700, as `run/` is: the names in it say which apps run here.
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    let socket = socket(&home);
    let lock_path = home.join("supervisor.lock");
    let lock_file = match &handover {
        Some(handover) => take_lock(
            handover.lock,
            &lock_path,
            "supervisor lock",
            "another supervisor runs",
        )?,
        None => lock_home(&lock_path, &socket)?,
    };
    let listener = match &handover {
        Some(handover) => take_listener(handover.listener, &socket)?,
        None => bind(&socket)?,
    };
    let _published = Published(socket.clone());
    let down = load_marks(&home.join("supervisor.json"))?;
    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
    // `bind` changes the umask for a moment, so no thread may start before it.
    let (echo, _finished) = Echo::start(std::io::stderr(), home.join("supervisor.log"));
    let fleet = Arc::new(Fleet {
        home,
        bin,
        socket,
        shutdown,
        apps: Mutex::new(Table {
            apps: BTreeMap::new(),
            down,
            busy: 0,
            reexecing: false,
        }),
        wake: Wake::new(),
        echo,
        lock_file,
        listener,
    });
    match handover {
        Some(handover) => fleet.take_over(handover),
        None => fleet.note(format!(
            "supervisor started, pid {}, socket {}",
            std::process::id(),
            fleet.socket.display()
        )),
    }
    let server = fleet.clone();
    thread::spawn(move || server.serve(&signals));
    fleet.supervise();
    fleet.shut_down();
    fleet.echo.flush();
    Ok(())
}

/// Take the `flock` on `supervisor.lock`, held as long as the returned file lives, and write this process's pid into
/// it for the next supervisor's refusal to name. std opens files close-on-exec, so no daemon inherits it.
fn lock_home(path: &Path, socket: &Path) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
            let mut pid = String::new();
            // The holder wrote its pid once it had the lock; a moment earlier the file is still empty.
            let _ = file.read_to_string(&mut pid);
            let pid = match pid.trim() {
                "" => "pid unknown".to_string(),
                pid => format!("pid {pid}"),
            };
            return Err(format!(
                "another supervisor already runs ({pid}, socket {}); stop it first (Ctrl-C or SIGTERM, or `fawkes \
                 service uninstall` when launchd runs it)",
                socket.display()
            ));
        }
        Err(e) => return Err(format!("cannot lock {}: {e}", path.display())),
    }
    file.set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| writeln!(file, "{}", std::process::id()))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(file)
}

/// The apps `supervisor.json` marks down; none when there is no file yet.
fn load_marks(path: &Path) -> Result<BTreeSet<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Marks>(&bytes)
            .map(|marks| marks.down)
            .map_err(|e| {
                format!(
                    "{} is corrupt ({e}); fix it, or remove it to start every app",
                    path.display()
                )
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------- the loop

impl Fleet {
    fn shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// One line on stderr, with the time, as the daemon's event lines have it.
    fn note(&self, line: impl std::fmt::Display) {
        self.echo.line(format!("{}  {line}", now()));
    }

    /// Look at the apps every `TICK`, when a daemon exits, and when a backoff runs out, until shutdown.
    fn supervise(self: &Arc<Self>) {
        loop {
            // Read before the look, so an exit after it ends the wait.
            let seen = self.wake.seen();
            if self.shutting_down() {
                return;
            }
            let next = self.tick();
            let wait = next.map_or(TICK, |at| {
                at.saturating_duration_since(Instant::now()).min(TICK)
            });
            self.wake.wait(seen, Some(wait));
        }
    }

    /// Reap the daemons that exited, read `<fawkes_home>` again, and start every app that should run. Returns when the
    /// next backoff runs out.
    fn tick(self: &Arc<Self>) -> Option<Instant> {
        let found = discovery::manifests(&self.home);
        let mut table = lock(&self.apps);
        self.reap(&mut table);
        let Table { apps, down, .. } = &mut *table;
        for (name, path) in &found {
            let app = apps
                .entry(name.clone())
                .or_insert_with(|| App::new(path.clone()));
            app.present = true;
            app.manifest.clone_from(path);
        }
        apps.retain(|name, app| {
            if found.iter().any(|(found, _)| found == name) {
                return true;
            }
            match &app.child {
                None => {
                    self.note(format!(
                        "{} is gone; the supervisor forgets {name}",
                        app.manifest.display()
                    ));
                    false
                }
                Some(child) => {
                    if app.present {
                        self.note(format!(
                            "{} is gone; {name}'s daemon (pid {}) runs on until it exits",
                            app.manifest.display(),
                            child.process.id()
                        ));
                        app.present = false;
                    }
                    true
                }
            }
        });
        if self.shutting_down() {
            return None;
        }
        let mut next: Option<Instant> = None;
        for (name, app) in apps.iter_mut() {
            if app.child.is_some() || down.contains(name) {
                continue;
            }
            match app.wait {
                Wait::Broken { mtime } if modified(&app.manifest) == mtime => continue,
                Wait::Backoff { until, .. } if until > Instant::now() => {
                    next = Some(next.map_or(until, |next| next.min(until)));
                    continue;
                }
                _ => {}
            }
            app.wait = Wait::None;
            // Logged, and a failure leaves the app in backoff or broken.
            let _ = self.launch(name, app);
            if let Wait::Backoff { until, .. } = app.wait {
                next = Some(next.map_or(until, |next| next.min(until)));
            }
        }
        next
    }

    /// Reap every daemon that has exited and decide what comes next for its app (`after_exit`).
    fn reap(&self, table: &mut Table) {
        let shutting_down = self.shutting_down();
        let Table { apps, down, .. } = table;
        let mut marked = false;
        for (name, app) in apps.iter_mut() {
            let Some(child) = &mut app.child else {
                continue;
            };
            // An error means it is no child of ours any more: gone, by whatever reaped it.
            let status = match child.process.try_wait() {
                Ok(None) => continue,
                Ok(Some(status)) => Some(status),
                Err(_) => None,
            };
            let pid = child.process.id();
            let asked = shutting_down || child.stopping;
            let (failures, next) = after_exit(
                status.and_then(|status| status.code()),
                app.failures,
                child.started.elapsed(),
                asked,
            );
            app.child = None;
            app.failures = failures;
            let ended = describe_exit(status);
            app.ended = Some((pid, ended.clone()));
            let what = format!("{name}'s daemon (pid {pid}) {ended}");
            match next {
                Next::Leave => self.note(what),
                Next::MarkDown => {
                    marked |= down.insert(name.clone());
                    self.note(format!(
                        "{what}; {name} stays down until `fawkes up --manifest {name}`"
                    ));
                }
                Next::Broken => {
                    app.wait = Wait::Broken {
                        mtime: modified(&app.manifest),
                    };
                    self.note(format!(
                        "{what}, which says its manifest is invalid; starting it again once {} changes",
                        app.manifest.display()
                    ));
                }
                Next::Restart { after } => {
                    app.wait = Wait::backoff(after);
                    self.note(format!(
                        "{what}; starting it again in {} s",
                        after.as_secs()
                    ));
                }
            }
        }
        if marked && let Err(e) = self.save_marks(down) {
            self.note(format!(
                "{}; the down mark holds until the supervisor ends",
                e.message
            ));
        }
    }

    /// Start the app's daemon unless another process holds its state lock. Called with `apps` held, which keeps the
    /// descriptors `reexec` makes inheritable out of the spawn.
    fn launch(self: &Arc<Self>, name: &str, app: &mut App) -> Launch {
        let manifest = match Manifest::load(&app.manifest) {
            Ok(manifest) => manifest,
            Err(e) => {
                app.attached = false;
                app.wait = Wait::Broken {
                    mtime: modified(&app.manifest),
                };
                self.note(format!(
                    "{name} is broken: {e}; it starts once {} changes",
                    app.manifest.display()
                ));
                return Launch::Failed(err(
                    ErrorCode::StartFailed,
                    format!("{e}; fix the manifest and run `fawkes up --manifest {name}` again"),
                ));
            }
        };
        let paths = manifest.paths();
        // As `fawkes service install` does: the daemon's log opens before the daemon exists to create the directory.
        if let Err(e) = DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&paths.root)
        {
            let why = format!("cannot create {}: {e}", paths.root.display());
            return self.not_started(name, app, &why);
        }
        match lock_held(&paths.lock()) {
            Ok(true) => {
                if !app.attached {
                    self.note(format!(
                        "{name}'s daemon already runs, not as the supervisor's child (its state lock {} is held); \
                         the supervisor leaves it be",
                        paths.lock().display()
                    ));
                }
                app.attached = true;
                return Launch::Attached { paths };
            }
            Ok(false) => app.attached = false,
            Err(why) => return self.not_started(name, app, &why),
        }
        let log = paths.daemon_log();
        match self.spawn_daemon(&app.manifest, &log) {
            Ok(process) => {
                let pid = process.id();
                let fleet = self.clone();
                process::on_exit(&process, move || fleet.wake.notify());
                app.child = Some(Child {
                    process,
                    started: Instant::now(),
                    stopping: false,
                    stop_wait: manifest.daemon_stop_wait(),
                });
                self.note(format!(
                    "started {name}'s daemon (pid {pid}); its log is {}",
                    log.display()
                ));
                Launch::Spawned { pid, paths }
            }
            Err(e) => {
                let why = format!("cannot run {}: {e}", self.bin.display());
                self.not_started(name, app, &why)
            }
        }
    }

    /// A start that failed before the daemon ran: counted as a failure, with its backoff.
    fn not_started(&self, name: &str, app: &mut App, why: &str) -> Launch {
        app.failures = app.failures.saturating_add(1);
        let after = backoff(app.failures);
        app.wait = Wait::backoff(after);
        self.note(format!(
            "cannot start {name}'s daemon: {why}; trying again in {} s",
            after.as_secs()
        ));
        Launch::Failed(err(
            ErrorCode::StartFailed,
            format!("cannot start {name}'s daemon: {why}"),
        ))
    }

    /// `<bin> --manifest <path> up`, in its own process group, its output appended to `log`. That `up` runs the daemon
    /// itself, not a request to start it, because its parent is the supervisor that answers (`main.rs`, `run_up`).
    fn spawn_daemon(&self, manifest: &Path, log: &Path) -> std::io::Result<ChildPid> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(log)?;
        let mut command = Command::new(&self.bin);
        command
            .arg("--manifest")
            .arg(manifest)
            .arg("up")
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .process_group(0);
        for name in NOT_INHERITED {
            command.env_remove(name);
        }
        command.spawn().map(ChildPid::from)
    }

    /// End every daemon the supervisor started with SIGTERM and wait for them, for at most the longest of their bounds.
    /// Attached daemons are not its own and run on.
    fn shut_down(&self) {
        let (count, bound) = {
            let table = lock(&self.apps);
            let children: Vec<&Child> = table
                .apps
                .values()
                .filter_map(|app| app.child.as_ref())
                .collect();
            for child in &children {
                // Unreaped, so the pid is still this child's. A daemon that has exited already needs no signal.
                let _ = kill_process(child.process.pid(), Signal::TERM);
            }
            let bound = children.iter().map(|child| child.stop_wait).max();
            (children.len(), bound.unwrap_or_default())
        };
        let until = Instant::now() + bound + STOP_MARGIN;
        if count > 0 {
            self.note(format!(
                "stopping {count} daemon{}, for at most {} s",
                if count == 1 { "" } else { "s" },
                (bound + STOP_MARGIN).as_secs()
            ));
        }
        loop {
            let seen = self.wake.seen();
            let left = {
                let mut table = lock(&self.apps);
                self.reap(&mut table);
                table
                    .apps
                    .values()
                    .filter(|app| app.child.is_some())
                    .count()
            };
            let wait = until.saturating_duration_since(Instant::now());
            if left == 0 {
                break;
            }
            if wait.is_zero() {
                self.note(format!(
                    "{left} daemon{} did not stop in time and run on",
                    if left == 1 { "" } else { "s" }
                ));
                break;
            }
            self.wake.wait(seen, Some(wait));
        }
        self.note("supervisor stopped");
    }

    /// Record the down marks in `supervisor.json`, durably: a mark lost to a crash would start an app that was brought
    /// down.
    fn save_marks(&self, down: &BTreeSet<String>) -> R<()> {
        let path = self.home.join("supervisor.json");
        let marks = Marks { down: down.clone() };
        let bytes = serde_json::to_vec(&marks).map_err(|e| {
            err(
                ErrorCode::Internal,
                format!("cannot write the down marks: {e}"),
            )
        })?;
        write_durably(&path, &bytes).map_err(|e| {
            err(
                ErrorCode::StorageFailed,
                format!("cannot write {}: {e}", path.display()),
            )
        })
    }
}

/// Whether another process holds the `flock` on the state lock at `path`: a daemon that runs. A missing file is a
/// free lock. The lock taken to find out goes with the file at once, before the daemon is spawned to take it.
fn lock_held(path: &Path) -> Result<bool, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(true),
        Err(e) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// The modification time of `path`, none when it cannot be read.
fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// How a daemon ended, for the lines: `exited with status 1`, `was ended by signal 9`.
fn describe_exit(status: Option<ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("exited with status {code}"),
            (None, Some(signal)) => format!("was ended by signal {signal}"),
            (None, None) => "exited".into(),
        },
        None => "exited".into(),
    }
}

// ----------------------------------------------------------- requests

impl Fleet {
    /// Accept clients until the process ends, asleep in `poll` on the socket and the signal pipe in between. A signal
    /// ends the sleep, and the start of shutdown moves `wake` for `supervise`.
    fn serve(self: Arc<Self>, signals: &UnixStream) {
        if let Err(e) = self
            .listener
            .set_nonblocking(true)
            .and_then(|()| signals.set_nonblocking(true))
        {
            self.note(format!("cannot serve {}: {e}", self.socket.display()));
            return;
        }
        let mut pipe = signals;
        loop {
            // Drained before the flag is read: a signal after the drain leaves a byte that ends the next `poll`.
            while pipe.read(&mut [0; 64]).is_ok_and(|n| n > 0) {}
            if self.shutting_down() {
                self.wake.notify();
            }
            let failed = match self.listener.accept() {
                Ok((stream, _)) => {
                    let fleet = self.clone();
                    thread::spawn(move || fleet.handle(stream));
                    None
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut ready = [
                        PollFd::new(&self.listener, PollFlags::IN),
                        PollFd::new(signals, PollFlags::IN),
                    ];
                    match poll(&mut ready, None) {
                        Ok(_) | Err(rustix::io::Errno::INTR) => None,
                        Err(e) => Some(format!("cannot wait for clients: {e}")),
                    }
                }
                Err(e) => Some(format!("accept failed: {e}")),
            };
            if let Some(e) = failed {
                self.note(e);
                sleep(ACCEPT_RETRY);
            }
        }
    }

    fn handle(self: Arc<Self>, stream: UnixStream) {
        // Blocking, with timeouts: on macOS an accepted socket inherits the listener's non-blocking mode.
        if stream.set_nonblocking(false).is_err() {
            return;
        }
        let read = read_request(&stream, Instant::now() + REQUEST_DEADLINE, MAX_REQUEST);
        let mut out = Out::new(stream);
        let result = read
            .and_then(|line| FleetRequest::parse(&line))
            .and_then(|request| match request {
                FleetRequest::Status => Ok(self.status().into()),
                FleetRequest::Start { app } => self.start(&app).map(Reply::from),
                FleetRequest::Down { app } => self.down(&app).map(Reply::from),
                FleetRequest::Reexec => self.reexec(&mut out).map(Reply::from),
            });
        out.finish(result);
    }

    /// `not_found` for an app the supervisor does not know, listing those it does.
    fn unknown(&self, name: &str, table: &Table) -> Error {
        let names: Vec<&str> = table.apps.keys().map(String::as_str).collect();
        let known = if names.is_empty() {
            format!("it has none: {} holds no <name>.toml", self.home.display())
        } else {
            format!("its apps are {}", names.join(", "))
        };
        err(
            ErrorCode::NotFound,
            format!(
                "the supervisor knows no app {name} ({known}); an app is a manifest {}/<name>.toml",
                self.home.display()
            ),
        )
    }

    fn status(&self) -> FleetStatus {
        let (mut apps, attached) = {
            // A client confirms a `reexec` by this answer, so it comes from the program after the exec, never from this
            // one in its last moments: the exec ends this connection and the client asks again. A `reexec` that gives
            // up clears the flag, and this one answers.
            let table = loop {
                let table = lock(&self.apps);
                if !table.reexecing {
                    break table;
                }
                drop(table);
                sleep(REEXEC_LOOK);
            };
            let mut attached = Vec::new();
            let apps: Vec<FleetApp> = table
                .apps
                .iter()
                .enumerate()
                .map(|(index, (name, app))| {
                    let state = if app.child.is_some() {
                        AppState::Running
                    } else if app.attached {
                        attached.push((index, app.manifest.clone()));
                        AppState::Attached
                    } else if table.down.contains(name) {
                        AppState::Down
                    } else if matches!(app.wait, Wait::Broken { .. }) {
                        AppState::Broken
                    } else {
                        AppState::Backoff
                    };
                    let restart_at = match (state, &app.wait) {
                        (AppState::Backoff, Wait::Backoff { at, .. }) => Some(format_time(*at)),
                        _ => None,
                    };
                    FleetApp {
                        name: name.clone(),
                        manifest: app.manifest.clone(),
                        state,
                        daemon_pid: app.child.as_ref().map(|child| child.process.id()),
                        restart_at,
                        failures: app.failures,
                    }
                })
                .collect();
            (apps, attached)
        };
        // Outside the lock: a daemon may be slow to answer.
        for (index, manifest) in attached {
            apps[index].daemon_pid = Manifest::load(&manifest)
                .ok()
                .and_then(|manifest| daemon_status(&manifest.paths().socket()))
                .map(|status| status.daemon_pid);
        }
        FleetStatus {
            pid: std::process::id(),
            version: VERSION.into(),
            socket: self.socket.clone(),
            apps,
        }
    }

    /// `start`: clear the app's down mark and start its daemon now, whatever backoff it is in, unless it runs. Answered
    /// once the daemon answers `status`, or with `start_failed` when it exits first.
    fn start(self: &Arc<Self>, name: &str) -> R<StartResult> {
        let busy = {
            let mut table = lock(&self.apps);
            if self.shutting_down() {
                return Err(invalid_state(
                    "the supervisor is shutting down; start the app once a supervisor runs again",
                ));
            }
            if !table.apps.contains_key(name) {
                return Err(self.unknown(name, &table));
            }
            table.refuse_reexecing()?;
            if table.down.remove(name) {
                if let Err(e) = self.save_marks(&table.down) {
                    table.down.insert(name.into());
                    return Err(e);
                }
                self.note(format!("{name} is no longer down"));
            }
            table.busy += 1;
            Busy(self)
        };
        // A daemon `down` ended may still be on its way out, its state lock released or not: the new one follows it.
        let stopping = |table: &Table| {
            let child = table.apps.get(name).and_then(|app| app.child.as_ref());
            child
                .filter(|child| child.stopping)
                .map(|child| child.stop_wait)
        };
        if let Some(stop_wait) = stopping(&lock(&self.apps)) {
            let until = Instant::now() + stop_wait + STOP_MARGIN;
            while stopping(&lock(&self.apps)).is_some() && Instant::now() < until {
                sleep(POLL);
            }
        }
        let launched = {
            let mut table = lock(&self.apps);
            // Checked again under the lock: `shut_down` signals only the daemons it finds in the table.
            if self.shutting_down() {
                return Err(invalid_state(
                    "the supervisor is shutting down; start the app once a supervisor runs again",
                ));
            }
            let Some(app) = table.apps.get_mut(name) else {
                return Err(self.unknown(name, &table));
            };
            if let Some(child) = &app.child {
                if child.stopping {
                    return Err(err(
                        ErrorCode::Busy,
                        format!(
                            "{name}'s daemon is still stopping; run `fawkes up` again once it has exited"
                        ),
                    ));
                }
                return Ok(StartResult {
                    app: name.into(),
                    daemon_pid: child.process.id(),
                    already: true,
                });
            }
            app.wait = Wait::None;
            self.launch(name, app)
        };
        let (pid, paths) = match launched {
            Launch::Spawned { pid, paths } => (Some(pid), paths),
            Launch::Attached { paths } => (None, paths),
            Launch::Failed(e) => return Err(e),
        };
        let result = self.answered(name, pid, &paths);
        drop(busy);
        result
    }

    /// Wait until the daemon of `paths` answers `status`, as process `pid` when the supervisor started it (attached
    /// otherwise), for at most `START_WAIT`.
    fn answered(&self, name: &str, pid: Option<u32>, paths: &Paths) -> R<StartResult> {
        let log = paths.daemon_log();
        let until = Instant::now() + START_WAIT;
        loop {
            if let Some(status) = daemon_status(&paths.socket())
                && pid.is_none_or(|pid| status.daemon_pid == pid)
            {
                return Ok(StartResult {
                    app: name.into(),
                    daemon_pid: status.daemon_pid,
                    already: pid.is_none(),
                });
            }
            if let Some(pid) = pid {
                let table = lock(&self.apps);
                let app = table.apps.get(name);
                let running = app
                    .and_then(|app| app.child.as_ref())
                    .is_some_and(|child| child.process.id() == pid);
                if !running {
                    let ended = app
                        .and_then(|app| app.ended.as_ref())
                        .filter(|(ended, _)| *ended == pid)
                        .map_or("exited", |(_, how)| how.as_str());
                    return Err(err(
                        ErrorCode::StartFailed,
                        format!(
                            "{name}'s daemon (pid {pid}) {ended} before it answered; {} says why",
                            log.display()
                        ),
                    )
                    .with_log(log));
                }
            }
            if Instant::now() >= until {
                return Err(err(
                    ErrorCode::StartFailed,
                    format!(
                        "{name}'s daemon did not answer on {} within {} s; {} says why",
                        paths.socket().display(),
                        START_WAIT.as_secs(),
                        log.display()
                    ),
                )
                .with_log(log));
            }
            sleep(POLL);
        }
    }

    /// `down`: mark the app down, durably, then end its daemon. Answers with the daemon's own answer, or a null
    /// `daemon_pid` when none ran.
    fn down(&self, name: &str) -> R<FleetDownResult> {
        let (busy, daemon, child) = {
            let mut table = lock(&self.apps);
            table.refuse_reexecing()?;
            let Table {
                apps, down, busy, ..
            } = &mut *table;
            let Some(app) = apps.get_mut(name) else {
                return Err(self.unknown(name, &table));
            };
            if down.insert(name.into())
                && let Err(e) = self.save_marks(down)
            {
                down.remove(name);
                return Err(e);
            }
            let child = app.child.as_mut().map(|child| {
                child.stopping = true;
                child.process.id()
            });
            app.attached = false;
            app.wait = Wait::None;
            // Read now, unlocked by nobody: a manifest that has become invalid leaves only the child's pid to end.
            let daemon = Manifest::load(&app.manifest)
                .ok()
                .map(|manifest| (manifest.paths().socket(), manifest.daemon_stop_wait()));
            *busy += 1;
            (Busy(self), daemon, child)
        };
        self.note(format!(
            "{name} is down until `fawkes up --manifest {name}`"
        ));
        let answer = daemon.and_then(|(socket, stop_wait)| {
            ask(&socket, &Request::Down, Some(stop_wait + STOP_MARGIN))
        });
        let result = match answer {
            Some(Ok(result)) => DownResult::deserialize(&result)
                .map(|down| FleetDownResult {
                    name: down.name,
                    daemon_pid: Some(down.daemon_pid),
                    stopped: down.stopped,
                })
                .map_err(|e| {
                    err(
                        ErrorCode::Internal,
                        format!("{name}'s daemon answered `down` with what the supervisor cannot read: {e}"),
                    )
                }),
            Some(Err(e)) => Err(e),
            // A daemon still starting does not answer yet: its own SIGTERM ends it.
            None => Ok(FleetDownResult {
                name: name.into(),
                daemon_pid: child.filter(|pid| self.terminate(name, *pid)),
                stopped: None,
            }),
        };
        // The daemon answers just before it exits: wait for the reap too, so a `status` after the answer says `down`.
        if let (Some(pid), Ok(_)) = (child, &result) {
            let until = Instant::now() + STOP_MARGIN;
            while self.runs(name, pid) && Instant::now() < until {
                sleep(POLL);
            }
        }
        drop(busy);
        result
    }

    /// Whether the app's daemon is still the unreaped child `pid`.
    fn runs(&self, name: &str, pid: u32) -> bool {
        let table = lock(&self.apps);
        let child = table.apps.get(name).and_then(|app| app.child.as_ref());
        child.is_some_and(|child| child.process.id() == pid)
    }

    /// SIGTERM to the app's daemon, when it is still the unreaped child `pid`, so the pid cannot name another process.
    fn terminate(&self, name: &str, pid: u32) -> bool {
        let table = lock(&self.apps);
        let child = table.apps.get(name).and_then(|app| app.child.as_ref());
        match child.filter(|child| child.process.id() == pid) {
            Some(child) => kill_process(child.process.pid(), Signal::TERM).is_ok(),
            None => false,
        }
    }

    /// `reexec`: answer, then replace the program with the binary installed at this one's path, handing over the lock,
    /// the socket and the daemons. Refused while a `start`, `down` or another `reexec` is at work, whose answer the
    /// exec would cut off; `reexecing` keeps new ones out meanwhile, so the binary is run and the answer written
    /// without `apps`, which the tick and every request wait for.
    fn reexec(&self, out: &mut Out) -> R<ReexecResult> {
        {
            let mut table = lock(&self.apps);
            if self.shutting_down() {
                return Err(invalid_state("the supervisor is shutting down"));
            }
            table.refuse_reexecing()?;
            if table.busy > 0 {
                return Err(err(
                    ErrorCode::Busy,
                    "a start or down is at work; ask again once it has answered",
                ));
            }
            table.reexecing = true;
        }
        let to = match installed_version(&self.bin) {
            Ok(to) => to,
            Err(why) => {
                lock(&self.apps).reexecing = false;
                return Err(err(
                    ErrorCode::StartFailed,
                    format!(
                        "{why}; the supervisor carries on as fawkes {VERSION}; install a working fawkes at {} and \
                         ask again",
                        self.bin.display()
                    ),
                ));
            }
        };
        let result = ReexecResult {
            pid: std::process::id(),
            from: VERSION.into(),
            to: to.clone(),
        };
        // Answered and closed before the exec: the program after it has no record of this request.
        let mut answer = std::mem::replace(out, Out::none());
        answer.finish(Ok(result.clone().into()));
        drop(answer);
        // Written before `apps` is taken: a stderr that does not drain holds this thread up to a second.
        self.note(format!("re-executing on fawkes {to}"));
        self.echo.flush();
        // Held to the exec: no spawn, which would inherit the lock and the socket, and no reap of a daemon handed over.
        let mut table = lock(&self.apps);
        if self.shutting_down() {
            // The program after the exec would not know a shutdown was asked for.
            table.reexecing = false;
            drop(table);
            self.note("the reexec gave way to the shutdown");
            return Ok(result);
        }
        let handover = Handover {
            from: VERSION.into(),
            lock: self.lock_file.as_raw_fd(),
            listener: self.listener.as_raw_fd(),
            apps: table
                .apps
                .iter()
                .filter_map(|(name, app)| {
                    let child = app.child.as_ref()?;
                    Some(HandedApp {
                        name: name.clone(),
                        manifest: app.manifest.clone(),
                        pid: child.process.id(),
                        uptime: child.started.elapsed().as_secs_f64(),
                        failures: app.failures,
                        stopping: child.stopping,
                        stop_wait: child.stop_wait.as_secs_f64(),
                    })
                })
                .collect(),
        };
        let args: [OsString; 2] = ["up".into(), "--all".into()];
        let keep = [self.lock_file.as_fd(), self.listener.as_fd()];
        let why = exec_with_handover(&self.bin, &args, &keep, &handover);
        table.reexecing = false;
        drop(table);
        self.note(format!(
            "cannot re-execute ({why}); the supervisor carries on as fawkes {VERSION}"
        ));
        Ok(result)
    }

    /// The program after a `reexec`: the daemons it was handed are its children again, waited for by pid.
    fn take_over(self: &Arc<Self>, handover: Handover) {
        let mut table = lock(&self.apps);
        let mut kept = 0;
        for handed in handover.apps {
            let mut app = App::new(handed.manifest);
            app.failures = handed.failures;
            match ChildPid::handed_over(handed.pid) {
                Some(process) => {
                    let fleet = self.clone();
                    process::on_exit(&process, move || fleet.wake.notify());
                    let uptime = Duration::try_from_secs_f64(handed.uptime).unwrap_or_default();
                    app.child = Some(Child {
                        process,
                        started: Instant::now()
                            .checked_sub(uptime)
                            .unwrap_or_else(Instant::now),
                        stopping: handed.stopping,
                        stop_wait: Duration::try_from_secs_f64(handed.stop_wait)
                            .unwrap_or_default(),
                    });
                    kept += 1;
                }
                None => self.note(format!(
                    "{}'s daemon (pid {}) is no child of this process any more",
                    handed.name, handed.pid
                )),
            }
            table.apps.insert(handed.name, app);
        }
        self.note(format!(
            "supervisor re-executed from fawkes {} to {VERSION}, pid {}; {kept} daemon{} keep running",
            handover.from,
            std::process::id(),
            if kept == 1 { "" } else { "s" }
        ));
    }
}

/// What the supervisor in `home` answers to `request`, waiting `timeout` (none: as long as it takes) for each line:
/// none when no supervisor answers there. For the CLI's `up` and `down` of an app in `home`, and its `--all`.
pub fn request(
    home: &Path,
    request: &FleetRequest,
    timeout: Option<Duration>,
) -> Option<R<serde_json::Value>> {
    ask(&socket(home), request, timeout)
}

/// Send `request` (a `Request` or a `FleetRequest`) on `socket` and read its final line, waiting `timeout` for each
/// line; none when nothing answers.
fn ask(
    socket: &Path,
    request: &impl Serialize,
    timeout: Option<Duration>,
) -> Option<R<serde_json::Value>> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(timeout).ok()?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok()?;
    let line = serde_json::to_string(request).ok()?;
    writeln!(stream, "{line}").ok()?;
    for line in BufReader::new(stream).lines() {
        match read_line(&line.ok()?) {
            Some((_, Line::Done { result, .. })) => return Some(Ok(result)),
            Some((_, Line::Failed { error, .. })) => return Some(Err(error)),
            _ => {}
        }
    }
    None
}

/// The `status` of the daemon on `socket`, when one answers in `ANSWER_WAIT`.
fn daemon_status(socket: &Path) -> Option<Status> {
    let result = ask(socket, &Request::Status, Some(ANSWER_WAIT))?.ok()?;
    Status::deserialize(&result).ok()
}

// ---------------------------------------------------------- handover

/// What one program of the supervisor hands the next, as the JSON of the hidden `up --all --handover`: the descriptors
/// it keeps open across the exec and the daemons it started. The down marks are in `supervisor.json`; an app in
/// backoff or broken is looked at afresh. A program reads the handover of an older one, so a field added later needs
/// `#[serde(default)]`.
#[derive(Serialize, Deserialize)]
struct Handover {
    /// The version the program before ran.
    from: String,
    /// The descriptor that holds the `flock` on `supervisor.lock`.
    lock: RawFd,
    /// The listening socket, with the connections waiting in its backlog.
    listener: RawFd,
    apps: Vec<HandedApp>,
}

/// One daemon the supervisor started, as `App` and `Child` hold it.
#[derive(Serialize, Deserialize)]
struct HandedApp {
    name: String,
    manifest: PathBuf,
    pid: u32,
    /// The seconds it has run, which set `Child::started` again on the next program's monotonic clock.
    uptime: f64,
    failures: u32,
    stopping: bool,
    /// `Child::stop_wait`, in seconds.
    stop_wait: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_secs(3);

    #[test]
    fn a_failing_daemon_backs_off_doubling_to_30_s() {
        let mut failures = 0;
        let mut waits = Vec::new();
        for _ in 0..8 {
            let (counted, next) = after_exit(Some(1), failures, SHORT, false);
            let Next::Restart { after } = next else {
                panic!("{next:?}");
            };
            failures = counted;
            waits.push(after.as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(failures, 8);
    }

    #[test]
    fn a_signal_is_a_failure_and_a_minute_of_running_starts_the_backoff_over() {
        assert_eq!(
            after_exit(None, 4, SHORT, false),
            (5, Next::Restart { after: backoff(5) })
        );
        assert_eq!(
            after_exit(None, 4, STABLE_RUN, false),
            (
                1,
                Next::Restart {
                    after: Duration::from_secs(1)
                }
            )
        );
        assert_eq!(
            after_exit(Some(1), 5, Duration::from_millis(59_999), false).0,
            6
        );
    }

    #[test]
    fn exit_0_marks_the_app_down_and_exit_2_waits_for_the_manifest() {
        assert_eq!(after_exit(Some(0), 3, SHORT, false), (0, Next::MarkDown));
        assert_eq!(after_exit(Some(2), 3, SHORT, false), (4, Next::Broken));
    }

    /// An exit the supervisor asked for (its shutdown, or `down`) marks nothing and starts nothing, however it ends.
    #[test]
    fn an_exit_the_supervisor_asked_for_changes_nothing() {
        for code in [Some(0), Some(1), Some(2), None] {
            assert_eq!(
                after_exit(code, 2, SHORT, true),
                (2, Next::Leave),
                "{code:?}"
            );
        }
    }

    #[test]
    fn an_app_in_home_is_known_by_its_file_however_the_path_is_spelt() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("xana.toml"), "").unwrap();
        std::fs::write(temp.path().join("xana.toml"), "").unwrap();
        assert_eq!(
            app_in_home(&home.join("xana.toml"), &home).as_deref(),
            Some("xana")
        );
        assert_eq!(
            app_in_home(&home.join("../home/./xana.toml"), &home).as_deref(),
            Some("xana")
        );
        let link = temp.path().join("link.toml");
        std::os::unix::fs::symlink(home.join("xana.toml"), &link).unwrap();
        assert_eq!(app_in_home(&link, &home), None, "named link, not xana");
        assert_eq!(app_in_home(&temp.path().join("xana.toml"), &home), None);
        assert_eq!(app_in_home(&home.join("gone.toml"), &home), None);
    }
}
