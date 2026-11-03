//! A throwaway Fawkes world for integration tests: a git repo of the sample app, a manifest, a daemon.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const APP_JS: &str = include_str!("../../examples/sample/app.js");
pub const BIN: &str = env!("CARGO_BIN_EXE_fawkes");

pub struct Harness {
    pub dir: PathBuf,
    pub port: u16,
    manifest: PathBuf,
    daemon: Option<Child>,
}

impl Harness {
    /// A world under `/tmp/fk<port>` (short: socket paths are limited) with live port `port` and spare
    /// ports `port+1..=port+9`. `extra` is added to the manifest's top-level keys.
    pub fn new(port: u16, extra: &str) -> Harness {
        let dir = PathBuf::from(format!("/tmp/fk{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let h = Harness {
            manifest: dir.join("fawkes.toml"),
            dir,
            port,
            daemon: None,
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

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
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
        let log = std::fs::File::create(self.dir.join("daemon.log")).unwrap();
        let child = Command::new(BIN)
            .args(["daemon", "--manifest"])
            .arg(&self.manifest)
            .env("FAWKES_HOME", self.manifest.parent().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        self.wait("the daemon to answer", 10, || self.fk(&["status"]).0 == 0);
    }

    /// Run `fawkes --json <args>`; the exit code and the single JSON document it printed.
    pub fn fk(&self, args: &[&str]) -> (i32, Value) {
        let output = Command::new(BIN)
            .arg("--json")
            .arg("--manifest")
            .arg(&self.manifest)
            .args(args)
            .env_remove("FAWKES_SOCKET")
            .output()
            .unwrap();
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

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
            let give_up = Instant::now() + Duration::from_secs(10);
            while daemon.try_wait().ok().flatten().is_none() && Instant::now() < give_up {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        // The daemon stops its app; if it died abnormally, a leftover app must not keep the port.
        if let Ok(text) = std::fs::read_to_string(self.dir.join("s/state.json"))
            && let Ok(state) = serde_json::from_str::<Value>(&text)
            && let Some(pid) = state["app_pid"].as_i64()
        {
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
