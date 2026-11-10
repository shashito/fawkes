//! `fawkes`: the daemon (`fawkes daemon`) and the thin command-line client of its socket.

mod daemon;
mod discovery;
mod manifest;
mod process;
mod protocol;
mod service;
mod state;
mod watch;

use clap::{Parser, Subcommand};
use discovery::{Given, Target};
use manifest::Manifest;
use protocol::{
    ErrorCode, Event, Events, GenState, Generation, History, Line, Process, ProcessState,
    PruneResult, REQUEST_TIMEOUT, ReloadResult, Request, RollbackResult, StageResult, Status, Step,
    SwitchResult, UpResult, UpStep, err, timed_out,
};
use serde::Deserialize;
use serde_json::Value;
use std::ffi::OsString;
use std::fmt::Display;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 daemon unreachable.
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_UNREACHABLE: u8 = 3;

/// `skills/fawkes/SKILL.md` as it was when this binary was built, for `fawkes skill`.
const SKILL: &str = include_str!("../skills/fawkes/SKILL.md");

const EXIT_CODES: &str =
    "Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 no daemon answers.";

/// The end of `fawkes --help` (not `-h`): how the app is found, what `--json` prints, examples, and the exit codes.
const LONG_HELP_END: &str = "\
Finding the app: --socket, else --manifest, else $FAWKES_SOCKET (set for the processes Fawkes starts), else
$FAWKES_MANIFEST, else the nearest fawkes.toml from the current directory up to the repository root (the first
directory with a .git), else the manifest in $FAWKES_HOME (default ~/.fawkes) whose repo holds the current
directory; when several do, name one. --manifest and $FAWKES_MANIFEST take a path (a value with a / or ending in
.toml) or an app name, for $FAWKES_HOME/<APP>.toml.

JSON: stdout carries one document, the result or the error object {\"code\", \"message\"}, and progress goes to
stderr as JSON lines. A result only ever gains fields. The streams differ: `log --follow` prints one event per
line, `watch` one {\"name\", \"event\"} document per event. `daemon` prints text only.

Examples:
  fawkes up main                     stage, test and switch main; the new generation confirms itself
  fawkes wait                        until the pending switch is confirmed or rolled back
  fawkes status --json               what runs, what is pending, and the commands accepted now
  fawkes rollback                    undo the pending switch
  fawkes --manifest xana log -n 20   the last 20 events of the app xana, from anywhere
  fawkes skill                       how an agent uses Fawkes, for this version

Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 no daemon answers.";

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
    /// Run the supervisor of one app in the foreground
    Daemon,
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
                                 fawkes up main\n  \
                                 fawkes up v1.4 --deadline 300 --json")]
    Up {
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
        limit: Option<u64>,
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
    /// Run the app's daemon as a launchd user agent (macOS only)
    ///
    /// The agent runs the daemon on the manifest's own socket, so --socket is not accepted.
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
    /// Removed: `fawkes watch` replaced it
    #[command(hide = true)]
    Tui,
}

impl Command {
    /// The request this command sends to the daemon; `daemon`, `watch`, `wait`, `service`, `skill`, `tui` and `up` (three
    /// requests) send none.
    /// `own_generation` is `$FAWKES_GENERATION`, the generation a process Fawkes started runs as: a bare `confirm` from
    /// it names that generation, so an app's test run (or anything it left behind) cannot confirm another generation
    /// that happens to be pending.
    fn request(&self, own_generation: Option<&str>) -> Option<Request> {
        Some(match self {
            Command::Daemon
            | Command::Watch { .. }
            | Command::Service { .. }
            | Command::Wait { .. }
            | Command::Skill
            | Command::Tui
            | Command::Up { .. } => return None,
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
            Command::Reload => Request::Reload,
            Command::Log {
                limit,
                since,
                follow,
            } => Request::Log {
                limit: *limit,
                since: *since,
                follow: *follow,
            },
        })
    }
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
            Command::Up { .. } => Some("up"),
            Command::Rollback { .. } => Some("rollback"),
            Command::Prune => Some("prune"),
            Command::Reload => Some("reload"),
            // A test run may confirm its own generation, which is never pending; `main` refuses the rest once it knows
            // which daemon the confirm reaches (DESIGN.md, Refusals).
            Command::Daemon
            | Command::Confirm { .. }
            | Command::Status
            | Command::History
            | Command::Log { .. }
            | Command::Watch { .. }
            | Command::Wait { .. }
            | Command::Service { .. }
            | Command::Skill
            | Command::Tui => None,
        }
    }
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
    if matches!(cli.command, Command::Tui) {
        let message = "the TUI was removed; `fawkes watch` prints every app's events".into();
        return ExitCode::from(fail(
            cli.json,
            &mut std::io::stdout(),
            ErrorCode::Usage,
            message,
        ));
    }
    let given = Given::new(cli.socket.clone(), cli.manifest.clone());
    if matches!(cli.command, Command::Daemon) {
        // A manifest that cannot be found or loaded is a usage error here as for every other command; the rest of
        // the daemon's startup failures are not.
        let manifest_path = match given.manifest() {
            Ok(path) => path,
            Err(message) => {
                warn(format!("error: {message}"));
                return ExitCode::from(EXIT_USAGE);
            }
        };
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
    let mut stdout = std::io::stdout().lock();
    let test_run = std::env::var("FAWKES_PHASE").is_ok_and(|phase| phase == "test");
    // Whichever daemon it targets: a test run serves its health check and nothing else.
    if let Some(name) = cli.command.changes_the_app()
        && test_run
    {
        let message = format!(
            "a test run (FAWKES_PHASE=test) only serves its health check: it does not `fawkes {name}`; the live app \
             or a shell outside the app does"
        );
        return ExitCode::from(fail(cli.json, &mut stdout, ErrorCode::TestPhase, message));
    }
    if matches!(cli.command, Command::Skill) {
        if cli.json {
            say(&mut stdout, serde_json::json!({ "skill": SKILL }));
        } else {
            // A reader that has gone (`fawkes skill | head`) is no failure of the command.
            let _ = stdout.write_all(SKILL.as_bytes());
        }
        return ExitCode::SUCCESS;
    }
    if let Command::Watch { since } = cli.command {
        return ExitCode::from(run_watch(&cli, &given, since, &mut stdout));
    }
    if let Command::Service { action } = cli.command {
        return ExitCode::from(run_service(&cli, &given, action, &mut stdout));
    }
    let endpoint = match Endpoint::find(&given) {
        Ok(endpoint) => endpoint,
        Err(message) => {
            return ExitCode::from(fail(cli.json, &mut stdout, ErrorCode::Usage, message));
        }
    };
    // Only towards the daemon that started the caller, the one `FAWKES_SOCKET` names: generation ids repeat across
    // apps, so another daemon, picked with `--socket` or `--manifest`, may have a pending generation of the same name.
    let own_generation = std::env::var("FAWKES_GENERATION")
        .ok()
        .filter(|_| endpoint.own);
    // A test run confirms its own generation towards its own daemon, or nothing: a generation it names, or a bare
    // confirm towards another daemon (which would send none and so confirm whatever is pending there), is another's.
    if let Command::Confirm { generation } = &cli.command
        && test_run
        && own_generation
            .as_deref()
            .filter(|id| !id.is_empty())
            .is_none_or(|own| generation.as_deref().unwrap_or(own) != own)
    {
        let named = generation
            .as_deref()
            .map_or_else(String::new, |id| format!(" {id}"));
        let message = format!(
            "a test run (FAWKES_PHASE=test) only serves its health check: it does not `fawkes confirm{named}`, which \
             names a generation other than its own $FAWKES_GENERATION or a daemon other than $FAWKES_SOCKET; the live \
             app or a shell outside the app does"
        );
        return ExitCode::from(fail(cli.json, &mut stdout, ErrorCode::TestPhase, message));
    }
    if let Command::Up { git_ref, deadline } = &cli.command {
        return ExitCode::from(run_up(&cli, git_ref, *deadline, &endpoint, &mut stdout));
    }
    if let Command::Wait { generation } = &cli.command {
        return ExitCode::from(run_wait(
            &cli,
            generation.as_deref(),
            &endpoint,
            &mut stdout,
        ));
    }
    // `daemon`, `watch`, `wait`, `service`, `skill`, `tui` and `up` returned above, so every command left sends a request.
    let Some(request) = cli.command.request(own_generation.as_deref()) else {
        let message = "this command sends no request to a daemon".to_string();
        return ExitCode::from(fail(cli.json, &mut stdout, ErrorCode::Usage, message));
    };
    ExitCode::from(client(
        &cli,
        &request,
        &endpoint,
        REQUEST_TIMEOUT,
        &mut stdout,
    ))
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

    /// How to start the daemon that does not answer: its launchd agent's when one is installed, which a daemon started
    /// from a terminal would only contend with for the state lock.
    fn start_hint(&self) -> String {
        let Some((path, name)) = &self.manifest else {
            return "start one with `fawkes daemon --manifest <app>`".into();
        };
        let path = path.display();
        if service::plist(name).is_some_and(|plist| plist.exists()) {
            format!(
                "it is installed as a launchd agent; from outside the app, `fawkes service restart --manifest {path}` \
                 starts it again"
            )
        } else {
            format!("start one with `fawkes daemon --manifest {path}`")
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
                || "start one with `fawkes daemon --manifest <app>`".into(),
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

/// `fawkes service`: the service of the manifest's daemon, which it reaches by its own socket. Returns the exit code.
fn run_service(cli: &Cli, given: &Given, action: service::Action, out: &mut dyn Write) -> u8 {
    if cli.socket.is_some() {
        let message =
            "the service runs the daemon on the manifest's own socket, so --socket does not apply"
                .into();
        return fail(cli.json, out, ErrorCode::Usage, message);
    }
    let manifest_path = match given.manifest() {
        Ok(path) => path,
        Err(message) => return fail(cli.json, out, ErrorCode::Usage, message),
    };
    // `restart` stops the daemon and its app, this process with them; `install` and `uninstall` act on the same agent.
    if let Ok(manifest) = Manifest::load(&manifest_path)
        && inside_its_app(
            std::env::var_os("FAWKES_PHASE").is_some(),
            given.socket_env.as_deref().map(Path::new),
            &manifest.paths().socket(),
        )
    {
        let name = match action {
            service::Action::Install => "install",
            service::Action::Uninstall => "uninstall",
            service::Action::Restart => "restart",
        };
        let message = format!(
            "`fawkes service {name}` stops this daemon and its app, this process with it; run it from a shell \
             outside the app"
        );
        return fail(cli.json, out, ErrorCode::InvalidState, message);
    }
    match service::run(action, &manifest_path) {
        Ok((result, text)) => {
            if cli.json {
                say(out, result);
            } else {
                say(out, text);
            }
            0
        }
        Err(error) => fail(cli.json, out, error.code, error.message),
    }
}

/// Whether the caller is a process of the app whose daemon listens on `socket`: Fawkes started it (`FAWKES_PHASE` is
/// set) and its `FAWKES_SOCKET` is that very socket file.
fn inside_its_app(phase_set: bool, own_socket: Option<&Path>, socket: &Path) -> bool {
    phase_set && own_socket.is_some_and(|own| same_file(socket, own))
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
/// sent it), and the exit code.
struct Failure {
    raw: Value,
    error: protocol::Error,
    exit: u8,
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
            exit: exit_code(error.code),
            error,
        }
    }

    /// The same failure, saying it ended the step `step` of `up`, in the object `--json` prints as well.
    fn in_step(self, step: UpStep) -> Failure {
        let mut raw = self.raw;
        raw["step"] = step.as_str().into();
        Failure {
            raw,
            error: self.error.in_step(step),
            exit: self.exit,
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
        self.exit
    }
}

/// Report a failure of the client itself. Returns the exit code.
fn fail(json: bool, out: &mut dyn Write, code: ErrorCode, message: String) -> u8 {
    Failure::new(code, message).report(json, out)
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
        Ok(_) if matches!(request, Request::Log { follow: true, .. }) => 0,
        Ok(result) => {
            if cli.json {
                say(out, result);
            } else {
                // An empty log or history is no blank line.
                let text = render_result(request, &result);
                if !text.is_empty() {
                    say(out, text);
                }
            }
            0
        }
        Err(failure) => failure.report(cli.json, out),
    }
}

/// `fawkes up`: `stage`, `test` and `switch` in turn, each as its own request, and the first failure is the
/// command's, with the step that failed. The result is the switch's, with what the stage and the test learned.
/// Returns the exit code.
fn run_up(
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
    match up(cli, git_ref, deadline, endpoint, out) {
        Ok(result) => {
            if cli.json {
                say(out, serde_json::to_string(&result).unwrap_or_default());
            } else {
                say(out, render_switch(&result.switch));
            }
            0
        }
        Err(failure) => failure.report(cli.json, out),
    }
}

/// The three steps of `up`; what each answered is read as its command's result.
fn up(
    cli: &Cli,
    git_ref: &str,
    deadline: Option<u64>,
    endpoint: &Endpoint,
    out: &mut dyn Write,
) -> Result<UpResult, Failure> {
    fn read<T: serde::de::DeserializeOwned>(result: &Value, step: UpStep) -> Result<T, Failure> {
        T::deserialize(result).map_err(|e| {
            let message = format!(
                "the daemon's {} result could not be read: {e}",
                step.as_str()
            );
            Failure::new(ErrorCode::Internal, message).in_step(step)
        })
    }
    let mut step = |name: UpStep, request: &Request| {
        exchange(cli, request, endpoint, REQUEST_TIMEOUT, out).map_err(|f| f.in_step(name))
    };
    let stage = Request::Stage {
        git_ref: git_ref.into(),
    };
    let staged: StageResult = read(&step(UpStep::Stage, &stage)?, UpStep::Stage)?;
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
    let tested: Generation = read(&step(UpStep::Test, &test)?, UpStep::Test)?;
    if !cli.json {
        warn(format!("{id} passed its test"));
    }
    let switch = Request::Switch {
        generation: id,
        deadline,
    };
    Ok(UpResult {
        switch: read(&step(UpStep::Switch, &switch)?, UpStep::Switch)?,
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
    match watch::wait(&endpoint.socket, id, &mut progress) {
        Ok(generation) => {
            if cli.json {
                say(out, serde_json::to_string(&generation).unwrap_or_default());
            } else {
                say(out, render_decided(&generation));
            }
            0
        }
        Err(error) => {
            let error = if error.code == ErrorCode::DaemonUnreachable {
                error.with_hint(endpoint.start_hint())
            } else {
                error
            };
            Failure::of(error).report(cli.json, out)
        }
    }
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
    let follow = matches!(request, Request::Log { follow: true, .. });
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
                    return Ok(Value::Null);
                }
            }
            // Only `subscribe` sends these.
            Line::Status { .. } | Line::History { .. } => {}
            // A followed log ends with a null result when the daemon ends the stream; the events were the output.
            Line::Done { .. } if follow => return Ok(Value::Null),
            Line::Done { result, .. } => return Ok(result),
            Line::Failed { error, .. } => {
                // The daemon checks some arguments itself (a `--deadline` out of range): still a usage error.
                let exit = if error.code == ErrorCode::Usage {
                    EXIT_USAGE
                } else {
                    EXIT_FAILED
                };
                return Err(Failure {
                    raw: raw["error"].clone(),
                    error,
                    exit,
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
    // An older daemon's status has none of these.
    let daemon = if s.version.is_empty() {
        String::new()
    } else {
        format!(
            "\ndaemon {}  manifest {}  state_dir {}  socket {}",
            s.version,
            s.manifest.display(),
            s.state_dir.display(),
            s.socket.display()
        )
    };
    format!(
        "{}  daemon pid {}  busy: {}\npermanent {}  current {}{}\nactions: {}\nprocess {}{}",
        s.name,
        s.daemon_pid,
        or_dash(s.busy.as_deref()),
        or_dash(s.permanent.as_deref()),
        or_dash(s.current.as_deref()),
        note,
        if s.actions.is_empty() {
            "none".into()
        } else {
            s.actions.join(", ")
        },
        render_process(&s.process),
        daemon
    )
}

fn render_switch(r: &SwitchResult) -> String {
    format!(
        "switched to {} (previous: {}); confirm before {} or it rolls back",
        r.generation,
        or_dash(r.previous.as_deref()),
        r.deadline
    )
}

/// What `fawkes wait` prints for a generation that was confirmed: still permanent, or retired by a later one.
fn render_decided(g: &Generation) -> String {
    let at = or_dash(g.confirmed_at.as_deref());
    if g.state == GenState::Retired {
        format!("{} was confirmed at {at} and has been retired since", g.id)
    } else {
        format!("{} is permanent (confirmed at {at})", g.id)
    }
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
        Request::Stage { .. } => typed(result, |r: StageResult| {
            let g = &r.generation;
            let hint = format!("`fawkes test {}` tests it", g.id);
            if r.existing {
                format!(
                    "{} is already staged as {} ({}); nothing was built; {hint}",
                    g.short_commit(),
                    g.id,
                    g.git_ref
                )
            } else {
                format!(
                    "staged {} ({} @ {}); {hint}",
                    g.id,
                    g.git_ref,
                    g.short_commit()
                )
            }
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
        Request::Switch { .. } => typed(result, |r: SwitchResult| render_switch(&r)),
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
                .map(|entry| &entry.generation)
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
        Request::Reload => typed(result, |r: ReloadResult| {
            if r.changed.is_empty() {
                "reloaded the manifest; nothing changed".into()
            } else {
                format!("reloaded the manifest; changed: {}", r.changed.join(", "))
            }
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

    /// `fawkes service` is refused only to a process of the app it would act on: one Fawkes started whose
    /// `FAWKES_SOCKET` is the manifest's own socket, however the path is spelt.
    #[test]
    fn the_service_is_refused_only_from_inside_its_own_app() {
        let dir = tempfile::tempdir().unwrap();
        // Plain files: `same_file` compares inodes, and a socket bound under a long temp directory would exceed the
        // socket path limit.
        let socket = dir.path().join("fawkes.sock");
        std::fs::write(&socket, "").unwrap();
        let other = dir.path().join("other.sock");
        std::fs::write(&other, "").unwrap();
        let spelt_otherwise = dir.path().join(".").join("fawkes.sock");

        assert!(inside_its_app(true, Some(&socket), &socket));
        assert!(inside_its_app(true, Some(&spelt_otherwise), &socket));
        assert!(
            !inside_its_app(false, Some(&socket), &socket),
            "not started by Fawkes"
        );
        assert!(
            !inside_its_app(true, Some(&other), &socket),
            "another app's process"
        );
        assert!(!inside_its_app(true, None, &socket));
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
        // `up` sends three requests, none of them its own.
        let up = parse("up main --deadline 60");
        assert!(
            matches!(&up.command, Command::Up { git_ref, deadline: Some(60) } if git_ref == "main")
        );
        assert_eq!(up.command.request(None), None);
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
                    since: None,
                    follow: true,
                },
            ),
            (
                "log --limit 2 --since 40",
                Request::Log {
                    limit: Some(2),
                    since: Some(40),
                    follow: false,
                },
            ),
            ("reload", Request::Reload),
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
        assert_eq!(parse("watch --since 3").command.request(None), None);
        assert!(matches!(
            parse("watch --since 3").command,
            Command::Watch { since: Some(3) }
        ));
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
            version: "0.3.0".into(),
            manifest: "/apps/sample/fawkes.toml".into(),
            state_dir: "/apps/sample/state".into(),
            socket: "/apps/sample/state/fawkes.sock".into(),
            permanent: Some("g1".into()),
            current: Some("g2".into()),
            pending: with_pending.then(|| Pending {
                generation: "g2".into(),
                previous: Some("g1".into()),
                deadline: "2030-01-01T00:00:00Z".into(),
            }),
            step,
            busy: None,
            actions: Vec::new(),
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
        assert!(text(None, false).contains("\nactions: none\n"));
        let mut status = status_in(Some(Step::Pending), true);
        status.actions = vec!["stop".into(), "confirm g2".into()];
        assert!(render_status(&status).contains("\nactions: stop, confirm g2\n"));

        let rolling_back = text(Some(Step::RollingBack), true);
        assert!(!rolling_back.contains("confirm"), "{rolling_back}");
        assert!(
            rolling_back.contains("rolling back g2 to g1"),
            "{rolling_back}"
        );

        // Which daemon answered, on a line of its own; an older daemon's status has none of it.
        let mut status = status_in(None, false);
        assert!(
            render_status(&status).ends_with(
                "\ndaemon 0.3.0  manifest /apps/sample/fawkes.toml  state_dir /apps/sample/state  socket /apps/sample/state/fawkes.sock"
            ),
            "{}",
            render_status(&status)
        );
        status.version.clear();
        assert!(!render_status(&status).contains("manifest"));

        let failed = text(Some(Step::RestoreFailed), true);
        assert!(!failed.contains("confirm"), "{failed}");
        assert!(
            failed.contains("g2's storage could not be restored")
                && failed.contains("`fawkes rollback` retries it")
                && failed.contains("`fawkes rollback --keep-storage` gives it up"),
            "{failed}"
        );
    }

    fn generation(id: &str, state: &str, reason: Option<&str>) -> Value {
        serde_json::json!({
            "id": id, "ref": "main", "commit": "0123456789abcdef", "state": state, "dir": "/w/g",
            "staged_at": "t", "test": {"port": 4791, "passed": true, "at": "t", "seconds": 1.5, "message": "ok"},
            "switched_at": null, "confirmed_at": null, "rolled_back_at": null, "rollback_reason": reason,
            "failed_starts": 0
        })
    }

    fn process(state: &str, last_exit: &Value) -> Value {
        serde_json::json!({
            "state": state, "generation": "g3", "pid": 812, "port": 4790, "started_at": "t",
            "uptime": 12.5, "restarts": 1, "last_exit": last_exit, "restart_at": null
        })
    }

    /// Every request, in the order of the cases below; the match makes a new variant fail to compile until it is
    /// listed here.
    fn position(request: &Request) -> usize {
        match request {
            Request::Start => 0,
            Request::Stop => 1,
            Request::Restart => 2,
            Request::Stage { .. } => 3,
            Request::Test { .. } => 4,
            Request::Switch { .. } => 5,
            Request::Confirm { .. } => 6,
            Request::Prune => 7,
            Request::Rollback { .. } => 8,
            Request::Status => 9,
            Request::History => 10,
            Request::Log { .. } => 11,
            Request::Subscribe { .. } => 12,
            Request::Reload => 13,
        }
    }

    #[test]
    fn each_result_is_rendered_as_a_sentence_with_its_key_facts() {
        let g = |id: &str| id.to_string();
        let exit = serde_json::json!({"at": "t", "code": null, "signal": 9, "requested": false});
        let event = serde_json::json!({
            "seq": 1, "at": "2030-01-01T00:00:00Z", "kind": "switched", "generation": "g3", "message": "went live"
        });
        let status = serde_json::json!({
            "name": "sample", "daemon_pid": 99, "permanent": "g2", "current": "g3",
            "pending": {"generation": "g3", "previous": "g2", "deadline": "2030-01-01T00:01:00Z"},
            "step": "pending", "busy": null, "process": process("running", &Value::Null)
        });
        let cases = [
            (
                Request::Start,
                process("running", &Value::Null),
                vec![
                    "running  g3",
                    "pid 812",
                    "port 4790",
                    "up 12.5s",
                    "restarts 1",
                ],
            ),
            (
                Request::Stop,
                process("stopped", &exit),
                vec![
                    "stopped  g3",
                    "restarts 1",
                    "last exit: signal 9 (unrequested)",
                ],
            ),
            (
                Request::Restart,
                process("failed", &Value::Null),
                vec!["failed  g3", "restarts 1"],
            ),
            (
                Request::Stage { git_ref: g("main") },
                generation("g3", "staged", None),
                vec!["staged g3", "main @ 01234567", "`fawkes test g3` tests it"],
            ),
            (
                Request::Test {
                    generation: g("g3"),
                },
                generation("g3", "tested", None),
                vec![
                    "g3 passed its test on port 4791 in 1.5s",
                    "`fawkes switch g3` goes live",
                ],
            ),
            (
                Request::Switch {
                    generation: g("g3"),
                    deadline: None,
                },
                serde_json::json!({"generation": "g3", "previous": "g2", "deadline": "2030-01-01T00:01:00Z"}),
                vec![
                    "switched to g3 (previous: g2)",
                    "confirm before 2030-01-01T00:01:00Z or it rolls back",
                ],
            ),
            (
                Request::Confirm { generation: None },
                generation("g3", "permanent", None),
                vec!["g3 is now permanent"],
            ),
            (
                Request::Prune,
                serde_json::json!({"removed": ["g1", "g2"], "kept": ["g3"]}),
                vec!["removed g1, g2", "kept g3"],
            ),
            (
                Request::Rollback { keep_storage: true },
                serde_json::json!({"from": "g3", "to": "g2", "reason": "deadline passed"}),
                vec![
                    "rolled back g3",
                    "now running g2 (deadline passed)",
                    "storage kept as it was",
                ],
            ),
            (
                Request::Status,
                status,
                vec![
                    "sample  daemon pid 99",
                    "permanent g2  current g3",
                    "waiting for confirm until 2030-01-01T00:01:00Z",
                    "process running  g3",
                ],
            ),
            (
                Request::History,
                serde_json::json!({"generations": [
                    generation("g2", "permanent", None),
                    generation("g3", "rolled_back", Some("unhealthy")),
                ]}),
                vec!["g2", "permanent", "g3", "rolled_back", "(unhealthy)"],
            ),
            (
                Request::Log {
                    limit: None,
                    since: None,
                    follow: false,
                },
                serde_json::json!({"events": [event]}),
                vec!["2030-01-01T00:00:00Z", "switched", "g3", "went live"],
            ),
            (
                Request::Reload,
                serde_json::json!({"changed": ["port", "env"]}),
                vec!["reloaded the manifest; changed: port, env"],
            ),
            (
                Request::Subscribe {
                    limit: None,
                    since: None,
                },
                Value::Null,
                vec!["null"],
            ),
        ];
        let mut seen: Vec<usize> = cases
            .iter()
            .map(|(request, ..)| position(request))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..cases.len()).collect::<Vec<_>>());
        for (request, result, words) in cases {
            let text = render_result(&request, &result);
            for word in words {
                assert!(text.contains(word), "{request:?}: {word:?} not in {text:?}");
            }
        }
    }

    #[test]
    fn the_variable_parts_of_a_rendered_result_follow_the_result() {
        let mut existing = generation("g3", "tested", None);
        existing["existing"] = serde_json::json!(true);
        let text = render_result(
            &Request::Stage {
                git_ref: "main".into(),
            },
            &existing,
        );
        assert!(
            text.contains("01234567 is already staged as g3")
                && text.contains("nothing was built")
                && text.contains("`fawkes test g3` tests it"),
            "{text}"
        );

        let rolled_back = serde_json::json!({"from": "g3", "to": null, "reason": "no previous"});
        let text = render_result(
            &Request::Rollback {
                keep_storage: false,
            },
            &rolled_back,
        );
        assert!(text.contains("now running -"), "{text}");
        assert!(!text.contains("storage kept"), "{text}");

        let nothing = serde_json::json!({"removed": [], "kept": ["g1"]});
        assert_eq!(
            render_result(&Request::Prune, &nothing),
            "nothing to remove"
        );

        // A result this client cannot read is shown as the JSON it is.
        let unknown = serde_json::json!({"from": "a newer daemon"});
        let text = render_result(
            &Request::Stage {
                git_ref: "main".into(),
            },
            &unknown,
        );
        assert_eq!(text, unknown.to_string());
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
