//! state.json (the source of truth), events.jsonl, on-disk paths and UTC timestamps.

use crate::protocol::{
    Error, Event, EventKind, GenState, Generation, Pending, R, RollbackCause, Step, invalid_state,
    not_found,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// `2026-11-03T14:00:01Z`: whole seconds, in UTC. A time past the end of the year 9999, which Fawkes never computes,
/// is written as the latest one jiff represents.
pub fn format_time(secs: u64) -> String {
    let secs = i64::try_from(secs)
        .unwrap_or(i64::MAX)
        .min(Timestamp::MAX.as_second());
    Timestamp::from_second(secs).unwrap_or_default().to_string()
}

/// The inverse of `format_time`: only the exact text it produces, so out-of-range fields and other spellings of
/// a time (`2026-02-30`, `T24:00:00`, `11-3`, an offset, a fraction) are refused.
pub fn parse_time(text: &str) -> Option<u64> {
    let secs = u64::try_from(text.parse::<Timestamp>().ok()?.as_second()).ok()?;
    (format_time(secs) == text).then_some(secs)
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
    /// The same for the install, test or `test_copy` hook in progress (one step runs at a time).
    pub fn step_pid(&self) -> PathBuf {
        self.root.join("step.pid")
    }
    /// The same for the `snapshot` or `restore` hook in progress. A file of its own, because an automatic rollback
    /// restores beside a stage or test, which holds `step.pid`.
    pub fn hook_pid(&self) -> PathBuf {
        self.root.join("hook.pid")
    }
    pub fn state_json(&self) -> PathBuf {
        self.root.join("state.json")
    }
    pub fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }
    /// The daemon's stderr when `fawkes service install` runs it; capped when it is (`Echo`).
    pub fn daemon_log(&self) -> PathBuf {
        self.root.join("daemon.log")
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
    /// Output of the storage hooks run for generation `id`: the snapshot of its switch, the restore of its
    /// rollback, the copy of its tests.
    pub fn hook_log(&self, id: &str) -> PathBuf {
        self.generation(id).join("logs").join("hooks.log")
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
    /// The live app is being stopped and its storage snapshotted for a switch to `generation`; no process of it has
    /// run yet. Written before the old app is stopped.
    Switching {
        generation: String,
        previous: Option<String>,
    },
    /// The switch is live and awaits confirm by `deadline`. Written before the target is started.
    Pending(Pending),
    /// The pending switch is being rolled back for `reason`, which `cause` gives as a code (null in a state.json
    /// written before it existed). Written, together with the generation's `rolled_back` state, before the target is
    /// stopped. `restore_failed`: the restore (or the save that ends the switch) failed; the app stays stopped until
    /// `rollback` or the next daemon start retries.
    RollingBack {
        pending: Pending,
        reason: String,
        #[serde(default)]
        cause: Option<RollbackCause>,
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
#[serde(default)]
pub struct State {
    pub generations: Vec<Generation>,
    pub next_id: u32,
    pub permanent: Option<String>,
    pub txn: Option<Txn>,
}

/// The edges of the generation diagram (DESIGN.md → Generations and states). A stage that fails and `prune` remove
/// the record instead, which is not a transition; nothing leaves `rolled_back`, and nothing returns to `staging`.
pub fn allowed(from: GenState, to: GenState) -> bool {
    use GenState::{Current, Permanent, Retired, RolledBack, Staged, Staging, TestFailed, Tested};
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
    .about(id)
}

/// `path` with `suffix` appended to its last component: `state.json` and `.bak` give `state.json.bak`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// `e`, saying which step of a save it ended.
fn late_failure(step: &str, e: &std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("could not {step}: {e}"))
}

/// `state.json.bak` next to `state.json`.
fn backup_path(path: &Path) -> PathBuf {
    with_suffix(path, ".bak")
}

/// Write `bytes` to `path` so that any crash, a power loss included, leaves the old or the new file whole: a temp
/// file, synced, renamed into place. Once this returns, `path` holds `bytes` for every reader; only the rename's
/// durability is still open (`sync_directory`).
fn write_in_place(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = with_suffix(path, ".tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Sync the directory of `path` so a rename into it is on disk too.
fn sync_directory(path: &Path) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(dir)?.sync_all()
}

/// `write_in_place`, then `sync_directory`. A failure of the second step is an error too: this is for files whose
/// only meaning is being durable (`state.json.bak`), where the caller has no earlier state to agree with.
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_in_place(path, bytes)?;
    sync_directory(path)
}

impl State {
    /// Read `state.json`, or its copy `state.json.bak` when `state.json` is unreadable. With the copy, the second
    /// value says what was wrong with `state.json`, for the daemon to report.
    pub fn load(path: &Path) -> Result<(State, Option<String>), String> {
        let backup = backup_path(path);
        let problem = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<State>(&bytes) {
                Ok(state) => return state.known().map(|state| (state, None)),
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
        let state = serde_json::from_slice::<State>(&bytes).map_err(|e| unusable(e.to_string()))?;
        Ok((
            state.known()?,
            Some(format!("{problem}; recovered from {}", backup.display())),
        ))
    }

    /// `self`, unless a generation is in a state this fawkes cannot read (`unknown`): state.json was written by a
    /// newer fawkes. Rolling such a generation back, pruning it or leaving it running would be a guess, so the daemon
    /// does not start. An older copy is no way around it.
    fn known(self) -> Result<State, String> {
        match self
            .generations
            .iter()
            .find(|g| g.state == GenState::Unknown)
        {
            Some(g) => Err(format!(
                "{} is in a state this fawkes does not know, so state.json was written by a newer fawkes; run the newer fawkes that wrote it, or upgrade this binary",
                g.id
            )),
            None => Ok(self),
        }
    }

    /// Write `state.json` durably, then the same content to `state.json.bak`. A crash leaves at least one of them
    /// whole, and `load` takes `state.json` whenever it can.
    ///
    /// `Err` means `state.json` is not in place: a restart reads what it read before. `Ok(Some(e))` means it is in
    /// place, and so is what a restart reads, but a later step failed (the directory sync or the copy); `e` names it.
    fn save(&self, path: &Path) -> std::io::Result<Option<std::io::Error>> {
        let bytes = serde_json::to_vec_pretty(self)?;
        write_in_place(path, &bytes)?;
        let late = sync_directory(path)
            .map_err(|e| late_failure("sync the directory of state.json", &e))
            .and_then(|()| {
                write_durably(&backup_path(path), &bytes)
                    .map_err(|e| late_failure("write state.json.bak", &e))
            });
        Ok(late.err())
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

    /// Where the transaction in flight is, for `status`.
    pub fn step(&self) -> Option<Step> {
        Some(match self.txn.as_ref()? {
            Txn::Switching { .. } => Step::Switching,
            Txn::Pending(_) => Step::Pending,
            Txn::RollingBack {
                restore_failed: false,
                ..
            } => Step::RollingBack,
            Txn::RollingBack {
                restore_failed: true,
                ..
            } => Step::RestoreFailed,
        })
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
    /// `rollback_reason`, `rollback_cause` and `test`.
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

    /// Decide to roll back the pending switch for `reason`, which `cause` gives as a code: the generation becomes
    /// `rolled_back` and the step `RollingBack`. Does nothing when no switch is pending, so the first reason sticks
    /// and a generation rolls back at most once. Says whether it decided.
    pub fn decide_rollback(&mut self, reason: &str, cause: Option<RollbackCause>) -> bool {
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
            g.rollback_cause = cause;
        }
        self.txn = Some(Txn::RollingBack {
            pending,
            reason: reason.into(),
            cause,
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
    /// `State::load`, with its fall back to the copy.
    pub fn load(path: PathBuf) -> Result<(Store, Option<String>), String> {
        let (state, recovered) = State::load(&path)?;
        Ok((Store { path, state }, recovered))
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// An intent (a switch, a confirm, the end of a rollback): applied to a copy and kept once state.json is on
    /// disk, because a daemon restart acts on what is on disk, not on what was meant. When `change` refuses or
    /// state.json cannot be written, nothing changed. When state.json is in place but a later step of the save
    /// failed, the change stands, as it does for a restart, and that failure comes back next to the value.
    pub fn modify<T>(
        &mut self,
        change: impl FnOnce(&mut State) -> R<T>,
    ) -> Result<(T, Option<std::io::Error>), Unchanged> {
        let mut next = self.state.clone();
        let value = change(&mut next).map_err(Unchanged::Refused)?;
        let late = next.save(&self.path).map_err(Unchanged::Unsaved)?;
        self.state = next;
        Ok((value, late))
    }

    /// Bookkeeping (pids, counters, a test result, a decided rollback): kept in memory even when the save fails,
    /// which is returned for the caller to report; a later save repeats it.
    pub fn update<T>(
        &mut self,
        change: impl FnOnce(&mut State) -> T,
    ) -> (T, Option<std::io::Error>) {
        let value = change(&mut self.state);
        let failed = self
            .state
            .save(&self.path)
            .unwrap_or_else(|e| Some(late_failure("write state.json", &e)));
        (value, failed)
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
    /// The `seq` of the newest event, written or about to be. Shared with the panic hook, which numbers its event
    /// without taking the log.
    last_seq: Arc<AtomicU64>,
}

/// `events.1.jsonl` next to `events.jsonl`.
fn rotated_path(path: &Path) -> PathBuf {
    path.with_extension("1.jsonl")
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Keep `event` among the newest `RECENT_EVENTS`, forgetting the oldest.
fn remember(recent: &mut VecDeque<Event>, event: Event) {
    if recent.len() == RECENT_EVENTS {
        recent.pop_front();
    }
    recent.push_back(event);
}

impl EventLog {
    pub fn open(path: PathBuf) -> std::io::Result<EventLog> {
        EventLog::with_limit(path, EVENT_LOG_BYTES)
    }

    fn with_limit(path: PathBuf, limit: u64) -> std::io::Result<EventLog> {
        let file = open_append(&path)?;
        let bytes = file.metadata()?.len();
        let mut recent = VecDeque::with_capacity(RECENT_EVENTS);
        let mut last_seq = 0;
        for file in [rotated_path(&path), path.clone()] {
            // Read as bytes and line by line: a line that is not UTF-8 (a torn write) is skipped like any unreadable
            // one, where reading the file as text would lose all of it, and `seq` would start over.
            let bytes = std::fs::read(file).unwrap_or_default();
            for event in bytes
                .split(|byte| *byte == b'\n')
                .filter_map(|l| serde_json::from_slice::<Event>(l).ok())
            {
                last_seq = last_seq.max(event.seq);
                // A kind from a newer fawkes would be replayed as `unknown`, its real kind lost; its `seq` still
                // counts, so numbering goes on after it.
                if event.kind != EventKind::Unknown {
                    remember(&mut recent, event);
                }
            }
        }
        Ok(EventLog {
            path,
            file,
            bytes,
            limit,
            recent,
            last_seq: Arc::new(AtomicU64::new(last_seq)),
        })
    }

    /// The counter `append_once` numbers events with.
    pub fn sequence(&self) -> Arc<AtomicU64> {
        self.last_seq.clone()
    }

    /// Append one event to the file at `path` without keeping it open: for the panic hook, which must not wait for
    /// the daemon's log, and must not read it either. It takes the next number from the log's counter, so it never
    /// repeats one.
    pub fn append_once(path: &Path, sequence: &AtomicU64, mut event: Event) {
        event.seq = sequence.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut file) = open_append(path) {
            // Best effort: the process is about to abort.
            let _ = writeln!(
                file,
                "{}",
                serde_json::to_string(&event).unwrap_or_default()
            );
        }
    }

    /// Number `event` (the next `seq`), write it and keep it in memory; returns it as recorded. The caller sends it
    /// to the subscribers before it releases the log, so they see events in `seq` order.
    pub fn append(&mut self, mut event: Event) -> Event {
        event.seq = self.last_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let line = format!("{}\n", serde_json::to_string(&event).unwrap_or_default());
        // A full disk must not take the supervisor down; the event still reaches live clients.
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.bytes += line.len() as u64;
        }
        remember(&mut self.recent, event.clone());
        if self.bytes >= self.limit {
            self.rotate();
        }
        event
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

    /// What a `subscribe` or `log` client gets before the live events. With `since`, the events after it that
    /// are still in memory (the newest `limit` of them). A `since` that no event can reach, because the log started
    /// over, is ignored: the client would wait for a number that never comes. Without `since`, the last `limit`
    /// events, or none without a limit.
    pub fn replay(&self, since: Option<u64>, limit: Option<usize>) -> Vec<Event> {
        let next = self.last_seq.load(Ordering::SeqCst) + 1;
        let Some(since) = since.filter(|&since| since < next) else {
            return limit.map_or_else(Vec::new, |n| self.tail(n));
        };
        let mut newer: Vec<Event> = self
            .recent
            .iter()
            .rev()
            .take_while(|e| e.seq > since)
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect();
        newer.reverse();
        newer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ErrorCode, EventKind, Facts};

    fn generation(id: &str, state: GenState) -> Generation {
        Generation {
            state,
            ..Generation::staging(id, "main", "0", PathBuf::new())
        }
    }

    fn event(n: usize) -> Event {
        Event::new(
            EventKind::Progress,
            None,
            format!("event {n}"),
            Facts::default(),
        )
    }

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
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let path = root.join("events.jsonl");
        let mut log = EventLog::with_limit(path.clone(), 4096).unwrap();
        for n in 0..RECENT_EVENTS + 10 {
            log.append(event(n));
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
    }

    #[test]
    fn events_are_numbered_across_reopens_and_rotations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let path = root.join("events.jsonl");
        // A line from before events were numbered loads as 0, and the first numbered event is 1.
        std::fs::write(
            &path,
            r#"{"at":"2026-11-03T14:00:01Z","kind":"daemon","generation":null,"message":"old"}"#
                .to_string()
                + "\n",
        )
        .unwrap();
        let mut log = EventLog::with_limit(path.clone(), 4096).unwrap();
        assert_eq!(log.tail(1)[0].seq, 0);
        let seqs: Vec<u64> = (0..3).map(|n| log.append(event(n)).seq).collect();
        assert_eq!(seqs, [1, 2, 3]);

        // Enough events to rotate: the numbers go on one by one, and a reopened log goes on after the last.
        for n in 3..60 {
            assert_eq!(log.append(event(n)).seq, n as u64 + 1);
        }
        assert!(rotated_path(&path).exists());
        drop(log);
        let mut reopened = EventLog::with_limit(path.clone(), 4096).unwrap();
        assert_eq!(reopened.append(event(60)).seq, 61);

        // The panic hook shares the counter.
        EventLog::append_once(&path, &reopened.sequence(), event(61));
        assert_eq!(reopened.append(event(62)).seq, 63);

        // The newest events are reachable by `seq`; a `since` beyond the log (it started over) is ignored.
        let after = |since, limit| -> Vec<u64> {
            reopened
                .replay(since, limit)
                .iter()
                .map(|e| e.seq)
                .collect()
        };
        assert_eq!(after(Some(60), None), [61, 63]);
        assert_eq!(after(Some(60), Some(1)), [63]);
        assert_eq!(after(Some(63), None), Vec::<u64>::new());
        assert_eq!(after(Some(64), Some(2)), [61, 63]);
        assert_eq!(after(None, None), Vec::<u64>::new());
        assert_eq!(after(None, Some(1)), [63]);
    }

    #[test]
    fn an_event_of_a_kind_from_a_newer_fawkes_is_not_replayed_but_keeps_its_number() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"seq":1,"at":"2026-11-03T14:00:01Z","kind":"daemon","generation":null,"message":"a"}"#,
                "\n",
                r#"{"seq":2,"at":"2026-11-03T14:00:02Z","kind":"quarantined","generation":"g1","message":"b"}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut log = EventLog::with_limit(path, 4096).unwrap();
        let kinds: Vec<EventKind> = log.tail(10).iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [EventKind::Daemon]);
        assert_eq!(log.append(event(0)).seq, 3);
    }

    #[test]
    fn a_line_that_is_not_utf_8_costs_only_itself() {
        // A torn or corrupted write leaves one bad line; the rest of the log, and the numbering after it, stay.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let mut text = Vec::new();
        text.extend_from_slice(
            br#"{"seq":1,"at":"2026-11-03T14:00:01Z","kind":"daemon","generation":null,"message":"a"}"#,
        );
        text.extend_from_slice(b"\n{\"seq\":2,\"message\":\"\xff\"}\n");
        text.extend_from_slice(
            br#"{"seq":3,"at":"2026-11-03T14:00:03Z","kind":"daemon","generation":null,"message":"c"}"#,
        );
        text.push(b'\n');
        std::fs::write(&path, text).unwrap();
        let mut log = EventLog::with_limit(path, 4096).unwrap();
        let seqs: Vec<u64> = log.tail(10).iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 3]);
        assert_eq!(log.append(event(0)).seq, 4);
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
            "2026-11-03T1000000000000000000:00:00Z",
            "2026-11-03T14:00:01+00:00",
            "2026-11-03T14:00:01.5Z",
            "2026-11-03t14:00:01z",
            "",
        ] {
            assert_eq!(parse_time(text), None, "{text}");
        }
    }

    #[test]
    fn the_first_reason_of_a_rollback_sticks() {
        let mut state = State {
            generations: vec![
                generation("g1", GenState::Retired),
                generation("g2", GenState::Permanent),
                generation("g3", GenState::Current),
            ],
            permanent: Some("g2".into()),
            txn: Some(Txn::Pending(Pending {
                generation: "g3".into(),
                previous: Some("g2".into()),
                deadline: "2026-11-03T14:05:00Z".into(),
            })),
            ..State::default()
        };
        assert!(state.decide_rollback("health check failed", Some(RollbackCause::Unhealthy)));
        assert_eq!(state.rolling_back(), Some("health check failed"));
        let g = state.find("g3").unwrap();
        assert_eq!(g.state, GenState::RolledBack);
        assert_eq!(g.rollback_reason.as_deref(), Some("health check failed"));
        assert_eq!(g.rollback_cause, Some(RollbackCause::Unhealthy));
        let at = g.rolled_back_at.clone();

        assert!(!state.decide_rollback("rollback requested", Some(RollbackCause::Requested)));
        assert_eq!(state.rolling_back(), Some("health check failed"));
        assert_eq!(
            state.find("g3").unwrap().rollback_cause,
            Some(RollbackCause::Unhealthy)
        );
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
                        staged_at: "then".into(),
                        ..generation("g1", from)
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
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
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
    }

    #[test]
    fn a_store_agrees_with_state_json_when_only_the_copy_cannot_be_written() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        // A directory where the copy's temp file goes: state.json lands, the copy does not.
        std::fs::create_dir(with_suffix(&backup_path(&path), ".tmp")).unwrap();
        let (mut store, _) = Store::load(path.clone()).unwrap();

        let ((), late) = store
            .modify(|state| {
                state.next_id = 9;
                Ok(())
            })
            .unwrap();

        assert!(late.unwrap().to_string().contains("state.json.bak"));
        assert_eq!(store.state().next_id, 9);
        assert_eq!(Store::load(path).unwrap().0.state().next_id, 9);
    }

    #[test]
    fn load_falls_back_to_the_copy_when_state_json_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
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
    }

    /// A state.json from a newer fawkes may hold a generation state this one cannot read as anything but `unknown`.
    /// Acting on it (rolling it back, pruning it, starting it) would be a guess, so the daemon refuses to start, and
    /// the copy of an older state.json is no way around that.
    #[test]
    fn a_state_json_with_a_generation_state_this_fawkes_does_not_know_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let state = State {
            generations: vec![generation("g1", GenState::Permanent)],
            permanent: Some("g1".into()),
            next_id: 2,
            ..State::default()
        };
        state.save(&path).unwrap();
        assert!(State::load(&path).is_ok());

        let newer = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"permanent\",", "\"quarantined\",");
        assert!(newer.contains("quarantined"), "{newer}");
        std::fs::write(&path, &newer).unwrap();
        let message = State::load(&path).unwrap_err();
        for expected in ["g1", "does not know", "newer fawkes"] {
            assert!(message.contains(expected), "{message}");
        }

        // An older copy is no way around it: the refusal is not an unreadable file.
        let copy = backup_path(&path);
        std::fs::write(&copy, "{}").unwrap();
        assert!(State::load(&path).is_err());
    }
}
