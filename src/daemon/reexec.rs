//! `reexec`: the daemon moves onto the fawkes binary installed at its path by executing it in its own process. The pid
//! stays, and with it the app, which is still the daemon's child: nothing stops and nothing rolls back. The program
//! before hands the program after its state lock, its listening socket and the live app (`Handover`), which that one
//! takes over instead of locking, binding and starting anew.

use super::{ANSWER_WAIT, App, Daemon, Inner, Live, Slot, lock};
use crate::process::{self, ChildPid};
use crate::protocol::{
    DaemonAction, ErrorCode, EventKind, Facts, LastExit, Out, R, ReexecResult, Reply, err,
    invalid_state,
};
use crate::state::{Paths, Txn};
use rustix::fs::{FlockOperation, flock};
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the installed binary has to print its version before `reexec` gives up on it.
const VERSION_WAIT: Duration = Duration::from_secs(5);

/// How long a `--version` that overran gets after SIGTERM, before SIGKILL.
const VERSION_GRACE: Duration = Duration::from_secs(1);

// ---------------------------------------------------------- handover

/// What one program of the daemon hands the next, as the JSON of the hidden `up --handover`: the descriptors it keeps
/// open across the exec, and what `Inner` holds beyond state.json. No transaction is under way (`check_reexec`), so
/// state.json says the rest.
#[derive(Serialize, Deserialize)]
pub(super) struct Handover {
    /// The version the program before ran.
    from: String,
    /// The descriptor that holds the `flock` on `<state_dir>/lock`.
    state_lock: RawFd,
    /// The listening socket, with the connections waiting in its backlog.
    listener: RawFd,
    /// The live app; None when none runs.
    app: Option<HandedApp>,
    /// No app runs because it was given up on (`Live::Failed`), not because it was stopped.
    failed: bool,
    restarts: u32,
    last_started: Option<String>,
    last_exit: Option<LastExit>,
}

/// The live app, as `App` holds it.
#[derive(Serialize, Deserialize)]
struct HandedApp {
    pid: u32,
    generation: String,
    started_at: String,
    /// The seconds it has run, which set `App::started` again on the next program's monotonic clock.
    uptime: f64,
    info: BTreeMap<String, String>,
}

impl Handover {
    pub(super) fn read(json: &str) -> Result<Handover, String> {
        serde_json::from_str(json).map_err(|e| format!("unreadable --handover: {e}"))
    }

    /// Take over the state lock: the descriptor must be `<state_dir>/lock` and hold its lock, or this daemon has no
    /// right to the state directory.
    pub(super) fn state_lock(&self, paths: &Paths) -> Result<File, String> {
        let file = File::from(take_fd(self.state_lock)?);
        let path = paths.lock();
        let same = match (file.metadata(), std::fs::metadata(&path)) {
            (Ok(held), Ok(named)) => (held.dev(), held.ino()) == (named.dev(), named.ino()),
            _ => false,
        };
        if !same {
            return Err(format!(
                "descriptor {} handed over is not {}",
                self.state_lock,
                path.display()
            ));
        }
        // The same open file locks again at once when it holds the lock, and fails when another one does.
        flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            format!(
                "the state lock handed over is not held ({e}); another daemon runs for {}",
                paths.root.display()
            )
        })?;
        Ok(file)
    }

    /// Take over the listening socket, which must be the one bound at `socket`. Its file and run link stay as they
    /// are.
    pub(super) fn listener(&self, socket: &Path) -> Result<UnixListener, String> {
        let listener = UnixListener::from(take_fd(self.listener)?);
        let bound = listener.local_addr().ok();
        if bound.as_ref().and_then(|addr| addr.as_pathname()) != Some(socket) {
            return Err(format!(
                "descriptor {} handed over is not the socket {}",
                self.listener,
                socket.display()
            ));
        }
        Ok(listener)
    }
}

/// Own descriptor `fd`, which the program before kept open across the exec, and make it close-on-exec again, so no
/// app or step inherits it.
fn take_fd(fd: RawFd) -> Result<OwnedFd, String> {
    // 0 to 2 are stdio, never handed over.
    if fd < 3 {
        return Err(format!("descriptor {fd} cannot be handed over"));
    }
    // SAFETY: the program before kept `fd` open across the exec for this one to own. Nothing here has opened or
    // closed it before: an open descriptor's number is not handed out again, and each handed number is taken once.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if let Err(e) = inherited(owned.as_fd(), false) {
        // Not open after all, and not ours to close then.
        let _ = owned.into_raw_fd();
        return Err(format!("descriptor {fd} handed over is unusable: {e}"));
    }
    Ok(owned)
}

/// Let `fd` stay open across an exec, or close it there (std's default for every descriptor it opens).
fn inherited(fd: BorrowedFd<'_>, keep: bool) -> rustix::io::Result<()> {
    let flags = fcntl_getfd(fd)?;
    fcntl_setfd(
        fd,
        if keep {
            flags.difference(FdFlags::CLOEXEC)
        } else {
            flags.union(FdFlags::CLOEXEC)
        },
    )
}

/// Replace this process's program with `bin <args> --handover <handover>`, keeping `keep` open across the exec. The pid
/// stays, and with it every child. Returns only when the exec failed, why, with `keep` close-on-exec again.
pub(super) fn exec_with_handover(
    bin: &Path,
    args: &[OsString],
    keep: &[BorrowedFd<'_>],
    handover: &Handover,
) -> String {
    let json = match serde_json::to_string(handover) {
        Ok(json) => json,
        Err(e) => return format!("cannot write the handover: {e}"),
    };
    let failed = match keep.iter().try_for_each(|fd| inherited(*fd, true)) {
        Ok(()) => Command::new(bin)
            .args(args)
            .arg("--handover")
            .arg(json)
            .exec()
            .to_string(),
        Err(e) => format!("cannot keep the state lock and the socket open across the exec: {e}"),
    };
    for fd in keep {
        // Fails only for a descriptor that is not open, which inherits nothing.
        let _ = inherited(*fd, false);
    }
    failed
}

// ------------------------------------------------------------- daemon

impl Daemon {
    /// `fawkes reexec`: check, answer, and have `run` replace the program (`reexec_now`). Both slots stay taken until
    /// then, so no step starts that the exec would cut short.
    pub(super) fn reexec_command(self: &Arc<Self>, out: &mut Out) -> R<Reply> {
        self.check_reexec(&mut self.inner())?;
        let _busy = self.begin("reexec", Slot::Both)?;
        let to = self.installed_version()?;
        // Answered and closed before the exec: the program after it has no record of this request, and the client
        // confirms by asking `status`.
        let mut answer = std::mem::replace(out, Out::none());
        answer.finish(Ok(ReexecResult {
            pid: std::process::id(),
            from: VERSION.into(),
            to,
        }
        .into()));
        drop(answer);
        let (asked, given_up) = mpsc::channel::<()>();
        {
            let mut waiting = lock(&self.reexec);
            // A shutdown that has begun would never look at it, and keep waiting for the slots held here.
            if self.shutting_down() {
                return Ok(Reply::Streamed);
            }
            *waiting = Some(asked);
        }
        self.wake_serve();
        // After an exec nothing comes back here; disconnected when `run` gave up on it (and logged why) or shut down
        // instead.
        let _ = given_up.recv();
        Ok(Reply::Streamed)
    }

    /// What `reexec` needs: no transaction, no crash restart waiting, and no step whose thread the exec would end.
    /// The slots are `begin`'s to check.
    fn check_reexec(&self, inner: &mut Inner) -> R<()> {
        if self.shutting_down() {
            return Err(err(ErrorCode::Busy, "the daemon is shutting down"));
        }
        if let Some(txn) = &inner.store.state().txn {
            let (generation, doing) = match txn {
                Txn::Switching { generation, .. } => (generation, "is being switched to"),
                Txn::Pending(pending) => (&pending.generation, "is pending"),
                Txn::RollingBack { pending, .. } => (&pending.generation, "is being rolled back"),
            };
            return Err(invalid_state(format!(
                "{generation} {doing}; run `fawkes reexec` again once `fawkes wait` reports it decided"
            ))
            .about(generation));
        }
        inner.refuse_exited()?;
        if let Live::Backoff { at, .. } = &inner.live {
            return Err(invalid_state(format!(
                "the app crashed and restarts at {at}; run `fawkes reexec` again once it runs"
            )));
        }
        if let Some(app) = inner.running()
            && inner.checking == Some(app.process.id())
        {
            return Err(err(
                ErrorCode::Busy,
                format!(
                    "the health check of {} is in progress; try again when it has finished",
                    app.generation
                ),
            ));
        }
        Ok(())
    }

    /// The version of the binary the exec would run, which must start and say it is fawkes.
    fn installed_version(&self) -> R<String> {
        let bin = self.bin.display();
        let what = format!("`{bin} --version`");
        let refused = |why: String| {
            err(
                ErrorCode::StartFailed,
                format!(
                    "{why}; the daemon carries on as fawkes {VERSION}; install a working fawkes at {bin} and run \
                     `fawkes reexec` again"
                ),
            )
        };
        let mut command = Command::new(&self.bin);
        command
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let finished =
            process::run_bounded(&mut command, VERSION_WAIT, VERSION_GRACE, &what, |_| {})
                .map_err(refused)?;
        if !finished.status.success() {
            return Err(refused(format!("{what} failed ({})", finished.status)));
        }
        let printed = finished.stdout.trim();
        printed
            .strip_prefix("fawkes ")
            .filter(|version| !version.is_empty() && !version.contains(char::is_whitespace))
            .map(String::from)
            .ok_or_else(|| {
                refused(format!(
                    "{what} printed {printed:?}, not `fawkes <version>`"
                ))
            })
    }

    /// Replace the program, once `reexec_command` has answered: the requests in flight get their answers first
    /// (streams are cut, and their clients reconnect with `since`), stderr its queued lines, and `inner` and the log
    /// are held across the exec, so no write to state.json or events.jsonl is cut short. Returns only when the exec
    /// failed, which is logged; the daemon then carries on as before.
    pub(super) fn reexec_now(&self, state_lock: &File, listener: &UnixListener) {
        self.draining.store(true, Ordering::SeqCst);
        // The `reexec` request itself waits for this one.
        self.wait_until(ANSWER_WAIT, || self.open.load(Ordering::SeqCst) <= 1);
        self.draining.store(false, Ordering::SeqCst);
        self.flush_echo();
        let failed = {
            let mut inner = self.inner();
            let _log = lock(&self.log);
            // Checked again: since the answer the app may have crashed and been restarted, with a health check.
            let checked = self.check_reexec(&mut inner).map_err(|e| e.message);
            match checked.map(|()| handover(&inner, state_lock, listener)) {
                Ok(handover) => exec_with_handover(
                    &self.bin,
                    &self.up_args(),
                    &[state_lock.as_fd(), listener.as_fd()],
                    &handover,
                ),
                Err(why) => why,
            }
        };
        self.event(
            EventKind::Error,
            None,
            format!(
                "reexec of {} failed: {failed}; the daemon carries on as fawkes {VERSION}",
                self.bin.display()
            ),
        );
    }

    /// The arguments `up` ran with: the manifest, and the socket when one was given.
    fn up_args(&self) -> Vec<OsString> {
        let mut args = vec!["--manifest".into(), self.manifest_path.clone().into()];
        if self.socket_given {
            args.extend(["--socket".into(), self.socket.clone().into()]);
        }
        args.push("up".into());
        args
    }

    /// At startup, in place of starting the app: carry on with what the program before handed over. The app goes on
    /// as this process's child, with no health check and no `started` event, since it did not start. False when the
    /// app handed over is no child of this process, so startup goes on as after a crash of the daemon.
    pub(super) fn take_over(self: &Arc<Self>, handover: &Handover) -> bool {
        let mut inner = self.inner();
        inner.restarts = handover.restarts;
        inner.last_started.clone_from(&handover.last_started);
        inner.last_exit.clone_from(&handover.last_exit);
        let (kept, taken_over) = match &handover.app {
            None if handover.failed => {
                inner.live = Live::Failed;
                ("the app stays given up on".to_string(), true)
            }
            None => ("no app runs".to_string(), true),
            Some(handed) => {
                if let Some(child) = ChildPid::handed_over(handed.pid) {
                    // Its exit wakes the supervisor to reap it, as `start_live` arranges for an app it starts.
                    let daemon = self.clone();
                    process::on_exit(&child, move || daemon.wake.notify());
                    let ran = Duration::try_from_secs_f64(handed.uptime).unwrap_or_default();
                    inner.live = Live::Running(App {
                        process: child,
                        generation: handed.generation.clone(),
                        started: Instant::now().checked_sub(ran).unwrap_or_else(Instant::now),
                        started_at: handed.started_at.clone(),
                        info: handed.info.clone(),
                    });
                    let kept = format!("{} keeps running, pid {}", handed.generation, handed.pid);
                    (kept, true)
                } else {
                    let lost = format!(
                        "the app handed over ({}, pid {}) is no child of this daemon, so it starts anew",
                        handed.generation, handed.pid
                    );
                    (lost, false)
                }
            }
        };
        drop(inner);
        self.event_with(
            EventKind::Daemon,
            None,
            format!("re-executed from fawkes {}; {kept}", handover.from),
            Facts {
                action: Some(DaemonAction::Reexecuted),
                pid: Some(std::process::id()),
                version: Some(VERSION.into()),
                ..Facts::default()
            },
        );
        taken_over
    }
}

/// What the next program needs to carry on, read under `inner` after `check_reexec`. The app is reaped only under
/// `inner`, so held across the exec its pid stays its own: an exit from here on waits unreaped for the next program.
fn handover(inner: &Inner, state_lock: &File, listener: &UnixListener) -> Handover {
    let app = inner.running().map(|app| HandedApp {
        pid: app.process.id(),
        generation: app.generation.clone(),
        started_at: app.started_at.clone(),
        uptime: app.started.elapsed().as_secs_f64(),
        info: app.info.clone(),
    });
    Handover {
        from: VERSION.into(),
        state_lock: state_lock.as_raw_fd(),
        listener: listener.as_raw_fd(),
        app,
        failed: matches!(inner.live, Live::Failed),
        restarts: inner.restarts,
        last_started: inner.last_started.clone(),
        last_exit: inner.last_exit.clone(),
    }
}
