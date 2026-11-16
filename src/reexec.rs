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

/// The version of the fawkes binary at `bin`, which must start, say it is fawkes, and know `reexec`; else why not, for
/// the caller to put in its refusal. A fawkes from before `reexec` has no `--handover` either: executed with one, it
/// would end at once and leave the app without its daemon.
pub fn installed_version(bin: &Path) -> Result<String, String> {
    let what = format!("`{} --version`", bin.display());
    let printed = run_installed(bin, &["--version"], &what)?;
    let version = printed
        .trim()
        .strip_prefix("fawkes ")
        .filter(|version| !version.is_empty() && !version.contains(char::is_whitespace))
        .map(String::from)
        .ok_or_else(|| {
            format!(
                "{what} printed {:?}, not `fawkes <version>`",
                printed.trim()
            )
        })?;
    let what = format!("`{} reexec --help`", bin.display());
    run_installed(bin, &["reexec", "--help"], &what)
        .map_err(|why| format!("{why}: fawkes {version} cannot take a running daemon over"))?;
    Ok(version)
}

/// What `bin <args>` printed, when it succeeded within `VERSION_WAIT`.
fn run_installed(bin: &Path, args: &[&str], what: &str) -> Result<String, String> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let finished = process::run_bounded(&mut command, VERSION_WAIT, VERSION_GRACE, what, |_| {})?;
    if !finished.status.success() {
        return Err(format!("{what} failed ({})", finished.status));
    }
    Ok(finished.stdout)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for an installed fawkes: a shell script running `body`.
    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The exec is only worth it to a fawkes that can take the daemon over: one from before `reexec` would end at
    /// once on `--handover`, and leave the app without its daemon.
    #[test]
    fn the_installed_binary_must_say_it_is_fawkes_and_know_reexec() {
        let dir = tempfile::tempdir().unwrap();
        let current = script(
            dir.path(),
            "current",
            r#"[ "$1" = --version ] && echo "fawkes 0.2.0"; exit 0"#,
        );
        assert_eq!(installed_version(&current).as_deref(), Ok("0.2.0"));

        let older = script(
            dir.path(),
            "older",
            r#"[ "$1" = --version ] && echo "fawkes 0.1.3" && exit 0; exit 2"#,
        );
        let why = installed_version(&older).unwrap_err();
        assert!(why.contains("reexec --help` failed"), "{why}");
        assert!(
            why.contains("fawkes 0.1.3 cannot take a running daemon over"),
            "{why}"
        );

        let other = script(dir.path(), "other", "echo something 1.0");
        let why = installed_version(&other).unwrap_err();
        assert!(why.contains("not `fawkes <version>`"), "{why}");

        let broken = script(dir.path(), "broken", "exit 1");
        let why = installed_version(&broken).unwrap_err();
        assert!(why.contains("--version` failed"), "{why}");
    }

    /// The program after an exec owns only what it was handed: the lock file at the path, still locked, and the
    /// socket bound where it serves. A wrong number in `--handover` is refused, never used.
    #[test]
    fn only_the_lock_and_the_socket_handed_over_are_taken() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let lock_path = dir.path().join("lock");
        let other_path = dir.path().join("other");
        let socket = dir.path().join("s.sock");

        let held = File::create(&lock_path).unwrap();
        flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();
        let other = File::create(&other_path).unwrap();
        let why = take_lock(other.into_raw_fd(), &lock_path, "state lock", "").unwrap_err();
        assert!(why.contains("is not"), "{why}");

        // Another open file of the lock is refused while the one handed over holds it.
        let elsewhere = File::open(&lock_path).unwrap();
        let why = take_lock(elsewhere.into_raw_fd(), &lock_path, "state lock", "held").unwrap_err();
        assert!(
            why.contains("the state lock handed over is not held"),
            "{why}"
        );
        assert!(take_lock(held.into_raw_fd(), &lock_path, "state lock", "").is_ok());

        let listener = UnixListener::bind(&socket).unwrap();
        let file = File::open(&other_path).unwrap();
        let why = take_listener(file.into_raw_fd(), &socket).unwrap_err();
        assert!(why.contains("is not the socket"), "{why}");
        let another = UnixListener::bind(dir.path().join("another.sock")).unwrap();
        let why = take_listener(another.into_raw_fd(), &socket).unwrap_err();
        assert!(why.contains("is not the socket"), "{why}");
        assert!(take_listener(listener.into_raw_fd(), &socket).is_ok());

        assert!(take_fd(2).is_err());
    }
}
