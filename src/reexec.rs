//! Moving a running process onto the fawkes binary installed at its path, in place: what an app's daemon
//! (`daemon/reexec.rs`) and the supervisor (`fleet.rs`) share. The program before checks that the binary starts, keeps
//! the descriptors the program after needs open across the exec, and passes the rest as the JSON of a hidden
//! `--handover`; the program after takes those descriptors over and checks they are what they claim to be.

use crate::process;
use rustix::fs::{FlockOperation, flock};
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use serde::Serialize;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long the installed binary has to print its version before a `reexec` gives up on it.
const VERSION_WAIT: Duration = Duration::from_secs(5);

/// How long a `--version` that overran gets after SIGTERM, before SIGKILL.
const VERSION_GRACE: Duration = Duration::from_secs(1);

/// The version of the fawkes binary at `bin`, which must start and say it is fawkes; else why not, for the caller to
/// put in its refusal.
pub fn installed_version(bin: &Path) -> Result<String, String> {
    let what = format!("`{} --version`", bin.display());
    let mut command = Command::new(bin);
    command
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let finished = process::run_bounded(&mut command, VERSION_WAIT, VERSION_GRACE, &what, |_| {})?;
    if !finished.status.success() {
        return Err(format!("{what} failed ({})", finished.status));
    }
    let printed = finished.stdout.trim();
    printed
        .strip_prefix("fawkes ")
        .filter(|version| !version.is_empty() && !version.contains(char::is_whitespace))
        .map(String::from)
        .ok_or_else(|| format!("{what} printed {printed:?}, not `fawkes <version>`"))
}

/// Own descriptor `fd`, which the program before kept open across the exec, and make it close-on-exec again, so no
/// child inherits it.
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

/// Take over the lock file handed over as `fd`: it must be the file at `path` and hold its `flock`, or this program
/// has no right to what the lock guards. `what` names the lock in the errors, and `held_elsewhere` completes the one
/// for a lock another process holds.
pub fn take_lock(fd: RawFd, path: &Path, what: &str, held_elsewhere: &str) -> Result<File, String> {
    let file = File::from(take_fd(fd)?);
    let same = match (file.metadata(), std::fs::metadata(path)) {
        (Ok(held), Ok(named)) => (held.dev(), held.ino()) == (named.dev(), named.ino()),
        _ => false,
    };
    if !same {
        return Err(format!(
            "descriptor {fd} handed over is not {}",
            path.display()
        ));
    }
    // The same open file locks again at once when it holds the lock, and fails when another one does.
    flock(&file, FlockOperation::NonBlockingLockExclusive)
        .map_err(|e| format!("the {what} handed over is not held ({e}); {held_elsewhere}"))?;
    Ok(file)
}

/// Take over the listening socket handed over as `fd`, which must be the one bound at `socket`. Its file stays as it
/// is.
pub fn take_listener(fd: RawFd, socket: &Path) -> Result<UnixListener, String> {
    let listener = UnixListener::from(take_fd(fd)?);
    let bound = listener.local_addr().ok();
    if bound.as_ref().and_then(|addr| addr.as_pathname()) != Some(socket) {
        return Err(format!(
            "descriptor {fd} handed over is not the socket {}",
            socket.display()
        ));
    }
    Ok(listener)
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
///
/// While `keep` is inheritable, a child spawned by another thread would inherit it too: the caller holds whatever
/// lock its spawns take.
pub fn exec_with_handover(
    bin: &Path,
    args: &[OsString],
    keep: &[BorrowedFd<'_>],
    handover: &impl Serialize,
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
        Err(e) => format!("cannot keep the lock and the socket open across the exec: {e}"),
    };
    for fd in keep {
        // Fails only for a descriptor that is not open, which inherits nothing.
        let _ = inherited(*fd, false);
    }
    failed
}
