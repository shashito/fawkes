//! `fawkes`: the daemon (`fawkes daemon`) and the thin command-line client of its socket.

mod daemon;
mod manifest;
mod process;
mod protocol;
mod state;
mod storage;
mod tui;

use clap::{Parser, Subcommand};
use manifest::Manifest;
use protocol::{
    ErrorCode, Event, Events, Generation, History, Line, Process, ProcessState, PruneResult,
    REQUEST_TIMEOUT, Request, RollbackResult, Status, Step, SwitchResult, err, timed_out,
};
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Display;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 daemon unreachable.
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_UNREACHABLE: u8 = 3;

/// A process supervisor whose upgrades are transactions: stage, test, switch, confirm, or roll back.
#[derive(Parser)]
#[command(
    name = "fawkes",
    disable_help_subcommand = true,
    after_help = "Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 no daemon answers."
)]
struct Cli {
    /// Print the result (or the error) as one JSON document, and progress as JSON lines on stderr; `log --follow` prints
    /// one event document per line. Not for `daemon` and `tui`
    #[arg(long, global = true)]
    json: bool,
    /// The app's manifest [default: fawkes.toml]
    #[arg(long, global = true, value_name = "FILE")]
    manifest: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "The daemon's socket [default: $FAWKES_SOCKET, else the manifest's]"
    )]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// What the command line asks for: one of the two local programs, or a request to a daemon.
#[derive(Subcommand)]
enum Command {
    /// Run the supervisor in the foreground
    Daemon,
    /// Run the current generation (pending, else permanent)
    Start,
    /// Stop the app
    Stop,
    /// Stop the app and start the current generation again
    Restart,
    /// Check out and install a git ref as a new generation
    Stage {
        #[arg(value_name = "REF")]
        git_ref: String,
    },
    /// Start a generation on a spare port with a copy of the storage
    Test {
        #[arg(value_name = "GEN")]
        generation: String,
    },
    /// Go live; roll back unless confirmed in time
    Switch {
        #[arg(value_name = "GEN")]
        generation: String,
        /// Seconds to confirm in, 1 to 604800 [default: the manifest's `deadline`]
        #[arg(long, value_name = "SECS")]
        deadline: Option<u64>,
    },
    /// Make the pending generation permanent
    Confirm {
        #[arg(
            value_name = "GEN",
            help = "The generation that vouches for itself [default: $FAWKES_GENERATION, else the pending one]"
        )]
        generation: Option<String>,
    },
    /// Undo the pending switch now
    Rollback {
        /// Give up a restore that failed: the permanent generation then runs on storage the failed generation may
        /// already have migrated
        #[arg(long)]
        keep_storage: bool,
    },
    /// Remove old generations beyond the manifest's `keep`
    Prune,
    /// Show the app, its process and the pending switch
    Status,
    /// Show the generations
    History,
    /// Show the event log
    Log {
        /// Show the last N events
        #[arg(short = 'n', value_name = "N")]
        limit: Option<u64>,
        /// Keep printing events as they happen
        #[arg(long)]
        follow: bool,
    },
    /// Full-screen live view and controls
    Tui,
}

impl Command {
    /// The request this command sends to the daemon; `daemon` and `tui` send none. `own_generation` is
    /// `$FAWKES_GENERATION`, the generation a process Fawkes started runs as: a bare `confirm` from it names that
    /// generation, so an app's test run (or anything it left behind) cannot confirm another generation that happens to
    /// be pending.
    fn request(&self, own_generation: Option<&str>) -> Option<Request> {
        Some(match self {
            Command::Daemon | Command::Tui => return None,
            Command::Start => Request::Start,
            Command::Stop => Request::Stop,
            Command::Restart => Request::Restart,
            Command::Stage { git_ref } => Request::Stage {
                git_ref: git_ref.clone(),
            },
            Command::Test { generation } => Request::Test {
                generation: generation.clone(),
            },
            Command::Switch {
                generation,
                deadline,
            } => Request::Switch {
                generation: generation.clone(),
                deadline: *deadline,
            },
            Command::Confirm { generation } => Request::Confirm {
                generation: generation
                    .as_deref()
                    .or(own_generation.filter(|id| !id.is_empty()))
                    .map(String::from),
            },
            Command::Rollback { keep_storage } => Request::Rollback {
                keep_storage: *keep_storage,
            },
            Command::Prune => Request::Prune,
            Command::Status => Request::Status,
            Command::History => Request::History,
            Command::Log { limit, follow } => Request::Log {
                limit: *limit,
                follow: *follow,
            },
        })
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // Nothing is left to report to if the terminal is gone.
            let _ = error.print();
            // `--help` is the one "error" that is not one.
            return if error.use_stderr() {
                ExitCode::from(EXIT_USAGE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let manifest_path = cli.manifest.clone().unwrap_or_else(|| "fawkes.toml".into());
    if matches!(cli.command, Command::Daemon) {
        // A manifest that cannot be loaded is a usage error here as for every other command; the rest of the
        // daemon's startup failures are not.
        if let Err(message) = Manifest::load(&manifest_path) {
            warn(format!("error: {message}"));
            return ExitCode::from(EXIT_USAGE);
        }
        return match daemon::Daemon::run(manifest_path, cli.socket) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                warn(format!("error: {message}"));
                ExitCode::from(EXIT_FAILED)
            }
        };
    }
    if matches!(cli.command, Command::Tui)
        && cli.socket.is_none()
        && cli.manifest.is_none()
        && std::env::var_os("FAWKES_SOCKET").is_none()
    {
        let run = match manifest::run_dir() {
            Ok(run) => run,
            Err(message) => {
                warn(format!("error: {message}"));
                return ExitCode::from(EXIT_USAGE);
            }
        };
        return ExitCode::from(tui::run(&tui::Target::Discover(run)));
    }
    let mut stdout = std::io::stdout().lock();
    let socket = match socket_path(&cli, &manifest_path) {
        Ok(socket) => socket,
        Err(message) => {
            return ExitCode::from(fail(cli.json, &mut stdout, ErrorCode::Usage, message));
        }
    };
    // `daemon` returned above, so what is left but a request is `tui`.
    // Only towards the daemon that started the caller: generation ids repeat across apps, so another daemon, picked
    // with `--socket`, may have a pending generation of the same name.
    let own_generation = std::env::var("FAWKES_GENERATION")
        .ok()
        .filter(|_| cli.socket.is_none());
    let Some(request) = cli.command.request(own_generation.as_deref()) else {
        return ExitCode::from(tui::run(&tui::Target::One(socket)));
    };
    ExitCode::from(client(
        &cli,
        &request,
        &socket,
        &manifest_path,
        REQUEST_TIMEOUT,
        &mut stdout,
    ))
}

/// `--socket`, else `FAWKES_SOCKET`, else the manifest's `<state_dir>/fawkes.sock`.
fn socket_path(cli: &Cli, manifest_path: &Path) -> Result<PathBuf, String> {
    if let Some(socket) = &cli.socket {
        return Ok(socket.clone());
    }
    if let Some(socket) = std::env::var_os("FAWKES_SOCKET") {
        return Ok(socket.into());
    }
    let manifest = Manifest::load(manifest_path)
        .map_err(|e| format!("{e}; pass --manifest or --socket to say which daemon to talk to"))?;
    Ok(manifest.paths().socket())
}

/// Print a line on `out`, the client's stdout; false when it could not be written. A reader that has gone
/// (`fawkes log --follow | head`) ends a followed log, but changes nothing about how a command went: the daemon's
/// answer decides its exit code.
fn say(out: &mut dyn Write, line: impl Display) -> bool {
    writeln!(out, "{line}").is_ok()
}

/// Print a line on stderr, best effort: a terminal that has gone is no reason to exit otherwise.
fn warn(line: impl Display) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

/// The exit code of a failed command, from its error code.
fn exit_code(code: ErrorCode) -> u8 {
    match code {
        ErrorCode::Usage => EXIT_USAGE,
        ErrorCode::DaemonUnreachable => EXIT_UNREACHABLE,
        _ => EXIT_FAILED,
    }
}

/// Report a failure of the client itself, the one way every such failure goes out: the `{"code", "message"}` error
/// object on `out` under `--json` (the document an agent parses), `error: …` on stderr otherwise. Returns the exit
/// code.
fn fail(json: bool, out: &mut dyn Write, code: ErrorCode, message: String) -> u8 {
    if json {
        say(
            out,
            serde_json::to_string(&err(code, message)).unwrap_or_default(),
        );
    } else {
        warn(format!("error: {message}"));
    }
    exit_code(code)
}

/// Send one request, show progress, render the answer on `out`. Returns the exit code. A quick command waits
/// `timeout` for each line of the answer.
fn client(
    cli: &Cli,
    request: &Request,
    socket: &Path,
    manifest_path: &Path,
    timeout: Duration,
    out: &mut dyn Write,
) -> u8 {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        let message = format!(
            "no daemon answers on {}; start one with `fawkes daemon --manifest {}`",
            socket.display(),
            manifest_path.display()
        );
        return fail(cli.json, out, ErrorCode::DaemonUnreachable, message);
    };
    if request.write_to(&mut stream).is_err() {
        let message = format!(
            "the daemon on {} closed the connection before the request was sent; check that it is running with `fawkes status`",
            socket.display()
        );
        return fail(cli.json, out, ErrorCode::DaemonUnreachable, message);
    }
    let follow = matches!(request, Request::Log { follow: true, .. });
    // A daemon that accepts and then never answers must not hang a quick command (`Request::is_quick`).
    if request.is_quick()
        && let Err(e) = stream.set_read_timeout(Some(timeout))
    {
        let message = format!("cannot set a read timeout on the connection: {e}");
        return fail(cli.json, out, ErrorCode::Internal, message);
    }
    for line in BufReader::new(stream).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) if timed_out(&e) => {
                let message = format!(
                    "the daemon on {} did not answer in time; check that it is healthy with `fawkes status`",
                    socket.display()
                );
                return fail(cli.json, out, ErrorCode::NoAnswer, message);
            }
            Err(_) => break,
        };
        // `--json` shows what the daemon sent, not what this client understood of it.
        let Ok(raw) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Ok(message) = Line::<Value>::deserialize(&raw) else {
            continue;
        };
        match message {
            Line::Event { event } => {
                let shown = match (follow, cli.json) {
                    (true, true) => say(out, &raw["event"]),
                    (true, false) => say(out, render_event(&event)),
                    (false, true) => {
                        warn(&raw["event"]);
                        true
                    }
                    (false, false) => {
                        warn(render_progress(&event));
                        true
                    }
                };
                if !shown {
                    return 0;
                }
            }
            // Only `subscribe` sends these.
            Line::Status { .. } | Line::History { .. } => {}
            // A followed log ends with a null result when the daemon ends the stream; the events were the output.
            Line::Done { .. } if follow => return 0,
            Line::Done { result, .. } => {
                if cli.json {
                    say(out, result);
                } else {
                    say(out, render_result(request, &result));
                }
                return 0;
            }
            Line::Failed { error, .. } => {
                if cli.json {
                    say(out, &raw["error"]);
                } else {
                    warn(format!("error: {}", error.message));
                }
                // The daemon checks some arguments itself (a `--deadline` out of range): still a usage error.
                return if error.code == ErrorCode::Usage {
                    EXIT_USAGE
                } else {
                    EXIT_FAILED
                };
            }
        }
    }
    if follow {
        return 0;
    }
    let message = format!(
        "the daemon on {} closed the connection without an answer; `fawkes status` shows whether it is running",
        socket.display()
    );
    fail(cli.json, out, ErrorCode::DaemonUnreachable, message)
}

// ----------------------------------------------------------- rendering

/// An optional field as text: `-` for null.
fn or_dash(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "-".into(), |v| v.to_string())
}

fn render_progress(event: &Event) -> String {
    format!(
        "{}: {}",
        or_dash(event.generation.as_deref()),
        event.message
    )
}

fn render_event(event: &Event) -> String {
    format!(
        "{}  {:<11} {:<4} {}",
        event.at,
        event.kind.as_str(),
        or_dash(event.generation.as_deref()),
        event.message
    )
}

fn render_generation(g: &Generation) -> String {
    format!(
        "{:<4} {:<12} {} @ {}",
        g.id,
        g.state.as_str(),
        g.git_ref,
        g.short_commit()
    )
}

fn render_process(p: &Process) -> String {
    let mut line = format!("{}  {}", p.state.as_str(), or_dash(p.generation.as_deref()));
    if p.state == ProcessState::Running {
        line = format!(
            "{line}  pid {}  port {}  up {}s",
            or_dash(p.pid),
            p.port,
            or_dash(p.uptime.map(|seconds| format!("{seconds:?}")))
        );
    }
    line = format!("{line}  restarts {}", p.restarts);
    if let Some(at) = &p.restart_at {
        line = format!("{line}  restart at {at}");
    }
    if let Some(exit) = &p.last_exit {
        let how = match (exit.code, exit.signal) {
            (Some(code), _) => format!("exit {code}"),
            (_, Some(signal)) => format!("signal {signal}"),
            _ => "unknown".into(),
        };
        let who = if exit.requested {
            "requested"
        } else {
            "unrequested"
        };
        line = format!("{line}\nlast exit: {how} ({who}) at {}", exit.at);
    }
    line
}

/// `pending` is also set while the switch is rolled back, so a confirm is asked for only when `step` allows one.
fn render_status(s: &Status) -> String {
    let note = if let Some(p) = s.awaiting_confirm() {
        format!("  (waiting for confirm until {})", p.deadline)
    } else {
        let switch = s.pending.as_ref();
        let generation = switch.map_or("the switch", |p| p.generation.as_str());
        match (s.step, switch) {
            (Some(Step::RollingBack), Some(p)) => format!(
                "  (rolling back {} to {})",
                p.generation,
                or_dash(p.previous.as_deref())
            ),
            (Some(Step::RestoreFailed), _) => format!(
                "\n{generation}'s storage could not be restored and the app is stopped; `fawkes rollback` retries it, `fawkes rollback --keep-storage` gives it up"
            ),
            (Some(step @ (Step::Switching | Step::Unknown)), _) => {
                format!("  (step: {})", step.as_str())
            }
            _ => String::new(),
        }
    };
    format!(
        "{}  daemon pid {}  busy: {}\npermanent {}  current {}{}\nprocess {}",
        s.name,
        s.daemon_pid,
        or_dash(s.busy.as_deref()),
        or_dash(s.permanent.as_deref()),
        or_dash(s.current.as_deref()),
        note,
        render_process(&s.process)
    )
}

/// The answer to `request` as text. A result this client cannot read (a newer daemon's) is shown as the JSON it is.
fn render_result(request: &Request, result: &Value) -> String {
    fn typed<T: serde::de::DeserializeOwned>(
        result: &Value,
        render: impl FnOnce(T) -> String,
    ) -> String {
        T::deserialize(result).map_or_else(|_| result.to_string(), render)
    }
    match request {
        Request::Stage { .. } => typed(result, |g: Generation| {
            format!("staged {} ({} @ {})", g.id, g.git_ref, g.short_commit())
        }),
        Request::Test { .. } => typed(result, |g: Generation| {
            format!(
                "{} passed its test on port {} in {}s; `fawkes switch {}` goes live",
                g.id,
                or_dash(g.test.as_ref().map(|t| t.port)),
                or_dash(g.test.as_ref().map(|t| format!("{:?}", t.seconds))),
                g.id
            )
        }),
        Request::Switch { .. } => typed(result, |r: SwitchResult| {
            format!(
                "switched to {} (previous: {}); confirm before {} or it rolls back",
                r.generation,
                or_dash(r.previous),
                r.deadline
            )
        }),
        Request::Confirm { .. } => {
            typed(result, |g: Generation| format!("{} is now permanent", g.id))
        }
        Request::Rollback { keep_storage } => typed(result, |r: RollbackResult| {
            format!(
                "rolled back {}; now running {} ({}){}",
                r.from,
                or_dash(r.to),
                r.reason,
                if *keep_storage {
                    "; storage kept as it was"
                } else {
                    ""
                }
            )
        }),
        Request::Prune => typed(result, |r: PruneResult| {
            if r.removed.is_empty() {
                "nothing to remove".into()
            } else {
                format!(
                    "removed {}; kept {}",
                    r.removed.join(", "),
                    r.kept.join(", ")
                )
            }
        }),
        Request::Start | Request::Stop | Request::Restart => {
            typed(result, |p: Process| render_process(&p))
        }
        Request::Status => typed(result, |s: Status| render_status(&s)),
        Request::History => typed(result, |h: History| {
            h.generations
                .iter()
                .map(|g| {
                    let reason = g
                        .rollback_reason
                        .as_ref()
                        .map(|s| format!("  ({s})"))
                        .unwrap_or_default();
                    format!("{}{reason}", render_generation(g))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        Request::Log { .. } => typed(result, |e: Events| {
            e.events
                .iter()
                .map(render_event)
                .collect::<Vec<_>>()
                .join("\n")
        }),
        Request::Subscribe { .. } => result.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Pending;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn parse(args: &str) -> Cli {
        Cli::try_parse_from(["fawkes"].into_iter().chain(args.split_whitespace()))
            .unwrap_or_else(|e| panic!("`{args}`: {e}"))
    }

    #[test]
    fn global_options_go_before_or_after_the_command() {
        for args in [
            "--json --manifest f.toml --socket s.sock status",
            "status --json --manifest f.toml --socket s.sock",
            "--socket s.sock status --manifest f.toml --json",
        ] {
            let cli = parse(args);
            assert!(cli.json, "{args}");
            assert_eq!(cli.manifest, Some("f.toml".into()), "{args}");
            assert_eq!(cli.socket, Some("s.sock".into()), "{args}");
            assert_eq!(cli.command.request(None), Some(Request::Status), "{args}");
        }
        let daemon = parse("daemon --manifest f.toml");
        assert!(matches!(daemon.command, Command::Daemon));
        assert_eq!(daemon.manifest, Some("f.toml".into()));
    }

    #[test]
    fn each_command_sends_its_request() {
        for (args, request) in [
            (
                "switch g3 --deadline 60",
                Request::Switch {
                    generation: "g3".into(),
                    deadline: Some(60),
                },
            ),
            (
                "log -n 5 --follow",
                Request::Log {
                    limit: Some(5),
                    follow: true,
                },
            ),
            ("confirm", Request::Confirm { generation: None }),
            (
                "confirm g2",
                Request::Confirm {
                    generation: Some("g2".into()),
                },
            ),
            (
                "rollback --keep-storage",
                Request::Rollback { keep_storage: true },
            ),
            // `help` is no command, so it can name a git ref.
            (
                "stage help",
                Request::Stage {
                    git_ref: "help".into(),
                },
            ),
        ] {
            assert_eq!(parse(args).command.request(None), Some(request), "{args}");
        }
        assert_eq!(parse("tui").command.request(None), None);
    }

    #[test]
    fn a_bare_confirm_names_the_generation_the_caller_runs_as() {
        let confirm = |args: &str, own: Option<&str>| match parse(args).command.request(own) {
            Some(Request::Confirm { generation }) => generation,
            other => panic!("{other:?}"),
        };
        assert_eq!(confirm("confirm", Some("g3")).as_deref(), Some("g3"));
        assert_eq!(confirm("confirm g2", Some("g3")).as_deref(), Some("g2"));
        assert_eq!(confirm("confirm", Some("")), None);
        assert_eq!(confirm("confirm", None), None);
    }

    fn status_in(step: Option<Step>, with_pending: bool) -> Status {
        Status {
            name: "sample".into(),
            daemon_pid: 812,
            permanent: Some("g1".into()),
            current: Some("g2".into()),
            pending: with_pending.then(|| Pending {
                generation: "g2".into(),
                previous: Some("g1".into()),
                deadline: "2030-01-01T00:00:00Z".into(),
            }),
            step,
            busy: None,
            process: Process {
                state: ProcessState::Stopped,
                generation: Some("g2".into()),
                pid: None,
                port: 4790,
                started_at: None,
                uptime: None,
                restarts: 0,
                last_exit: None,
                restart_at: None,
            },
        }
    }

    /// `pending` stays set during a rollback, so only `step` says whether a confirm is still possible.
    #[test]
    fn status_text_asks_for_a_confirm_only_while_the_switch_awaits_one() {
        let text = |step, with_pending| render_status(&status_in(step, with_pending));
        let waits = "waiting for confirm until 2030-01-01T00:00:00Z";
        assert!(text(Some(Step::Pending), true).contains(waits));
        // An older daemon sends no step.
        assert!(text(None, true).contains(waits));
        assert!(!text(None, false).contains("confirm"));

        let rolling_back = text(Some(Step::RollingBack), true);
        assert!(!rolling_back.contains("confirm"), "{rolling_back}");
        assert!(
            rolling_back.contains("rolling back g2 to g1"),
            "{rolling_back}"
        );

        let failed = text(Some(Step::RestoreFailed), true);
        assert!(!failed.contains("confirm"), "{failed}");
        assert!(
            failed.contains("g2's storage could not be restored")
                && failed.contains("`fawkes rollback` retries it")
                && failed.contains("`fawkes rollback --keep-storage` gives it up"),
            "{failed}"
        );
    }

    /// Run `status` against a daemon that accepts and then says nothing for 3 s; returns the exit code, what the
    /// client printed on stdout, and how long it waited.
    fn status_from_a_silent_daemon(json: bool) -> (u8, String, Duration) {
        // Under /tmp: a socket path over about 100 bytes does not bind, and `$TMPDIR` can be that long.
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("fawkes.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accepts and keeps the connection open without a word.
        let silent = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(stream);
        });
        let cli = Cli {
            json,
            manifest: None,
            socket: Some(socket.clone()),
            command: Command::Status,
        };

        let mut out = Vec::new();
        let started = Instant::now();
        let code = client(
            &cli,
            &Request::Status,
            &socket,
            Path::new("fawkes.toml"),
            Duration::from_millis(300),
            &mut out,
        );
        let waited = started.elapsed();
        silent.join().unwrap();
        (code, String::from_utf8(out).unwrap(), waited)
    }

    #[test]
    fn a_quick_command_gives_up_on_a_daemon_that_never_answers() {
        let (code, out, waited) = status_from_a_silent_daemon(false);

        assert_eq!(code, EXIT_FAILED);
        // The message goes to stderr, not stdout.
        assert_eq!(out, "");
        // The read timed out; the daemon had not closed the connection yet.
        assert!(
            waited >= Duration::from_millis(300),
            "gave up after {waited:?}"
        );
        assert!(waited < Duration::from_secs(2), "hung for {waited:?}");
    }

    #[test]
    fn under_json_a_daemon_that_never_answers_gives_a_no_answer_error_object() {
        let (code, out, _) = status_from_a_silent_daemon(true);

        assert_eq!(code, EXIT_FAILED);
        let error: Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(error["code"], "no_answer", "{out}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("did not answer in time"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 1, "{out}");
    }

    /// An event of a kind a newer daemon logs reaches `--json` as the line the daemon sent.
    #[test]
    fn a_followed_log_under_json_prints_an_event_of_an_unknown_kind_as_sent() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("fawkes.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let sent = r#"{"seq":3,"at":"t","kind":"from_the_future","generation":null,"message":"m","extra":1}"#;
        let daemon = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            writeln!(stream, r#"{{"event":{sent}}}"#).unwrap();
            writeln!(stream, r#"{{"ok":true,"result":null}}"#).unwrap();
        });
        let cli = Cli {
            json: true,
            manifest: None,
            socket: Some(socket.clone()),
            command: Command::Status,
        };
        let request = Request::Log {
            limit: None,
            follow: true,
        };
        let mut out = Vec::new();
        let code = client(
            &cli,
            &request,
            &socket,
            Path::new("fawkes.toml"),
            Duration::from_secs(5),
            &mut out,
        );
        daemon.join().unwrap();
        assert_eq!(code, 0);
        let printed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed, serde_json::from_str::<Value>(sent).unwrap());
    }
}
