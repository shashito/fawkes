//! `fawkes`: the daemon (`fawkes daemon`) and the thin command-line client of its socket.

mod daemon;
mod manifest;
mod process;
mod protocol;
mod state;
mod storage;
mod tui;

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

const USAGE: &str = "usage: fawkes [--json] [--manifest FILE] [--socket PATH] <command>

  daemon  [--manifest FILE]      run the supervisor in the foreground
  start | stop | restart         run the current generation (pending, else permanent)
  stage <ref>                    check out and install a git ref as a new generation
  test <gen>                     start it on a spare port with a copy of the storage
  switch <gen> [--deadline SECS] go live; roll back unless confirmed in time
  confirm [<gen>]                make the pending generation permanent
  rollback [--keep-storage]      undo the pending switch now
                                 --keep-storage gives up a restore that failed: the permanent generation then
                                 runs on storage the failed generation may already have migrated
  prune                          remove old generations beyond the manifest's `keep`
  status | history               show the app and the generations
  log [-n N] [--follow]          show the event log
  tui                            full-screen live view and controls";

/// Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 daemon unreachable.
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_UNREACHABLE: u8 = 3;

/// What the command line asks for: one of the two local programs, or a request to a daemon.
enum Command {
    Daemon,
    Tui,
    Send(Request),
}

struct Cli {
    json: bool,
    manifest: Option<PathBuf>,
    socket: Option<PathBuf>,
    command: Command,
}

fn parse_cli(args: Vec<String>) -> Result<Cli, String> {
    let (mut json, mut manifest, mut socket) = (false, None, None);
    let (mut count, mut follow, mut deadline) = (None, false, None);
    let mut keep_storage = false;
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    let number = |flag: &str, value: Option<String>| -> Result<u64, String> {
        let value = value.ok_or(format!("{flag} needs a value"))?;
        value
            .parse()
            .map_err(|_| format!("{flag} needs a whole number, not `{value}`"))
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--follow" => follow = true,
            "--keep-storage" => keep_storage = true,
            "--manifest" => manifest = Some(args.next().ok_or("--manifest needs a file")?.into()),
            "--socket" => socket = Some(args.next().ok_or("--socket needs a path")?.into()),
            "--deadline" => deadline = Some(number("--deadline", args.next())?),
            "-n" => count = Some(number("-n", args.next())?),
            flag if flag.starts_with('-') => return Err(format!("unknown option `{flag}`")),
            _ => positional.push(arg),
        }
    }
    if positional.is_empty() {
        return Err("missing command".into());
    }
    let name = positional.remove(0);
    let wants = |n: std::ops::RangeInclusive<usize>| {
        if n.contains(&positional.len()) {
            Ok(())
        } else {
            Err(format!("`{name}` got the wrong number of arguments"))
        }
    };
    // `positional[0]` is read only once `wants` has seen exactly one.
    let command = match name.as_str() {
        "daemon" => wants(0..=0).map(|()| Command::Daemon),
        "tui" => wants(0..=0).map(|()| Command::Tui),
        "start" => wants(0..=0).map(|()| Command::Send(Request::Start)),
        "stop" => wants(0..=0).map(|()| Command::Send(Request::Stop)),
        "restart" => wants(0..=0).map(|()| Command::Send(Request::Restart)),
        "rollback" => wants(0..=0).map(|()| Command::Send(Request::Rollback { keep_storage })),
        "prune" => wants(0..=0).map(|()| Command::Send(Request::Prune)),
        "status" => wants(0..=0).map(|()| Command::Send(Request::Status)),
        "history" => wants(0..=0).map(|()| Command::Send(Request::History)),
        "log" => wants(0..=0).map(|()| {
            Command::Send(Request::Log {
                limit: count,
                follow,
            })
        }),
        "stage" => wants(1..=1).map(|()| {
            Command::Send(Request::Stage {
                git_ref: positional[0].clone(),
            })
        }),
        "test" => wants(1..=1).map(|()| {
            Command::Send(Request::Test {
                generation: positional[0].clone(),
            })
        }),
        "switch" => wants(1..=1).map(|()| {
            Command::Send(Request::Switch {
                generation: positional[0].clone(),
                deadline,
            })
        }),
        "confirm" => wants(0..=1).map(|()| {
            Command::Send(Request::Confirm {
                generation: positional.first().cloned(),
            })
        }),
        other => Err(format!("unknown command `{other}`")),
    }?;
    if deadline.is_some() && name != "switch" {
        return Err("--deadline belongs to `switch`".into());
    }
    if keep_storage && name != "rollback" {
        return Err("--keep-storage belongs to `rollback`".into());
    }
    if (count.is_some() || follow) && name != "log" {
        return Err("-n and --follow belong to `log`".into());
    }
    Ok(Cli {
        json,
        manifest,
        socket,
        command,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let cli = match parse_cli(args) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
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
    let Command::Send(request) = &cli.command else {
        return ExitCode::from(tui::run(&tui::Target::One(socket)));
    };
    ExitCode::from(client(
        &cli,
        request,
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

    #[test]
    fn a_quick_command_gives_up_on_a_daemon_that_never_answers() {
        let dir = std::env::temp_dir().join(format!("fk-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
            command: Command::Send(Request::Status),
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
