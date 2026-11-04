//! The TUI's connection layer: one reader thread per daemon holding a `subscribe` stream, the scanner that finds
//! daemons, and the one-shot request an action sends.

use super::Msg;
use crate::protocol::{Line, REQUEST_TIMEOUT, Request, timed_out};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RETRY: Duration = Duration::from_secs(1);
const RESCAN: Duration = Duration::from_secs(3);
const BACKLOG: u64 = 100;

/// The keys of the apps that have a reader thread.
pub(super) type Active = Arc<Mutex<HashSet<String>>>;

/// Follow one daemon. A single target is retried for as long as the screen lives. A discovered one is retried, shown
/// as disconnected, while its link still leads to a socket file (a crashed daemon); it leaves the list once the link
/// is gone (a daemon that stopped), and the scanner starts a fresh reader if it comes back. A link that never
/// answered is not listed at all.
pub(super) fn reader(key: &str, socket: &Path, active: Option<&Active>, tx: &Sender<Msg>) {
    let mut seen = false;
    // The `seq` of the last event passed on: a reconnect asks for what came after it instead of starting over.
    let mut last_seq = None;
    loop {
        let (up, why) = follow(key, socket, &mut last_seq, tx);
        seen |= up;
        let msg = match active {
            None => Some(Msg::Down(key.into(), why)),
            Some(_) if !seen => None,
            Some(_) if socket.exists() => Some(Msg::Down(key.into(), why)),
            Some(_) => Some(Msg::Gone(key.into())),
        };
        let retry = matches!(msg, Some(Msg::Down(..)));
        if let Some(msg) = msg
            && tx.send(msg).is_err()
        {
            return;
        }
        if !retry {
            if let Some(active) = active {
                active
                    .lock()
                    .unwrap_or_else(|_| std::process::abort())
                    .remove(key);
            }
            return;
        }
        std::thread::sleep(RETRY);
    }
}

/// Fetch the recent events (those after `last_seq`, once there is one), then follow `subscribe`. The error says
/// whether the app had answered.
fn follow(
    key: &str,
    socket: &Path,
    last_seq: &mut Option<u64>,
    tx: &Sender<Msg>,
) -> (bool, String) {
    let request = Request::Subscribe {
        limit: Some(BACKLOG),
        since: *last_seq,
    };
    let stream = match open(socket, &request) {
        Ok(stream) => stream,
        Err(e) => return (false, e),
    };
    let mut up = false;
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let line = serde_json::from_str::<Line>(&line).ok();
        let seq = match &line {
            Some(Line::Event { event }) => Some(event.seq),
            _ => None,
        };
        if !up {
            up = true;
            // The screen keeps its events only when the replay continues them without a gap: more events than the
            // replay holds, or a daemon whose log started over, start the pane afresh.
            let resumed = last_seq.is_some_and(|last| seq.is_none_or(|seq| seq == last + 1));
            if tx
                .send(Msg::Up(key.into(), socket.into(), resumed))
                .is_err()
            {
                return (true, "the screen is gone".into());
            }
        }
        if let Some(line) = line {
            if tx.send(Msg::Line(key.into(), line)).is_err() {
                return (true, "the screen is gone".into());
            }
            // Not a maximum: a daemon whose log started over numbers from 1 again.
            *last_seq = seq.or(*last_seq);
        }
    }
    if up {
        (true, "connection lost".into())
    } else {
        (
            false,
            "the daemon closed the connection without an answer".into(),
        )
    }
}

/// Every few seconds, start a reader for each `<name>.sock` link in `dir` that has none.
pub(super) fn scan(dir: &Path, active: &Active, tx: &Sender<Msg>) {
    loop {
        let links = std::fs::read_dir(dir).into_iter().flatten().flatten();
        for link in links {
            let path = link.path();
            let Some(key) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".sock"))
                .map(String::from)
            else {
                continue;
            };
            if active
                .lock()
                .unwrap_or_else(|_| std::process::abort())
                .insert(key.clone())
            {
                let (active, tx) = (active.clone(), tx.clone());
                std::thread::spawn(move || reader(&key, &path, Some(&active), &tx));
            }
        }
        std::thread::sleep(RESCAN);
    }
}

fn open(socket: &Path, request: &Request) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    request.write_to(&mut stream).map_err(|e| e.to_string())?;
    Ok(stream)
}

/// One request: progress lines are skipped (they reach the screen through `subscribe`), the answer is the outcome.
pub(super) fn request(socket: &Path, request: &Request) -> Result<(), String> {
    let stream = open(socket, request)?;
    // A daemon that accepts and then hangs must not leave the action thread and its notice waiting for ever.
    if request.is_quick() {
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|e| e.to_string())?;
    }
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| {
            if timed_out(&e) {
                "the daemon did not answer in time".to_string()
            } else {
                e.to_string()
            }
        })?;
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Done { .. }) => return Ok(()),
            Ok(Line::Failed { error, .. }) => return Err(error.message),
            _ => {}
        }
    }
    Err("the daemon closed the connection without an answer".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    /// A daemon that answers its connections one after another, each with the given lines, then closes it.
    struct FakeDaemon {
        socket: PathBuf,
        served: Option<JoinHandle<Vec<String>>>,
    }

    impl FakeDaemon {
        /// The socket path stays short: a path over about 100 bytes does not bind, and the temp dir is long.
        fn start(name: &str, connections: Vec<Vec<Value>>) -> FakeDaemon {
            let socket = PathBuf::from(format!("/tmp/fkt{}-{name}.sock", std::process::id()));
            std::fs::remove_file(&socket).ok();
            let listener = UnixListener::bind(&socket).unwrap();
            let served = std::thread::spawn(move || {
                let mut requests = Vec::new();
                for lines in connections {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut request = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut request)
                        .unwrap();
                    requests.push(request);
                    for line in lines {
                        writeln!(stream, "{line}").unwrap();
                    }
                }
                requests
            });
            FakeDaemon {
                socket,
                served: Some(served),
            }
        }

        /// The request lines received, after every connection has been served.
        fn requests(&mut self) -> Vec<Request> {
            let lines = self.served.take().unwrap().join().unwrap();
            lines.iter().map(|l| Request::parse(l).unwrap()).collect()
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            std::fs::remove_file(&self.socket).ok();
        }
    }

    fn event(seq: u64) -> Value {
        json!({ "event": { "seq": seq, "at": "2026-11-03T14:02:01Z", "kind": "progress",
                           "generation": null, "message": format!("event {seq}") } })
    }

    fn status() -> Value {
        json!({ "status": {
            "name": "sample", "daemon_pid": 812, "permanent": null, "current": null, "busy": null,
            "pending": null,
            "process": { "state": "stopped", "generation": null, "pid": null, "port": 3000,
                         "started_at": null, "uptime": null, "restarts": 0, "last_exit": null } } })
    }

    fn history() -> Value {
        json!({ "history": { "generations": [] } })
    }

    fn describe(msg: &Msg) -> String {
        match msg {
            Msg::Up(_, _, resumed) => format!("up resumed={resumed}"),
            Msg::Line(_, Line::Event { event }) => format!("event {}", event.seq),
            Msg::Line(_, Line::Status { .. }) => "status".into(),
            Msg::Line(_, Line::History { .. }) => "history".into(),
            Msg::Line(..) => "other line".into(),
            Msg::Down(_, why) => format!("down {why}"),
            Msg::Gone(_) => "gone".into(),
            Msg::Key(_) | Msg::Done(..) => "unexpected".into(),
        }
    }

    #[test]
    fn the_reader_reports_up_then_the_replay_and_snapshots_in_order_then_down() {
        let mut daemon = FakeDaemon::start(
            "reader",
            vec![vec![event(1), event(2), status(), history()]],
        );
        let (tx, rx) = mpsc::channel();
        let socket = daemon.socket.clone();
        std::thread::spawn(move || reader("sample", &socket, None, &tx));
        let seen: Vec<String> = rx.iter().take(6).map(|m| describe(&m)).collect();
        // Dropping the receiver ends the reader at its next send, after the retry pause.
        drop(rx);
        assert_eq!(
            seen,
            [
                "up resumed=false",
                "event 1",
                "event 2",
                "status",
                "history",
                "down connection lost"
            ]
        );
        assert_eq!(
            daemon.requests(),
            [Request::Subscribe {
                limit: Some(BACKLOG),
                since: None
            }]
        );
    }

    #[test]
    fn a_reconnect_resumes_only_when_the_replay_continues_the_events_already_shown() {
        let mut daemon = FakeDaemon::start(
            "resume",
            vec![
                // The first connection: events up to 3.
                vec![event(1), event(2), event(3), status(), history()],
                // Continues at 4: resumed.
                vec![event(4), event(5), status(), history()],
                // Nothing happened meanwhile: the replay is empty and the snapshot comes first: resumed.
                vec![status(), history()],
                // The daemon logged more than the replay holds: the first event is 9, not 6: a gap.
                vec![event(9), status(), history()],
                // A log that started over numbers from 1 again: not a continuation.
                vec![event(1), event(2), status(), history()],
                // And the next reconnect asks after the new numbering.
                vec![event(3), status(), history()],
            ],
        );
        let mut last_seq = None;
        let mut connect = || {
            let (tx, rx) = mpsc::channel();
            let (up, why) = follow("sample", &daemon.socket, &mut last_seq, &tx);
            drop(tx);
            assert!(up, "{why}");
            rx.iter()
                .find_map(|m| match m {
                    Msg::Up(_, _, resumed) => Some(resumed),
                    _ => None,
                })
                .unwrap()
        };
        let resumed: Vec<bool> = (0..6).map(|_| connect()).collect();
        assert_eq!(resumed, [false, true, true, false, false, true]);
        let since: Vec<Option<u64>> = daemon
            .requests()
            .into_iter()
            .map(|r| match r {
                Request::Subscribe { since, .. } => since,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(since, [None, Some(3), Some(5), Some(5), Some(9), Some(2)]);
    }

    #[test]
    fn a_request_is_done_failed_with_the_daemons_message_or_unanswered() {
        let mut daemon = FakeDaemon::start(
            "request",
            vec![
                // Progress lines are skipped on the way to the outcome.
                vec![event(1), json!({ "ok": true, "result": {} })],
                vec![json!({ "ok": false,
                             "error": { "code": "busy", "message": "switch is in progress" } })],
                vec![event(2)],
            ],
        );
        assert_eq!(request(&daemon.socket, &Request::Prune), Ok(()));
        assert_eq!(
            request(&daemon.socket, &Request::Prune),
            Err("switch is in progress".into())
        );
        assert_eq!(
            request(&daemon.socket, &Request::Prune),
            Err("the daemon closed the connection without an answer".into())
        );
        assert_eq!(daemon.requests(), vec![Request::Prune; 3]);
    }
}
