//! `fawkes watch`: one `subscribe` stream per daemon, every daemon linked in `<fawkes_home>/run/` or the one a socket
//! names, and every event printed as one line with the app's name first. A daemon that drops is reconnected, asking
//! for the events after the last one printed, so none is lost or printed twice while the daemon still keeps it.
//!
//! `fawkes wait` follows the same stream of one daemon, for the pictures it sends instead of the events, until one
//! generation is decided.

use crate::protocol::{
    Error, ErrorCode, Event, GenState, Generation, History, Line, Request, Status, Step, err,
    invalid_state, not_found, timed_out,
};
use crate::state::{now_secs, parse_time};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

const RETRY: Duration = Duration::from_secs(1);
const RESCAN: Duration = Duration::from_secs(3);
/// The lines waiting for stdout. A stdout that blocks (a paused pager) then blocks the readers, the daemon drops a
/// stream that falls behind, and the reconnect resumes it with `since`, instead of `watch` buffering without bound.
const QUEUE: usize = 1024;

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
    let (tx, rx) = sync_channel(QUEUE);
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
fn scan(dir: &Path, start: Start, json: bool, tx: &SyncSender<Msg>) {
    let mut following = HashSet::new();
    let mut said = false;
    loop {
        let links = std::fs::read_dir(dir).into_iter().flatten().flatten();
        for path in links.map(|link| link.path()) {
            if path.extension().is_some_and(|e| e == "sock") && following.insert(path.clone()) {
                let reader = Reader::new(&path, false, start, json, tx.clone());
                std::thread::spawn(move || reader.run());
            }
        }
        // Silence reads as a hang: say once that `watch` is alive and what it waits for.
        if following.is_empty() && !std::mem::replace(&mut said, true) {
            let note = format!("no daemon runs yet; waiting for one in {}", dir.display());
            if tx.send(Msg::Note(note)).is_err() {
                return;
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
    /// The daemon refused the stream with this message (`busy`: too many connections).
    Refused(String),
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
    tx: SyncSender<Msg>,
}

impl Reader {
    fn new(socket: &Path, single: bool, start: Start, json: bool, tx: SyncSender<Msg>) -> Reader {
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
                // Said once per outage, however many retries it takes; the next stream says `connected again`.
                Ended::Lost | Ended::Refused(_) if seen.lost => {}
                Ended::Lost => {
                    seen.lost = true;
                    if !self.note(&seen, "the connection was lost; reconnecting") {
                        return;
                    }
                }
                Ended::Refused(why) => {
                    seen.answered = true;
                    seen.lost = true;
                    if !self.note(&seen, &format!("{why}; retrying")) {
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
            let Ok(raw) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Ok(message) = Line::<Value>::deserialize(&raw) else {
                continue;
            };
            if let Line::Failed { error, .. } = message {
                return Ended::Refused(error.message);
            }
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
            match message {
                Line::Event { event } => held.push((event, raw)),
                Line::Status { status } => {
                    seen.name.get_or_insert(status.name);
                }
                Line::History { .. } | Line::Done { .. } | Line::Failed { .. } => {}
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

// ------------------------------------------------ wait

/// How long past `pending.deadline` a generation may stay `current` before `wait` gives up on the daemon. The
/// supervisor rolls a missed deadline back within a second; the rest covers a slow or sleeping machine.
const GRACE_SECS: u64 = 30;

/// How long a quiet stream is read before the bound is looked at again.
const TICK: Duration = Duration::from_secs(1);

/// Wait until a generation is decided: `id`, else the pending one. Ok: it is `permanent` (or `retired`, which it was
/// once confirmed), as its record. Err: why it will not be, with the code `fawkes wait` fails with. `progress` gets
/// the one line saying what is waited for, once.
///
/// It decides from the `status` and `history` pictures of one `subscribe` stream, the first and then after every
/// change, and reconnects every second when the stream ends (a restarted daemon rolls a pending switch back). A
/// daemon that never answers is `daemon_unreachable`; one that leaves the generation `current` past its deadline and
/// `GRACE_SECS` is `no_answer`, so a wait never hangs on a daemon that cannot decide. A daemon that is gone is waited
/// for the same time after its last answer, or after that deadline when later.
pub fn wait(
    socket: &Path,
    id: Option<&str>,
    progress: &mut dyn FnMut(String),
) -> Result<Generation, Error> {
    let mut waiter = Waiter {
        socket,
        id: id.map(String::from),
        answered: false,
        announced: false,
        due: None,
        lost: None,
    };
    loop {
        let ended = match open(socket, &PICTURES) {
            Ok(stream) => match waiter.follow(stream, progress)? {
                Some(generation) => return Ok(generation),
                None => "the stream ended".to_string(),
            },
            Err(why) => why,
        };
        let shown = waiter.socket.display();
        if !waiter.answered {
            return Err(err(
                ErrorCode::DaemonUnreachable,
                format!("no daemon answers on {shown}: {ended}"),
            ));
        }
        let now = now_secs();
        let lost = *waiter.lost.get_or_insert(now);
        let give_up = waiter
            .due
            .map_or(0, |due| due + GRACE_SECS)
            .max(lost + GRACE_SECS);
        if now >= give_up {
            let what = waiter.id.as_deref().unwrap_or("the pending switch");
            return Err(err(
                ErrorCode::DaemonUnreachable,
                format!("the daemon on {shown} stopped answering while waiting for {what}"),
            ));
        }
        std::thread::sleep(RETRY);
    }
}

/// `subscribe` without the events' replay: `wait` reads the pictures.
const PICTURES: Request = Request::Subscribe {
    limit: Some(0),
    since: None,
};

/// What `wait` knows from one connection to the next.
struct Waiter<'a> {
    socket: &'a Path,
    /// The generation waited for, once known.
    id: Option<String>,
    /// Whether any connection has been answered.
    answered: bool,
    /// Whether the line saying what is waited for was shown.
    announced: bool,
    /// The confirm deadline of the generation, as last seen.
    due: Option<u64>,
    /// When the stream was lost, and none has answered since.
    lost: Option<u64>,
}

impl Waiter<'_> {
    /// Read one connection until the generation is decided (`Some`) or the stream ends (`None`). The pictures are
    /// judged on every line and on every quiet second, since the bound passes without anything being sent.
    fn follow(
        &mut self,
        stream: UnixStream,
        progress: &mut dyn FnMut(String),
    ) -> Result<Option<Generation>, Error> {
        // Refused on macOS once the daemon has closed its end: the read then ends at once instead.
        let _ = stream.set_read_timeout(Some(TICK));
        let mut reader = BufReader::new(stream);
        let (mut status, mut history): (Option<Status>, Option<History>) = (None, None);
        // Kept across a timeout, which can come with half a line read.
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    self.answered = true;
                    self.lost = None;
                    let message = serde_json::from_str::<Value>(&line)
                        .ok()
                        .and_then(|raw| Line::<Value>::deserialize(&raw).ok());
                    line.clear();
                    match message {
                        Some(Line::Status { status: new }) => status = Some(*new),
                        Some(Line::History { history: new }) => history = Some(new),
                        // `busy`: too many connections.
                        Some(Line::Failed { .. }) => return Ok(None),
                        _ => {}
                    }
                }
                Err(e) if timed_out(&e) => {}
                Err(_) => return Ok(None),
            }
            if let (Some(status), Some(history)) = (&status, &history)
                && let Some(generation) = self.judge(status, history, progress)?
            {
                return Ok(Some(generation));
            }
        }
    }

    fn judge(
        &mut self,
        status: &Status,
        history: &History,
        progress: &mut dyn FnMut(String),
    ) -> Result<Option<Generation>, Error> {
        let id = match (&self.id, &status.pending) {
            (Some(id), _) => id.clone(),
            (None, Some(pending)) => pending.generation.clone(),
            (None, None) => {
                return Err(invalid_state(
                    "nothing is pending: no switch awaits a confirm; `fawkes wait <gen>` waits for a given one",
                ));
            }
        };
        self.id = Some(id.clone());
        let pending = status.pending.as_ref().filter(|p| p.generation == id);
        if let Some(pending) = pending {
            self.due = parse_time(&pending.deadline);
        }
        let decided = decide(&id, status, history, now_secs())?;
        if decided.is_none() && !std::mem::replace(&mut self.announced, true) {
            progress(match pending {
                Some(pending) => format!("waiting for {id}: confirm due by {}", pending.deadline),
                None => format!("waiting for {id}"),
            });
        }
        Ok(decided)
    }
}

/// Where generation `id` stands in these pictures at time `now`: confirmed (its record), not yet decided (`None`), or
/// an error saying what it came to.
fn decide(
    id: &str,
    status: &Status,
    history: &History,
    now: u64,
) -> Result<Option<Generation>, Error> {
    let Some(entry) = history.generations.iter().find(|e| e.generation.id == id) else {
        return Err(not_found(id));
    };
    let g = &entry.generation;
    // `pending` stays set while the switch is rolled back, `step` says how far that is.
    let live = status.pending.as_ref().filter(|p| p.generation == id);
    match (g.state, live.and(status.step)) {
        (GenState::Permanent | GenState::Retired, _) => Ok(Some(g.clone())),
        (GenState::RolledBack, Some(Step::RollingBack)) => Ok(None),
        (GenState::RolledBack, Some(Step::RestoreFailed)) => {
            let runs = status.permanent.as_deref().map_or_else(
                || "leaves the app stopped (there is no permanent generation)".into(),
                |permanent| format!("starts {permanent}"),
            );
            Err(err(
                ErrorCode::StorageFailed,
                format!(
                    "{id} was rolled back but its storage could not be restored; `fawkes rollback` retries the \
                     restore, or `fawkes rollback --keep-storage` {runs} on the storage as it is (it may already be \
                     migrated)"
                ),
            )
            .about(id))
        }
        (GenState::RolledBack, _) => {
            let reason = g.rollback_reason.as_deref().unwrap_or("no reason recorded");
            let runs = status
                .permanent
                .as_deref()
                .map_or_else(|| "no generation runs".into(), |p| format!("{p} runs"));
            Err(err(
                ErrorCode::RolledBack,
                format!("{id} was rolled back ({reason}); {runs}; `fawkes history` has its log tail"),
            )
            .about(id)
            .because(g.rollback_cause))
        }
        (GenState::Current, _) => match live.and_then(|p| parse_time(&p.deadline)) {
            Some(due) if now > due + GRACE_SECS => Err(err(
                ErrorCode::NoAnswer,
                format!(
                    "the daemon did not decide {id} by its deadline ({}) or within {GRACE_SECS} s after it; check \
                     `fawkes status`",
                    live.map_or("", |p| p.deadline.as_str())
                ),
            )
            .about(id)),
            _ => Ok(None),
        },
        // A switch to it that is starting: the target is not `current` until it has stopped the live app.
        (_, _) if status.busy.as_deref() == Some(&format!("switch {id}")) => Ok(None),
        (state, _) => Err(invalid_state(format!(
            "{id} is {}; it is not switched to, so nothing will decide it",
            state.as_str()
        ))
        .about(id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RollbackCause;
    use crate::state::parse_time;
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::Receiver;
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
        let (tx, rx) = sync_channel(QUEUE);
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

        // A refusal is an answer: it is retried, like a lost connection once the daemon answered, and the events
        // continue. Each outage is said once, however many retries it takes.
        let busy =
            json!({ "ok": false, "error": { "code": "busy", "message": "too many connections" } });
        let daemon = FakeDaemon::start(vec![
            vec![busy.clone()],
            vec![status(), event(1)],
            vec![busy],
            vec![],
            vec![event(2), status()],
        ]);
        let (reader, rx) = reader_of(&daemon, true, Start::After(0), false);
        std::thread::spawn(move || reader.run());
        let msgs: Vec<String> = rx.iter().take(7).map(describe).collect();
        // Before the first status line, the daemon is named by its socket.
        let socket = daemon.socket.display();
        assert_eq!(
            msgs,
            [
                format!("note: {socket}: too many connections; retrying"),
                format!("note: {socket}: connected again"),
                format!("sample  {AT}  progress    -    event 1"),
                "note: sample: the connection was lost; reconnecting".into(),
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
        let (tx, rx) = sync_channel(QUEUE);
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
    fn generation(id: &str, state: &str) -> Value {
        json!({ "id": id, "ref": "main", "commit": "0123456789abcdef", "state": state, "dir": "/w/g",
                "staged_at": AT, "test": null, "switched_at": null, "confirmed_at": null,
                "rolled_back_at": null, "rollback_reason": null, "failed_starts": 0 })
    }

    fn rolled_back(id: &str) -> Value {
        let mut g = generation(id, "rolled_back");
        g["rollback_reason"] = json!("missed confirm deadline");
        g["rollback_cause"] = json!("deadline");
        g
    }

    /// The two pictures of a daemon whose `g2` is pending (`step`, `deadline`) beside the permanent `g1`.
    fn pictures(g2: &Value, step: Option<&str>, deadline: &str) -> Vec<Value> {
        let mut status = status();
        status["status"]["permanent"] = json!("g1");
        status["status"]["pending"] =
            json!({ "generation": "g2", "previous": "g1", "deadline": deadline });
        status["status"]["step"] = json!(step);
        vec![
            status,
            json!({ "history": { "generations": [generation("g1", "permanent"), g2] } }),
        ]
    }

    fn waited(daemon: &FakeDaemon, id: Option<&str>) -> Result<Generation, Error> {
        wait(&daemon.socket, id, &mut |_| {})
    }

    const FUTURE: &str = "2099-01-01T00:00:00Z";
    const PAST: &str = "2020-01-01T00:00:00Z";

    #[test]
    fn a_generation_is_decided_by_its_state_and_the_step_of_the_switch() {
        let decide_in = |g2: Value, step: Option<&str>, deadline: &str, busy: Option<&str>| {
            let pictures = pictures(&g2, step, deadline);
            let mut status = pictures[0]["status"].clone();
            status["busy"] = json!(busy);
            let status: Status = serde_json::from_value(status).unwrap();
            let history: History = serde_json::from_value(pictures[1]["history"].clone()).unwrap();
            decide("g2", &status, &history, parse_time(AT).unwrap())
        };
        let code = |result: Result<Option<Generation>, Error>| {
            let e = result.unwrap_err();
            (e.code, e.generation, e.cause)
        };
        let g2 = |state| generation("g2", state);

        let confirmed = decide_in(g2("permanent"), None, FUTURE, None);
        assert_eq!(confirmed.unwrap().unwrap().id, "g2");
        // Confirmed, and replaced since.
        assert!(
            decide_in(g2("retired"), None, FUTURE, None)
                .unwrap()
                .is_some()
        );
        assert!(
            decide_in(g2("current"), Some("pending"), FUTURE, None)
                .unwrap()
                .is_none()
        );
        // Past its deadline, but within the grace the daemon has to roll it back.
        assert!(
            decide_in(g2("current"), Some("pending"), AT, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            code(decide_in(g2("current"), Some("pending"), PAST, None)),
            (ErrorCode::NoAnswer, Some("g2".into()), None)
        );
        // The rollback is decided: its end is waited for, and a restore that failed is its own answer.
        assert!(
            decide_in(rolled_back("g2"), Some("rolling_back"), PAST, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            code(decide_in(
                rolled_back("g2"),
                Some("restore_failed"),
                PAST,
                None
            )),
            (ErrorCode::StorageFailed, Some("g2".into()), None)
        );
        let done = decide_in(rolled_back("g2"), None, FUTURE, None).unwrap_err();
        assert_eq!(
            (done.code, done.cause, done.message.as_str()),
            (
                ErrorCode::RolledBack,
                Some(RollbackCause::Deadline),
                "g2 was rolled back (missed confirm deadline); g1 runs; `fawkes history` has its log tail"
            )
        );
        // Not switched to: nothing decides it, unless a switch to it is starting.
        let tested = decide_in(g2("tested"), None, FUTURE, None);
        assert_eq!(
            tested.unwrap_err().message,
            "g2 is tested; it is not switched to, so nothing will decide it"
        );
        assert!(
            decide_in(g2("tested"), Some("switching"), FUTURE, Some("switch g2"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            code(decide_in(g2("tested"), None, FUTURE, Some("switch g3"))).0,
            ErrorCode::InvalidState
        );
    }

    #[test]
    fn wait_follows_the_stream_across_a_daemon_restart_to_the_rollback_it_made() {
        let daemon = FakeDaemon::start(vec![
            // Pending and waiting; the daemon goes away.
            pictures(&generation("g2", "current"), Some("pending"), FUTURE),
            // It is back, and rolled the switch back as it started.
            pictures(&rolled_back("g2"), None, FUTURE),
        ]);
        let mut said = Vec::new();
        let error = wait(&daemon.socket, None, &mut |line| said.push(line)).unwrap_err();
        assert_eq!(
            (error.code, error.generation.as_deref(), error.cause),
            (
                ErrorCode::RolledBack,
                Some("g2"),
                Some(RollbackCause::Deadline)
            )
        );
        assert_eq!(said, [format!("waiting for g2: confirm due by {FUTURE}")]);
    }

    #[test]
    fn wait_gives_up_on_a_daemon_that_leaves_a_generation_undecided_past_its_deadline() {
        // Held open, as a daemon that is up but never decides: the answer comes at once from the pictures.
        let daemon = FakeDaemon::start(vec![pictures(
            &generation("g2", "current"),
            Some("pending"),
            PAST,
        )]);
        let started = std::time::Instant::now();
        let error = waited(&daemon, Some("g2")).unwrap_err();
        assert_eq!(error.code, ErrorCode::NoAnswer, "{error:?}");
        assert!(
            error.message.contains(PAST) && error.message.contains("`fawkes status`"),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn wait_without_a_daemon_is_unreachable_and_names_what_is_pending_by_default() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let error = wait(&dir.path().join("none.sock"), None, &mut |_| {}).unwrap_err();
        assert_eq!(error.code, ErrorCode::DaemonUnreachable);

        let daemon = FakeDaemon::start(vec![vec![status(), history()]]);
        let error = waited(&daemon, None).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidState);
        assert!(error.message.starts_with("nothing is pending"), "{error:?}");
    }
}
