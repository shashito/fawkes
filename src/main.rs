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
    REQUEST_TIMEOUT, Request, RollbackResult, Status, SwitchResult, err, timed_out,
};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader};
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
    /// Print the result as one JSON document, and progress as JSON lines on stderr
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
        #[arg(value_name = "GEN")]
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
    /// The request this command sends to the daemon; `daemon` and `tui` send none.
    fn request(&self) -> Option<Request> {
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
                generation: generation.clone(),
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
        return match daemon::Daemon::run(manifest_path, cli.socket) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("error: {message}");
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
                eprintln!("error: {message}");
                return ExitCode::from(EXIT_USAGE);
            }
        };
        return ExitCode::from(tui::run(&tui::Target::Discover(run)));
    }
    let socket = match socket_path(&cli, &manifest_path) {
        Ok(socket) => socket,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // `daemon` returned above, so what is left but a request is `tui`.
    let Some(request) = cli.command.request() else {
        return ExitCode::from(tui::run(&tui::Target::One(socket)));
    };
    ExitCode::from(client(
        &cli,
        &request,
        &socket,
        &manifest_path,
        REQUEST_TIMEOUT,
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

/// Send one request, show progress, render the answer. Returns the exit code. A quick command waits `timeout` for
/// each line of the answer.
fn client(
    cli: &Cli,
    request: &Request,
    socket: &Path,
    manifest_path: &Path,
    timeout: Duration,
) -> u8 {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        let message = format!(
            "no daemon answers on {}; start one with `fawkes daemon --manifest {}`",
            socket.display(),
            manifest_path.display()
        );
        if cli.json {
            println!(
                "{}",
                serde_json::to_string(&err(ErrorCode::DaemonUnreachable, message))
                    .unwrap_or_default()
            );
        } else {
            eprintln!("error: {message}");
        }
        return EXIT_UNREACHABLE;
    };
    if request.write_to(&mut stream).is_err() {
        eprintln!(
            "error: the daemon on {} closed the connection",
            socket.display()
        );
        return EXIT_UNREACHABLE;
    }
    let follow = matches!(request, Request::Log { follow: true, .. });
    // A daemon that accepts and then never answers must not hang a quick command (`Request::is_quick`).
    if request.is_quick() && stream.set_read_timeout(Some(timeout)).is_err() {
        eprintln!("error: cannot set a read timeout on the connection");
        return EXIT_FAILED;
    }
    for line in BufReader::new(stream).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) if timed_out(&e) => {
                eprintln!("error: the daemon did not answer in time");
                return EXIT_FAILED;
            }
            Err(_) => break,
        };
        // `--json` shows what the daemon sent, not what this client understood of it.
        let Ok(raw) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let message = Line::<Value>::deserialize(&raw);
        // An event this client cannot read (a kind from a newer daemon) still reaches `--json`.
        if cli.json
            && message.is_err()
            && let Some(event) = raw.get("event")
        {
            if follow {
                println!("{event}");
            } else {
                eprintln!("{event}");
            }
            continue;
        }
        let Ok(message) = message else {
            continue;
        };
        match message {
            Line::Event { event } => match (follow, cli.json) {
                (true, true) => println!("{}", raw["event"]),
                (true, false) => println!("{}", render_event(&event)),
                (false, true) => eprintln!("{}", raw["event"]),
                (false, false) => eprintln!("{}", render_progress(&event)),
            },
            // Only `subscribe` sends these.
            Line::Status { .. } | Line::History { .. } => {}
            Line::Done { result, .. } => {
                if cli.json {
                    println!("{result}");
                } else {
                    println!("{}", render_result(request, &result));
                }
                return 0;
            }
            Line::Failed { error, .. } => {
                if cli.json {
                    println!("{}", raw["error"]);
                } else {
                    eprintln!("error: {}", error.message);
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
    eprintln!(
        "error: the daemon on {} closed the connection without an answer",
        socket.display()
    );
    EXIT_FAILED
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
        Request::Status => typed(result, |s: Status| {
            let pending = s.pending.map_or_else(String::new, |p| {
                format!("  (waiting for confirm until {})", p.deadline)
            });
            format!(
                "{}  daemon pid {}  busy: {}\npermanent {}  current {}{}\nprocess {}",
                s.name,
                s.daemon_pid,
                or_dash(s.busy),
                or_dash(s.permanent),
                or_dash(s.current),
                pending,
                render_process(&s.process)
            )
        }),
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
            assert_eq!(cli.command.request(), Some(Request::Status), "{args}");
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
            assert_eq!(parse(args).command.request(), Some(request), "{args}");
        }
        assert_eq!(parse("tui").command.request(), None);
    }

    #[test]
    fn a_quick_command_gives_up_on_a_daemon_that_never_answers() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let socket = dir.join("fawkes.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accepts and keeps the connection open without a word.
        let silent = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(stream);
        });
        let cli = Cli {
            json: false,
            manifest: None,
            socket: Some(socket.clone()),
            command: Command::Status,
        };

        let started = Instant::now();
        let code = client(
            &cli,
            &Request::Status,
            &socket,
            Path::new("fawkes.toml"),
            Duration::from_millis(300),
        );
        let waited = started.elapsed();

        assert_eq!(code, EXIT_FAILED);
        // The read timed out; the daemon had not closed the connection yet.
        assert!(
            waited >= Duration::from_millis(300),
            "gave up after {waited:?}"
        );
        assert!(waited < Duration::from_secs(2), "hung for {waited:?}");
        silent.join().unwrap();
    }
}
