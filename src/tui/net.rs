//! The TUI's connection layer: one reader thread per daemon holding a `subscribe` stream, the scanner that finds
//! daemons, and the one-shot request an action sends.

use super::Msg;
use crate::protocol::{Line, Request};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RETRY: Duration = Duration::from_secs(1);
const RESCAN: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const BACKLOG: u64 = 100;

/// The keys of the apps that have a reader thread.
pub(super) type Active = Arc<Mutex<HashSet<String>>>;

/// Follow one daemon. A single target is retried for as long as the screen lives. A discovered one is retried, shown
/// as disconnected, while its link still leads to a socket file (a crashed daemon); it leaves the list once the link
/// is gone (a daemon that stopped), and the scanner starts a fresh reader if it comes back. A link that never
/// answered is not listed at all.
pub(super) fn reader(key: &str, socket: &Path, active: Option<&Active>, tx: &Sender<Msg>) {
    let mut seen = false;
    loop {
        let (up, why) = follow(key, socket, tx);
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
                active.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
            }
            return;
        }
        std::thread::sleep(RETRY);
    }
}

/// Fetch the recent events, then follow `subscribe`. The error says whether the app had answered.
fn follow(key: &str, socket: &Path, tx: &Sender<Msg>) -> (bool, String) {
    let request = Request::Subscribe {
        limit: Some(BACKLOG),
    };
    let stream = match open(socket, &request) {
        Ok(stream) => stream,
        Err(e) => return (false, e),
    };
    let mut up = false;
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if !up {
            up = true;
            if tx.send(Msg::Up(key.into(), socket.into())).is_err() {
                return (true, "the screen is gone".into());
            }
        }
        if let Ok(line) = serde_json::from_str(&line)
            && tx.send(Msg::Line(key.into(), line)).is_err()
        {
            return (true, "the screen is gone".into());
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
                .unwrap_or_else(|e| e.into_inner())
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
    let line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    writeln!(stream, "{line}").map_err(|e| e.to_string())?;
    Ok(stream)
}

/// One request: progress lines are skipped (they reach the screen through `subscribe`), the answer is the outcome.
pub(super) fn request(socket: &Path, request: &Request) -> Result<(), String> {
    let stream = open(socket, request)?;
    // A daemon that accepts and then hangs must not leave the action thread and its notice waiting for ever. Only
    // quick actions get the limit: a stage, test, switch, stop or prune can stay silent for as long as an install,
    // a health check, a stop timeout or a removal takes, and the daemon bounds those by the manifest's timeouts.
    if matches!(request, Request::Start | Request::Confirm { .. }) {
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|e| e.to_string())?;
    }
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "the daemon did not answer in time".to_string()
            }
            _ => e.to_string(),
        })?;
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Done { .. }) => return Ok(()),
            Ok(Line::Failed { error, .. }) => return Err(error.message),
            _ => {}
        }
    }
    Err("the daemon closed the connection without an answer".into())
}
