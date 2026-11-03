//! `fawkes`: the daemon (`fawkes daemon`) and the thin command-line client of its socket.

mod daemon;
mod manifest;
mod process;
mod protocol;
mod state;
mod storage;
mod tui;

use manifest::Manifest;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: fawkes [--json] [--manifest FILE] [--socket PATH] <command>

  daemon  [--manifest FILE]      run the supervisor in the foreground
  start | stop | restart         run the current generation (pending, else permanent)
  stage <ref>                    check out and install a git ref as a new generation
  test <gen>                     start it on a spare port with a copy of the storage
  switch <gen> [--deadline SECS] go live; roll back unless confirmed in time
  confirm [<gen>]                make the pending generation permanent
  rollback                       undo the pending switch now
  status | history               show the app and the generations
  log [-n N] [--follow]          show the event log
  tui                            full-screen live view and controls";

/// Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 daemon unreachable.
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_UNREACHABLE: u8 = 3;

struct Cli {
    json: bool,
    manifest: Option<PathBuf>,
    socket: Option<PathBuf>,
    command: String,
    positional: Vec<String>,
    count: Option<u64>,
    follow: bool,
    deadline: Option<u64>,
}

fn parse_cli(args: Vec<String>) -> Result<Cli, String> {
    let mut cli = Cli {
        json: false,
        manifest: None,
        socket: None,
        command: String::new(),
        positional: Vec::new(),
        count: None,
        follow: false,
        deadline: None,
    };
    let mut args = args.into_iter();
    let number = |flag: &str, value: Option<String>| -> Result<u64, String> {
        let value = value.ok_or(format!("{flag} needs a value"))?;
        value
            .parse()
            .map_err(|_| format!("{flag} needs a whole number, not `{value}`"))
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => cli.json = true,
            "--follow" => cli.follow = true,
            "--manifest" => {
                cli.manifest = Some(args.next().ok_or("--manifest needs a file")?.into())
            }
            "--socket" => cli.socket = Some(args.next().ok_or("--socket needs a path")?.into()),
            "--deadline" => cli.deadline = Some(number("--deadline", args.next())?),
            "-n" => cli.count = Some(number("-n", args.next())?),
            flag if flag.starts_with('-') => return Err(format!("unknown option `{flag}`")),
            _ => cli.positional.push(arg),
        }
    }
    if cli.positional.is_empty() {
        return Err("missing command".into());
    }
    cli.command = cli.positional.remove(0);
    let wants = |n: std::ops::RangeInclusive<usize>| {
        if n.contains(&cli.positional.len()) {
            Ok(())
        } else {
            Err(format!(
                "`{}` got the wrong number of arguments",
                cli.command
            ))
        }
    };
    match cli.command.as_str() {
        "daemon" | "start" | "stop" | "restart" | "rollback" | "status" | "history" | "log"
        | "tui" => wants(0..=0)?,
        "stage" | "test" | "switch" => wants(1..=1)?,
        "confirm" => wants(0..=1)?,
        other => return Err(format!("unknown command `{other}`")),
    }
    if cli.deadline.is_some() && cli.command != "switch" {
        return Err("--deadline belongs to `switch`".into());
    }
    if (cli.count.is_some() || cli.follow) && cli.command != "log" {
        return Err("-n and --follow belong to `log`".into());
    }
    Ok(cli)
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
    if cli.command == "daemon" {
        return match daemon::Daemon::run(manifest_path, cli.socket) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("error: {message}");
                ExitCode::from(EXIT_FAILED)
            }
        };
    }
    if cli.command == "tui"
        && cli.socket.is_none()
        && cli.manifest.is_none()
        && std::env::var_os("FAWKES_SOCKET").is_none()
    {
        return ExitCode::from(tui::run(tui::Target::Discover(tui::run_dir())));
    }
    let socket = match socket_path(&cli, &manifest_path) {
        Ok(socket) => socket,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if cli.command == "tui" {
        return ExitCode::from(tui::run(tui::Target::One(socket)));
    }
    ExitCode::from(client(&cli, &socket, &manifest_path))
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
    Ok(manifest.state_dir().join("fawkes.sock"))
}

fn request_for(cli: &Cli) -> Value {
    let arg = |i: usize| cli.positional[i].clone();
    let args = match cli.command.as_str() {
        "stage" => json!({ "ref": arg(0) }),
        "test" => json!({ "generation": arg(0) }),
        "switch" => json!({ "generation": arg(0), "deadline": cli.deadline }),
        "confirm" => json!({ "generation": cli.positional.first() }),
        "log" => json!({ "limit": cli.count, "follow": cli.follow }),
        _ => json!({}),
    };
    json!({ "command": cli.command, "args": args })
}

/// Send one request, show progress, render the answer. Returns the exit code.
fn client(cli: &Cli, socket: &Path, manifest_path: &Path) -> u8 {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        let message = format!(
            "no daemon answers on {}; start one with `fawkes daemon --manifest {}`",
            socket.display(),
            manifest_path.display()
        );
        if cli.json {
            println!(
                "{}",
                json!({ "code": "daemon_unreachable", "message": message })
            );
        } else {
            eprintln!("error: {message}");
        }
        return EXIT_UNREACHABLE;
    };
    if writeln!(stream, "{}", request_for(cli)).is_err() {
        eprintln!(
            "error: the daemon on {} closed the connection",
            socket.display()
        );
        return EXIT_UNREACHABLE;
    }
    let follow = cli.command == "log" && cli.follow;
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(event) = message.get("event") {
            match (follow, cli.json) {
                (true, true) => println!("{event}"),
                (true, false) => println!("{}", render_event(event)),
                (false, true) => eprintln!("{event}"),
                (false, false) => eprintln!("{}", render_progress(event)),
            }
        } else if message["ok"] == json!(true) {
            let result = &message["result"];
            if cli.json {
                println!("{result}");
            } else {
                println!("{}", render_result(&cli.command, result));
            }
            return 0;
        } else {
            let error = &message["error"];
            if cli.json {
                println!("{error}");
            } else {
                eprintln!(
                    "error: {}",
                    error["message"].as_str().unwrap_or("unknown error")
                );
            }
            return EXIT_FAILED;
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

/// A field as text: strings and numbers as they are, `-` for null.
fn text(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn short(commit: &Value) -> String {
    commit.as_str().unwrap_or("").chars().take(8).collect()
}

fn render_progress(event: &Value) -> String {
    format!(
        "{}: {}",
        text(&event["generation"]),
        text(&event["message"])
    )
}

fn render_event(event: &Value) -> String {
    format!(
        "{}  {:<11} {:<4} {}",
        text(&event["at"]),
        text(&event["kind"]),
        text(&event["generation"]),
        text(&event["message"])
    )
}

fn render_generation(g: &Value) -> String {
    format!(
        "{:<4} {:<12} {} @ {}",
        text(&g["id"]),
        text(&g["state"]),
        text(&g["ref"]),
        short(&g["commit"])
    )
}

fn render_process(p: &Value) -> String {
    let mut line = format!("{}  {}", text(&p["state"]), text(&p["generation"]));
    if p["state"] == "running" {
        line += &format!(
            "  pid {}  port {}  up {}s",
            text(&p["pid"]),
            text(&p["port"]),
            text(&p["uptime"])
        );
    }
    line += &format!("  restarts {}", text(&p["restarts"]));
    let exit = &p["last_exit"];
    if !exit.is_null() {
        let how = match (exit["code"].as_i64(), exit["signal"].as_i64()) {
            (Some(code), _) => format!("exit {code}"),
            (_, Some(signal)) => format!("signal {signal}"),
            _ => "unknown".into(),
        };
        let who = if exit["requested"] == json!(true) {
            "requested"
        } else {
            "unrequested"
        };
        line += &format!("\nlast exit: {how} ({who}) at {}", text(&exit["at"]));
    }
    line
}

fn render_result(command: &str, r: &Value) -> String {
    match command {
        "stage" => format!(
            "staged {} ({} @ {})",
            text(&r["id"]),
            text(&r["ref"]),
            short(&r["commit"])
        ),
        "test" => format!(
            "{} passed its test on port {} in {}s; `fawkes switch {}` goes live",
            text(&r["id"]),
            text(&r["test"]["port"]),
            text(&r["test"]["seconds"]),
            text(&r["id"])
        ),
        "switch" => format!(
            "switched to {} (previous: {}); confirm before {} or it rolls back",
            text(&r["generation"]),
            text(&r["previous"]),
            text(&r["deadline"])
        ),
        "confirm" => format!("{} is now permanent", text(&r["id"])),
        "rollback" => format!(
            "rolled back {}; now running {} ({})",
            text(&r["from"]),
            text(&r["to"]),
            text(&r["reason"])
        ),
        "start" | "stop" | "restart" => render_process(r),
        "status" => {
            let pending = match &r["pending"] {
                Value::Null => String::new(),
                p => format!("  (waiting for confirm until {})", text(&p["deadline"])),
            };
            format!(
                "{}  daemon pid {}  busy: {}\npermanent {}  current {}{}\nprocess {}",
                text(&r["name"]),
                text(&r["daemon_pid"]),
                text(&r["busy"]),
                text(&r["permanent"]),
                text(&r["current"]),
                pending,
                render_process(&r["process"])
            )
        }
        "history" => r["generations"].as_array().map_or_else(String::new, |gs| {
            gs.iter()
                .map(|g| {
                    let reason = g["rollback_reason"]
                        .as_str()
                        .map(|s| format!("  ({s})"))
                        .unwrap_or_default();
                    format!("{}{reason}", render_generation(g))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        "log" => r["events"].as_array().map_or_else(String::new, |es| {
            es.iter().map(render_event).collect::<Vec<_>>().join("\n")
        }),
        _ => r.to_string(),
    }
}
