//! Child processes: spawning in their own process group, waiting for them to exit, stopping the group, and health
//! checks.

use crate::manifest::Check;
use crate::state;
use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, sleep};
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

/// Run `then` once `child` has exited. A thread blocks in `waitid` with `WNOWAIT`, which leaves the exit unreaped:
/// the status stays for `Child::try_wait`, so whoever owns the `Child` reaps it as before. A thread per child keeps
/// one code path for macOS and Linux, where kqueue's `NOTE_EXIT` and a pidfd would need one each.
pub fn on_exit(child: &Child, then: impl FnOnce() + Send + 'static) {
    let pid = Pid::from_child(child);
    thread::spawn(move || {
        // Any error but an interruption means `pid` is no longer an unreaped child of ours: it has exited and been
        // reaped already.
        while matches!(
            waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT
            ),
            Err(Errno::INTR)
        ) {}
        then();
    });
}

/// A receiver that gets a message once `child` has exited (`on_exit`), and is disconnected after it.
pub fn exited(child: &Child) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    on_exit(child, move || {
        // The receiver is gone when nobody waits any more.
        let _ = tx.send(());
    });
    rx
}

/// `child`'s exit status once it has exited, reaped; None when `until` passes first.
pub fn wait_until(child: &mut Child, until: Instant) -> std::io::Result<Option<ExitStatus>> {
    if let Some(status) = child.try_wait()? {
        return Ok(Some(status));
    }
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Ok(None);
    }
    // A message or a timeout: `try_wait` tells which.
    let _ = exited(child).recv_timeout(left);
    child.try_wait()
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
    let per_second = u32::try_from(rustix::param::clock_ticks_per_second()).ok()?;
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
    let mut status = None;
    let until = Instant::now() + Duration::from_secs_f64(timeout);
    if wait_group(pgid, leader.as_deref_mut(), &mut status, until) {
        return status;
    }
    kill_group(pgid);
    // The leader is reaped inside the same bound: until it is, it stays a zombie, and a zombie still counts as alive.
    wait_group(pgid, leader, &mut status, Instant::now() + KILL_WAIT);
    status
}

/// Wait until `until` for group `pgid` to end: its leader, when it is our child, exited and reaped into `status`,
/// and no other member left. True when it has ended.
fn wait_group(
    pgid: u32,
    leader: Option<&mut Child>,
    status: &mut Option<ExitStatus>,
    until: Instant,
) -> bool {
    if let Some(child) = leader {
        *status = status.or_else(|| wait_until(child, until).ok().flatten());
        if status.is_none() {
            return false;
        }
    }
    // The other members are not our children, so nothing reports their exit: they are polled, which takes one look
    // when the leader took its group with it, and runs only as long as a member outlives its leader.
    loop {
        if !group_alive(pgid) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        sleep(Duration::from_millis(20));
    }
}

/// Try the health check every `interval` seconds until it passes, the app stops being alive, or `timeout` seconds
/// pass. The app's exit (`exited`, from `exited`) ends the wait between two tries at once.
pub fn wait_healthy(
    check: &Check,
    cwd: &Path,
    env: &Env,
    interval: f64,
    timeout: f64,
    mut alive: impl FnMut() -> bool,
    exited: &Receiver<()>,
) -> Result<(), String> {
    let give_up = Instant::now() + Duration::from_secs_f64(timeout);
    let gone = || Err("the app exited before it became healthy".into());
    loop {
        if !alive() {
            return gone();
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
            Err(_) => {
                // Never past `give_up`: with `interval` longer than `timeout` the last try would come late.
                let wait = Duration::from_secs_f64(interval)
                    .min(give_up.saturating_duration_since(Instant::now()));
                if !matches!(
                    exited.recv_timeout(wait),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    return gone();
                }
            }
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
    let waited = wait_until(&mut child, Instant::now() + COMMAND_TIMEOUT);
    // Whatever the check left in its group ends with it, passed or not, and the wait for the group is bounded: a
    // command stuck in the kernel outlives SIGKILL, and must not hold the thread that runs the check.
    stop_group(child.id(), 0.0, Some(&mut child));
    match waited.map_err(|e| e.to_string())? {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("health command failed ({status})")),
        None => Err("health command timed out".into()),
    }
}

/// What one health request may take in all: connecting, sending and reading the status line.
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest status line read; the rest of a longer one is ignored.
const STATUS_LINE_BYTES: u64 = 1024;

/// The first line of the answer on `stream`, which must be complete by `until`. The read timeout is the time left
/// before each `read`, because a timeout alone bounds only each `read`, which a peer sending a byte at a time never
/// exceeds.
fn read_status_line(stream: &TcpStream, until: Instant) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut reader = stream.take(STATUS_LINE_BYTES);
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(left))?;
        match reader.read(&mut byte)? {
            0 => break,
            _ => line.push(byte[0]),
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
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
    let mut last = format!("cannot resolve {authority}");
    for addr in authority
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {authority}: {e}"))?
    {
        let attempt = (|| -> std::io::Result<String> {
            let until = Instant::now() + HTTP_TIMEOUT;
            let mut stream = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT)?;
            stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
            write!(
                stream,
                "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
            )?;
            read_status_line(&stream, until)
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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
    }

    #[test]
    fn a_group_whose_leader_has_exited_is_still_a_leftover() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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

    #[test]
    fn a_health_url_is_judged_by_its_status_line_alone() {
        for host in ["127.0.0.1", "[::1]"] {
            // Each answer promises a body it never sends, and its connection stays open until the checks are over:
            // only a check that stops at the status line returns before its timeout.
            let listener = std::net::TcpListener::bind(format!("{host}:0")).unwrap();
            let url = format!(
                "http://{host}:{}/health",
                listener.local_addr().unwrap().port()
            );
            let answers = [
                "200 OK",
                "302 Found\r\nLocation: http://127.0.0.1:1/",
                "503 Service Unavailable",
            ];
            let server = std::thread::spawn(move || {
                let mut open = Vec::new();
                for answer in answers {
                    let (mut stream, _) = listener.accept().unwrap();
                    write!(stream, "HTTP/1.1 {answer}\r\nContent-Length: 1000\r\n\r\n").unwrap();
                    open.push(stream);
                }
                open
            });

            let started = Instant::now();
            let results = [http_get(&url), http_get(&url), http_get(&url)];
            let waited = started.elapsed();

            drop(server.join().unwrap());
            assert_eq!(results[0], Ok(()), "{url}");
            assert_eq!(
                results[1],
                Ok(()),
                "{url}: a redirect is healthy, not followed"
            );
            assert_eq!(
                results[2],
                Err(format!(
                    "{url} answered \"HTTP/1.1 503 Service Unavailable\""
                ))
            );
            assert!(waited < Duration::from_secs(2), "{url} waited {waited:?}");
        }
    }

    #[test]
    fn a_status_line_that_never_ends_is_given_up_on_within_the_attempts_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        // A byte every 250 ms keeps every single read inside its own timeout, and the line never ends.
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for _ in 0..24 {
                if stream.write_all(b"H").is_err() {
                    break;
                }
                sleep(Duration::from_millis(250));
            }
        });

        let started = Instant::now();
        let result = http_get(&url);
        let waited = started.elapsed();

        server.join().unwrap();
        assert!(result.is_err(), "{result:?}");
        assert!(waited < Duration::from_secs(3), "waited {waited:?}");
    }

    #[test]
    fn the_wait_between_tries_never_runs_past_the_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let (_alive, exited) = mpsc::channel::<()>();
        let started = Instant::now();
        let result = wait_healthy(
            &Check::Command("exit 1".into()),
            temp.path(),
            &[],
            30.0,
            1.0,
            || true,
            &exited,
        );
        let waited = started.elapsed();

        assert!(result.is_err(), "{result:?}");
        assert!(waited < Duration::from_secs(3), "waited {waited:?}");
    }

    #[test]
    fn a_health_command_that_passes_leaves_nothing_running_in_its_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("background.pid");
        let command = format!("sleep 30 & echo $! > {}", pid_file.display());
        assert_eq!(run_check(&command, temp.path(), &[]), Ok(()));

        let background: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            start_time(background).is_none(),
            "the backgrounded `sleep` outlived the check"
        );
    }
}
