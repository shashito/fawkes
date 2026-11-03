//! Socket protocol types: requests, errors and the writer that sends response lines.

use crate::manifest::MAX_DEADLINE;
use crate::state::Event;
use rustix::net::{RecvFlags, recv};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// How long one write to a client may block. A client that stops reading (a suspended TUI, a paused pager) then counts
/// as gone, instead of stalling the step or the stream that writes to it.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
pub struct Request {
    pub command: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}

pub type R<T> = Result<T, Error>;

/// The `deadline` argument of `switch`: absent, or whole seconds in `1..=MAX_DEADLINE`. Checked at the boundary, so
/// a switch with a deadline it cannot keep is refused before anything is stopped.
pub fn deadline_arg(args: &Value) -> R<Option<u64>> {
    match args.get("deadline") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|seconds| (1..=MAX_DEADLINE).contains(seconds))
            .map(Some)
            .ok_or_else(|| {
                err(
                    "usage",
                    format!(
                        "the deadline must be whole seconds from 1 to {MAX_DEADLINE} (a week), not {value}"
                    ),
                )
            }),
    }
}

pub fn err(code: &'static str, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        err("internal", e.to_string())
    }
}

/// The client connection of one request; `Out::none()` for work nobody is waiting on.
pub struct Out {
    stream: Option<UnixStream>,
    /// A write failed or timed out: nothing more is sent, so a client that stopped reading costs one timeout, not
    /// one per line.
    gone: bool,
}

impl Out {
    pub fn new(stream: UnixStream) -> Out {
        // Fails only for a zero duration.
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        Out {
            stream: Some(stream),
            gone: false,
        }
    }

    pub fn none() -> Out {
        Out {
            stream: None,
            gone: false,
        }
    }

    /// Send one JSON line; false when the client is gone.
    pub fn line(&mut self, value: &Value) -> bool {
        if self.gone {
            return false;
        }
        if let Some(stream) = &mut self.stream {
            self.gone = stream.write_all(format!("{value}\n").as_bytes()).is_err();
        }
        !self.gone
    }

    /// Has the client closed its end? Streams use this to notice a client that left while nothing was sent.
    pub fn closed(&self) -> bool {
        if self.gone {
            return true;
        }
        let Some(stream) = &self.stream else {
            return false;
        };
        let mut byte = [0u8];
        matches!(
            recv(stream, &mut byte[..], RecvFlags::PEEK | RecvFlags::DONTWAIT),
            Ok((0, _))
        )
    }

    pub fn event(&mut self, event: &Event) -> bool {
        self.line(&json!({ "event": event }))
    }

    pub fn finish(&mut self, result: R<Value>) {
        let line = match result {
            Ok(result) => json!({ "ok": true, "result": result }),
            Err(e) => json!({ "ok": false, "error": { "code": e.code, "message": e.message } }),
        };
        self.line(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    /// The timeout applies to each system call, so the line in flight may wait for it twice (a partial write, then
    /// the rest); after that nothing is sent at all.
    fn a_client_that_stops_reading_costs_a_bounded_wait_once() {
        let (daemon_end, _client_end) = UnixStream::pair().unwrap();
        let mut out = Out::new(daemon_end);
        let line = json!({ "event": "x".repeat(4096) });
        let started = Instant::now();
        let mut sent = 0;
        while out.line(&line) {
            sent += 1;
            assert!(sent < 100_000, "the socket buffer never filled");
        }
        assert!(
            started.elapsed() < WRITE_TIMEOUT * 3,
            "{:?}",
            started.elapsed()
        );
        let again = Instant::now();
        assert!(!out.line(&line) && out.closed());
        assert!(again.elapsed() < Duration::from_millis(100));
    }
}
