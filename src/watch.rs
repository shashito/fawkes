//! `fawkes watch`: one `subscribe` stream per daemon, every daemon linked in `<fawkes_home>/run/` or the one a socket
//! names, and every event printed as one line with the app's name first. A daemon that drops is reconnected, asking
//! for the events after the last one printed, so none is lost or printed twice while the daemon still keeps it.

use crate::protocol::{Event, Line, Request};
use crate::state::{now_secs, parse_time};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

const RETRY: Duration = Duration::from_secs(1);
const RESCAN: Duration = Duration::from_secs(3);

/// The daemons `watch` follows.
pub enum Target {
    /// Every daemon linked as `<name>.sock` in this `run/` directory, those that start later included.
    Discover(PathBuf),
    /// The daemon on this socket.
    One(PathBuf),
}

/// Where the first connection to a daemon starts its replay: after an event number (`--since`), or at a time, so a
/// daemon found after `watch` started still shows what it logged since then (its start among it).
#[derive(Clone, Copy)]
enum Start {
    After(u64),
    At(u64),
}

/// What the readers hand to the thread that prints.
enum Msg {
    /// An event, as its line for stdout.
    Event(String),
    /// A line for stderr: a connection lost or back, events the daemon no longer had.
    Note(String),
    /// The one daemon asked for did not answer the first connection.
    Unreachable(String),
}

/// An event under `--json`: the `subscribe` line, with the app's name in front.
#[derive(Serialize)]
struct Named<'a> {
    name: &'a str,
    event: &'a Value,
}

/// Follow `target` for as long as stdout takes the lines; `since` is `--since`. Err: the one daemon asked for did not
/// answer.
pub fn run(
    target: &Target,
    json: bool,
    since: Option<u64>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let start = since.map_or_else(|| Start::At(now_secs()), Start::After);
    let (tx, rx) = channel();
    match target {
        Target::One(socket) => {
            let reader = Reader::new(socket, true, start, json, tx);
            std::thread::spawn(move || reader.run());
        }
        Target::Discover(dir) => {
            let dir = dir.clone();
            std::thread::spawn(move || scan(&dir, start, json, &tx));
        }
    }
    for msg in rx {
        match msg {
            Msg::Event(line) => {
                if !crate::say(out, line) {
                    return Ok(());
                }
            }
            Msg::Note(line) => crate::warn(line),
            Msg::Unreachable(why) => return Err(why),
        }
    }
    Ok(())
}

/// Every few seconds, start a reader for each `<name>.sock` link in `dir` that has none. A reader follows its link for
/// as long as `watch` runs, so a daemon that stops and starts again continues where it was.
fn scan(dir: &Path, start: Start, json: bool, tx: &Sender<Msg>) {
    let mut following = HashSet::new();
    loop {
        let links = std::fs::read_dir(dir).into_iter().flatten().flatten();
        for path in links.map(|link| link.path()) {
            if path.extension().is_some_and(|e| e == "sock") && following.insert(path.clone()) {
                let reader = Reader::new(&path, false, start, json, tx.clone());
                std::thread::spawn(move || reader.run());
            }
        }
        std::thread::sleep(RESCAN);
    }
}

/// How a connection to a daemon ended.
enum Ended {
    /// Nothing came back: no daemon, or one that closed without a line.
    Unanswered(String),
    /// The stream ended after the daemon had answered.
    Lost,
    /// The printing thread is gone.
    Stop,
}

/// What a reader knows of its daemon from one connection to the next.
#[derive(Default)]
struct Seen {
    /// The app's name, from the first `status` line; the events before it wait for it.
    name: Option<String>,
    /// The `seq` of the last event passed on: a reconnect asks for what came after it.
    last_seq: Option<u64>,
    /// Whether a connection has been answered.
    answered: bool,
    /// Whether a connection was lost and none has answered since.
    lost: bool,
}

/// Follows one daemon.
struct Reader {
    socket: PathBuf,
    /// Asked for by name (`--socket`, `--manifest`): when the first connection gets no answer, `watch` ends.
    single: bool,
    start: Start,
    json: bool,
    tx: Sender<Msg>,
}

impl Reader {
    fn new(socket: &Path, single: bool, start: Start, json: bool, tx: Sender<Msg>) -> Reader {
        Reader {
            socket: socket.to_path_buf(),
            single,
            start,
            json,
            tx,
        }
    }

    /// Follow the daemon, and connect again a second after each connection ends, for as long as the printing
    /// thread is there. A discovered link nobody answers on (a daemon that stopped or crashed) is retried quietly.
    fn run(self) {
        let mut seen = Seen::default();
        loop {
            match self.follow(&mut seen) {
                Ended::Stop => return,
                Ended::Unanswered(why) if self.single && !seen.answered => {
                    let why = format!("no daemon answers on {}: {why}", self.socket.display());
                    // Nothing is left to do either way: the printing thread ends `watch` with it, or is gone.
                    let _ = self.tx.send(Msg::Unreachable(why));
                    return;
                }
                Ended::Unanswered(_) => {}
                Ended::Lost => {
                    seen.lost = true;
                    if !self.note(&seen, "the connection was lost; reconnecting") {
                        return;
                    }
                }
            }
            std::thread::sleep(RETRY);
        }
    }

    /// One connection: `subscribe` after the last event passed on (or from the start), each event passed on once
    /// the app's name is known.
    fn follow(&self, seen: &mut Seen) -> Ended {
        let since = match (seen.last_seq, self.start) {
            (Some(seq), _) | (None, Start::After(seq)) => seq,
            (None, Start::At(_)) => 0,
        };
        // No `limit`: the replay holds every event after `since` that the daemon still keeps.
        let request = Request::Subscribe {
            limit: None,
            since: Some(since),
        };
        let stream = match open(&self.socket, &request) {
            Ok(stream) => stream,
            Err(e) => return Ended::Unanswered(e),
        };
        // On a reconnect, the first event passed on should be the one after this; another says what happened.
        let mut resumed_after = seen.last_seq;
        let mut held = Vec::new();
        let mut answered = false;
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if !answered {
                answered = true;
                seen.answered = true;
                if seen.lost {
                    seen.lost = false;
                    if !self.note(seen, "connected again") {
                        return Ended::Stop;
                    }
                }
            }
            let Ok(raw) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Ok(message) = Line::<Value>::deserialize(&raw) else {
                continue;
            };
            match message {
                Line::Event { event } => held.push((event, raw)),
                Line::Status { status } => {
                    seen.name.get_or_insert(status.name);
                }
                // A daemon that refused the stream (`busy`: too many connections); the reader tries again.
                Line::Failed { error, .. } => {
                    if !self.note(seen, &error.message) {
                        return Ended::Stop;
                    }
                }
                Line::History { .. } | Line::Done { .. } => {}
            }
            if seen.name.is_some() {
                for (event, raw) in held.drain(..) {
                    if !self.pass(seen, &event, &raw, &mut resumed_after) {
                        return Ended::Stop;
                    }
                }
            }
        }
        if answered {
            Ended::Lost
        } else {
            Ended::Unanswered("the daemon closed the connection without an answer".into())
        }
    }

    /// Pass one event on, as the daemon sent it (`raw`), and remember its `seq`; false when the printing thread is
    /// gone. Until one is passed on, an event logged before `watch` started is skipped.
    fn pass(
        &self,
        seen: &mut Seen,
        event: &Event,
        raw: &Value,
        resumed_after: &mut Option<u64>,
    ) -> bool {
        if let Start::At(started) = self.start
            && seen.last_seq.is_none()
            && parse_time(&event.at).is_some_and(|at| at < started)
        {
            return true;
        }
        if let Some(last) = resumed_after.take() {
            let gap = if event.seq <= last {
                Some("the daemon's event log started over".to_string())
            } else if event.seq > last.saturating_add(1) {
                Some(format!(
                    "events {} to {} were missed: the daemon no longer keeps them",
                    last.saturating_add(1),
                    event.seq - 1
                ))
            } else {
                None
            };
            if let Some(gap) = gap
                && !self.note(seen, &gap)
            {
                return false;
            }
        }
        seen.last_seq = Some(event.seq);
        let name = seen.name.as_deref().unwrap_or_default();
        let line = if self.json {
            let named = Named {
                name,
                event: &raw["event"],
            };
            serde_json::to_string(&named).unwrap_or_default()
        } else {
            format!("{name}  {}", crate::render_event(event))
        };
        self.tx.send(Msg::Event(line)).is_ok()
    }

    /// A line for stderr about this daemon, named by its app once known, else by its socket.
    fn note(&self, seen: &Seen, text: &str) -> bool {
        let label = seen
            .name
            .clone()
            .unwrap_or_else(|| self.socket.display().to_string());
        self.tx.send(Msg::Note(format!("{label}: {text}"))).is_ok()
    }
}

fn open(socket: &Path, request: &Request) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    request.write_to(&mut stream).map_err(|e| e.to_string())?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::parse_time;
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;

    /// A daemon that answers its connections one after another, each with the given lines, then closes it.
    struct FakeDaemon {
        /// The socket's directory: under `/tmp`, because a socket path over about 100 bytes does not bind and the
        /// default temp dir is long. Unique, so tests in parallel never share a socket.
        _dir: tempfile::TempDir,
        socket: PathBuf,
        served: Option<JoinHandle<Vec<String>>>,
    }

    impl FakeDaemon {
        fn start(connections: Vec<Vec<Value>>) -> FakeDaemon {
            let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
            let socket = dir.path().join("d.sock");
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
                _dir: dir,
                socket,
                served: Some(served),
            }
        }

        /// The `since` of each request received, after every connection has been served.
        fn since(&mut self) -> Vec<Option<u64>> {
            let lines = self.served.take().unwrap().join().unwrap();
            lines
                .iter()
                .map(|l| match Request::parse(l).unwrap() {
                    Request::Subscribe { limit: None, since } => since,
                    other => panic!("{other:?}"),
                })
                .collect()
        }
    }

    const AT: &str = "2026-11-03T14:02:01Z";

    fn event_at(seq: u64, at: &str) -> Value {
        json!({ "event": { "seq": seq, "at": at, "kind": "progress", "generation": null,
                           "message": format!("event {seq}") } })
    }

    fn event(seq: u64) -> Value {
        event_at(seq, AT)
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

    fn reader_of(
        daemon: &FakeDaemon,
        single: bool,
        start: Start,
        json: bool,
    ) -> (Reader, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        (Reader::new(&daemon.socket, single, start, json, tx), rx)
    }

    /// What a reader sent, shortened: an event line's message or seq, a note as `note: …`.
    fn describe(msg: Msg) -> String {
        match msg {
            Msg::Event(line) => line,
            Msg::Note(line) => format!("note: {line}"),
            Msg::Unreachable(why) => format!("unreachable: {why}"),
        }
    }

    #[test]
    fn each_event_is_one_line_with_the_app_name_first_and_nothing_from_before_the_start() {
        for json in [false, true] {
            let mut daemon = FakeDaemon::start(vec![vec![
                // Logged a second before `watch` started: skipped.
                event_at(1, "2026-11-03T14:02:00Z"),
                event(2),
                // The events of the replay wait for the name the status line brings.
                status(),
                history(),
                event(3),
            ]]);
            let start = Start::At(parse_time(AT).unwrap());
            let (reader, rx) = reader_of(&daemon, false, start, json);
            assert!(matches!(reader.follow(&mut Seen::default()), Ended::Lost));
            drop(reader);
            let lines: Vec<String> = rx.iter().map(describe).collect();
            if json {
                assert_eq!(
                    lines,
                    [2, 3].map(|seq| format!(
                        r#"{{"name":"sample","event":{}}}"#,
                        event(seq)["event"]
                    ))
                );
            } else {
                assert_eq!(
                    lines,
                    [
                        format!("sample  {AT}  progress    -    event 2"),
                        format!("sample  {AT}  progress    -    event 3"),
                    ]
                );
            }
            // A start at a time replays all the daemon keeps, to pick from it by time.
            assert_eq!(daemon.since(), [Some(0)]);
        }
    }

    #[test]
    fn a_reconnect_asks_after_the_last_event_and_says_when_the_replay_does_not_continue_it() {
        let mut daemon = FakeDaemon::start(vec![
            // The first connection: events up to 3.
            vec![event(1), event(2), event(3), status(), history()],
            // Continues at 4.
            vec![event(4), event(5), status(), history()],
            // Nothing happened meanwhile: the replay is empty and the snapshot comes first.
            vec![status(), history()],
            // The daemon no longer keeps 6 to 8: the first event is 9.
            vec![event(9), status(), history()],
            // A log that started over numbers from 1 again.
            vec![event(1), event(2), status(), history()],
            // And the next reconnect asks after the new numbering.
            vec![event(3), status(), history()],
        ]);
        let (reader, rx) = reader_of(&daemon, false, Start::After(0), false);
        let mut seen = Seen::default();
        for _ in 0..6 {
            assert!(matches!(reader.follow(&mut seen), Ended::Lost));
        }
        drop(reader);
        let seen: Vec<String> = rx
            .iter()
            .map(|msg| match msg {
                Msg::Event(line) => line.rsplit(' ').next().unwrap().to_string(),
                other => describe(other),
            })
            .collect();
        assert_eq!(
            seen,
            [
                "1",
                "2",
                "3",
                "4",
                "5",
                "note: sample: events 6 to 8 were missed: the daemon no longer keeps them",
                "9",
                "note: sample: the daemon's event log started over",
                "1",
                "2",
                "3"
            ]
        );
        assert_eq!(
            daemon.since(),
            [Some(0), Some(3), Some(5), Some(5), Some(9), Some(2)]
        );
    }

    #[test]
    fn the_one_daemon_asked_for_must_answer_once_and_is_then_reconnected() {
        // A daemon that closes without an answer ends a watch of that one daemon.
        let silent = FakeDaemon::start(vec![vec![]]);
        let (reader, rx) = reader_of(&silent, true, Start::After(0), false);
        reader.run();
        let msgs: Vec<String> = rx.iter().map(describe).collect();
        assert_eq!(
            msgs,
            [format!(
                "unreachable: no daemon answers on {}: the daemon closed the connection without an answer",
                silent.socket.display()
            )]
        );

        // Once it answered, a lost connection is retried, and the events continue.
        let daemon = FakeDaemon::start(vec![
            vec![status(), event(1)],
            vec![],
            vec![event(2), status()],
        ]);
        let (reader, rx) = reader_of(&daemon, true, Start::After(0), false);
        std::thread::spawn(move || reader.run());
        let msgs: Vec<String> = rx.iter().take(5).map(describe).collect();
        assert_eq!(
            msgs,
            [
                format!("sample  {AT}  progress    -    event 1"),
                "note: sample: the connection was lost; reconnecting".into(),
                // The connection that closed without a line is retried quietly, not counted as one more loss.
                "note: sample: connected again".into(),
                format!("sample  {AT}  progress    -    event 2"),
                "note: sample: the connection was lost; reconnecting".into(),
            ]
        );
    }

    #[test]
    fn the_scanner_follows_each_sock_link_and_nothing_else() {
        let daemon = FakeDaemon::start(vec![vec![status(), event(1)]]);
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        std::os::unix::fs::symlink(&daemon.socket, dir.path().join("sample.sock")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("dead.sock"))
            .unwrap();
        // Not a link to a daemon: connecting to it would fail, but it is never tried.
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        let (tx, rx) = mpsc::channel();
        let path = dir.path().to_path_buf();
        std::thread::spawn(move || scan(&path, Start::After(0), true, &tx));
        let first = rx.recv_timeout(Duration::from_secs(5)).map(describe);
        assert_eq!(
            first,
            Ok(format!(
                r#"{{"name":"sample","event":{}}}"#,
                event(1)["event"]
            ))
        );
    }
}
