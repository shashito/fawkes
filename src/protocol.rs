//! Socket protocol types: requests, errors and the writer that sends response lines.

use crate::state::Event;
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
        use std::os::fd::AsRawFd;
        let Some(stream) = &self.0 else { return false };
        let mut byte = 0u8;
        let n = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        n == 0
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
