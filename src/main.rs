//! `fawkes`: an app's daemon (`fawkes up`), the supervisor of every app's daemon (`fawkes up --all`), and the thin
//! command-line client of their sockets.

mod daemon;
mod discovery;
mod fleet;
mod manifest;
mod process;
mod protocol;
mod reexec;
mod render;
mod service;
mod state;
mod watch;

use clap::{Parser, Subcommand};
use discovery::{Given, Target};
use manifest::Manifest;
use protocol::{
    ErrorCode, FleetDownResult, FleetRequest, Generation, Line, REQUEST_TIMEOUT, ReexecResult,
    Request, StageResult, StartResult, Status, UpgradeResult, UpgradeStep, err, read_line,
    timed_out,
};
use render::{
    render_decided, render_event, render_progress, render_reexec, render_result, render_switch,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::fmt::Display;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The exit codes, which `fawkes -h` and `fawkes --help` end with. A macro, because `concat!` takes only literals.
macro_rules! exit_codes {
    () => {
        "Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 no daemon answers."
    };
}

// The process's exit codes besides 0, as `exit_codes!` names them.
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_UNREACHABLE: u8 = 3;

/// `skills/fawkes/SKILL.md` as it was when this binary was built, for `fawkes skill`.
const SKILL: &str = include_str!("../skills/fawkes/SKILL.md");

/// The end of `fawkes -h`.
const EXIT_CODES: &str = exit_codes!();

/// The end of `fawkes --help`: how the app is found, what `--json` prints, examples, and the exit codes. clap
/// reflows each paragraph to the terminal, so a paragraph is one line; an example line must fit 80 columns, since a
/// wrapped one would lose its column.
const LONG_HELP_END: &str = concat!(
    "Finding the app: --socket, else --manifest, else $FAWKES_SOCKET (set for the processes Fawkes starts), else \
     $FAWKES_MANIFEST, else the nearest fawkes.toml from the current directory up to the repository root (the first \
     directory with a .git), else the manifest in $FAWKES_HOME (default ~/.fawkes) whose repo holds the current \
     directory; when several do, name one. --manifest and $FAWKES_MANIFEST take a path (a value with a / or ending \
     in .toml) or an app name, for $FAWKES_HOME/<APP>.toml.\n\n",
    "JSON: stdout carries one document, the result or the error object {\"code\", \"message\"}, and progress goes to \
     stderr as JSON lines. A result only ever gains fields. The streams differ: `log --follow` prints one event per \
     line, `watch` one {\"name\", \"event\"} document per event. `up` prints text only.\n\n",
    "Examples:\n  \
     fawkes up                          run this project's app's daemon\n  \
     fawkes up --all                    run the supervisor of every app's daemon\n  \
     fawkes upgrade main                stage, test and switch to main\n  \
     fawkes wait                        until the switch confirms or rolls back\n  \
     fawkes status --json               what runs, and the commands accepted now\n  \
     fawkes rollback                    undo the pending switch\n  \
     fawkes --manifest xana log -n 20   the last 20 events of xana, from anywhere\n  \
     fawkes down                        stop the app and its daemon\n  \
     fawkes skill                       how an agent uses Fawkes, for this version\n\n",
    exit_codes!()
);

/// A process supervisor whose upgrades are transactions: stage, test, switch, confirm, or roll back.
#[derive(Parser)]
#[command(
    name = "fawkes",
    version,
    disable_help_subcommand = true,
    after_help = EXIT_CODES,
    after_long_help = LONG_HELP_END
)]
struct Cli {
    /// Print the result as one JSON document (see `fawkes --help`)
    #[arg(long, global = true)]
    json: bool,
    #[arg(
        long,
        global = true,
        value_name = "APP",
        help = "The app: a manifest's path, or an app name in $FAWKES_HOME [env: FAWKES_MANIFEST]"
    )]
    manifest: Option<OsString>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "The daemon's socket [env: FAWKES_SOCKET]"
    )]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// What the command line asks for: one of the two local programs, or a request to a daemon.
#[derive(Subcommand)]
enum Command {
    /// Run the app's daemon, or the supervisor of every app's daemon
    #[command(
        long_about = "Run the app's daemon, or the supervisor of every app's daemon\n\n\
                      The app's daemon starts the app, serves every other command on its socket and prints its events \
                      on stderr, text only. It runs in the foreground until `fawkes down`, SIGTERM or Ctrl-C, and \
                      stops the app on its way out. When the supervisor runs and the app is one of its own (a \
                      manifest $FAWKES_HOME/<APP>.toml), `up` has the supervisor start the daemon instead, and \
                      returns once it answers.\n\n\
                      With --all, the supervisor runs in the foreground: it runs the daemon of every manifest in \
                      $FAWKES_HOME as its child, starts one that fails again, and stops them all on SIGTERM or \
                      Ctrl-C. `fawkes service install` runs it under launchd.",
        after_long_help = "Examples:\n  \
                           fawkes up\n  \
                           fawkes up --manifest xana\n  \
                           fawkes up --all"
    )]
    Up {
        #[arg(
            long,
            help = "Run the supervisor of every app's daemon in $FAWKES_HOME"
        )]
        all: bool,
        /// What the earlier program hands over when it re-executes (`fawkes reexec`)
        #[arg(long, hide = true, value_name = "JSON")]
        handover: Option<String>,
    },
    /// Stop the app and its daemon
    ///
    /// Answers once the daemon has let go of its socket and state lock, so `fawkes up` can follow at once. A daemon
    /// the supervisor runs stays down, across the supervisor's restarts too, until `fawkes up` names it again. Run it
    /// from a shell outside the app: from inside, it would end the caller and leave nothing to bring the app back.
    #[command(after_long_help = "Examples:\n  \
                                 fawkes down\n  \
                                 fawkes down --manifest xana --json")]
    Down,
    /// Run this app's daemon on the installed fawkes binary, keeping the app running
    ///
    /// The daemon executes the fawkes binary at its path in its own process: its pid stays, the app keeps running as
    /// its child, and nothing rolls back. This is how a new Fawkes install reaches a running daemon. It is refused
    /// while a switch is pending or a step runs (stage, test, prune, start, restart, the health check of a start).
    /// Answers once the daemon answers `status` again on the new binary.
    #[command(after_long_help = "Examples:\n  \
                                 fawkes reexec\n  \
                                 fawkes reexec --manifest xana --json")]
    Reexec,
    /// Run the current generation (pending, else permanent)
    Start,
    /// Stop the app
    Stop,
    /// Stop the app and start the current generation again
    Restart,
    /// Check out and install a git ref as a new generation
    Stage {
        /// A git branch, tag or commit
        #[arg(value_name = "REF")]
        git_ref: String,
    },
    /// Run a generation on a spare port with a copy of the storage
    ///
    /// The copy is the manifest's `test_copy` hook; an app without hooks is tested on an empty home. The test passes
    /// when the health check does, and the live app keeps running meanwhile.
    Test {
        /// A generation id, as `history` shows it (g4)
        #[arg(value_name = "GEN")]
        generation: String,
    },
    /// Go live; roll back unless confirmed in time
    ///
    /// The command returns once the generation is live. Whether it then confirmed or rolled back is what `fawkes wait`
    /// reports.
    Switch {
        /// A generation id, as `history` shows it (g4)
        #[arg(value_name = "GEN")]
        generation: String,
        /// Seconds to confirm in, 1 to 604800 [default: the manifest's `deadline`]
        #[arg(long, value_name = "SECS")]
        deadline: Option<u64>,
    },
    /// Stage, test and switch a git ref in one call
    ///
    /// Confirming stays the new generation's own step: it runs `fawkes confirm` once it works, and the deadline rolls
    /// it back otherwise. A failed step ends the command with that step's error (under --json, its `step` says which),
    /// and nothing after it runs. The command exits once the generation is live; `fawkes wait` reports whether it
    /// confirmed or rolled back.
    #[command(after_long_help = "Examples:\n  \
                                 fawkes upgrade main\n  \
                                 fawkes upgrade v1.4 --deadline 300 --json")]
    Upgrade {
        /// A git branch, tag or commit
        #[arg(value_name = "REF")]
        git_ref: String,
        /// Seconds to confirm in, 1 to 604800 [default: the manifest's `deadline`]
        #[arg(long, value_name = "SECS")]
        deadline: Option<u64>,
    },
    /// Wait until a generation is confirmed or rolled back
    ///
    /// Follows the daemon until the generation is decided, and answers at once when it already is. Exit 0 and its record
    /// when it became permanent; exit 1 with the code `rolled_back` (and its `cause`) when it was rolled back, or
    /// `storage_failed` when the restore of that rollback failed. A generation that is not switched to cannot be
    /// decided (`invalid_state`). It gives up with `no_answer` when the daemon has not decided by the confirm deadline
    /// and a short grace after it.
    #[command(after_long_help = "Examples:\n  \
                                 fawkes wait\n  \
                                 fawkes wait g4 --json")]
    Wait {
        #[arg(
            value_name = "GEN",
            help = "A generation id, as `history` shows it (g4) [default: the pending one]"
        )]
        generation: Option<String>,
    },
    /// Make the pending generation permanent
    #[command(after_long_help = "Example, from the app once it knows it works:\n  \
                                 \"$FAWKES_BIN\" confirm \"$FAWKES_GENERATION\"")]
    Confirm {
        #[arg(
            value_name = "GEN",
            help = "The generation that vouches for itself [default: $FAWKES_GENERATION towards the daemon \
                    $FAWKES_SOCKET names, else the pending one]"
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
    /// Read the manifest again, as SIGHUP does
    ///
    /// Answers which fields changed. A manifest that is invalid, or a field that cannot change while the daemon (or
    /// the app) runs, is refused, and the daemon keeps the manifest it has.
    Reload,
    /// Show the event log
    Log {
        /// Show the last N events [default: 20, or all of them with --since]
        #[arg(short = 'n', long, value_name = "N")]
        limit: Option<usize>,
        /// Show the events after event N, as far back as the daemon keeps them (the newest 1000); with --limit, the
        /// newest N of those
        #[arg(long, value_name = "N")]
        since: Option<u64>,
        /// Keep printing events as they happen
        #[arg(long)]
        follow: bool,
    },
    /// Print the events of every running daemon, one line each
    #[command(
        long_about = "Print the events of every running daemon, one line each\n\n\
                            Each line starts with the app's name. The daemons are the ones linked in \
                            $FAWKES_HOME/run/, including those started later; --manifest, --socket, $FAWKES_SOCKET \
                            or $FAWKES_MANIFEST names one daemon instead."
    )]
    Watch {
        /// Start after event N of each daemon, as far back as it keeps them [default: from now]
        #[arg(long, value_name = "N")]
        since: Option<u64>,
    },
    /// Run the supervisor of every app's daemon as a launchd user agent (macOS only)
    #[command(
        long_about = "Run the supervisor of every app's daemon as a launchd user agent (macOS only)\n\n\
                      The agent dev.fawkes runs `fawkes up --all`, which runs the daemon of every manifest in \
                      $FAWKES_HOME, so --manifest and --socket are not accepted. Run it from a shell outside the \
                      apps."
    )]
    Service {
        #[command(subcommand)]
        action: service::Action,
    },
    /// Print the agent skill for this version of Fawkes
    ///
    /// The skill is the SKILL.md built into this binary: how an agent finds, calls and confirms with Fawkes. Under
    /// --json it is the string of {"skill"}.
    #[command(after_long_help = "Example:\n  fawkes skill > .agents/skills/fawkes/SKILL.md")]
    Skill,
}

impl Command {
    /// The name of a command that changes the app or its daemon, which a test run (`FAWKES_PHASE=test`) may not run:
    /// it serves its health check and nothing else. Every command is listed, so a new one must say which it is.
    fn changes_the_app(&self) -> Option<&'static str> {
        match self {
            Command::Start => Some("start"),
            Command::Stop => Some("stop"),
            Command::Restart => Some("restart"),
            Command::Stage { .. } => Some("stage"),
            Command::Test { .. } => Some("test"),
            Command::Switch { .. } => Some("switch"),
            Command::Upgrade { .. } => Some("upgrade"),
            Command::Rollback { .. } => Some("rollback"),
            Command::Prune => Some("prune"),
            Command::Reload => Some("reload"),
            Command::Up { .. } => Some("up"),
            Command::Down => Some("down"),
            Command::Reexec => Some("reexec"),
            // A test run may confirm its own generation, which is never pending; `run_confirm` refuses the rest once it
            // knows which daemon the confirm reaches (DESIGN.md, Refusals).
            Command::Confirm { .. }
            | Command::Status
            | Command::History
            | Command::Log { .. }
            | Command::Watch { .. }
            | Command::Wait { .. }
            | Command::Service { .. }
            | Command::Skill => None,
        }
    }
}

/// The generation a `confirm` names: the one asked for, else `own_generation`, which is `$FAWKES_GENERATION`, the
/// generation a process Fawkes started runs as. A bare `confirm` from it names that generation, so an app's test run
/// (or anything it left behind) cannot confirm another generation that happens to be pending.
fn confirm_generation(named: Option<&str>, own_generation: Option<&str>) -> Option<String> {
    named
        .or_else(|| own_generation.filter(|id| !id.is_empty()))
        .map(String::from)
}

/// Whether the caller is a test run (`FAWKES_PHASE=test`), which serves its health check and nothing else.
fn in_test_run() -> bool {
    std::env::var("FAWKES_PHASE").is_ok_and(|phase| phase == "test")
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // `--help` is the one "error" that is not one.
        Err(error) if error.use_stderr() && json_asked() => {
            // The arguments could not be read, but an agent that asked for `--json` still parses stdout.
            let message = error.render().to_string();
            let message = message.trim().trim_start_matches("error: ").to_string();
            return ExitCode::from(fail(
                true,
                &mut std::io::stdout(),
                ErrorCode::Usage,
                message,
            ));
        }
        Err(error) => {
            // Nothing is left to report to if the terminal is gone.
            let _ = error.print();
            return if error.use_stderr() {
                ExitCode::from(EXIT_USAGE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let given = Given::new(cli.socket.clone(), cli.manifest.clone());
    let out = &mut std::io::stdout();
    // Whichever daemon it targets: a test run serves its health check and nothing else.
    if let Some(name) = cli.command.changes_the_app()
        && in_test_run()
    {
        let message = test_run_message(name, "");
        return ExitCode::from(fail(cli.json, out, ErrorCode::TestPhase, message));
    }
    // One request to the daemon, for the commands that send exactly one.
    let request = |request: Request| {
        let out = &mut std::io::stdout();
        with_endpoint(&cli, &given, out, |endpoint, out| {
            client(&cli, &request, &endpoint, REQUEST_TIMEOUT, out)
        })
    };
    let code = match &cli.command {
        Command::Up {
            all: true,
            handover,
        } => run_supervisor(&cli, handover.as_deref()),
        Command::Up {
            all: false,
            handover,
        } => run_up(&given, cli.socket.clone(), handover.as_deref()),
        Command::Skill => run_skill(&cli, out),
        Command::Watch { since } => run_watch(&cli, &given, *since, out),
        Command::Service { action } => run_service(&cli, *action, out),
        Command::Upgrade { git_ref, deadline } => {
            with_endpoint(&cli, &given, out, |endpoint, out| {
                run_upgrade(&cli, git_ref, *deadline, &endpoint, out)
            })
        }
        Command::Down => with_endpoint(&cli, &given, out, |endpoint, out| {
            run_down(&cli, &endpoint, out)
        }),
        Command::Reexec => with_endpoint(&cli, &given, out, |endpoint, out| {
            run_reexec(&cli, &endpoint, out)
        }),
        Command::Wait { generation } => with_endpoint(&cli, &given, out, |endpoint, out| {
            run_wait(&cli, generation.as_deref(), &endpoint, out)
        }),
        Command::Confirm { generation } => with_endpoint(&cli, &given, out, |endpoint, out| {
            run_confirm(&cli, generation.as_deref(), &endpoint, out)
        }),
        Command::Start => request(Request::Start),
        Command::Stop => request(Request::Stop),
        Command::Restart => request(Request::Restart),
        Command::Stage { git_ref } => request(Request::Stage {
            git_ref: git_ref.clone(),
        }),
        Command::Test { generation } => request(Request::Test {
            generation: generation.clone(),
        }),
        Command::Switch {
            generation,
            deadline,
        } => request(Request::Switch {
            generation: generation.clone(),
            deadline: *deadline,
        }),
        Command::Rollback { keep_storage } => request(Request::Rollback {
            keep_storage: *keep_storage,
        }),
        Command::Prune => request(Request::Prune),
        Command::Status => request(Request::Status),
        Command::History => request(Request::History),
        Command::Reload => request(Request::Reload),
        Command::Log {
            limit,
            since,
            follow,
        } => request(Request::Log {
            limit: *limit,
            since: *since,
            follow: *follow,
        }),
    };
    ExitCode::from(code)
}

/// `fawkes up --all`: the supervisor of every app's daemon, in the foreground. Returns the exit code.
fn run_supervisor(cli: &Cli, handover: Option<&str>) -> u8 {
    if cli.manifest.is_some() || cli.socket.is_some() {
        warn(
            "error: `fawkes up --all` runs the daemon of every manifest in $FAWKES_HOME, so --manifest and --socket \
             do not apply; `fawkes up --manifest <app>` runs one app's daemon",
        );
        return EXIT_USAGE;
    }
    match fleet::run(handover) {
        Ok(()) => 0,
        Err(message) => {
            warn(format!("error: {message}"));
            EXIT_FAILED
        }
    }
}

/// `fawkes up`: the app's daemon, in the foreground, or a request to the supervisor that runs it. Returns the exit
/// code.
fn run_up(given: &Given, socket: Option<PathBuf>, handover: Option<&str>) -> u8 {
    // A manifest that cannot be found or loaded is a usage error here as for every other command; the rest of the
    // daemon's startup failures are not.
    let manifest_path = match given.manifest() {
        Ok(path) => path,
        Err(message) => {
            warn(format!("error: {message}"));
            return EXIT_USAGE;
        }
    };
    if let Err(message) = Manifest::load(&manifest_path) {
        warn(format!("error: {message}"));
        return EXIT_USAGE;
    }
    // A daemon re-executing (`--handover`) is the daemon already, and one given its own socket is not the
    // supervisor's.
    if handover.is_none()
        && socket.is_none()
        && let Some(code) = start_under_supervisor(&manifest_path)
    {
        return code;
    }
    match daemon::Daemon::run(manifest_path, socket, handover) {
        Ok(()) => 0,
        Err(message) => {
            warn(format!("error: {message}"));
            EXIT_FAILED
        }
    }
}

/// The exit code of an `up` the supervisor answered for the app of `manifest_path`, when it is one of the supervisor's
/// apps (`<fawkes_home>/<name>.toml`) and a supervisor answers; none when this process should run the daemon itself.
/// The supervisor's own child is one such `up`: its parent is the supervisor that answers.
fn start_under_supervisor(manifest_path: &Path) -> Option<u8> {
    let home = manifest::fawkes_home().ok()?;
    let name = fleet::app_in_home(manifest_path, &home)?;
    let status = fleet::request(&home, &FleetRequest::Status, Some(REQUEST_TIMEOUT))?.ok()?;
    let supervisor = protocol::FleetStatus::deserialize(&status).ok()?;
    if rustix::process::getppid()
        .is_some_and(|parent| parent.as_raw_pid().unsigned_abs() == supervisor.pid)
    {
        return None;
    }
    let request = FleetRequest::Start { app: name.clone() };
    // `start` waits up to 30 s for the daemon to answer; a line more covers that.
    let answer = fleet::request(&home, &request, Some(REQUEST_TIMEOUT * 2));
    let failure = match answer {
        Some(Ok(result)) => {
            let Ok(started) = StartResult::deserialize(&result) else {
                let message = format!("the supervisor's start result could not be read: {result}");
                warn(format!("error: {message}"));
                return Some(EXIT_FAILED);
            };
            say(
                &mut std::io::stdout(),
                format!(
                    "{name}'s daemon runs under the supervisor (pid {}); `fawkes watch` follows it",
                    started.daemon_pid
                ),
            );
            return Some(0);
        }
        Some(Err(error)) => error,
        None => err(
            ErrorCode::DaemonUnreachable,
            format!(
                "the supervisor on {} took `start` for {name} but did not answer; `fawkes up --manifest {name}` asks \
                 again",
                fleet::socket(&home).display()
            ),
        ),
    };
    warn(format!("error: {}", failure.message));
    Some(exit_code(failure.code))
}

/// `fawkes down`: the app and its daemon, which answers once it has let go of its socket and state lock; through the
/// supervisor when it runs the app, so the app stays down. Returns the exit code.
fn run_down(cli: &Cli, endpoint: &Endpoint, out: &mut dyn Write) -> u8 {
    // From inside the app it ends, the caller goes with it, and no daemon is left to start the app again.
    if endpoint.own && std::env::var_os("FAWKES_PHASE").is_some() {
        let message = "`fawkes down` stops this daemon and its app, this process with it, and nothing would start \
                       them again; run it from a shell outside the app"
            .into();
        return fail(cli.json, out, ErrorCode::InvalidState, message);
    }
    if let Some(outcome) = down_under_supervisor(cli, endpoint) {
        return finish(cli, out, outcome);
    }
    let outcome = exchange(cli, &Request::Down, endpoint, REQUEST_TIMEOUT, out).map(|result| {
        let text = render_result(&Request::Down, &result);
        let text = format!("{text}; {}", endpoint.start_hint());
        (result, text)
    });
    finish(cli, out, outcome)
}

/// `fawkes down` through the supervisor, when the app the endpoint reaches is one of its apps and a supervisor answers:
/// it marks the app down first, so it stays down. None when the daemon is to be asked itself.
fn down_under_supervisor(
    cli: &Cli,
    endpoint: &Endpoint,
) -> Option<Result<(Value, String), Failure>> {
    let home = manifest::fawkes_home().ok()?;
    if !fleet::socket(&home).exists() {
        return None;
    }
    // A socket named outright: its daemon says which manifest it runs.
    let manifest = if let Some((path, _)) = &endpoint.manifest {
        path.clone()
    } else {
        let quiet = &mut std::io::sink();
        let raw = exchange(cli, &Request::Status, endpoint, REQUEST_TIMEOUT, quiet).ok()?;
        Status::deserialize(&raw).ok()?.manifest
    };
    let name = fleet::app_in_home(&manifest, &home)?;
    // The supervisor bounds the daemon's shutdown by its manifest, as a `down` sent to the daemon is not bounded.
    let answer = fleet::request(&home, &FleetRequest::Down { app: name.clone() }, None)?;
    Some(answer.map_err(Failure::of).map(|raw| {
        let back = format!("`fawkes up --manifest {name}` brings it back under the supervisor");
        let text = match FleetDownResult::deserialize(&raw) {
            Ok(FleetDownResult {
                daemon_pid: Some(pid),
                stopped,
                ..
            }) => {
                let stopped = stopped.map_or_else(String::new, |id| format!("{id} and "));
                format!("stopped {stopped}the daemon of {name} (pid {pid}); it stays down: {back}")
            }
            Ok(_) => format!("{name}'s daemon was not running; it stays down: {back}"),
            Err(_) => raw.to_string(),
        };
        (raw, text)
    }))
}

/// How long `fawkes reexec` waits for the daemon to answer on its new binary.
const REEXEC_WAIT: Duration = Duration::from_secs(30);

/// How often `fawkes reexec` asks again meanwhile.
const REEXEC_POLL: Duration = Duration::from_millis(100);

/// `fawkes reexec`: the daemon answers before it replaces its program, so the command ends once `status` answers
/// with the same pid and the version it named. Returns the exit code.
fn run_reexec(cli: &Cli, endpoint: &Endpoint, out: &mut dyn Write) -> u8 {
    let outcome = exchange(cli, &Request::Reexec, endpoint, REQUEST_TIMEOUT, out).and_then(|raw| {
        let result = ReexecResult::deserialize(&raw).map_err(|e| {
            let message = format!("the daemon's reexec result could not be read: {e}");
            Failure::new(ErrorCode::Internal, message)
        })?;
        let status = reexecuted(cli, &result, endpoint)?;
        let text = render_reexec(&result, &status);
        Ok((raw, text))
    });
    finish(cli, out, outcome)
}

/// The status of the daemon once it answers as process `result.pid` on fawkes `result.to`, within `REEXEC_WAIT`.
fn reexecuted(cli: &Cli, result: &ReexecResult, endpoint: &Endpoint) -> Result<Status, Failure> {
    let until = Instant::now() + REEXEC_WAIT;
    loop {
        // A status asked while the daemon executes waits in its socket's backlog for the next program.
        let left = until.saturating_duration_since(Instant::now());
        let answer = exchange(
            cli,
            &Request::Status,
            endpoint,
            left.clamp(REEXEC_POLL, REQUEST_TIMEOUT),
            &mut std::io::sink(),
        );
        if let Ok(status) = answer.and_then(|raw| {
            Status::deserialize(&raw).map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))
        }) && status.daemon_pid == result.pid
            && status.version == result.to
        {
            return Ok(status);
        }
        if Instant::now() >= until {
            let message = format!(
                "the daemon on {} took `reexec` but did not answer as fawkes {} within {} s; `fawkes log` says why",
                endpoint.socket.display(),
                result.to,
                REEXEC_WAIT.as_secs()
            );
            return Err(Failure::new(ErrorCode::NoAnswer, message));
        }
        std::thread::sleep(REEXEC_POLL);
    }
}

/// `fawkes skill`: the agent skill built into this binary. Returns the exit code.
fn run_skill(cli: &Cli, out: &mut dyn Write) -> u8 {
    if cli.json {
        say(out, serde_json::json!({ "skill": SKILL }));
    } else {
        // A reader that has gone (`fawkes skill | head`) is no failure of the command.
        let _ = out.write_all(SKILL.as_bytes());
    }
    0
}

/// Find the daemon the command line names and run `run` against it; a daemon that cannot be found is a usage error.
fn with_endpoint(
    cli: &Cli,
    given: &Given,
    out: &mut dyn Write,
    run: impl FnOnce(Endpoint, &mut dyn Write) -> u8,
) -> u8 {
    match Endpoint::find(given) {
        Ok(endpoint) => run(endpoint, out),
        Err(message) => fail(cli.json, out, ErrorCode::Usage, message),
    }
}

/// `fawkes confirm`: the generation that vouches for itself, towards `endpoint`. Returns the exit code.
fn run_confirm(
    cli: &Cli,
    generation: Option<&str>,
    endpoint: &Endpoint,
    out: &mut dyn Write,
) -> u8 {
    // Only towards the daemon that started the caller, the one `FAWKES_SOCKET` names: generation ids repeat across
    // apps, so another daemon, picked with `--socket` or `--manifest`, may have a pending generation of the same name.
    let own_generation = std::env::var("FAWKES_GENERATION")
        .ok()
        .filter(|_| endpoint.own);
    if in_test_run()
        && let Some(why) = test_run_refusal(generation, own_generation.as_deref(), endpoint.own)
    {
        let named = generation.map_or_else(|| "confirm".into(), |id| format!("confirm {id}"));
        let message = test_run_message(&named, &format!(" {why}"));
        return fail(cli.json, out, ErrorCode::TestPhase, message);
    }
    let request = Request::Confirm {
        generation: confirm_generation(generation, own_generation.as_deref()),
    };
    client(cli, &request, endpoint, REQUEST_TIMEOUT, out)
}

/// The refusal of `fawkes {command}` in a test run, which serves its health check and nothing else; `why`, when not
/// empty, follows the command.
fn test_run_message(command: &str, why: &str) -> String {
    // The live app may not end its own daemon either (`run_down`), nor start a second one.
    let who = if matches!(command, "up" | "down") {
        "a shell outside the app"
    } else {
        "the live app or a shell outside the app"
    };
    format!(
        "a test run (FAWKES_PHASE=test) only serves its health check: it does not `fawkes {command}`{why}; {who} does"
    )
}

/// Why a test run may not `confirm`, completing "it does not `fawkes confirm` …"; none when it may. A test run confirms
/// its own generation towards its own daemon, or nothing: a generation it names, or a bare confirm towards another
/// daemon (which would send none and so confirm whatever is pending there), is another's. `own_generation` is
/// `$FAWKES_GENERATION`, already dropped when `own_endpoint` is false.
fn test_run_refusal(
    generation: Option<&str>,
    own_generation: Option<&str>,
    own_endpoint: bool,
) -> Option<&'static str> {
    let own = own_generation.filter(|id| !id.is_empty());
    if !own_endpoint {
        Some("towards a daemon other than $FAWKES_SOCKET")
    } else if own.is_none() {
        Some("without a $FAWKES_GENERATION of its own")
    } else if generation.is_some_and(|id| Some(id) != own) {
        Some("of a generation other than its own $FAWKES_GENERATION")
    } else {
        None
    }
}

/// The daemon a client command talks to.
struct Endpoint {
    socket: PathBuf,
    /// The manifest it was found by and the app's name, for the message that says how to start it; none for a socket
    /// named outright.
    manifest: Option<(PathBuf, String)>,
    /// The socket is `FAWKES_SOCKET`: the daemon that started the caller.
    own: bool,
}

impl Endpoint {
    /// The daemon `given` names (`discovery`): a socket as given, or the socket of the manifest found.
    fn find(given: &Given) -> Result<Endpoint, String> {
        let mut endpoint = Self::found(given)?;
        // However it was found, the daemon is the caller's own when its socket is the file `FAWKES_SOCKET` names.
        endpoint.own = endpoint.own
            || given
                .socket_env
                .as_ref()
                .is_some_and(|own| same_file(&endpoint.socket, Path::new(own)));
        Ok(endpoint)
    }

    fn found(given: &Given) -> Result<Endpoint, String> {
        Ok(match given.target()? {
            Target::Socket { path, inherited } => Endpoint {
                socket: path,
                manifest: None,
                own: inherited,
            },
            Target::Manifest(path) => {
                let manifest = Manifest::load(&path).map_err(|e| {
                    // A manifest that is there but invalid, or writable by others, is fixed in the manifest.
                    if path.exists() {
                        e
                    } else {
                        format!("{e}; pass --manifest or --socket to say which daemon to talk to")
                    }
                })?;
                Endpoint {
                    socket: manifest.paths().socket(),
                    manifest: Some((path, manifest.name)),
                    own: false,
                }
            }
        })
    }

    /// How to start the daemon that does not answer. The same `up` asks the supervisor to start it when the supervisor
    /// runs the app.
    fn start_hint(&self) -> String {
        match &self.manifest {
            Some((path, _)) => format!("start one with `fawkes up --manifest {}`", path.display()),
            None => "start one with `fawkes up --manifest <app>`".into(),
        }
    }
}

/// Whether two paths name one file (device and inode); false when either is missing.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// `fawkes watch`: every daemon linked in `<fawkes_home>/run/`, unless a flag or a variable names one, which must
/// then answer. Returns the exit code.
fn run_watch(cli: &Cli, given: &Given, since: Option<u64>, out: &mut dyn Write) -> u8 {
    let (target, endpoint) = if given.names_one() {
        match Endpoint::find(given) {
            Ok(endpoint) => (
                Ok(watch::Target::One(endpoint.socket.clone())),
                Some(endpoint),
            ),
            Err(message) => (Err(message), None),
        }
    } else {
        (manifest::run_dir().map(watch::Target::Discover), None)
    };
    let target = match target {
        Ok(target) => target,
        Err(message) => return fail(cli.json, out, ErrorCode::Usage, message),
    };
    match watch::run(&target, cli.json, since, out) {
        Ok(()) => 0,
        Err(why) => {
            let hint = endpoint.map_or_else(
                || "start one with `fawkes up --manifest <app>`".into(),
                |endpoint| endpoint.start_hint(),
            );
            fail(
                cli.json,
                out,
                ErrorCode::DaemonUnreachable,
                format!("{why}; {hint}"),
            )
        }
    }
}

/// `fawkes service`: the launchd agent of the supervisor of every app in `$FAWKES_HOME`. Returns the exit code.
fn run_service(cli: &Cli, action: service::Action, out: &mut dyn Write) -> u8 {
    if cli.manifest.is_some() || cli.socket.is_some() {
        let message = "the service runs the supervisor of every app in $FAWKES_HOME, so --manifest and --socket do \
                       not apply"
            .into();
        return fail(cli.json, out, ErrorCode::Usage, message);
    }
    if let Some(message) = service_refusal(action, std::env::var_os("FAWKES_PHASE").is_some()) {
        return fail(cli.json, out, ErrorCode::InvalidState, message);
    }
    let outcome = manifest::fawkes_home()
        .map_err(|e| Failure::new(ErrorCode::Usage, e))
        .and_then(|home| service::run(action, &home).map_err(Failure::of));
    finish(cli, out, outcome)
}

/// Why `fawkes service` is refused, when the caller is a process Fawkes started (`FAWKES_PHASE` is set): each action
/// stops the supervisor and every daemon it runs, which is the caller's own or one beside it.
fn service_refusal(action: service::Action, phase_set: bool) -> Option<String> {
    let name = match action {
        service::Action::Install => "install",
        service::Action::Uninstall => "uninstall",
        service::Action::Restart => "restart",
    };
    phase_set.then(|| {
        format!(
            "`fawkes service {name}` stops the supervisor and every app's daemon it runs, this app's with them; run \
             it from a shell outside the apps"
        )
    })
}

/// Whether the command line has `--json` before any `--`, for an error clap found before it could say.
fn json_asked() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json")
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

/// Why a command failed: the `{"code", "message"}` object as it goes out under `--json` (the daemon's own, as it
/// sent it), and the error whose code gives the exit code.
struct Failure {
    raw: Value,
    error: protocol::Error,
}

impl Failure {
    /// A failure of the client itself.
    fn new(code: ErrorCode, message: String) -> Failure {
        Failure::of(err(code, message))
    }

    /// A failure of the client itself, with the facts `error` carries.
    fn of(error: protocol::Error) -> Failure {
        Failure {
            raw: serde_json::to_value(&error).unwrap_or_default(),
            error,
        }
    }

    /// The same failure, saying it ended the step `step` of `upgrade`, in the object `--json` prints as well.
    fn in_step(self, step: UpgradeStep) -> Failure {
        let mut raw = self.raw;
        raw["step"] = step.as_str().into();
        Failure {
            raw,
            error: self.error.in_step(step),
        }
    }

    /// Print it, the one way every failure goes out: the error object on `out` under `--json` (the document an agent
    /// parses), `error: …` on stderr otherwise. Returns the exit code.
    fn report(&self, json: bool, out: &mut dyn Write) -> u8 {
        if json {
            say(out, &self.raw);
        } else {
            warn(format!("error: {}", self.error.message));
        }
        exit_code(self.error.code)
    }
}

/// Report a failure of the client itself. Returns the exit code.
fn fail(json: bool, out: &mut dyn Write, code: ErrorCode, message: String) -> u8 {
    Failure::new(code, message).report(json, out)
}

/// The end of a command that worked or failed, the one way every such command goes out: its result as one JSON
/// document under `--json`, else its text (a line, or nothing when the text is empty, as an empty log is), and exit
/// 0; or the failure. Returns the exit code.
fn finish(
    cli: &Cli,
    out: &mut dyn Write,
    outcome: Result<(impl Serialize, String), Failure>,
) -> u8 {
    match outcome {
        Ok((result, text)) => {
            if cli.json {
                say(out, serde_json::to_string(&result).unwrap_or_default());
            } else if !text.is_empty() {
                say(out, text);
            }
            0
        }
        Err(failure) => failure.report(cli.json, out),
    }
}

/// Send one request, show progress, render the answer on `out`. Returns the exit code. A quick command waits
/// `timeout` for each line of the answer.
fn client(
    cli: &Cli,
    request: &Request,
    endpoint: &Endpoint,
    timeout: Duration,
    out: &mut dyn Write,
) -> u8 {
    match exchange(cli, request, endpoint, timeout, out) {
        // A followed log printed its events as they came; there is no result to show.
        Ok(_) if request.is_stream() => 0,
        outcome => finish(
            cli,
            out,
            outcome.map(|result| {
                let text = render_result(request, &result);
                (result, text)
            }),
        ),
    }
}

/// `fawkes upgrade`: `stage`, `test` and `switch` in turn, each as its own request, and the first failure is the
/// command's, with the step that failed. The result is the switch's, with what the stage and the test learned.
/// Returns the exit code.
fn run_upgrade(
    cli: &Cli,
    git_ref: &str,
    deadline: Option<u64>,
    endpoint: &Endpoint,
    out: &mut dyn Write,
) -> u8 {
    // Refused before anything is staged, as `switch --deadline` is before anything is stopped.
    if let Some(seconds) = deadline
        && let Err(error) = protocol::check_deadline(seconds)
    {
        return fail(cli.json, out, error.code, error.message);
    }
    let outcome = upgrade(cli, git_ref, deadline, endpoint, out).map(|result| {
        let text = render_switch(&result.switch);
        (result, text)
    });
    finish(cli, out, outcome)
}

/// The three steps of `upgrade`; what each answered is read as its command's result.
fn upgrade(
    cli: &Cli,
    git_ref: &str,
    deadline: Option<u64>,
    endpoint: &Endpoint,
    out: &mut dyn Write,
) -> Result<UpgradeResult, Failure> {
    fn read<T: serde::de::DeserializeOwned>(
        result: &Value,
        step: UpgradeStep,
    ) -> Result<T, Failure> {
        T::deserialize(result).map_err(|e| {
            let message = format!(
                "the daemon's {} result could not be read: {e}",
                step.as_str()
            );
            Failure::new(ErrorCode::Internal, message).in_step(step)
        })
    }
    let mut step = |name: UpgradeStep, request: &Request| {
        exchange(cli, request, endpoint, REQUEST_TIMEOUT, out).map_err(|f| f.in_step(name))
    };
    let stage = Request::Stage {
        git_ref: git_ref.into(),
    };
    let staged: StageResult = read(&step(UpgradeStep::Stage, &stage)?, UpgradeStep::Stage)?;
    let id = staged.generation.id;
    if !cli.json {
        warn(if staged.existing {
            format!("{id} was already staged")
        } else {
            format!("staged {id}")
        });
    }
    let test = Request::Test {
        generation: id.clone(),
    };
    let tested: Generation = read(&step(UpgradeStep::Test, &test)?, UpgradeStep::Test)?;
    if !cli.json {
        warn(format!("{id} passed its test"));
    }
    let switch = Request::Switch {
        generation: id,
        deadline,
    };
    Ok(UpgradeResult {
        switch: read(&step(UpgradeStep::Switch, &switch)?, UpgradeStep::Switch)?,
        existing: staged.existing,
        test: tested.test,
    })
}

/// `fawkes wait`: until the generation is confirmed (its record) or decided otherwise (the error says how). Returns
/// the exit code.
fn run_wait(cli: &Cli, id: Option<&str>, endpoint: &Endpoint, out: &mut dyn Write) -> u8 {
    // One line on stderr when it starts to wait; under `--json` stdout is the answer and stderr stays quiet.
    let mut progress = |line: String| {
        if !cli.json {
            warn(line);
        }
    };
    let outcome = watch::wait(&endpoint.socket, id, &mut progress)
        .map(|generation| {
            let text = render_decided(&generation);
            (generation, text)
        })
        .map_err(|error| {
            Failure::of(if error.code == ErrorCode::DaemonUnreachable {
                error.with_hint(endpoint.start_hint())
            } else {
                error
            })
        });
    finish(cli, out, outcome)
}

/// Send one request and stream its progress to stderr; the daemon's final result, or why there is none. A followed
/// log's events go to `out` as they come and its result is null.
fn exchange(
    cli: &Cli,
    request: &Request,
    endpoint: &Endpoint,
    timeout: Duration,
    out: &mut dyn Write,
) -> Result<Value, Failure> {
    let socket = &endpoint.socket;
    let Ok(mut stream) = UnixStream::connect(socket) else {
        let message = format!(
            "no daemon answers on {}; {}",
            socket.display(),
            endpoint.start_hint()
        );
        return Err(Failure::new(ErrorCode::DaemonUnreachable, message));
    };
    // A daemon with no place for another connection answers `busy` and closes without reading, which can fail the
    // write: its answer is still there to be read.
    let sent = request.write_to(&mut stream).is_ok();
    let follow = request.is_stream();
    // A daemon that accepts and then never answers must not hang a quick command (`Request::is_quick`). On macOS the
    // timeout cannot be set (EINVAL) once the daemon has closed its end, which a quick answer or a refusal does at
    // once; reading what it left needs no timeout then, so the failure is ignored.
    if request.is_quick() || !sent {
        let _ = stream.set_read_timeout(Some(timeout));
    }
    for line in BufReader::new(stream).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) if timed_out(&e) => {
                let message = format!(
                    "the daemon on {} did not answer in time; check that it is healthy with `fawkes status`",
                    socket.display()
                );
                return Err(Failure::new(ErrorCode::NoAnswer, message));
            }
            Err(_) => break,
        };
        // `--json` shows what the daemon sent, not what this client understood of it.
        let Some((raw, message)) = read_line(&line) else {
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
                    return Ok(Value::Null);
                }
            }
            // Only `subscribe` sends these.
            Line::Status { .. } | Line::History { .. } => {}
            // A followed log ends with a null result when the daemon ends the stream; the events were the output.
            Line::Done { .. } if follow => return Ok(Value::Null),
            Line::Done { result, .. } => return Ok(result),
            // The daemon checks some arguments itself (a `--deadline` out of range): still a usage error.
            Line::Failed { error, .. } => {
                return Err(Failure {
                    raw: raw["error"].clone(),
                    error,
                });
            }
        }
    }
    if follow {
        return Ok(Value::Null);
    }
    let message = if sent {
        format!(
            "the daemon on {} closed the connection without an answer; `fawkes status` shows whether it is running",
            socket.display()
        )
    } else {
        format!(
            "the daemon on {} closed the connection before the request was sent; check that it is running with `fawkes status`",
            socket.display()
        )
    };
    Err(Failure::new(ErrorCode::DaemonUnreachable, message))
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

    /// `fawkes service` stops the supervisor and every daemon it runs, so any process Fawkes started is refused it,
    /// whichever app it belongs to.
    #[test]
    fn the_service_is_refused_whenever_fawkes_phase_is_set() {
        for action in [
            service::Action::Install,
            service::Action::Uninstall,
            service::Action::Restart,
        ] {
            assert!(service_refusal(action, false).is_none());
            let refused = service_refusal(action, true).unwrap();
            assert!(
                refused.contains("from a shell outside the apps"),
                "{refused}"
            );
        }
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
            assert!(matches!(cli.command, Command::Status), "{args}");
        }
        let up = parse("up --manifest f.toml");
        assert!(matches!(
            up.command,
            Command::Up {
                all: false,
                handover: None
            }
        ));
        assert!(matches!(
            parse("up --all").command,
            Command::Up { all: true, .. }
        ));
        assert!(matches!(parse("reexec").command, Command::Reexec));
        assert_eq!(up.manifest, Some("f.toml".into()));
        let upgrade = parse("upgrade main --deadline 60");
        assert!(
            matches!(&upgrade.command, Command::Upgrade { git_ref, deadline: Some(60) } if git_ref == "main")
        );
        assert!(matches!(parse("down --json").command, Command::Down));
        // The old names are gone, not aliased: `daemon` is no command, and `up` takes no ref.
        for args in ["daemon", "up main"] {
            let words = ["fawkes"].into_iter().chain(args.split_whitespace());
            assert!(Cli::try_parse_from(words).is_err(), "{args}");
        }
    }

    #[test]
    fn a_command_line_parses_to_its_command() {
        assert!(matches!(
            parse("switch g3 --deadline 60").command,
            Command::Switch { generation, deadline: Some(60) } if generation == "g3"
        ));
        assert!(matches!(
            parse("log -n 5 --follow").command,
            Command::Log {
                limit: Some(5),
                since: None,
                follow: true
            }
        ));
        assert!(matches!(
            parse("log --limit 2 --since 40").command,
            Command::Log {
                limit: Some(2),
                since: Some(40),
                follow: false
            }
        ));
        assert!(matches!(
            parse("rollback --keep-storage").command,
            Command::Rollback { keep_storage: true }
        ));
        assert!(matches!(
            parse("confirm g2").command,
            Command::Confirm { generation: Some(id) } if id == "g2"
        ));
        // `help` is no command, so it can name a git ref.
        assert!(matches!(
            parse("stage help").command,
            Command::Stage { git_ref } if git_ref == "help"
        ));
        assert!(matches!(
            parse("watch --since 3").command,
            Command::Watch { since: Some(3) }
        ));
    }

    #[test]
    fn a_bare_confirm_names_the_generation_the_caller_runs_as() {
        let confirm = confirm_generation;
        assert_eq!(confirm(None, Some("g3")).as_deref(), Some("g3"));
        assert_eq!(confirm(Some("g2"), Some("g3")).as_deref(), Some("g2"));
        assert_eq!(confirm(None, Some("")), None);
        assert_eq!(confirm(None, None), None);
    }

    /// A test run confirms its own generation towards its own daemon, and nothing else.
    #[test]
    fn a_test_run_may_confirm_only_its_own_generation_towards_its_own_daemon() {
        let refusal = test_run_refusal;
        let other = Some("towards a daemon other than $FAWKES_SOCKET");
        let none = Some("without a $FAWKES_GENERATION of its own");
        let another = Some("of a generation other than its own $FAWKES_GENERATION");
        // Another daemon: whatever it names, whatever it runs as.
        assert_eq!(refusal(None, None, false), other);
        assert_eq!(refusal(Some("g3"), Some("g3"), false), other);
        // Its own daemon, but no generation of its own, an empty one included.
        assert_eq!(refusal(None, None, true), none);
        assert_eq!(refusal(None, Some(""), true), none);
        assert_eq!(refusal(Some("g3"), Some(""), true), none);
        // A generation it names must be its own.
        assert_eq!(refusal(Some("g2"), Some("g3"), true), another);
        assert_eq!(refusal(Some("g3"), Some("g3"), true), None);
        // A bare confirm towards its own daemon names its own generation.
        assert_eq!(refusal(None, Some("g3"), true), None);
    }

    /// Only a manifest that is not there asks for another one: an invalid one, or one others can write, is fixed in
    /// the file itself.
    #[test]
    fn the_hint_to_name_another_daemon_comes_only_without_a_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let find = |manifest: &Path| {
            let given = Given {
                socket: None,
                manifest: Some(manifest.into()),
                socket_env: None,
                manifest_env: None,
                cwd: Ok(temp.path().into()),
                fawkes_home: Ok(temp.path().into()),
            };
            Endpoint::find(&given).err().unwrap()
        };
        let hint = "; pass --manifest or --socket to say which daemon to talk to";
        let invalid = temp.path().join("invalid.toml");
        std::fs::write(&invalid, "name = 1").unwrap();
        let writable = temp.path().join("writable.toml");
        std::fs::write(&writable, "").unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o666)).unwrap();
        for (path, says) in [(&invalid, "invalid manifest"), (&writable, "o-w")] {
            let err = find(path);
            assert!(err.contains(says) && !err.contains(hint), "{err}");
        }
        let err = find(&temp.path().join("gone.toml"));
        assert!(
            err.starts_with("cannot read manifest") && err.ends_with(hint),
            "{err}"
        );
    }

    /// A daemon named by another path to `FAWKES_SOCKET`'s file (`--manifest` of the app's own manifest, a symlink)
    /// is still the caller's own.
    #[test]
    fn the_own_daemon_is_known_by_its_file_not_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("fawkes.sock");
        std::fs::write(&socket, "").unwrap();
        let link = temp.path().join("link.sock");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        let other = temp.path().join("other.sock");
        std::fs::write(&other, "").unwrap();
        let own = |flag: &Path, env: Option<&Path>| {
            let given = Given {
                socket: Some(flag.into()),
                manifest: None,
                socket_env: env.map(Into::into),
                manifest_env: None,
                cwd: Ok(temp.path().into()),
                fawkes_home: Ok(temp.path().into()),
            };
            Endpoint::find(&given).ok().unwrap().own
        };
        assert!(own(&link, Some(&socket)));
        assert!(!own(&other, Some(&socket)));
        assert!(!own(&socket, None));
        assert!(!own(
            &temp.path().join("gone.sock"),
            Some(&temp.path().join("gone.sock"))
        ));
    }

    fn endpoint(socket: &Path) -> Endpoint {
        Endpoint {
            socket: socket.into(),
            manifest: None,
            own: false,
        }
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
            &endpoint(&socket),
            Duration::from_millis(300),
            &mut out,
        );
        let waited = started.elapsed();
        silent.join().unwrap();
        (code, String::from_utf8(out).unwrap(), waited)
    }

    /// A daemon may answer and close before the client is done sending (a refusal for too many connections), or
    /// before it sets its read timeout (a quick answer); the client reads the answer either way.
    #[test]
    fn an_answer_from_a_daemon_that_already_closed_is_still_read() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("fawkes.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let rounds = 200;
        let daemon = std::thread::spawn(move || {
            for _ in 0..rounds {
                let (mut stream, _) = listener.accept().unwrap();
                writeln!(
                    stream,
                    r#"{{"ok":false,"error":{{"code":"busy","message":"m"}}}}"#
                )
                .unwrap();
            }
        });
        let cli = Cli {
            json: true,
            manifest: None,
            socket: Some(socket.clone()),
            command: Command::Status,
        };
        for _ in 0..rounds {
            let mut out = Vec::new();
            let code = client(
                &cli,
                &Request::Status,
                &endpoint(&socket),
                Duration::from_secs(2),
                &mut out,
            );
            let out = String::from_utf8(out).unwrap();
            assert_eq!(
                (code, out.as_str()),
                (EXIT_FAILED, "{\"code\":\"busy\",\"message\":\"m\"}\n")
            );
        }
        daemon.join().unwrap();
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
            since: None,
            follow: true,
        };
        let mut out = Vec::new();
        let code = client(
            &cli,
            &request,
            &endpoint(&socket),
            Duration::from_secs(5),
            &mut out,
        );
        daemon.join().unwrap();
        assert_eq!(code, 0);
        let printed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed, serde_json::from_str::<Value>(sent).unwrap());
    }
}
