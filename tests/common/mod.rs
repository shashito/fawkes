//! A throwaway Fawkes world for integration tests: a git repo of the sample app, a manifest, a daemon.

use rusqlite::{Connection, OpenFlags};
use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const APP_JS: &str = include_str!("../../examples/sample/app.js");
pub const BIN: &str = env!("CARGO_BIN_EXE_fawkes");

/// Where the port blocks' lock files live, shared by every suite on the machine whatever worktree it runs from.
const PORT_LOCKS: &str = "/tmp/fawkes-test-ports";
/// Blocks of ten ports from 20000 to 29999: below the ephemeral ranges of macOS (49152) and Linux (32768), so no
/// outgoing connection takes one.
const FIRST_PORT: u16 = 20000;
const BLOCKS: u16 = 1000;

/// Claim a block of ten free ports, `base..base + 10`, for as long as the returned lock file stays open. Suites that
/// run at the same time, in this process or in another worktree, skip a block whose file is locked, and the probe
/// skips ports something else listens on. A `flock` ends with its process, so a killed run leaves no stale claim.
fn claim_ports() -> (u16, File) {
    // Spread the starting points, so concurrent suites and the tests within one rarely try the same blocks.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let offset = std::process::id() + NEXT.fetch_add(7, Ordering::Relaxed);
    std::fs::create_dir_all(PORT_LOCKS).unwrap();
    for i in 0..u32::from(BLOCKS) {
        let block = u16::try_from(offset.wrapping_add(i) % u32::from(BLOCKS)).unwrap();
        let base = FIRST_PORT + block * 10;
        let lock = File::create(format!("{PORT_LOCKS}/{base}")).unwrap();
        if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok()
            && (base..base + 10).all(port_free)
        {
            return (base, lock);
        }
    }
    panic!(
        "no free block of ten ports between {FIRST_PORT} and {}",
        FIRST_PORT + BLOCKS * 10
    );
}

/// The daemon's own probe: nothing listens on the port on either address.
fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok() && TcpListener::bind(("0.0.0.0", port)).is_ok()
}

pub struct Harness {
    pub dir: PathBuf,
    pub port: u16,
    manifest: PathBuf,
    daemon: Option<Child>,
    /// The claim on `port..port + 10` and on `dir`, released when the harness drops (after `drop` cleaned up).
    _ports: File,
}

impl Harness {
    /// A world of its own: live port `port` and spare ports `port+1..=port+9` from a block claimed for this test
    /// (`claim_ports`), under `/tmp/fk<port>`, kept short because socket paths are limited. `extra` is added to the
    /// manifest's top-level keys.
    pub fn new(extra: &str) -> Harness {
        let (port, ports) = claim_ports();
        // A run that was killed may have left the directory; the claim makes it ours now.
        let dir = PathBuf::from(format!("/tmp/fk{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let h = Harness {
            manifest: dir.join("fawkes.toml"),
            dir,
            port,
            daemon: None,
            _ports: ports,
        };
        h.git(&["init", "-q"]);
        let d = h.dir.display();
        std::fs::write(
            &h.manifest,
            format!(
                r#"name = "t"
repo = "{d}/repo"
state_dir = "{d}/s"
home = "{d}/home"
storage = ["app.sqlite"]
port = {port}
spare_ports = [{}, {}]
start = "node app.js"
deadline = 30
stop_timeout = 5
min_uptime = 1
{extra}
[health]
url = "http://127.0.0.1:{{port}}/"
interval = 0.1
timeout = 20

[env]
APP_PORT = "{{port}}"
APP_HOME = "{{home}}"
"#,
                port + 1,
                port + 9
            ),
        )
        .unwrap();
        h
    }

    /// Replace `from`, which must occur in the manifest, with `to`.
    pub fn edit_manifest(&self, from: &str, to: &str) {
        let text = std::fs::read_to_string(&self.manifest).unwrap();
        assert!(text.contains(from), "{from:?} is not in the manifest");
        std::fs::write(&self.manifest, text.replacen(from, to, 1)).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .current_dir(self.dir.join("repo"))
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// Commit the sample app at `version` behaving as `mode`, and tag it with the version.
    pub fn commit(&self, version: &str, mode: &str) {
        let repo = self.dir.join("repo");
        std::fs::write(repo.join("app.js"), APP_JS).unwrap();
        std::fs::write(repo.join("VERSION"), version).unwrap();
        std::fs::write(repo.join("MODE"), mode).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", version]);
        self.git(&["tag", version]);
    }

    pub fn start_daemon(&mut self) {
        // Appended, so a test that restarts the daemon keeps the log of each one.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("daemon.log"))
            .unwrap();
        let child = self.daemon_command().stderr(log).spawn().unwrap();
        self.daemon = Some(child);
        self.wait("the daemon to answer", 10, || self.fk(&["status"]).0 == 0);
    }

    /// SIGKILL the daemon, as a crash or an OOM kill would: no cleanup runs and its app keeps running.
    pub fn kill_daemon(&mut self) {
        let mut daemon = self.daemon.take().expect("the daemon was started");
        // Already dead is fine: some tests make the app kill the daemon.
        let _ = daemon.kill();
        daemon.wait().unwrap();
    }

    /// Send `signal` to the daemon this harness started (SIGHUP asks it to reload its manifest).
    pub fn signal_daemon(&self, signal: Signal) {
        let daemon = self.daemon.as_ref().expect("the daemon was started");
        // The pid is our own child's, which is not reaped while `self.daemon` holds it.
        kill_process(Pid::from_child(daemon), signal)
            .unwrap_or_else(|e| panic!("{signal:?} could not be sent to the daemon: {e}"));
    }

    /// Wait up to `within` for the daemon to exit by itself (after a SIGTERM); whether it did.
    pub fn daemon_exits_within(&mut self, within: Duration) -> bool {
        let daemon = self.daemon.as_mut().expect("the daemon was started");
        let give_up = Instant::now() + within;
        while daemon.try_wait().unwrap().is_none() {
            if Instant::now() >= give_up {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.daemon = None;
        true
    }

    /// A connection to the daemon's socket, for a test that decides when its request goes out.
    pub fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(self.dir.join("s/fawkes.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream
    }

    /// `fawkes --json log --follow <args>` running in the background, its output read line by line.
    pub fn follow(&self, args: &[&str]) -> Follow {
        let mut child = self
            .cli()
            .args(["log", "--follow"])
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Follow { child, lines }
    }

    /// Run `fawkes daemon` for this manifest in the foreground; its exit code and stderr.
    pub fn run_second_daemon(&self) -> (i32, String) {
        let output = self.daemon_command().output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// `fawkes daemon` for this manifest, with its `run/` links kept inside the test directory.
    fn daemon_command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(["daemon", "--manifest"])
            .arg(&self.manifest)
            .env("FAWKES_HOME", self.manifest.parent().unwrap());
        command
    }

    /// `fawkes --json --manifest <manifest>`, deaf to a `FAWKES_SOCKET` of the environment the tests run in.
    fn cli(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .arg("--json")
            .arg("--manifest")
            .arg(&self.manifest)
            .env_remove("FAWKES_SOCKET");
        command
    }

    /// Run `fawkes --json <args>`; the exit code and the single JSON document it printed.
    pub fn fk(&self, args: &[&str]) -> (i32, Value) {
        let output = self.cli().args(args).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json = if stdout.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&stdout)
                .unwrap_or_else(|e| panic!("`fawkes {args:?}` printed non-JSON ({e}): {stdout}"))
        };
        (output.status.code().unwrap(), json)
    }

    /// `fk` that must succeed.
    pub fn ok(&self, args: &[&str]) -> Value {
        let (code, json) = self.fk(args);
        assert_eq!(code, 0, "`fawkes {args:?}` failed: {json}");
        json
    }

    pub fn status(&self) -> Value {
        self.ok(&["status"])
    }

    pub fn generation(&self, id: &str) -> Value {
        let history = self.ok(&["history"]);
        history["generations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub fn wait(&self, what: &str, seconds: u64, mut done: impl FnMut() -> bool) {
        let give_up = Instant::now() + Duration::from_secs(seconds);
        while !done() {
            assert!(
                Instant::now() < give_up,
                "timed out waiting for {what}; daemon log:\n{}",
                self.daemon_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }

    /// The body of `GET /` on the live port, or None when nothing answers.
    pub fn get(&self) -> Option<String> {
        let mut stream =
            TcpStream::connect_timeout(&([127, 0, 0, 1], self.port).into(), Duration::from_secs(1))
                .ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
    }

    /// The versions the app has recorded in its storage, one per start.
    pub fn versions(&self) -> Vec<String> {
        let path: &Path = &self.dir.join("home/app.sqlite");
        let Ok(db) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
            return vec![];
        };
        let Ok(mut query) = db.prepare("SELECT version FROM starts") else {
            return vec![];
        };
        query
            .query_map([], |row| row.get(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }
}

/// A followed log (`Harness::follow`): the events it prints, one JSON document per line.
pub struct Follow {
    child: Child,
    lines: Receiver<String>,
}

impl Follow {
    /// The next line as printed, or None when nothing came within `within` (or the command ended).
    pub fn line(&mut self, within: Duration) -> Option<String> {
        self.lines.recv_timeout(within).ok()
    }

    /// The next event; every line must be one JSON document.
    pub fn next(&mut self, within: Duration) -> Option<Value> {
        let line = self.line(within)?;
        Some(
            serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("`log --follow` printed non-JSON ({e}): {line}")),
        )
    }

    /// Read events until `found` accepts one, and return it; the events passed over are dropped.
    pub fn until(&mut self, what: &str, seconds: u64, found: impl Fn(&Value) -> bool) -> Value {
        let give_up = Instant::now() + Duration::from_secs(seconds);
        let mut passed = Vec::new();
        // None: the time is up, or the command ended and nothing more will come.
        while let Some(left) = give_up.checked_duration_since(Instant::now())
            && let Some(event) = self.next(left)
        {
            if found(&event) {
                return event;
            }
            passed.push(event.to_string());
        }
        panic!(
            "timed out waiting for {what} in the followed log; events passed over:\n{}",
            passed.join("\n")
        );
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            // The pid is our own child's, which is not reaped yet, so it cannot name another process.
            let _ = kill_process(Pid::from_child(&daemon), Signal::TERM);
            let give_up = Instant::now() + Duration::from_secs(10);
            while daemon.try_wait().ok().flatten().is_none() && Instant::now() < give_up {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        // The daemon stops its app; if it died abnormally, a leftover app must not keep the port.
        if let Ok(text) = std::fs::read_to_string(self.dir.join("s/app.pid"))
            && let Ok(pid) = text.trim().parse::<i32>()
            && pid > 1
            && let Some(group) = Pid::from_raw(pid)
        {
            // `pid > 1` is neither our own group nor init's; the group is the app's, which must not outlive the test.
            let _ = kill_process_group(group, Signal::KILL);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
