//! The socket protocol: requests, errors, the typed records and results that cross the wire, and the writer that
//! sends response lines. The daemon, the CLI and the TUI all compile against these types, so a renamed field or a new
//! error code is a compile error in every one of them, not a missing key at run time.

use crate::manifest::MAX_DEADLINE;
use crate::state::now;
use rustix::net::{RecvFlags, recv};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

/// How long one write to a client may block. A client that stops reading (a suspended TUI, a paused pager) then counts
/// as gone, instead of stalling the step or the stream that writes to it.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a client waits for each line of the answer to a quick request (`Request::is_quick`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a read gave up because its timeout passed.
pub fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

// ------------------------------------------------------------- errors

/// The stable `code` of an error. Agents depend on the strings, so a code is added, never renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Usage,
    NotFound,
    InvalidState,
    Busy,
    GitFailed,
    InstallFailed,
    HealthFailed,
    StorageFailed,
    StartFailed,
    PortInUse,
    Internal,
    /// Produced by the client only: no daemon answers on the socket, or it closed the connection without answering.
    DaemonUnreachable,
    /// Produced by the client only: the daemon accepted a quick request and did not answer in time.
    NoAnswer,
    /// A code a newer daemon sent that this client does not know; the command failed all the same.
    #[serde(other)]
    Unknown,
}

/// An error answer: a stable code and a message written for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    /// The same error with `hint` (what changed, what to do next) after its message.
    pub fn with_hint(self, hint: impl std::fmt::Display) -> Error {
        err(self.code, format!("{}; {hint}", self.message))
    }
}

pub type R<T> = Result<T, Error>;

pub fn err(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
    }
}

pub fn invalid_state(message: impl Into<String>) -> Error {
    err(ErrorCode::InvalidState, message)
}

pub fn not_found(id: &str) -> Error {
    err(
        ErrorCode::NotFound,
        format!("there is no generation {id}; `fawkes history` lists them"),
    )
}

// ----------------------------------------------------------- requests

/// One command with its arguments. On the wire it is `{"command": "stage", "args": {"ref": "main"}}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Start,
    Stop,
    Restart,
    Stage {
        git_ref: String,
    },
    Test {
        generation: String,
    },
    Switch {
        generation: String,
        /// Whole seconds in `1..=MAX_DEADLINE`; the manifest's default when absent.
        deadline: Option<u64>,
    },
    Confirm {
        /// Must be the pending generation; the pending one when absent.
        generation: Option<String>,
    },
    Prune,
    Rollback {
        /// Give up a restore that failed and run the permanent generation on the storage as it is.
        keep_storage: bool,
    },
    Status,
    History,
    Log {
        /// How many of the newest events; 20 when absent.
        limit: Option<u64>,
        follow: bool,
    },
    Subscribe {
        /// How many recent events to replay first.
        limit: Option<u64>,
        /// Replay the events after this `seq` instead of the last `limit`.
        since: Option<u64>,
    },
}

/// What a client sends, before its arguments are checked.
#[derive(Deserialize)]
struct Envelope {
    command: String,
    #[serde(default)]
    args: Value,
}

impl Request {
    /// Read one request line. Every failure is a `usage` error: the daemon answers it and does nothing else. `args`
    /// may be absent, null or `{}` for a command without arguments, and keys it does not use are ignored; an
    /// optional argument may be null.
    pub fn parse(line: &str) -> R<Request> {
        let Envelope { command, args } = serde_json::from_str(line)
            .map_err(|e| err(ErrorCode::Usage, format!("malformed request: {e}")))?;
        Ok(match command.as_str() {
            "start" => Request::Start,
            "stop" => Request::Stop,
            "restart" => Request::Restart,
            "stage" => Request::Stage {
                git_ref: required(&args, "ref")?,
            },
            "test" => Request::Test {
                generation: required(&args, "generation")?,
            },
            "switch" => Request::Switch {
                generation: required(&args, "generation")?,
                deadline: deadline_arg(&args)?,
            },
            "confirm" => Request::Confirm {
                generation: optional_text(&args, "generation")?,
            },
            "prune" => Request::Prune,
            "rollback" => Request::Rollback {
                keep_storage: optional_bool(&args, "keep_storage")?,
            },
            "status" => Request::Status,
            "history" => Request::History,
            "log" => Request::Log {
                limit: optional_count(&args, "limit")?,
                follow: optional_bool(&args, "follow")?,
            },
            "subscribe" => Request::Subscribe {
                limit: optional_count(&args, "limit")?,
                since: optional_count(&args, "since")?,
            },
            other => {
                return Err(err(ErrorCode::Usage, format!("unknown command `{other}`")));
            }
        })
    }

    /// The `command` of the request on the wire.
    pub fn command(&self) -> &'static str {
        match self {
            Request::Start => "start",
            Request::Stop => "stop",
            Request::Restart => "restart",
            Request::Stage { .. } => "stage",
            Request::Test { .. } => "test",
            Request::Switch { .. } => "switch",
            Request::Confirm { .. } => "confirm",
            Request::Prune => "prune",
            Request::Rollback { .. } => "rollback",
            Request::Status => "status",
            Request::History => "history",
            Request::Log { .. } => "log",
            Request::Subscribe { .. } => "subscribe",
        }
    }

    /// Whether the daemon answers at once, so a client gives up on a daemon that accepts and then never answers
    /// (`fawkes confirm`, which the app itself calls, must not hang). Any other request can stay silent for as long as
    /// an install, a health check, a stop timeout or a removal takes, which the daemon bounds by the manifest's
    /// timeouts, and a followed log is silent between events by design.
    pub fn is_quick(&self) -> bool {
        matches!(
            self,
            Request::Start
                | Request::Confirm { .. }
                | Request::Status
                | Request::History
                | Request::Log { follow: false, .. }
        )
    }

    /// Send the request as one line.
    pub fn write_to(&self, stream: &mut impl Write) -> std::io::Result<()> {
        let line = serde_json::to_string(self)?;
        writeln!(stream, "{line}")
    }
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let args = match self {
            Request::Stage { git_ref } => json!({ "ref": git_ref }),
            Request::Test { generation } => json!({ "generation": generation }),
            Request::Switch {
                generation,
                deadline,
            } => json!({ "generation": generation, "deadline": deadline }),
            Request::Confirm { generation } => json!({ "generation": generation }),
            Request::Rollback { keep_storage } => json!({ "keep_storage": keep_storage }),
            Request::Log { limit, follow } => json!({ "limit": limit, "follow": follow }),
            Request::Subscribe { limit, since } => json!({ "limit": limit, "since": since }),
            Request::Start
            | Request::Stop
            | Request::Restart
            | Request::Prune
            | Request::Status
            | Request::History => json!({}),
        };
        let mut envelope = serializer.serialize_struct("Request", 2)?;
        envelope.serialize_field("command", self.command())?;
        envelope.serialize_field("args", &args)?;
        envelope.end()
    }
}

fn required(args: &Value, key: &str) -> R<String> {
    optional_text(args, key)?
        .ok_or_else(|| err(ErrorCode::Usage, format!("missing argument `{key}`")))
}

// A present argument of the wrong type is refused, not read as absent: `{"generation": 7}` must not confirm whatever
// is pending. Null still means absent, and keys no command uses are ignored.

/// The argument `key` through `read`, `None` when absent or null; `type_name` names what it must be in the error.
fn optional_arg<T>(
    args: &Value,
    key: &str,
    type_name: &str,
    read: impl FnOnce(&Value) -> Option<T>,
) -> R<Option<T>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read(value).map(Some).ok_or_else(|| {
            err(
                ErrorCode::Usage,
                format!("the argument `{key}` must be {type_name}, not {value}"),
            )
        }),
    }
}

fn optional_text(args: &Value, key: &str) -> R<Option<String>> {
    optional_arg(args, key, "a string", |v| v.as_str().map(String::from))
}

fn optional_count(args: &Value, key: &str) -> R<Option<u64>> {
    optional_arg(args, key, "a non-negative whole number", Value::as_u64)
}

/// A flag: false when absent or null.
fn optional_bool(args: &Value, key: &str) -> R<bool> {
    optional_arg(args, key, "true or false", Value::as_bool).map(|flag| flag == Some(true))
}

/// The `deadline` argument of `switch`: absent, or whole seconds in `1..=MAX_DEADLINE`. Checked at the boundary, so
/// a switch with a deadline it cannot keep is refused before anything is stopped.
fn deadline_arg(args: &Value) -> R<Option<u64>> {
    match args.get("deadline") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|seconds| (1..=MAX_DEADLINE).contains(seconds))
            .map(Some)
            .ok_or_else(|| {
                err(
                    ErrorCode::Usage,
                    format!(
                        "the deadline must be whole seconds from 1 to {MAX_DEADLINE} (a week), not {value}"
                    ),
                )
            }),
    }
}

// ------------------------------------------------------------ records

/// Where a generation is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenState {
    /// The worktree and install are in progress.
    Staging,
    Staged,
    TestFailed,
    Tested,
    Current,
    Permanent,
    Retired,
    RolledBack,
    /// A state a newer daemon wrote that this client does not know. Decode-only: this daemon never writes it, and
    /// refuses a `state.json` that holds it.
    #[serde(other)]
    Unknown,
}

impl GenState {
    /// The name on the wire, for messages and screens.
    pub fn as_str(self) -> &'static str {
        match self {
            GenState::Staging => "staging",
            GenState::Staged => "staged",
            GenState::TestFailed => "test_failed",
            GenState::Tested => "tested",
            GenState::Current => "current",
            GenState::Permanent => "permanent",
            GenState::Retired => "retired",
            GenState::RolledBack => "rolled_back",
            GenState::Unknown => "unknown",
        }
    }
}

/// The outcome of a generation's last test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestInfo {
    pub port: u16,
    pub passed: bool,
    pub at: String,
    pub seconds: f64,
    pub message: String,
}

/// A generation record, as `state.json` keeps it and `stage`, `test`, `confirm` and `history` return it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Generation {
    pub id: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub commit: String,
    pub state: GenState,
    pub dir: PathBuf,
    pub staged_at: String,
    pub test: Option<TestInfo>,
    pub switched_at: Option<String>,
    pub confirmed_at: Option<String>,
    pub rolled_back_at: Option<String>,
    pub rollback_reason: Option<String>,
    pub failed_starts: u32,
    /// When `rollback --keep-storage` gave up this generation's failed restore; null otherwise.
    #[serde(default)]
    pub restore_abandoned_at: Option<String>,
}

impl Generation {
    /// The first eight characters of the commit.
    pub fn short_commit(&self) -> String {
        self.commit.chars().take(8).collect()
    }

    /// `ref@commit8`
    pub fn label(&self) -> String {
        format!("{}@{}", self.git_ref, self.short_commit())
    }
}

/// A switch that is live and not yet confirmed. `Status::step` says whether it still awaits its confirm: while it is
/// rolled back, or its restore failed, `pending` stays set and nothing can confirm it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub generation: String,
    pub previous: Option<String>,
    pub deadline: String,
}

/// Where the transaction in flight is. Clients read this, not the presence of `Status::pending`, to tell a switch that
/// awaits its confirm from one that is being rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The live app is being stopped and its storage snapshotted; the target has not started.
    Switching,
    /// The switch is live and waits for `confirm` until its deadline.
    Pending,
    /// The rollback is decided and being carried out; it can no longer be confirmed.
    RollingBack,
    /// The storage restore of a rollback failed: the app stays stopped until `rollback` retries it or
    /// `rollback --keep-storage` gives it up.
    RestoreFailed,
    /// A step a newer daemon reports that this client does not know. Decode-only.
    #[serde(other)]
    Unknown,
}

impl Step {
    /// The name on the wire, for messages and screens.
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Switching => "switching",
            Step::Pending => "pending",
            Step::RollingBack => "rolling_back",
            Step::RestoreFailed => "restore_failed",
            Step::Unknown => "unknown",
        }
    }
}

/// What an entry of the event log is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Daemon,
    Progress,
    Started,
    Healthy,
    Unhealthy,
    Exited,
    Failed,
    Stopped,
    Staged,
    Tested,
    TestFailed,
    Switched,
    Confirmed,
    RolledBack,
    Pruned,
    Error,
    /// A kind a newer daemon logs that this client does not know. Decode-only; a log written by a newer daemon
    /// replays with it.
    #[serde(other)]
    Unknown,
}

impl EventKind {
    /// The name on the wire, for logs and screens.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Daemon => "daemon",
            EventKind::Progress => "progress",
            EventKind::Started => "started",
            EventKind::Healthy => "healthy",
            EventKind::Unhealthy => "unhealthy",
            EventKind::Exited => "exited",
            EventKind::Failed => "failed",
            EventKind::Stopped => "stopped",
            EventKind::Staged => "staged",
            EventKind::Tested => "tested",
            EventKind::TestFailed => "test_failed",
            EventKind::Switched => "switched",
            EventKind::Confirmed => "confirmed",
            EventKind::RolledBack => "rolled_back",
            EventKind::Pruned => "pruned",
            EventKind::Error => "error",
            EventKind::Unknown => "unknown",
        }
    }
}

/// One entry of the event log (`events.jsonl`), and one `event` line of a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Daemon-wide, increasing; 0 for a line written before events were numbered.
    #[serde(default)]
    pub seq: u64,
    pub at: String,
    pub kind: EventKind,
    pub generation: Option<String>,
    pub message: String,
}

/// How the app's most recent process ended, and whether Fawkes asked for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastExit {
    pub at: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub requested: bool,
}

impl LastExit {
    pub fn new(status: Option<ExitStatus>, requested: bool) -> LastExit {
        LastExit {
            at: now(),
            code: status.and_then(|s| s.code()),
            signal: status.and_then(|s| s.signal()),
            requested,
        }
    }

    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exit status {code}"),
            (_, Some(signal)) => format!("signal {signal}"),
            _ => "unknown exit".into(),
        }
    }
}

// ------------------------------------------------------------ results

/// Whether the app's process is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Stopped,
    /// It crashed `max_failed_starts` times in a row and is no longer restarted.
    Failed,
    /// A state a newer daemon reports that this client does not know. Decode-only.
    #[serde(other)]
    Unknown,
}

impl ProcessState {
    /// The name on the wire, for screens.
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessState::Running => "running",
            ProcessState::Stopped => "stopped",
            ProcessState::Failed => "failed",
            ProcessState::Unknown => "unknown",
        }
    }
}

/// The app's process: the result of `start`, `stop` and `restart`, and the `process` of `status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Process {
    pub state: ProcessState,
    pub generation: Option<String>,
    pub pid: Option<u32>,
    pub port: u16,
    pub started_at: Option<String>,
    /// Seconds, null when not running.
    pub uptime: Option<f64>,
    pub restarts: u32,
    pub last_exit: Option<LastExit>,
    /// When the supervisor restarts a crashed app; null unless one is scheduled. `state` is `stopped` until then.
    pub restart_at: Option<String>,
}

/// The result of `status`, and the `status` line of a `subscribe` stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub daemon_pid: u32,
    pub permanent: Option<String>,
    pub current: Option<String>,
    /// The live switch, also while it is rolled back or its restore failed: read `step` to tell which.
    pub pending: Option<Pending>,
    /// The transaction in flight, null when none. Absent from an older daemon's status, which then has no way to say
    /// that `pending` is being rolled back.
    #[serde(default)]
    pub step: Option<Step>,
    /// The step in progress: a transaction step, else one that moves the live process (see DESIGN.md).
    pub busy: Option<String>,
    pub process: Process,
}

impl Status {
    /// The pending switch, only while it still awaits its confirm. A status without `step` is an older daemon's, which
    /// publishes `pending` for a switch that awaits its confirm and says nothing else.
    pub fn awaiting_confirm(&self) -> Option<&Pending> {
        match self.step {
            Some(Step::Pending) | None => self.pending.as_ref(),
            Some(_) => None,
        }
    }
}

/// The result of `history`, and the `history` line of a `subscribe` stream. Oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub generations: Vec<Generation>,
}

/// The result of `log`. Oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Events {
    pub events: Vec<Event>,
}

/// The result of `switch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchResult {
    pub generation: String,
    pub previous: Option<String>,
    pub deadline: String,
}

/// The result of `rollback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackResult {
    pub from: String,
    pub to: Option<String>,
    pub reason: String,
}

/// The result of `prune`: the generation ids removed (oldest first) and those kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneResult {
    pub removed: Vec<String>,
    pub kept: Vec<String>,
}

/// The result of any command, as the daemon sends it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Reply {
    Process(Process),
    Generation(Generation),
    Switch(SwitchResult),
    Rollback(RollbackResult),
    Prune(PruneResult),
    Status(Status),
    History(History),
    Events(Events),
    /// A stream (`log --follow`, `subscribe`) has ended: its result is null.
    Streamed,
}

macro_rules! reply_from {
    ($($variant:ident($type:ty)),* $(,)?) => {
        $(impl From<$type> for Reply {
            fn from(value: $type) -> Reply {
                Reply::$variant(value)
            }
        })*
    };
}

reply_from!(
    Process(Process),
    Generation(Generation),
    Switch(SwitchResult),
    Rollback(RollbackResult),
    Prune(PruneResult),
    Status(Status),
    History(History),
    Events(Events),
);

/// One line of a response. The daemon writes it with `T` the `Reply`; a client reads it with `T` a `Value` and
/// deserializes the result into the struct of the command it sent, so a result it cannot read can still be shown.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Line<T = Value> {
    /// A progress line or an event of a stream.
    Event {
        event: Event,
    },
    /// The first and every changed picture of a `subscribe` stream.
    Status {
        status: Box<Status>,
    },
    History {
        history: History,
    },
    /// The final line of a command that worked.
    Done {
        ok: bool,
        result: T,
    },
    /// The final line of a command that failed.
    Failed {
        ok: bool,
        error: Error,
    },
}

impl<T> Line<T> {
    pub fn done(result: T) -> Line<T> {
        Line::Done { ok: true, result }
    }

    pub fn failed(error: Error) -> Line<T> {
        Line::Failed { ok: false, error }
    }
}

// ----------------------------------------------------- connection

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
    pub fn line(&mut self, line: &impl Serialize) -> bool {
        if self.gone {
            return false;
        }
        // Nothing here can fail to serialize; a line that did would be skipped, not sent half.
        let Ok(mut bytes) = serde_json::to_vec(line) else {
            return true;
        };
        bytes.push(b'\n');
        if let Some(stream) = &mut self.stream {
            self.gone = stream.write_all(&bytes).is_err();
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
        self.line(&Line::<Reply>::Event {
            event: event.clone(),
        })
    }

    pub fn finish(&mut self, result: R<Reply>) {
        match result {
            Ok(result) => self.line(&Line::done(result)),
            Err(e) => self.line(&Line::<Reply>::failed(e)),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn parse(line: &str) -> R<Request> {
        Request::parse(line)
    }

    fn refused(line: &str) -> String {
        let e = parse(line).unwrap_err();
        assert_eq!(e.code, ErrorCode::Usage, "{line}");
        e.message
    }

    /// For a wire enum: the name of every variant, as a literal, once. The match has no wildcard, so a new variant
    /// does not compile until it is listed here, and the `ALL` list the tests walk is built from the same lines.
    macro_rules! wire_names {
        ($name_of:ident, $all:ident: $type:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
            fn $name_of(value: $type) -> &'static str {
                match value {
                    $($type::$variant => $name),+
                }
            }

            const $all: &[$type] = &[$($type::$variant),+];
        };
    }

    wire_names!(error_code_name, ERROR_CODES: ErrorCode {
        Usage => "usage",
        NotFound => "not_found",
        InvalidState => "invalid_state",
        Busy => "busy",
        GitFailed => "git_failed",
        InstallFailed => "install_failed",
        HealthFailed => "health_failed",
        StorageFailed => "storage_failed",
        StartFailed => "start_failed",
        PortInUse => "port_in_use",
        Internal => "internal",
        DaemonUnreachable => "daemon_unreachable",
        NoAnswer => "no_answer",
        Unknown => "unknown",
    });

    wire_names!(gen_state_name, GEN_STATES: GenState {
        Staging => "staging",
        Staged => "staged",
        TestFailed => "test_failed",
        Tested => "tested",
        Current => "current",
        Permanent => "permanent",
        Retired => "retired",
        RolledBack => "rolled_back",
        Unknown => "unknown",
    });

    wire_names!(event_kind_name, EVENT_KINDS: EventKind {
        Daemon => "daemon",
        Progress => "progress",
        Started => "started",
        Healthy => "healthy",
        Unhealthy => "unhealthy",
        Exited => "exited",
        Failed => "failed",
        Stopped => "stopped",
        Staged => "staged",
        Tested => "tested",
        TestFailed => "test_failed",
        Switched => "switched",
        Confirmed => "confirmed",
        RolledBack => "rolled_back",
        Pruned => "pruned",
        Error => "error",
        Unknown => "unknown",
    });

    wire_names!(process_state_name, PROCESS_STATES: ProcessState {
        Running => "running",
        Stopped => "stopped",
        Failed => "failed",
        Unknown => "unknown",
    });

    wire_names!(step_name, STEPS: Step {
        Switching => "switching",
        Pending => "pending",
        RollingBack => "rolling_back",
        RestoreFailed => "restore_failed",
        Unknown => "unknown",
    });

    #[test]
    fn a_request_parses_in_every_shape_clients_send() {
        // Commands without arguments: `args` absent, null, empty, or carrying keys nobody reads.
        for (name, request) in [
            ("start", Request::Start),
            ("stop", Request::Stop),
            ("restart", Request::Restart),
            ("prune", Request::Prune),
            ("status", Request::Status),
            ("history", Request::History),
        ] {
            for args in ["", r#","args":null"#, r#","args":{}"#, r#","args":{"x":1}"#] {
                let line = format!(r#"{{"command":"{name}"{args}}}"#);
                assert_eq!(parse(&line).unwrap(), request, "{line}");
            }
        }
        // `keep_storage` absent, null or false is the ordinary rollback.
        for (args, keep_storage) in [
            ("", false),
            (r#","args":{"keep_storage":null}"#, false),
            (r#","args":{"keep_storage":false}"#, false),
            (r#","args":{"keep_storage":true}"#, true),
        ] {
            let line = format!(r#"{{"command":"rollback"{args}}}"#);
            assert_eq!(
                parse(&line).unwrap(),
                Request::Rollback { keep_storage },
                "{line}"
            );
        }
        let stage = Request::Stage {
            git_ref: "main".into(),
        };
        assert_eq!(
            parse(r#"{"command":"stage","args":{"ref":"main","x":[]}}"#).unwrap(),
            stage
        );
        assert_eq!(
            parse(r#"{"command":"test","args":{"generation":"g2"}}"#).unwrap(),
            Request::Test {
                generation: "g2".into()
            }
        );
        for (args, deadline) in [
            (r#"{"generation":"g3"}"#, None),
            (r#"{"generation":"g3","deadline":null}"#, None),
            (r#"{"generation":"g3","deadline":60}"#, Some(60)),
            (
                r#"{"generation":"g3","deadline":604800}"#,
                Some(MAX_DEADLINE),
            ),
        ] {
            assert_eq!(
                parse(&format!(r#"{{"command":"switch","args":{args}}}"#)).unwrap(),
                Request::Switch {
                    generation: "g3".into(),
                    deadline
                },
                "{args}"
            );
        }
        for (args, generation) in [
            ("", None),
            (r#","args":{}"#, None),
            (r#","args":{"generation":null}"#, None),
            (r#","args":{"generation":"g3"}"#, Some("g3".to_string())),
        ] {
            assert_eq!(
                parse(&format!(r#"{{"command":"confirm"{args}}}"#)).unwrap(),
                Request::Confirm { generation }
            );
        }
        for (args, limit, follow) in [
            (r"{}", None, false),
            (r#"{"limit":null,"follow":false}"#, None, false),
            (r#"{"limit":5,"follow":true}"#, Some(5), true),
        ] {
            assert_eq!(
                parse(&format!(r#"{{"command":"log","args":{args}}}"#)).unwrap(),
                Request::Log { limit, follow }
            );
        }
        assert_eq!(
            parse(r#"{"command":"subscribe","args":{"limit":100}}"#).unwrap(),
            Request::Subscribe {
                limit: Some(100),
                since: None
            }
        );
        assert_eq!(
            parse(r#"{"command":"subscribe","args":{"since":42,"limit":null}}"#).unwrap(),
            Request::Subscribe {
                limit: None,
                since: Some(42)
            }
        );
        assert_eq!(
            parse(r#"{"command":"subscribe"}"#).unwrap(),
            Request::Subscribe {
                limit: None,
                since: None
            }
        );
    }

    #[test]
    fn a_bad_request_is_a_usage_error_with_its_message() {
        assert_eq!(refused(r#"{"command":"stage"}"#), "missing argument `ref`");
        assert_eq!(
            refused(r#"{"command":"stage","args":{"ref":null}}"#),
            "missing argument `ref`"
        );
        assert_eq!(
            refused(r#"{"command":"test","args":{}}"#),
            "missing argument `generation`"
        );
        assert_eq!(
            refused(r#"{"command":"switch","args":{"deadline":5}}"#),
            "missing argument `generation`"
        );
        assert_eq!(
            refused(r#"{"command":"frobnicate"}"#),
            "unknown command `frobnicate`"
        );
        assert!(refused("{").starts_with("malformed request: "));
        assert!(refused(r#"{"args":{}}"#).starts_with("malformed request: "));
        for bad in ["0", "604801", "-1", "1.5", r#""soon""#] {
            assert_eq!(
                refused(&format!(
                    r#"{{"command":"switch","args":{{"generation":"g3","deadline":{bad}}}}}"#
                )),
                format!(
                    "the deadline must be whole seconds from 1 to {MAX_DEADLINE} (a week), not {bad}"
                )
            );
        }
        // A present argument of the wrong type is refused instead of read as absent.
        for (request, says) in [
            (
                r#"{"command":"confirm","args":{"generation":7}}"#,
                "the argument `generation` must be a string, not 7",
            ),
            (
                r#"{"command":"stage","args":{"ref":["main"]}}"#,
                r#"the argument `ref` must be a string, not ["main"]"#,
            ),
            (
                r#"{"command":"rollback","args":{"keep_storage":"true"}}"#,
                r#"the argument `keep_storage` must be true or false, not "true""#,
            ),
            (
                r#"{"command":"log","args":{"limit":-1}}"#,
                "the argument `limit` must be a non-negative whole number, not -1",
            ),
            (
                r#"{"command":"log","args":{"follow":1}}"#,
                "the argument `follow` must be true or false, not 1",
            ),
            (
                r#"{"command":"subscribe","args":{"limit":"5"}}"#,
                r#"the argument `limit` must be a non-negative whole number, not "5""#,
            ),
            (
                r#"{"command":"subscribe","args":{"since":1.5}}"#,
                "the argument `since` must be a non-negative whole number, not 1.5",
            ),
        ] {
            assert_eq!(refused(request), says, "{request}");
        }
        // Null still means absent, and keys no command uses are ignored.
        assert_eq!(
            Request::parse(r#"{"command":"confirm","args":{"generation":null,"other":[1]}}"#)
                .unwrap(),
            Request::Confirm { generation: None }
        );
        assert_eq!(
            Request::parse(r#"{"command":"log","args":{"limit":null,"follow":null}}"#).unwrap(),
            Request::Log {
                limit: None,
                follow: false
            }
        );
    }

    #[test]
    fn a_request_serializes_to_the_envelope_clients_send() {
        let wire = |request: Request| serde_json::to_string(&request).unwrap();
        assert_eq!(
            wire(Request::Stage {
                git_ref: "main".into()
            }),
            r#"{"command":"stage","args":{"ref":"main"}}"#
        );
        assert_eq!(wire(Request::Status), r#"{"command":"status","args":{}}"#);
        assert_eq!(
            wire(Request::Switch {
                generation: "g3".into(),
                deadline: None
            }),
            r#"{"command":"switch","args":{"deadline":null,"generation":"g3"}}"#
        );
        assert_eq!(
            wire(Request::Log {
                limit: Some(5),
                follow: false
            }),
            r#"{"command":"log","args":{"follow":false,"limit":5}}"#
        );
        // What is written is read back as the same request.
        for request in [
            Request::Confirm { generation: None },
            Request::Subscribe {
                limit: Some(100),
                since: Some(7),
            },
            Request::Rollback { keep_storage: true },
        ] {
            assert_eq!(parse(&wire(request.clone())).unwrap(), request);
        }
    }

    #[test]
    fn every_error_code_keeps_its_string() {
        for &code in ERROR_CODES {
            let name = error_code_name(code);
            assert_eq!(serde_json::to_value(code).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<ErrorCode>(json!(name)).unwrap(),
                code
            );
        }
        // A code from a newer daemon still reads, as a failure.
        let line: Line = serde_json::from_str(
            r#"{"ok":false,"error":{"code":"from_the_future","message":"m"}}"#,
        )
        .unwrap();
        assert!(matches!(
            line,
            Line::Failed { error, .. } if error.code == ErrorCode::Unknown && error.message == "m"
        ));
    }

    /// A daemon outlives the binary that was upgraded under it, so a client meets values it does not know. The line
    /// must still decode: a dropped status or history leaves a stale picture, and a dropped event a gap.
    #[test]
    fn lines_with_values_from_a_newer_daemon_still_decode() {
        let status = r#"{"status":{"name":"s","daemon_pid":1,"permanent":null,"current":null,"pending":null,"step":"from_the_future","busy":null,"process":{"state":"paused","generation":null,"pid":null,"port":4790,"started_at":null,"uptime":null,"restarts":0,"last_exit":null,"restart_at":null}}}"#;
        let Line::Status { status } = serde_json::from_str::<Line>(status).unwrap() else {
            panic!("a status line reads as one");
        };
        assert_eq!(status.process.state, ProcessState::Unknown);
        assert_eq!(status.step, Some(Step::Unknown));

        let history = r#"{"history":{"generations":[{"id":"g1","ref":"main","commit":"0","state":"quarantined","dir":"/d","staged_at":"t","test":null,"switched_at":null,"confirmed_at":null,"rolled_back_at":null,"rollback_reason":null,"failed_starts":0}]}}"#;
        let Line::History { history } = serde_json::from_str::<Line>(history).unwrap() else {
            panic!("a history line reads as one");
        };
        assert_eq!(history.generations[0].state, GenState::Unknown);

        let event = r#"{"event":{"seq":7,"at":"t","kind":"from_the_future","generation":null,"message":"m"}}"#;
        let Line::Event { event } = serde_json::from_str::<Line>(event).unwrap() else {
            panic!("an event line reads as one");
        };
        assert_eq!((event.seq, event.kind), (7, EventKind::Unknown));
    }

    #[test]
    fn names_shown_to_people_are_the_names_on_the_wire() {
        fn wire(value: &impl Serialize) -> Value {
            serde_json::to_value(value).unwrap()
        }
        for &state in GEN_STATES {
            assert_eq!(wire(&state), json!(gen_state_name(state)));
            assert_eq!(state.as_str(), gen_state_name(state));
        }
        for &kind in EVENT_KINDS {
            assert_eq!(wire(&kind), json!(event_kind_name(kind)));
            assert_eq!(kind.as_str(), event_kind_name(kind));
        }
        for &state in PROCESS_STATES {
            assert_eq!(wire(&state), json!(process_state_name(state)));
            assert_eq!(state.as_str(), process_state_name(state));
        }
        for &step in STEPS {
            assert_eq!(wire(&step), json!(step_name(step)));
            assert_eq!(step.as_str(), step_name(step));
        }
    }

    #[test]
    fn a_status_line_has_the_fields_of_the_contract_in_order() {
        let status = Status {
            name: "sample".into(),
            daemon_pid: 812,
            permanent: Some("g2".into()),
            current: Some("g3".into()),
            pending: Some(Pending {
                generation: "g3".into(),
                previous: Some("g2".into()),
                deadline: "t".into(),
            }),
            step: Some(Step::Pending),
            busy: None,
            process: Process {
                state: ProcessState::Running,
                generation: Some("g3".into()),
                pid: Some(9001),
                port: 4790,
                started_at: Some("t".into()),
                uptime: Some(42.5),
                restarts: 1,
                last_exit: Some(LastExit {
                    at: "t".into(),
                    code: None,
                    signal: Some(15),
                    requested: true,
                }),
                restart_at: None,
            },
        };
        let text = serde_json::to_string(&Line::<Reply>::Status {
            status: Box::new(status),
        })
        .unwrap();
        assert_eq!(
            text,
            r#"{"status":{"name":"sample","daemon_pid":812,"permanent":"g2","current":"g3","pending":{"generation":"g3","previous":"g2","deadline":"t"},"step":"pending","busy":null,"process":{"state":"running","generation":"g3","pid":9001,"port":4790,"started_at":"t","uptime":42.5,"restarts":1,"last_exit":{"at":"t","code":null,"signal":15,"requested":true},"restart_at":null}}}"#
        );
        let Line::Status { status } = serde_json::from_str::<Line>(&text).unwrap() else {
            panic!("a status line reads as one");
        };
        assert_eq!(status.process.port, 4790);
        assert_eq!(status.step, Some(Step::Pending));
        // An older daemon's status has no `step`.
        let older = text.replace(r#""step":"pending","#, "");
        let Line::Status { status } = serde_json::from_str::<Line>(&older).unwrap() else {
            panic!("a status line reads as one");
        };
        assert!(status.step.is_none() && status.pending.is_some());
        // Final lines: a stream ends with a null result, a failure carries its error.
        assert_eq!(
            serde_json::to_string(&Line::done(Reply::Streamed)).unwrap(),
            r#"{"ok":true,"result":null}"#
        );
        assert_eq!(
            serde_json::to_string(&Line::<Reply>::failed(err(ErrorCode::Busy, "m"))).unwrap(),
            r#"{"ok":false,"error":{"code":"busy","message":"m"}}"#
        );
    }

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
