//! The socket server: binding and publishing the socket, and answering each client's request.

use super::{Daemon, MAX_REQUEST, Msg, Slot, lock};
use crate::protocol::{
    DEFAULT_LOG_EVENTS, ErrorCode, EventKind, Events, Line, Out, R, Reply, Request, RollbackCause,
    err, timed_out,
};
use rustix::fs::Mode;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// Link the socket as `<fawkes_home>/run/<name>.sock`, where `fawkes watch` finds every daemon.
pub(super) fn register(name: &str, socket: &Path) -> Result<PathBuf, String> {
    let dir = crate::manifest::run_dir()?;
    // 0700, and so is a `fawkes_home` created with it: the names in it say which apps run here.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
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

/// The longest socket path `bind` accepts. `sockaddr_un.sun_path` holds 104 bytes on macOS and 108 on Linux; 100
/// leaves room for the terminating nul on both.
const SOCKET_PATH_LIMIT: usize = 100;

/// Bind the socket. The caller holds the state lock, so a socket file nobody answers on belongs to a dead daemon.
pub(super) fn bind(socket: &Path) -> Result<UnixListener, String> {
    if socket.as_os_str().len() >= SOCKET_PATH_LIMIT {
        return Err(format!(
            "socket path {} is too long (limit about {SOCKET_PATH_LIMIT} bytes); use a shorter state_dir",
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

/// How long a client has to send its whole request line. A local client sends it at once, so this only ends one
/// that connected and stalled, before it holds a connection slot for long.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// How often a streaming client's thread looks whether its client has gone when no event arrives: the longest a
/// closed `watch` or `log --follow` holds its thread and connection slot.
const CLOSE_CHECK: Duration = Duration::from_millis(500);

/// How many connections may be open at once, streams (`subscribe`, `log --follow`) excepted: each holds a thread and
/// a file descriptor, and a client that opens them without end must not starve `confirm` of both.
const MAX_CONNECTIONS: usize = 64;

/// A place among the open connections (`Daemon::open`), given back when the connection has its answer or turns out to
/// be a stream. `shutdown` waits for every place to be given back.
pub(super) struct Admission(Option<Arc<Daemon>>);

impl Admission {
    /// A place, or None when `MAX_CONNECTIONS` are open.
    pub(super) fn take(daemon: &Arc<Daemon>) -> Option<Admission> {
        if daemon.open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            daemon.open.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Admission(Some(daemon.clone())))
    }

    fn release(&mut self) {
        if let Some(daemon) = self.0.take() {
            daemon.open.fetch_sub(1, Ordering::SeqCst);
            // Read after the count changed, as `shutdown` sets the flag before it reads the count: whichever comes
            // second sees the other. Only then, so an ordinary request does not wake the supervisor.
            if daemon.shutting_down() {
                daemon.wake.notify();
            }
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
                REQUEST_DEADLINE.as_secs()
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
            Err(e) if timed_out(&e) || e.kind() == ErrorKind::Interrupted => continue,
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
        let read = read_request(&stream, Instant::now() + REQUEST_DEADLINE, MAX_REQUEST);
        let mut out = Out::new(stream);
        let result = read
            .and_then(|line| Request::parse(&line))
            .and_then(|request| {
                // Streams last as long as their client listens, by design, so they do not count against the cap.
                if request.is_stream() {
                    admitted.release();
                }
                self.dispatch(&mut out, request)
            });
        out.finish(result);
        // Given back only now: `shutdown` waits for it, so the answer is sent before the daemon exits.
        drop(admitted);
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
                let (record, confirmed) = self.confirm(generation.as_deref())?;
                if confirmed {
                    self.prune_soon();
                }
                Ok(record.into())
            }
            Request::Prune => self.prune(out).map(Reply::from),
            Request::Rollback { keep_storage } => {
                let _busy = self.begin("rollback", Slot::Both)?;
                if keep_storage {
                    self.abandon_restore().map(Reply::from)
                } else {
                    self.rollback_pending(out, "rollback requested", Some(RollbackCause::Requested))
                        .map(Reply::from)
                }
            }
            Request::Status => Ok(self.status().into()),
            Request::History => Ok(self.history().into()),
            Request::Report { generation, info } => self.report(&generation, info).map(Reply::from),
            Request::Log {
                limit,
                since,
                follow,
            } => {
                // Without `since`, the last `DEFAULT_LOG_EVENTS`; with it, all that come after it.
                let limit = limit.or_else(|| since.is_none().then_some(DEFAULT_LOG_EVENTS));
                if follow {
                    self.stream(out, since, limit, false);
                    return Ok(Reply::Streamed);
                }
                Ok(Events {
                    events: lock(&self.log).replay(since, limit),
                }
                .into())
            }
            Request::Reload => self.reload_manifest().map(Reply::from),
            Request::Subscribe { limit, since } => {
                self.stream(out, since, limit, true);
                Ok(Reply::Streamed)
            }
            // Answered by `run`, on the connection `down` took: what is finished here goes nowhere.
            Request::Down => {
                self.down(out);
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
            let first = match rx.recv_timeout(CLOSE_CHECK) {
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
    use super::super::tests::{KillOnDrop, scratch_daemon};
    use super::super::{App, Live};
    use super::*;
    use crate::protocol::Line;
    use std::io::{BufRead, BufReader, Write};

    /// What `handle` answers to a client that sends `bytes` and then listens.
    fn answer(daemon: &Arc<Daemon>, bytes: Vec<u8>) -> Line {
        let (server, client) = UnixStream::pair().unwrap();
        // The client writes while the daemon reads: more than a socket buffer holds would block a single thread.
        let mut writer = client.try_clone().unwrap();
        let sender = std::thread::spawn(move || {
            // The daemon may stop reading and answer before the last byte is sent.
            let _ = writer.write_all(&bytes);
        });
        daemon
            .clone()
            .handle(server, Admission::take(daemon).unwrap());
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        sender.join().unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn a_request_over_the_limit_is_answered_usage_and_a_request_within_it_is_served() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = Arc::new(scratch_daemon(temp.path()));

        let oversized = vec![b'x'; usize::try_from(MAX_REQUEST).unwrap() + 6 * 1024];
        let Line::Failed { error, .. } = answer(&daemon, oversized) else {
            panic!("an oversized request should fail");
        };
        assert_eq!(error.code, ErrorCode::Usage);
        assert!(error.message.contains("longer than"), "{}", error.message);

        let status = answer(&daemon, b"{\"command\":\"status\"}\n".to_vec());
        assert!(matches!(status, Line::Done { .. }), "{status:?}");
    }

    fn report_line(generation: &str, info: &str) -> Vec<u8> {
        format!(r#"{{"command":"report","args":{{"generation":"{generation}","info":{info}}}}}"#)
            .into_bytes()
            .into_iter()
            .chain(*b"\n")
            .collect()
    }

    fn reported_events(daemon: &Daemon) -> Vec<String> {
        lock(&daemon.log)
            .replay(Some(0), None)
            .into_iter()
            .filter(|event| event.kind == EventKind::Reported)
            .map(|event| event.message)
            .collect()
    }

    #[test]
    fn only_the_running_live_app_reports_and_what_it_reports_is_checked_and_logged_once() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = Arc::new(scratch_daemon(temp.path()));
        let refused = |line: Line| match line {
            Line::Failed { error, .. } => error,
            other => panic!("expected a failure, got {other:?}"),
        };
        let info = r#"{"url":"http://127.0.0.1:4790","log":"/p/supervisor.log"}"#;

        let nothing = refused(answer(&daemon, report_line("g2", info)));
        assert_eq!(nothing.code, ErrorCode::InvalidState);
        assert!(
            nothing.message.contains("nothing runs"),
            "{}",
            nothing.message
        );

        let sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _reaper = KillOnDrop(sleeper.id());
        daemon.inner().live = Live::Running(App::new(sleeper, "g2"));

        // A test run's generation, or one that was switched away from, is not the one that runs.
        let other = refused(answer(&daemon, report_line("g3", info)));
        assert_eq!(other.code, ErrorCode::InvalidState);
        assert!(
            other.message.contains("g2 runs, not g3"),
            "{}",
            other.message
        );
        assert!(daemon.status().process.info.is_empty());

        // Refused before anything changes.
        let nine = (0..9)
            .map(|i| format!(r#""k{i}":"v""#))
            .collect::<Vec<_>>()
            .join(",");
        for bad in [
            format!("{{{nine}}}"),
            r#"{"url":"http://x\u001b[2J"}"#.into(),
            r#"{"URL":"http://x"}"#.into(),
            r#"{"url":""}"#.into(),
        ] {
            let error = refused(answer(&daemon, report_line("g2", &bad)));
            assert_eq!(error.code, ErrorCode::Usage, "{bad}");
        }
        assert!(daemon.status().process.info.is_empty());
        assert_eq!(reported_events(&daemon), Vec::<String>::new());

        let Line::Done { result, .. } = answer(&daemon, report_line("g2", info)) else {
            panic!("the running app's report is accepted");
        };
        assert_eq!(
            result,
            serde_json::json!({"info": {"url": "http://127.0.0.1:4790", "log": "/p/supervisor.log"}})
        );
        assert_eq!(daemon.status().process.info.len(), 2);
        // A retry changes nothing and says nothing.
        answer(&daemon, report_line("g2", info));
        assert_eq!(
            reported_events(&daemon),
            ["url http://127.0.0.1:4790 · log /p/supervisor.log"]
        );
        // Each report replaces the whole info.
        answer(
            &daemon,
            report_line("g2", r#"{"url":"http://127.0.0.1:1"}"#),
        );
        assert_eq!(daemon.status().process.info.len(), 1);
        answer(&daemon, report_line("g2", "{}"));
        assert_eq!(reported_events(&daemon).len(), 3);
        assert_eq!(reported_events(&daemon)[2], "info cleared");
        let facts = lock(&daemon.log).replay(None, Some(1)).remove(0).facts;
        assert_eq!(facts.info, Some(std::collections::BTreeMap::new()));

        // The info belongs to the process: the next app starts with none.
        answer(&daemon, report_line("g2", info));
        let app = daemon.inner().take_running().unwrap();
        daemon.inner().live = Live::Running(App::new(app.child, "g2"));
        assert!(daemon.status().process.info.is_empty());
    }

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

    /// The process exits soon after `shutdown` returns: a request still being answered, such as a switch that the
    /// shutdown cut short, must have sent its answer by then.
    #[test]
    fn shutdown_waits_for_the_answer_an_open_connection_owes() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = Arc::new(scratch_daemon(temp.path()));
        let owed = Admission::take(&daemon).unwrap();
        daemon.shutdown.store(true, Ordering::SeqCst);
        let (done, finished) = std::sync::mpsc::channel();
        let shutting = {
            let daemon = daemon.clone();
            std::thread::spawn(move || {
                daemon.shutdown();
                done.send(()).unwrap();
            })
        };
        // A wait that ends too early is all this can catch, so it never fails a slow machine.
        assert!(
            finished.recv_timeout(Duration::from_millis(500)).is_err(),
            "shutdown returned with an answer still owed"
        );
        // Well inside the rest of `ANSWER_WAIT`: the answer sent wakes `shutdown`, not its time running out.
        drop(owed);
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        shutting.join().unwrap();
    }

    #[test]
    fn the_65th_open_connection_gets_no_place_until_one_ends() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = Arc::new(scratch_daemon(temp.path()));
        let open = || daemon.open.load(Ordering::SeqCst);
        let mut places: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| Admission::take(&daemon).unwrap())
            .collect();
        assert!(Admission::take(&daemon).is_none());
        assert_eq!(open(), MAX_CONNECTIONS);

        // A stream gives its place back at once, and a second release (the drop) gives nothing more.
        places[0].release();
        assert_eq!(open(), MAX_CONNECTIONS - 1);
        places.remove(0);
        assert_eq!(open(), MAX_CONNECTIONS - 1);
        let again = Admission::take(&daemon).unwrap();
        assert!(Admission::take(&daemon).is_none());
        drop(again);
        drop(places);
        assert_eq!(open(), 0);
    }
}
