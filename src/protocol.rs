//! Socket protocol types: requests, errors and the writer that sends response lines.

use crate::manifest::MAX_DEADLINE;
use crate::state::Event;
use rustix::net::{RecvFlags, recv};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::net::UnixStream;

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
pub struct Out(Option<UnixStream>);

impl Out {
    pub fn new(stream: UnixStream) -> Out {
        Out(Some(stream))
    }

    pub fn none() -> Out {
        Out(None)
    }

    /// Send one JSON line; false when the client is gone.
    pub fn line(&mut self, value: &Value) -> bool {
        match &mut self.0 {
            Some(stream) => stream.write_all(format!("{value}\n").as_bytes()).is_ok(),
            None => true,
        }
    }

    /// Has the client closed its end? Streams use this to notice a client that left while nothing was sent.
    pub fn closed(&self) -> bool {
        let Some(stream) = &self.0 else { return false };
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
