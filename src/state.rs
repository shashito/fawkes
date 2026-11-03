//! state.json (the source of truth), events.jsonl, on-disk paths and UTC timestamps.

use crate::protocol::{Error, Event, GenState, Generation, Pending, R, invalid_state, not_found};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// The transaction step in flight, saved to state.json before each of its side effects, so a daemon that dies
/// anywhere inside a switch or a rollback finds out at its next start what it was doing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Txn {
    /// The live app is being stopped and the storage snapshotted for a switch to `generation`; no process of it has
    /// run yet. Written before the old app is stopped.
    Switching {
        generation: String,
        previous: Option<String>,
    },
    /// The switch is live and awaits confirm by `deadline`. Written before the target is started.
    Pending(Pending),
    /// The pending switch is being rolled back for `reason`. Written, together with the generation's `rolled_back`
    /// state, before the target is stopped. `restore_failed`: the restore (or the save that ends the switch)
    /// failed; the app stays stopped until `rollback` or the next daemon start retries.
    RollingBack {
        pending: Pending,
        reason: String,
        restore_failed: bool,
    },
}

impl Txn {
    /// The generation the step switches to or rolls back.
    pub fn generation(&self) -> &str {
        match self {
            Txn::Switching { generation, .. } => generation,
            Txn::Pending(pending) | Txn::RollingBack { pending, .. } => &pending.generation,
        }
    }

    /// The generation the step returns to.
    fn previous(&self) -> Option<&str> {
        match self {
            Txn::Switching { previous, .. } => previous.as_deref(),
            Txn::Pending(pending) | Txn::RollingBack { pending, .. } => pending.previous.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "StateFile")]
pub struct State {
    pub generations: Vec<Generation>,
    pub next_id: u32,
    pub permanent: Option<String>,
    pub txn: Option<Txn>,
    pub daemon_pid: Option<u32>,
}

/// What `state.json` may hold: the current shape and the one before `txn`, which kept a `pending` switch and
/// derived a failed restore from its generation being `rolled_back`.
#[derive(Deserialize)]
struct StateFile {
    #[serde(default)]
    generations: Vec<Generation>,
    #[serde(default)]
    next_id: u32,
    permanent: Option<String>,
    txn: Option<Txn>,
    pending: Option<Pending>,
    daemon_pid: Option<u32>,
}

impl From<StateFile> for State {
    fn from(file: StateFile) -> State {
        let StateFile {
            generations,
            next_id,
            permanent,
            txn,
            pending,
            daemon_pid,
        } = file;
        let txn = txn.or_else(|| {
            let pending = pending?;
            let rolled_back = generations
                .iter()
                .find(|g| g.id == pending.generation && g.state == GenState::RolledBack);
            Some(match rolled_back {
                Some(g) => Txn::RollingBack {
                    reason: g
                        .rollback_reason
                        .clone()
                        .unwrap_or_else(|| "no reason recorded".into()),
                    pending,
                    restore_failed: true,
                },
                None => Txn::Pending(pending),
            })
        });
        State {
            generations,
            next_id,
            permanent,
            txn,
            daemon_pid,
        }
    }
}

/// The edges of the generation diagram (DESIGN.md → Generations and states). A stage that fails and `prune` remove
/// the record instead, which is not a transition; nothing leaves `rolled_back`, and nothing returns to `staging`.
pub fn allowed(from: GenState, to: GenState) -> bool {
    use GenState::*;
    matches!(
        (from, to),
        (Staging, Staged)
            | (Staged | TestFailed | Tested | Retired, Tested | TestFailed)
            | (Tested, Current)
            | (Current, Permanent | RolledBack)
            | (Permanent, Retired)
    )
}

/// Why a generation in this state cannot become `to`: the answer to a refused test (`tested`), switch (`current`)
/// or confirm (`permanent`).
pub fn refusal(g: &Generation, to: GenState) -> Error {
    let id = &g.id;
    let state = g.state.as_str();
    invalid_state(match (g.state, to) {
        (GenState::RolledBack, _) => format!(
            "{id} was rolled back at {} ({}); stage the ref again for a new generation",
            g.rolled_back_at.as_deref().unwrap_or("?"),
            g.rollback_reason.as_deref().unwrap_or("no reason recorded")
        ),
        (GenState::Staging, _) => {
            format!("{id} is still being staged; try again when its stage has finished")
        }
        (GenState::Staged, GenState::Current) => {
            format!("{id} is staged, not tested; run `fawkes test {id}` first")
        }
        (GenState::TestFailed, GenState::Current) => {
            format!(
                "{id} failed its last test; run `fawkes test {id}` again or stage the ref again"
            )
        }
        (GenState::Retired, GenState::Current) => {
            format!("{id} is retired; run `fawkes test {id}` first")
        }
        (
            GenState::Current | GenState::Permanent,
            GenState::Tested | GenState::TestFailed | GenState::Current,
        ) => format!("{id} is {state}, it already is the live generation"),
        (_, GenState::Permanent) => {
            format!("{id} is {state}, not pending; there is nothing to confirm")
        }
        (_, to) => format!("{id} is {state}, it cannot become {}", to.as_str()),
    })
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
    fn save(&self, path: &Path) -> std::io::Result<()> {
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

    /// The switch that is live and not yet confirmed, also while it is being rolled back or its restore failed.
    pub fn pending(&self) -> Option<&Pending> {
        match &self.txn {
            Some(Txn::Pending(pending) | Txn::RollingBack { pending, .. }) => Some(pending),
            _ => None,
        }
    }

    /// The reason of a rollback that is decided and not yet done, which can no longer be confirmed.
    pub fn rolling_back(&self) -> Option<&str> {
        match &self.txn {
            Some(Txn::RollingBack {
                reason,
                restore_failed: false,
                ..
            }) => Some(reason),
            _ => None,
        }
    }

    /// The only way a generation changes state: refuses an edge `allowed` does not list (`invalid_state`, with the
    /// reason the command gives), otherwise sets the state and the timestamp of the edge. The caller sets
    /// `rollback_reason` and `test`.
    pub fn transition(&mut self, id: &str, to: GenState) -> R<&mut Generation> {
        let g = self.find_mut(id).ok_or_else(|| not_found(id))?;
        if !allowed(g.state, to) {
            return Err(refusal(g, to));
        }
        g.state = to;
        match to {
            GenState::Staged => g.staged_at = now(),
            GenState::Current => g.switched_at = Some(now()),
            GenState::Permanent => g.confirmed_at = Some(now()),
            GenState::RolledBack => g.rolled_back_at = Some(now()),
            _ => {}
        }
        Ok(g)
    }

    /// Decide to roll back the pending switch for `reason`: the generation becomes `rolled_back` and the step
    /// `RollingBack`. Does nothing when no switch is pending, so the first reason sticks and a generation rolls back
    /// at most once. Says whether it decided.
    pub fn decide_rollback(&mut self, reason: &str) -> bool {
        let Some(Txn::Pending(pending)) = self.txn.clone() else {
            return false;
        };
        // A retried rollback finds the generation rolled back already and keeps its first time and reason.
        if self
            .find(&pending.generation)
            .is_some_and(|g| g.state != GenState::RolledBack)
            && let Ok(g) = self.transition(&pending.generation, GenState::RolledBack)
        {
            g.rollback_reason = Some(reason.into());
        }
        self.txn = Some(Txn::RollingBack {
            pending,
            reason: reason.into(),
            restore_failed: false,
        });
        true
    }

    /// The restore of the rollback in flight (or the save that ends it) failed: it awaits a retry.
    pub fn fail_restore(&mut self) {
        if let Some(Txn::RollingBack { restore_failed, .. }) = &mut self.txn {
            *restore_failed = true;
        }
    }

    /// The generations `prune` removes so that `keep` remain: the oldest first, never the permanent one, those of a
    /// transaction step (the one it switches to and the one it returns to: a rollback needs the code of the one
    /// and the snapshot of the other), nor one still staging. More than `keep` remain when those alone are more.
    pub fn prunable(&self, keep: usize) -> Vec<String> {
        let protected = |g: &Generation| {
            g.state == GenState::Staging
                || self.permanent.as_deref() == Some(g.id.as_str())
                || self
                    .txn
                    .as_ref()
                    .is_some_and(|t| t.generation() == g.id || t.previous() == Some(g.id.as_str()))
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
        self.pending()
            .map(|p| p.generation.as_str())
            .or(self.permanent.as_deref())
    }
}

// --------------------------------------------------------------- store

/// Why `Store::modify` kept the old state.
#[derive(Debug)]
pub enum Unchanged {
    /// The change refused itself.
    Refused(Error),
    /// The change was made, but state.json could not be written.
    Unsaved(std::io::Error),
}

/// Owns state.json: the only way to change the state is through it, and every change is saved.
pub struct Store {
    path: PathBuf,
    state: State,
}

impl Store {
    /// `State::load`, with its migration and its fall back to the copy.
    pub fn load(path: PathBuf) -> Result<(Store, Option<String>), String> {
        let (state, recovered) = State::load(&path)?;
        Ok((Store { path, state }, recovered))
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// An intent (a switch, a confirm, the end of a rollback): applied to a copy and kept only once it is on disk,
    /// because a daemon restart acts on what is on disk, not on what was meant. When `change` refuses or the save
    /// fails, nothing changed.
    pub fn modify<T>(&mut self, change: impl FnOnce(&mut State) -> R<T>) -> Result<T, Unchanged> {
        let mut next = self.state.clone();
        let value = change(&mut next).map_err(Unchanged::Refused)?;
        next.save(&self.path).map_err(Unchanged::Unsaved)?;
        self.state = next;
        Ok(value)
    }

    /// Bookkeeping (pids, counters, a test result, a decided rollback): kept in memory even when the save fails,
    /// which is returned for the caller to report; a later save repeats it.
    pub fn update<T>(
        &mut self,
        change: impl FnOnce(&mut State) -> T,
    ) -> (T, Option<std::io::Error>) {
        let value = change(&mut self.state);
        (value, self.state.save(&self.path).err())
    }
}

// -------------------------------------------------------------- events

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ErrorCode, EventKind};

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
            txn: Some(Txn::Pending(Pending {
                generation: "g3".into(),
                previous: Some("g1".into()),
                deadline: String::new(),
            })),
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
            kind: EventKind::Progress,
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

    /// A `state.json` as phase 2 wrote it (`State::save` before `txn`): three generations and the `pending` field
    /// in place of `txn`. `pending` and the state of `g3` vary.
    fn phase_two_state(pending: &str, g3_state: &str, g3_reason: &str) -> String {
        let generation = |id: &str, state: &str, reason: &str| {
            format!(
                r#"    {{
      "id": "{id}",
      "ref": "main",
      "commit": "0123456789abcdef",
      "state": "{state}",
      "dir": "/tmp/fk/generations/{id}/code",
      "staged_at": "2026-11-03T14:00:00Z",
      "test": null,
      "switched_at": null,
      "confirmed_at": null,
      "rolled_back_at": null,
      "rollback_reason": {reason},
      "failed_starts": 0
    }}"#
            )
        };
        format!(
            r#"{{
  "generations": [
{},
{},
{}
  ],
  "next_id": 3,
  "permanent": "g2",
  "pending": {pending},
  "daemon_pid": 812
}}"#,
            generation("g1", "retired", "null"),
            generation("g2", "permanent", "null"),
            generation("g3", g3_state, g3_reason),
        )
    }

    #[test]
    fn a_phase_two_state_json_loads_with_its_pending_switch_as_a_txn() {
        let root = std::env::temp_dir().join(format!("fk-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("state.json");
        let pending = pending_json();
        let load = |text: String| {
            std::fs::write(&path, text).unwrap();
            let (state, warning) = State::load(&path).unwrap();
            assert_eq!(warning, None);
            state
        };

        let state = load(phase_two_state(pending, "current", "null"));
        assert_eq!(state.generations.len(), 3);
        assert!(["g1", "g2", "g3"].iter().all(|id| state.find(id).is_some()));
        assert_eq!(
            (state.next_id, state.permanent.as_deref(), state.daemon_pid),
            (3, Some("g2"), Some(812))
        );
        let Some(Txn::Pending(p)) = &state.txn else {
            panic!("{:?}", state.txn);
        };
        assert_eq!(
            (p.generation.as_str(), p.previous.as_deref()),
            ("g3", Some("g2"))
        );
        assert_eq!(state.current(), Some("g3"));

        // Its generation is rolled back: the switch ended and only the storage restore awaits a retry.
        let state = load(phase_two_state(
            pending,
            "rolled_back",
            r#""missed confirm deadline""#,
        ));
        let Some(Txn::RollingBack {
            pending,
            reason,
            restore_failed,
        }) = &state.txn
        else {
            panic!("{:?}", state.txn);
        };
        assert_eq!(pending.generation, "g3");
        assert_eq!(reason, "missed confirm deadline");
        assert!(restore_failed);

        let state = load(phase_two_state("null", "tested", "null"));
        assert!(state.txn.is_none());
        assert_eq!(state.current(), Some("g2"));

        // Saved, it keeps `txn` only and reads back the same.
        let state = load(phase_two_state(pending_json(), "current", "null"));
        state.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""step": "pending""#) && !text.contains(r#""pending":"#));
        let (again, _) = State::load(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&state).unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn pending_json() -> &'static str {
        r#"{"generation": "g3", "previous": "g2", "deadline": "2026-11-03T14:05:00Z"}"#
    }

    #[test]
    fn the_first_reason_of_a_rollback_sticks() {
        let mut state: State =
            serde_json::from_str(&phase_two_state(pending_json(), "current", "null")).unwrap();
        assert!(state.decide_rollback("health check failed"));
        assert_eq!(state.rolling_back(), Some("health check failed"));
        let g = state.find("g3").unwrap();
        assert_eq!(g.state, GenState::RolledBack);
        assert_eq!(g.rollback_reason.as_deref(), Some("health check failed"));
        let at = g.rolled_back_at.clone();

        assert!(!state.decide_rollback("rollback requested"));
        assert_eq!(state.rolling_back(), Some("health check failed"));
        assert_eq!(state.find("g3").unwrap().rolled_back_at, at);
        assert_eq!(state.pending().map(|p| p.generation.as_str()), Some("g3"));

        state.fail_restore();
        assert_eq!(state.rolling_back(), None);
        assert!(state.pending().is_some());
    }

    #[test]
    fn a_generation_changes_state_only_along_the_edges_of_the_diagram() {
        use GenState::*;
        let all = [
            Staging, Staged, TestFailed, Tested, Current, Permanent, Retired, RolledBack,
        ];
        let legal = [
            (Staging, Staged),
            (Staged, Tested),
            (Staged, TestFailed),
            (TestFailed, Tested),
            (TestFailed, TestFailed),
            (Tested, Tested),
            (Tested, TestFailed),
            (Retired, Tested),
            (Retired, TestFailed),
            (Tested, Current),
            (Current, Permanent),
            (Current, RolledBack),
            (Permanent, Retired),
        ];
        for from in all {
            for to in all {
                let mut state = State {
                    generations: vec![Generation {
                        id: "g1".into(),
                        git_ref: "main".into(),
                        commit: "0".into(),
                        state: from,
                        dir: PathBuf::new(),
                        staged_at: "then".into(),
                        test: None,
                        switched_at: None,
                        confirmed_at: None,
                        rolled_back_at: None,
                        rollback_reason: None,
                        failed_starts: 0,
                    }],
                    ..State::default()
                };
                let before = serde_json::to_value(&state).unwrap();
                let result = state.transition("g1", to);
                let case = format!("{from:?} -> {to:?}");
                assert_eq!(allowed(from, to), legal.contains(&(from, to)), "{case}");
                if legal.contains(&(from, to)) {
                    let g = result.unwrap_or_else(|e| panic!("{case}: {}", e.message));
                    assert_eq!(g.state, to, "{case}");
                    let stamped = match to {
                        Staged => g.staged_at != "then",
                        Current => g.switched_at.is_some(),
                        Permanent => g.confirmed_at.is_some(),
                        RolledBack => g.rolled_back_at.is_some(),
                        _ => true,
                    };
                    assert!(stamped, "{case} sets its timestamp");
                } else {
                    let e = result.err().unwrap_or_else(|| panic!("{case} was allowed"));
                    assert_eq!(e.code, ErrorCode::InvalidState, "{case}");
                    assert_eq!(serde_json::to_value(&state).unwrap(), before, "{case}");
                }
            }
        }
        assert_eq!(
            State::default()
                .transition("g9", Staged)
                .err()
                .map(|e| e.code),
            Some(ErrorCode::NotFound)
        );
    }

    #[test]
    fn a_store_keeps_an_intent_only_once_it_is_on_disk() {
        let root = std::env::temp_dir().join(format!("fk-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("state.json");
        let (mut store, warning) = Store::load(path.clone()).unwrap();
        assert!(warning.is_none());

        store
            .modify(|state| {
                state.next_id = 7;
                Ok(())
            })
            .unwrap();
        assert_eq!(Store::load(path).unwrap().0.state().next_id, 7);

        // A directory that does not exist: nothing can be written there.
        let (mut store, _) = Store::load(root.join("missing").join("state.json")).unwrap();
        let saved = store.modify(|state| {
            state.next_id = 1;
            Ok(())
        });
        assert!(matches!(saved, Err(Unchanged::Unsaved(_))));
        assert_eq!(store.state().next_id, 0);

        let refused = store.modify(|state| -> R<()> {
            state.next_id = 2;
            Err(not_found("g9"))
        });
        assert!(matches!(refused, Err(Unchanged::Refused(_))));
        assert_eq!(store.state().next_id, 0);

        let (value, failed) = store.update(|state| {
            state.next_id = 3;
            "done"
        });
        assert_eq!((value, store.state().next_id), ("done", 3));
        assert!(failed.is_some());
        std::fs::remove_dir_all(&root).unwrap();
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
