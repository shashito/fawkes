//! state.json (the source of truth), events.jsonl, on-disk paths and UTC timestamps.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- time

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn now() -> String {
    format_time(now_secs())
}

/// `2026-11-03T14:00:01Z`, via days-to-civil (Howard Hinnant's algorithm).
pub fn format_time(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rest = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The inverse of `format_time`.
pub fn parse_time(text: &str) -> Option<u64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (t.next()??, t.next()??, t.next()??);
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + h * 3600 + m * 60 + s).ok()
}

// --------------------------------------------------------------- paths

pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn socket(&self) -> PathBuf {
        self.root.join("fawkes.sock")
    }
    pub fn state_json(&self) -> PathBuf {
        self.root.join("state.json")
    }
    pub fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }
    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }
    pub fn generation(&self, id: &str) -> PathBuf {
        self.root.join("generations").join(id)
    }
    pub fn code(&self, id: &str) -> PathBuf {
        self.generation(id).join("code")
    }
    pub fn snapshot(&self, id: &str) -> PathBuf {
        self.generation(id).join("snapshot")
    }
    pub fn test_home(&self, id: &str) -> PathBuf {
        self.generation(id).join("test-home")
    }
    pub fn install_log(&self, id: &str) -> PathBuf {
        self.generation(id).join("logs").join("install.log")
    }
    pub fn app_log(&self, id: &str) -> PathBuf {
        self.generation(id).join("logs").join("app.log")
    }
}

// --------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenState {
    Staged,
    TestFailed,
    Tested,
    Current,
    Permanent,
    Retired,
    RolledBack,
}

impl GenState {
    pub fn as_str(self) -> &'static str {
        match self {
            GenState::Staged => "staged",
            GenState::TestFailed => "test_failed",
            GenState::Tested => "tested",
            GenState::Current => "current",
            GenState::Permanent => "permanent",
            GenState::Retired => "retired",
            GenState::RolledBack => "rolled_back",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestInfo {
    pub port: u16,
    pub passed: bool,
    pub at: String,
    pub seconds: f64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Generation {
    pub id: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub commit: String,
    pub state: GenState,
    pub dir: PathBuf,
    pub staged_at: String,
    pub test: Option<TestInfo>,
    pub switched_at: Option<String>,
    pub confirmed_at: Option<String>,
    pub rolled_back_at: Option<String>,
    pub rollback_reason: Option<String>,
    pub failed_starts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub generation: String,
    pub previous: Option<String>,
    pub deadline: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub generations: Vec<Generation>,
    #[serde(default)]
    pub next_id: u32,
    pub permanent: Option<String>,
    pub pending: Option<Pending>,
    pub daemon_pid: Option<u32>,
    /// Pid (= process group) of the live app, so a restarted daemon can kill a leftover one.
    pub app_pid: Option<u32>,
}

impl State {
    pub fn load(path: &Path) -> Result<State, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("corrupt {}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Atomic write: temp file, then rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn find(&self, id: &str) -> Option<&Generation> {
        self.generations.iter().find(|g| g.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Generation> {
        self.generations.iter_mut().find(|g| g.id == id)
    }

    /// The generation `start`, `stop` and `restart` act on: the pending one, else the permanent one.
    pub fn current(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .map(|p| p.generation.as_str())
            .or(self.permanent.as_deref())
    }
}

// -------------------------------------------------------------- events

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: String,
    pub kind: String,
    pub generation: Option<String>,
    pub message: String,
}

/// events.jsonl, append-only.
pub struct EventLog {
    path: PathBuf,
    file: File,
}

impl EventLog {
    pub fn open(path: PathBuf) -> std::io::Result<EventLog> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(EventLog { path, file })
    }

    pub fn append(&mut self, event: &Event) {
        // A full disk must not take the supervisor down; the event still reaches live clients.
        let _ = writeln!(
            self.file,
            "{}",
            serde_json::to_string(event).unwrap_or_default()
        );
    }

    /// The last `n` events, oldest first.
    pub fn tail(&self, n: usize) -> Vec<Event> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let events: Vec<Event> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        events[events.len().saturating_sub(n)..].to_vec()
    }
}

// ----------------------------------------------------------- last exit

/// How the app's most recent process ended, and whether Fawkes asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct LastExit {
    pub at: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub requested: bool,
}

impl LastExit {
    pub fn new(status: Option<ExitStatus>, requested: bool) -> LastExit {
        LastExit {
            at: now(),
            code: status.and_then(|s| s.code()),
            signal: status.and_then(|s| s.signal()),
            requested,
        }
    }

    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exit status {code}"),
            (_, Some(signal)) => format!("signal {signal}"),
            _ => "unknown exit".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_round_trips() {
        for secs in [
            0,
            951_782_400, /* 2000-02-29 */
            1_793_714_521,
            4_102_444_799,
        ] {
            assert_eq!(parse_time(&format_time(secs)), Some(secs));
        }
        assert_eq!(format_time(1_793_714_521), "2026-11-03T14:02:01Z");
    }
}
