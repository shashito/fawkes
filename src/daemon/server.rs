//! The socket server: binding and publishing the socket, and answering each client's request.

use super::{Daemon, MAX_REQUEST, Msg, Slot, lock};
use crate::protocol::{ErrorCode, EventKind, Events, Line, Out, R, Reply, Request, err};
use rustix::fs::Mode;
use std::io::{ErrorKind, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// Link the socket as `<fawkes_home>/run/<name>.sock`, where `fawkes tui` finds every daemon.
pub(super) fn register(name: &str, socket: &Path) -> Result<PathBuf, String> {
    let dir = crate::manifest::run_dir()?;
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
    if let Err(e) = std::fs::remove_file(socket)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!(
            "cannot remove the old socket {}: {e}",
            socket.display()
        ));
    }
    // Created 0600 from the start: no moment in which the umask's permissions apply. The umask is process-wide, so
    // this must run before the daemon starts its first thread.
    let umask = rustix::process::umask(Mode::from_raw_mode(0o177));
    let bound = UnixListener::bind(socket);
    rustix::process::umask(umask);
    bound.map_err(|e| format!("cannot bind {}: {e}", socket.display()))
}

/// How long a client has to send its whole request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How many connections may be open at once, streams (`subscribe`, `log --follow`) excepted: each holds a thread and
/// a file descriptor, and a client that opens them without end must not starve `confirm` of both.
const MAX_CONNECTIONS: usize = 64;

/// A place among the open connections, given back when the connection ends or turns out to be a stream.
pub(super) struct Admission(Option<Arc<AtomicUsize>>);

impl Admission {
    /// A place, or None when `MAX_CONNECTIONS` are open.
    pub(super) fn take(open: &Arc<AtomicUsize>) -> Option<Admission> {
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Admission(Some(open.clone())))
    }

    fn release(&mut self) {
        if let Some(open) = self.0.take() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.release();
    }
}

/// Answer `busy` to a client the daemon has no place for, without reading what it sends.
pub(super) fn refuse(stream: UnixStream) {
    // Blocking, as in `handle`; the reply is one short line into an empty socket buffer.
    if stream.set_nonblocking(false).is_ok() {
        Out::new(stream).finish(Err(err(
            ErrorCode::Busy,
            format!("too many connections ({MAX_CONNECTIONS} open); retry shortly"),
        )));
    }
}

/// One request line (without its newline) from `stream`: all of it must arrive by `until`, in at most `limit` bytes.
/// The read timeout is the time left before each `read`, because a timeout alone bounds only each `read`, which a
/// client sending a byte at a time never exceeds.
fn read_request(stream: &UnixStream, until: Instant, limit: u64) -> R<String> {
    let too_slow = || {
        err(
            ErrorCode::Usage,
            format!(
                "the request did not arrive within {} s",
                REQUEST_TIMEOUT.as_secs()
            ),
        )
    };
    let mut line = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut reader = stream;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(too_slow());
        }
        // Refused with EINVAL on macOS once the client has closed its end, when the read below returns what it sent
        // and then end-of-file at once, so there is nothing to bound.
        let _ = stream.set_read_timeout(Some(left));
        let n = match reader.read(&mut chunk) {
            Ok(n) => n,
            // A timeout, or a signal: the deadline above decides whether to go on.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => {
                return Err(err(
                    ErrorCode::Usage,
                    format!("cannot read the request: {e}"),
                ));
            }
        };
        if n == 0 {
            break;
        }
        let newline = chunk[..n].iter().position(|&byte| byte == b'\n');
        line.extend_from_slice(&chunk[..newline.map_or(n, |at| at + 1)]);
        if line.len() as u64 > limit || (newline.is_none() && line.len() as u64 >= limit) {
            return Err(err(
                ErrorCode::Usage,
                format!("the request is longer than {limit} bytes"),
            ));
        }
        if newline.is_some() {
            break;
        }
    }
    if line.is_empty() {
        return Err(err(ErrorCode::Usage, "no request received"));
    }
    String::from_utf8(line).map_err(|_| err(ErrorCode::Usage, "the request is not UTF-8"))
}

impl Daemon {
    pub(super) fn handle(self: Arc<Self>, stream: UnixStream, mut admitted: Admission) {
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
        let read = read_request(&stream, Instant::now() + REQUEST_TIMEOUT, MAX_REQUEST);
        let mut out = Out::new(stream);
        let result = read
            .and_then(|line| Request::parse(&line))
            .and_then(|request| {
                // Streams last as long as their client listens, by design, so they do not count against the cap.
                if matches!(
                    request,
                    Request::Subscribe { .. } | Request::Log { follow: true, .. }
                ) {
                    admitted.release();
                }
                self.dispatch(&mut out, request)
            });
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
            Request::Rollback { keep_storage } => {
                let _busy = self.begin("rollback", Slot::Both)?;
                if keep_storage {
                    self.abandon_restore().map(Reply::from)
                } else {
                    self.rollback_pending(out, "rollback requested")
                        .map(Reply::from)
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_request_trickled_past_the_deadline_is_refused_not_waited_for() {
        let (server, mut client) = UnixStream::pair().unwrap();
        // One byte every 100 ms keeps every single `read` inside any per-read timeout, and never sends the newline.
        let trickle = std::thread::spawn(move || {
            for _ in 0..30 {
                if client.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let started = Instant::now();
        let refused =
            read_request(&server, started + Duration::from_millis(300), MAX_REQUEST).unwrap_err();
        let waited = started.elapsed();

        drop(server);
        trickle.join().unwrap();
        assert_eq!(refused.code, ErrorCode::Usage);
        assert!(waited < Duration::from_secs(1), "waited {waited:?}");
    }

    #[test]
    fn a_request_line_is_read_whole_and_bounded() {
        let ask = |bytes: &[u8], limit| {
            let (server, mut client) = UnixStream::pair().unwrap();
            client.write_all(bytes).unwrap();
            drop(client);
            read_request(&server, Instant::now() + Duration::from_secs(5), limit)
        };
        assert_eq!(ask(b"{\"a\":1}\nrest", 64).unwrap(), "{\"a\":1}\n");
        assert_eq!(ask(b"", 64).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(ask(&[b'x'; 65], 64).unwrap_err().code, ErrorCode::Usage);
        let mut longest = vec![b'x'; 63];
        longest.push(b'\n');
        assert!(ask(&longest, 64).is_ok());
        longest.insert(0, b'x');
        assert_eq!(ask(&longest, 64).unwrap_err().code, ErrorCode::Usage);
    }

    #[test]
    fn the_65th_open_connection_gets_no_place_until_one_ends() {
        let open = Arc::new(AtomicUsize::new(0));
        let mut places: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| Admission::take(&open).unwrap())
            .collect();
        assert!(Admission::take(&open).is_none());
        assert_eq!(open.load(Ordering::SeqCst), MAX_CONNECTIONS);

        // A stream gives its place back at once, and a second release (the drop) gives nothing more.
        places[0].release();
        assert_eq!(open.load(Ordering::SeqCst), MAX_CONNECTIONS - 1);
        places.remove(0);
        assert_eq!(open.load(Ordering::SeqCst), MAX_CONNECTIONS - 1);
        let again = Admission::take(&open).unwrap();
        assert!(Admission::take(&open).is_none());
        drop(again);
        drop(places);
        assert_eq!(open.load(Ordering::SeqCst), 0);
    }
}
