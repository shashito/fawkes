//! state.json (the source of truth), events.jsonl, on-disk paths and UTC timestamps.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
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

/// The inverse of `format_time`: only the exact text it produces, so out-of-range fields and other spellings of
/// a time (`2026-02-30`, `T24:00:00`, `11-3`) are refused.
pub fn parse_time(text: &str) -> Option<u64> {
    let secs = parse_fields(text)?;
    (format_time(secs) == text).then_some(secs)
}

/// The arithmetic behind `parse_time`, which accepts any field values.
fn parse_fields(text: &str) -> Option<u64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    // Keeps the arithmetic below far from overflow; `format_time` writes four-digit years.
    if !(0..=9999).contains(&year) {
        return None;
    }
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
    pub fn lock(&self) -> PathBuf {
        self.root.join("lock")
    }
    /// Written by the live app's start wrapper (`process::spawn`), so a later daemon can find a leftover app.
    pub fn app_pid(&self) -> PathBuf {
        self.root.join("app.pid")
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
    /// The install log of the latest stage that failed; its generation is gone.
    pub fn failed_install_log(&self) -> PathBuf {
        self.root.join("install-failed.log")
    }
}

// --------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenState {
    /// The worktree and install are in progress.
    Staging,
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
            GenState::Staging => "staging",
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub generations: Vec<Generation>,
    #[serde(default)]
    pub next_id: u32,
    pub permanent: Option<String>,
    pub pending: Option<Pending>,
    pub daemon_pid: Option<u32>,
}

/// `state.json.bak` next to `state.json`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    name.into()
}

/// Write `bytes` to `path` so that any crash, a power loss included, leaves the old or the new file whole: a temp
/// file, synced, renamed into place, then the directory synced so the rename is on disk too.
pub fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(dir)?.sync_all()
}

impl State {
    /// Read `state.json`, or its copy `state.json.bak` when `state.json` is unreadable. With the copy, the second
    /// value says what was wrong with `state.json`, for the daemon to report.
    pub fn load(path: &Path) -> Result<(State, Option<String>), String> {
        let backup = backup_path(path);
        let problem = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(state) => return Ok((state, None)),
                Err(e) => format!("{} is corrupt ({e})", path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !backup.exists() => {
                return Ok((State::default(), None));
            }
            Err(e) => format!("cannot read {} ({e})", path.display()),
        };
        let unusable = |e: String| {
            format!(
                "{problem} and its copy {} is unusable ({e}); repair one of them, or delete both to start over without any generation",
                backup.display()
            )
        };
        let bytes = std::fs::read(&backup).map_err(|e| unusable(e.to_string()))?;
        let state = serde_json::from_slice(&bytes).map_err(|e| unusable(e.to_string()))?;
        Ok((
            state,
            Some(format!("{problem}; recovered from {}", backup.display())),
        ))
    }

    /// Write `state.json` durably, then the same content to `state.json.bak`. A crash leaves at least one of them
    /// whole, and `load` takes `state.json` whenever it can.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        write_durably(path, &bytes)?;
        write_durably(&backup_path(path), &bytes)
    }

    pub fn find(&self, id: &str) -> Option<&Generation> {
        self.generations.iter().find(|g| g.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Generation> {
        self.generations.iter_mut().find(|g| g.id == id)
    }

    /// The pending generation is already rolled back: only its storage restore failed and awaits a retry.
    pub fn restore_failed(&self, pending: &Pending) -> bool {
        self.find(&pending.generation)
            .is_some_and(|g| g.state == GenState::RolledBack)
    }

    /// The generations `prune` removes so that `keep` remain: the oldest first, never the permanent one, the pending
    /// one or the one it rolls back to (a rollback needs its code and the pending one's snapshot), nor one still
    /// staging. More than `keep` remain when those alone are more.
    pub fn prunable(&self, keep: usize) -> Vec<String> {
        let protected = |g: &Generation| {
            g.state == GenState::Staging
                || self.permanent.as_deref() == Some(g.id.as_str())
                || self.pending.as_ref().is_some_and(|p| {
                    p.generation == g.id || p.previous.as_deref() == Some(g.id.as_str())
                })
        };
        self.generations
            .iter()
            .filter(|g| !protected(g))
            .take(self.generations.len().saturating_sub(keep))
            .map(|g| g.id.clone())
            .collect()
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

/// How large `events.jsonl` grows before it becomes `events.1.jsonl` (replacing the one before).
const EVENT_LOG_BYTES: u64 = 10 << 20;

/// How many of the newest events stay in memory, which is as far back as `tail` reaches.
pub const RECENT_EVENTS: usize = 1000;

/// events.jsonl, append-only and rotated by size. The newest events are also kept in memory, so `log` and every
/// `subscribe` answer without reading the file.
pub struct EventLog {
    path: PathBuf,
    file: File,
    bytes: u64,
    limit: u64,
    recent: VecDeque<Event>,
}

/// `events.1.jsonl` next to `events.jsonl`.
fn rotated_path(path: &Path) -> PathBuf {
    path.with_extension("1.jsonl")
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl EventLog {
    pub fn open(path: PathBuf) -> std::io::Result<EventLog> {
        EventLog::with_limit(path, EVENT_LOG_BYTES)
    }

    fn with_limit(path: PathBuf, limit: u64) -> std::io::Result<EventLog> {
        let file = open_append(&path)?;
        let bytes = file.metadata()?.len();
        let mut recent = VecDeque::with_capacity(RECENT_EVENTS);
        for file in [rotated_path(&path), path.clone()] {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            for event in text.lines().filter_map(|l| serde_json::from_str(l).ok()) {
                if recent.len() == RECENT_EVENTS {
                    recent.pop_front();
                }
                recent.push_back(event);
            }
        }
        Ok(EventLog {
            path,
            file,
            bytes,
            limit,
            recent,
        })
    }

    /// Append one event to the file at `path` without keeping it open: for the panic hook, which must not wait for
    /// the daemon's log, and must not read it either.
    pub fn append_once(path: &Path, event: &Event) {
        if let Ok(mut file) = open_append(path) {
            // Best effort: the process is about to abort.
            let _ = writeln!(file, "{}", serde_json::to_string(event).unwrap_or_default());
        }
    }

    pub fn append(&mut self, event: &Event) {
        let line = format!("{}\n", serde_json::to_string(event).unwrap_or_default());
        // A full disk must not take the supervisor down; the event still reaches live clients.
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.bytes += line.len() as u64;
        }
        if self.recent.len() == RECENT_EVENTS {
            self.recent.pop_front();
        }
        self.recent.push_back(event.clone());
        if self.bytes >= self.limit {
            self.rotate();
        }
    }

    /// Move the full file aside and start a new one. On failure the log keeps growing and the next event tries again.
    fn rotate(&mut self) {
        if std::fs::rename(&self.path, rotated_path(&self.path)).is_ok()
            && let Ok(file) = open_append(&self.path)
        {
            self.file = file;
            self.bytes = 0;
        }
    }

    /// The last `n` events (at most `RECENT_EVENTS`), oldest first.
    pub fn tail(&self, n: usize) -> Vec<Event> {
        self.recent
            .iter()
            .skip(self.recent.len().saturating_sub(n))
            .cloned()
            .collect()
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

    #[test]
    fn prune_takes_the_oldest_and_spares_what_a_rollback_needs() {
        let generation = |id: &str, state| Generation {
            id: id.into(),
            git_ref: "main".into(),
            commit: "0".into(),
            state,
            dir: PathBuf::new(),
            staged_at: String::new(),
            test: None,
            switched_at: None,
            confirmed_at: None,
            rolled_back_at: None,
            rollback_reason: None,
            failed_starts: 0,
        };
        let state = State {
            generations: vec![
                generation("g1", GenState::Permanent),
                generation("g2", GenState::RolledBack),
                generation("g3", GenState::Current),
                generation("g4", GenState::Tested),
                generation("g5", GenState::Staging),
            ],
            permanent: Some("g1".into()),
            pending: Some(Pending {
                generation: "g3".into(),
                previous: Some("g1".into()),
                deadline: String::new(),
            }),
            ..State::default()
        };
        assert_eq!(state.prunable(4), ["g2"]);
        assert_eq!(state.prunable(1), ["g2", "g4"]);
        assert_eq!(state.prunable(5), Vec::<String>::new());
    }

    #[test]
    fn the_event_log_rotates_by_size_and_tails_from_memory() {
        let root = std::env::temp_dir().join(format!("fk-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("events.jsonl");
        let event = |n: usize| Event {
            at: now(),
            kind: "progress".into(),
            generation: None,
            message: format!("event {n}"),
        };
        let mut log = EventLog::with_limit(path.clone(), 4096).unwrap();
        for n in 0..RECENT_EVENTS + 10 {
            log.append(&event(n));
        }
        assert!(std::fs::metadata(&path).unwrap().len() < 4096);
        assert!(std::fs::metadata(rotated_path(&path)).unwrap().len() >= 4096);
        let tail = log.tail(usize::MAX);
        assert_eq!(tail.len(), RECENT_EVENTS);
        assert_eq!(
            tail.last().unwrap().message,
            format!("event {}", RECENT_EVENTS + 9)
        );

        // A new daemon's log starts from what both files still hold.
        drop(log);
        let reopened = EventLog::with_limit(path, 4096).unwrap();
        let tail = reopened.tail(3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[2].message, format!("event {}", RECENT_EVENTS + 9));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parse_time_refuses_anything_but_the_canonical_form() {
        for text in [
            "2026-13-45T99:99:99Z",
            "2026-02-30T00:00:00Z",
            "2026-11-03T24:00:00Z",
            "2026-11-3T14:00:01Z",
            "1969-12-31T23:59:59Z",
            "99999999999999999-01-01T00:00:00Z",
            "",
        ] {
            assert_eq!(parse_time(text), None, "{text}");
        }
    }

    #[test]
    fn load_falls_back_to_the_copy_when_state_json_is_unreadable() {
        let root = std::env::temp_dir().join(format!("fk-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("state.json");
        assert!(State::load(&path).unwrap().0.permanent.is_none());

        let state = State {
            permanent: Some("g2".into()),
            next_id: 2,
            ..State::default()
        };
        state.save(&path).unwrap();
        let (loaded, warning) = State::load(&path).unwrap();
        assert_eq!((loaded.permanent.as_deref(), warning), (Some("g2"), None));

        // What a power loss before the rename reached the disk could leave: an empty state.json.
        std::fs::write(&path, "").unwrap();
        let (loaded, warning) = State::load(&path).unwrap();
        assert_eq!(loaded.permanent.as_deref(), Some("g2"));
        assert!(warning.unwrap().contains("recovered from"));

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            State::load(&path).unwrap().0.permanent.as_deref(),
            Some("g2"),
            "a missing state.json next to its copy is not a fresh start"
        );

        std::fs::write(&path, "{").unwrap();
        std::fs::write(backup_path(&path), "").unwrap();
        assert!(State::load(&path).unwrap_err().contains("delete both"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
