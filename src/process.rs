//! Child processes: spawning in their own process group, stopping the group, and health checks.

use crate::manifest::Check;
use crate::state;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub type Env = [(String, String)];

/// Run `command` with `/bin/sh -c` in its own process group; stdout and stderr append to `log`.
pub fn spawn(
    command: &str,
    cwd: &Path,
    env: &Env,
    log: &Path,
    header: &str,
) -> std::io::Result<Child> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "=== {} {header} ===", state::now())?;
    Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0)
        .spawn()
}

/// `pgid` as a process group the kernel can signal. 0 would mean our own group and 1 is init, so a value that cannot
/// be an app's group (a corrupt `state.json`) names no group at all.
fn group(pgid: u32) -> Option<Pid> {
    i32::try_from(pgid)
        .ok()
        .filter(|&pid| pid > 1)
        .and_then(Pid::from_raw)
}

fn signal_group(pgid: u32, signal: Signal) {
    if let Some(group) = group(pgid) {
        // Fails only when the group is already gone (or never was ours), which leaves nothing to stop.
        let _ = kill_process_group(group, signal);
    }
}

/// Does any process of group `pgid` still exist?
pub fn group_alive(pgid: u32) -> bool {
    group(pgid).is_some_and(|group| test_kill_process_group(group).is_ok())
}

pub fn kill_group(pgid: u32) {
    signal_group(pgid, Signal::KILL);
}

/// SIGTERM the group `pgid`, wait up to `timeout` seconds for all of it to end, then SIGKILL it.
/// With the group leader as `leader` (our child) its exit status is returned; a leftover group of an earlier
/// daemon has no leader to reap and gives `None`.
pub fn stop_group(pgid: u32, timeout: f64, mut leader: Option<&mut Child>) -> Option<ExitStatus> {
    signal_group(pgid, Signal::TERM);
    let give_up = Instant::now() + Duration::from_secs_f64(timeout);
    let mut status = None;
    loop {
        if let Some(child) = leader.as_deref_mut() {
            status = status.or(child.try_wait().ok().flatten());
        }
        let leader_gone = leader.is_none() || status.is_some();
        if leader_gone && !group_alive(pgid) {
            return status;
        }
        if Instant::now() >= give_up {
            kill_group(pgid);
            if let Some(child) = leader {
                return child.wait().ok();
            }
            return None;
        }
        sleep(Duration::from_millis(20));
    }
}

/// Poll the health check until it passes, the app stops being alive, or `timeout` seconds pass.
pub fn wait_healthy(
    check: &Check,
    cwd: &Path,
    env: &Env,
    interval: f64,
    timeout: f64,
    mut alive: impl FnMut() -> bool,
) -> Result<(), String> {
    let give_up = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        if !alive() {
            return Err("the app exited before it became healthy".into());
        }
        let last = match check {
            Check::Url(url) => http_get(url),
            Check::Command(command) => run_check(command, cwd, env),
        };
        match last {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= give_up => {
                return Err(format!("not healthy after {timeout}s: {e}"));
            }
            Err(_) => sleep(Duration::from_secs_f64(interval)),
        }
    }
}

/// A health command gets this long before it counts as failed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn run_check(command: &str, cwd: &Path, env: &Env) -> Result<(), String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot run health command: {e}"))?;
    let give_up = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("health command failed ({status})")),
            None if Instant::now() >= give_up => {
                kill_group(child.id());
                let _ = child.wait();
                return Err("health command timed out".into());
            }
            None => sleep(Duration::from_millis(20)),
        }
    }
}

/// One `GET` over a plain TCP connection; healthy on a 2xx or 3xx status.
fn http_get(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("health url must start with http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let timeout = Duration::from_secs(2);
    let mut last = format!("cannot resolve {authority}");
    for addr in authority
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {authority}: {e}"))?
    {
        let attempt = (|| -> std::io::Result<String> {
            let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            write!(
                stream,
                "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
            )?;
            let mut status_line = String::new();
            BufReader::new(stream).read_line(&mut status_line)?;
            Ok(status_line)
        })();
        match attempt {
            Ok(line) => {
                let code = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|c| c.parse::<u16>().ok());
                return match code {
                    Some(200..=399) => Ok(()),
                    _ => Err(format!("{url} answered {:?}", line.trim())),
                };
            }
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    Err(last)
}
