//! The socket server: binding and publishing the socket, and answering each client's request.

use super::{Daemon, MAX_REQUEST, Msg, Slot, lock};
use crate::protocol::{ErrorCode, EventKind, Events, Line, Out, R, Reply, Request, err};
use rustix::fs::Mode;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

/// Link the socket as `<fawkes_home>/run/<name>.sock`, where `fawkes tui` finds every daemon.
pub(super) fn register(name: &str, socket: &Path) -> Result<PathBuf, String> {
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

/// Bind the socket. The caller holds the state lock, so a socket file nobody answers on belongs to a dead daemon.
pub(super) fn bind(socket: &Path) -> Result<UnixListener, String> {
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

impl Daemon {
    pub(super) fn handle(self: Arc<Self>, stream: UnixStream) {
        // Blocking, with timeouts: on macOS an accepted socket inherits the listener's non-blocking mode, which would
        // fail a request that arrives in pieces and every write that finds the socket buffer full.
        if let Err(e) = stream.set_nonblocking(false) {
            self.event(
                EventKind::Error,
                None,
                format!("cannot serve a client: {e}"),
            );
            return;
        }
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
                ErrorCode::Usage,
                format!("the request is longer than {MAX_REQUEST} bytes"),
            )),
            Ok(n) if n > 0 => {
                Request::parse(&line).and_then(|request| self.dispatch(&mut out, request))
            }
            _ => Err(err(ErrorCode::Usage, "no request received")),
        };
        out.finish(result);
    }

    fn dispatch(self: &Arc<Self>, out: &mut Out, request: Request) -> R<Reply> {
        match request {
            Request::Start => self.start_command().map(Reply::from),
            Request::Stop => self.stop_command(out).map(Reply::from),
            Request::Restart => self.restart_command(out).map(Reply::from),
            Request::Stage { git_ref } => self.stage(out, &git_ref).map(Reply::from),
            Request::Test { generation } => self.test(out, &generation).map(Reply::from),
            Request::Switch {
                generation,
                deadline,
            } => self.switch(out, &generation, deadline).map(Reply::from),
            Request::Confirm { generation } => {
                let record = self.confirm(generation.as_deref())?;
                self.prune_soon();
                Ok(record.into())
            }
            Request::Prune => self.prune(out).map(Reply::from),
            Request::Rollback => {
                let _busy = self.begin("rollback", Slot::Both)?;
                self.rollback_pending(out, "rollback requested")
                    .map(Reply::from)
            }
            Request::Status => Ok(self.status().into()),
            Request::History => Ok(self.history().into()),
            Request::Log { limit, follow } => {
                let limit = limit.map_or(20, |n| usize::try_from(n).unwrap_or(usize::MAX));
                if follow {
                    self.stream(out, None, Some(limit), false);
                    return Ok(Reply::Streamed);
                }
                Ok(Events {
                    events: lock(&self.log).tail(limit),
                }
                .into())
            }
            Request::Subscribe { limit, since } => {
                let limit = limit.map(|n| usize::try_from(n).unwrap_or(usize::MAX));
                self.stream(out, since, limit, true);
                Ok(Reply::Streamed)
            }
        }
    }

    /// Send events (and, for `subscribe`, status and history) to a client until it closes.
    fn stream(&self, out: &mut Out, since: Option<u64>, limit: Option<usize>, snapshots: bool) {
        // Join and read the replay under one hold of the log, the lock `event` broadcasts under: an event logged
        // before it is in the replay, one logged after it is on the channel.
        let (rx, replay) = {
            let log = lock(&self.log);
            (self.hub.join(), log.replay(since, limit))
        };
        let mut ok = replay.iter().all(|e| out.event(e));
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
        out.line(&Line::<Reply>::Status {
            status: Box::new(self.status()),
        }) && out.line(&Line::<Reply>::History {
            history: self.history(),
        })
    }
}
