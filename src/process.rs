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

/// Run `command` with `/bin/sh -c` in its own process group; stdout and stderr append to `log`. With `pid_file`,
/// the shell first writes its pid (the group's id) there and then `exec`s the command: the record exists before
/// the app does anything, so a daemon that dies right after the spawn still leaves its successor a way to find it.
pub fn spawn(
    command: &str,
    cwd: &Path,
    env: &Env,
    log: &Path,
    header: &str,
    pid_file: Option<&Path>,
) -> std::io::Result<Child> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "=== {} {header} ===", state::now())?;
    let mut shell = Command::new("/bin/sh");
    match pid_file {
        // The paths go in as arguments, never into the script, so no quoting can go wrong.
        Some(pid_file) => shell
            .args(["-c", r#"echo $$ > "$0" && exec /bin/sh -c "$1""#])
            .arg(pid_file)
            .arg(command),
        None => shell.args(["-c", command]),
    };
    shell
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

/// When process `pid` started, in seconds since the epoch; None when there is no such process.
#[cfg(target_os = "macos")]
#[expect(
    clippy::cast_precision_loss,
    reason = "epoch seconds and microseconds are far below 2^52, so the conversion is exact"
)]
fn start_time(pid: u32) -> Option<f64> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `proc_bsdinfo` is plain integers and byte arrays, for which zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a live local of exactly `size` bytes, which is all `proc_pidinfo` writes.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then(|| info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6)
}

/// When process `pid` started, in seconds since the epoch; None when there is no such process.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<f64> {
    // Field 22 of /proc/<pid>/stat, in clock ticks since boot; the fields after the command name (in parentheses,
    // and free to contain spaces) start at field 3.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks: f64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let boot = boot_time()?;
    // SAFETY: sysconf only reads a configuration value.
    let per_second = u32::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    (per_second > 0).then(|| boot + ticks / f64::from(per_second))
}

/// When the machine booted, in seconds since the epoch; None when the system does not say.
#[cfg(target_os = "macos")]
#[expect(
    clippy::cast_precision_loss,
    reason = "epoch seconds are far below 2^52, so the conversion is exact"
)]
fn boot_time() -> Option<f64> {
    // SAFETY: `timeval` is two integers, for which zero is valid.
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: `boot` is a live local of exactly `size` bytes, which is all `sysctlbyname` writes for this key; the
    // name is a NUL-terminated literal and no new value is set.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut boot).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && size == std::mem::size_of::<libc::timeval>())
        .then(|| boot.tv_sec as f64 + f64::from(boot.tv_usec) / 1e6)
}

/// When the machine booted, in seconds since the epoch; None when the system does not say.
#[cfg(target_os = "linux")]
fn boot_time() -> Option<f64> {
    std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// The app an earlier daemon left running, as named by the pid file its start wrapper wrote (`spawn`). Only a
/// process that started just before the file was written is that app: a pid the system has reused since, after a
/// reboot too, belongs to a process that started later, so it is never signalled.
///
/// When the leader is gone but its group still has members (`start = "npm start"`, the shell exited and its
/// children did not), there is no start time to compare. The group is the app's all the same if the file was
/// written after the machine booted: a process group id cannot be reused while the group has members, and before
/// a boot nothing survives. Without a boot time that case is not recognised.
pub fn leftover(pid_file: &Path) -> Option<u32> {
    let written = std::fs::metadata(pid_file)
        .and_then(|meta| meta.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    let pid: u32 = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let Some(started) = start_time(pid) else {
        return (boot_time()? <= written && group_alive(pid)).then_some(pid);
    };
    // The shell writes the file at once, so a minute is far more than it takes even on a loaded machine; the second
    // after covers coarse timestamps.
    (written - 60.0 <= started && started <= written + 1.0 && group_alive(pid)).then_some(pid)
}

/// Does any process of group `pgid` still exist?
pub fn group_alive(pgid: u32) -> bool {
    group(pgid).is_some_and(|group| test_kill_process_group(group).is_ok())
}

pub fn kill_group(pgid: u32) {
    signal_group(pgid, Signal::KILL);
}

/// How long `stop_group` waits for a `SIGKILL`ed group to disappear. Bounded, because a process stuck in the kernel
/// (uninterruptible sleep) outlives even SIGKILL for as long as it is stuck.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// SIGTERM the group `pgid`, wait up to `timeout` seconds for all of it to end, then SIGKILL it and wait until it is
/// gone, so a caller that snapshots or restores storage, or starts the next process, never races a straggler.
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
            // The leader is reaped inside the same bound: until it is, it stays a zombie, and a zombie still
            // counts as alive.
            let give_up = Instant::now() + KILL_WAIT;
            loop {
                if let Some(child) = leader.as_deref_mut() {
                    status = status.or(child.try_wait().ok().flatten());
                }
                let leader_gone = leader.is_none() || status.is_some();
                if (leader_gone && !group_alive(pgid)) || Instant::now() >= give_up {
                    return status;
                }
                sleep(Duration::from_millis(10));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn a_leftover_is_only_the_process_that_wrote_its_pid_file() {
        let dir = std::env::temp_dir().join(format!("fk-process-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("app.pid");
        let mut app = spawn(
            "exec sleep 30",
            &dir,
            &[],
            &dir.join("app.log"),
            "test",
            Some(&pid_file),
        )
        .unwrap();
        let give_up = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&pid_file).map_or(true, |text| !text.ends_with('\n')) {
            assert!(Instant::now() < give_up, "the wrapper never wrote its pid");
            sleep(Duration::from_millis(10));
        }
        assert_eq!(leftover(&pid_file), Some(app.id()));

        // A file older than the process: the pid was reused by a process started after the app wrote it.
        let file = std::fs::File::options()
            .write(true)
            .open(&pid_file)
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        assert_eq!(leftover(&pid_file), None);
        // Much newer than the process: not the process that wrote it either.
        file.set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        assert_eq!(leftover(&pid_file), None);

        kill_group(app.id());
        app.wait().unwrap();
        file.set_modified(SystemTime::now()).unwrap();
        assert_eq!(leftover(&pid_file), None, "a process that is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_group_whose_leader_has_exited_is_still_a_leftover() {
        let dir = std::env::temp_dir().join(format!("fk-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("app.pid");
        let mut leader = spawn(
            "sleep 30 & exit",
            &dir,
            &[],
            &dir.join("app.log"),
            "test",
            Some(&pid_file),
        )
        .unwrap();
        // Reaping the leader leaves the sleeping member alone in the group.
        leader.wait().unwrap();
        let pgid = leader.id();
        assert!(group_alive(pgid), "the member outlives its leader");
        assert_eq!(leftover(&pid_file), Some(pgid));

        stop_group(pgid, 1.0, None);
        assert_eq!(leftover(&pid_file), None, "the group is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_group_does_not_wait_for_ever_on_a_leader_that_will_not_exit() {
        let sleeper = || {
            Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap()
        };
        // The leader is in a group of its own, so signalling the other group never reaches it and it never exits,
        // as a process stuck in the kernel would not.
        let mut leader = sleeper();
        let mut other = sleeper();

        let started = Instant::now();
        let status = stop_group(other.id(), 0.2, Some(&mut leader));
        let waited = started.elapsed();

        kill_group(leader.id());
        leader.wait().unwrap();
        other.wait().unwrap();
        assert_eq!(status, None);
        assert!(
            waited < Duration::from_millis(200) + KILL_WAIT + Duration::from_secs(1),
            "waited {waited:?}"
        );
    }
}
