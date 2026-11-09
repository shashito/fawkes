//! The supervision loop: reaping and restarting the app, the health check, and the process and status reports.
//!
//! The loop sleeps on the daemon's `wake` between passes. An exit of the live app wakes it (`process::on_exit`), and
//! so does every change subscribers hear of; the timers it keeps (a crash restart's backoff, the confirm deadline,
//! the app-log cap) bound the sleep. An idle daemon without an app wakes for nothing.

use super::{APP_LOG_BYTES, App, Daemon, Inner, Live, Slot, Why, tenths};
use crate::process;
use crate::protocol::{
    ErrorCode, EventKind, History, HistoryEntry, LastExit, Out, Process, ProcessState, R, Status,
    err, not_found,
};
use crate::state::{Txn, format_time, now_secs, parse_time, with_suffix};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ------------------------------------------------ restart policy

/// What follows an exit the supervisor did not ask for.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Outcome {
    /// The pending generation died before confirm: it is not restarted, its rollback is decided.
    RollBack,
    /// Start the same generation again after a backoff.
    Restart { after: Duration },
    /// Too many failed starts in a row: stay down until started by hand.
    GiveUp,
}

/// The crash count after an exit and what to do about it. A run of at least `min_uptime` seconds resets the count, so
/// a crash after a long run is a first failure again. Pure: the caller keeps the saving, the events and the clock.
fn after_exit(
    failed_starts: u32,
    ran: Duration,
    pending: bool,
    min_uptime: f64,
    max_failed_starts: u32,
) -> (u32, Outcome) {
    let failed_starts = if ran.as_secs_f64() < min_uptime {
        failed_starts + 1
    } else {
        0
    };
    (
        failed_starts,
        outcome(failed_starts, pending, max_failed_starts),
    )
}

fn outcome(failed_starts: u32, pending: bool, max_failed_starts: u32) -> Outcome {
    if pending {
        Outcome::RollBack
    } else if failed_starts >= max_failed_starts {
        Outcome::GiveUp
    } else {
        // 1, 2, 4, 8, 16, then 30 seconds; a count of 0 waits the first second.
        let seconds = (1u64 << failed_starts.saturating_sub(1).min(5)).min(30);
        Outcome::Restart {
            after: Duration::from_secs(seconds),
        }
    }
}

// ------------------------------------------------ timers

/// How often the live generation's `logs/app.log` is measured against `APP_LOG_BYTES`.
const CAP_EVERY: Duration = Duration::from_secs(10);

/// The pause after a pass that acted, before the next pass looks again. It bounds a step that keeps failing (a
/// rollback that cannot write state.json) to ten tries a second instead of a spin.
const AFTER_ACTING: Duration = Duration::from_millis(100);

/// The longest the supervisor sleeps while a switch awaits its confirm deadline. The deadline is wall-clock time and
/// the sleep is not: it runs on a monotonic clock, which neither a change of the system clock nor a machine asleep
/// moves on. Looking at the wall clock again every second notices either within a second; an idle daemon has no
/// deadline, so this costs it nothing.
const WALL_CLOCK_LOOK: Duration = Duration::from_secs(1);

/// How long the supervisor may sleep before one of its timers runs out: a crash restart's backoff, which ends at
/// `backoff` on the monotonic clock, or a pending switch's `deadline`, in seconds since the epoch. None when no timer
/// runs: it sleeps until something changes. Pure: the caller passes both clocks.
fn sleep_for(
    backoff: Option<Instant>,
    deadline: Option<u64>,
    now: Instant,
    wall: SystemTime,
) -> Option<Duration> {
    let restart = backoff.map(|until| until.saturating_duration_since(now));
    let confirm = deadline.map(|secs| {
        (UNIX_EPOCH + Duration::from_secs(secs))
            .duration_since(wall)
            .unwrap_or(Duration::ZERO)
            .min(WALL_CLOCK_LOOK)
    });
    [restart, confirm].into_iter().flatten().min()
}

/// What a supervision pass found to do now, or how long nothing is due.
enum Due {
    Now(&'static str),
    /// Nothing until a timer runs out after this long (`sleep_for`), or something changes.
    Later(Option<Duration>),
}

/// What a supervision pass did, which decides how long the supervisor sleeps after it.
enum Pass {
    Idle(Option<Duration>),
    /// Something was due, but another step holds the live slot: freeing it moves `wake`.
    Blocked,
    Acted,
}

impl Daemon {
    /// The supervision thread: a pass whenever `wake` moves or a timer runs out, until shutdown.
    pub(super) fn watch(self: &Arc<Self>) {
        let mut next_cap = Instant::now() + CAP_EVERY;
        while !self.shutting_down() {
            let seen = self.wake.seen();
            let pass = self.tick();
            let now = Instant::now();
            if now >= next_cap {
                self.cap_app_log();
                next_cap = now + CAP_EVERY;
            }
            let (seen, sleep) = match pass {
                Pass::Idle(sleep) => (seen, sleep),
                Pass::Blocked => (seen, None),
                // The pass's own steps moved the count: only a change after it ends the pause early.
                Pass::Acted => (self.wake.seen(), Some(AFTER_ACTING)),
            };
            // Only while an app runs, so a daemon without one has no timer at all.
            let cap = self
                .inner()
                .running()
                .is_some()
                .then(|| next_cap.saturating_duration_since(now));
            self.wake
                .wait(seen, [sleep, cap].into_iter().flatten().min());
        }
    }

    /// One supervision pass: reap the app, roll back a pending switch that is dead, overdue or unhealthy, restart a
    /// crash. Each of them moves the live process, so the pass holds the live slot like `stop` or `restart` do: no
    /// start, stop, switch or rollback runs while it cleans up after a crash. A stage or test never delays it.
    fn tick(self: &Arc<Self>) -> Pass {
        let step = match self.due() {
            Due::Now(step) => step,
            Due::Later(sleep) => return Pass::Idle(sleep),
        };
        // Busy: the step holding the slot deals with the live process itself, or the pass after it does.
        let Ok(_busy) = self.begin(step, Slot::Live) else {
            return Pass::Blocked;
        };
        let (stragglers, rollback) = {
            let mut inner = self.inner();
            if self.shutting_down() {
                return Pass::Acted;
            }
            let stragglers = self.reap(&mut inner);
            self.decide_overdue(&mut inner);
            (
                stragglers,
                inner.store.state().rolling_back().map(String::from),
            )
        };
        // The leader is gone; end the rest of its group too, so nothing of it lives on unsupervised or keeps the
        // storage open while a rollback restores it.
        if let Some(pgid) = stragglers {
            process::stop_group(pgid, 1.0, None);
            self.forget_pid_file();
        }
        if let Some(reason) = rollback {
            // `busy` is a rollback that ended while the daemon shuts down: `start_or_give_up` logged what it did
            // not start.
            if let Err(e) = self.rollback_pending(&mut Out::none(), &reason)
                && e.code != ErrorCode::Busy
            {
                self.event(EventKind::Error, None, e.message);
            }
            return Pass::Acted;
        }
        let mut inner = self.inner();
        if Self::restart_due(&inner)
            && let Some(id) = inner.store.state().permanent.clone()
        {
            self.start_or_give_up(&mut inner, &id, Why::Crash);
        }
        Pass::Acted
    }

    /// What a supervision pass has to do, named for `busy`; in the usual case nothing, so an idle pass takes no slot,
    /// and how long nothing will be due.
    fn due(&self) -> Due {
        let mut inner = self.inner();
        if self.shutting_down() {
            return Due::Later(None);
        }
        let exited = inner
            .running_mut()
            .is_some_and(|app| matches!(app.child.try_wait(), Ok(Some(_))));
        let rollback = match &inner.store.state().txn {
            Some(Txn::Pending(pending)) => {
                exited
                    || parse_time(&pending.deadline).is_none_or(|deadline| now_secs() >= deadline)
            }
            Some(Txn::RollingBack { restore_failed, .. }) => !restore_failed,
            _ => false,
        };
        if rollback {
            Due::Now("rollback")
        } else if exited {
            Due::Now("reap")
        } else if Self::restart_due(&inner) {
            Due::Now("restart")
        } else {
            // The same conditions as above, so no timer that has run out is left to wake the next pass at once:
            // a backoff counts only between switches, and a deadline only when it can be read.
            let state = inner.store.state();
            let backoff = match &inner.live {
                Live::Backoff { until, .. } if state.txn.is_none() => Some(*until),
                _ => None,
            };
            let deadline = match &state.txn {
                Some(Txn::Pending(pending)) => parse_time(&pending.deadline),
                _ => None,
            };
            Due::Later(sleep_for(
                backoff,
                deadline,
                Instant::now(),
                SystemTime::now(),
            ))
        }
    }

    /// Keep the live generation's `logs/app.log` under `APP_LOG_BYTES`: copy it to `app.log.1` and truncate it. The
    /// app's descriptor appends, so its next write lands at the new end; what it writes during the copy is lost, the
    /// price of capping a log the daemon does not sit between.
    pub(super) fn cap_app_log(&self) {
        let Some(id) = self.inner().running().map(|app| app.generation.clone()) else {
            return;
        };
        let log = self.paths.app_log(&id);
        if std::fs::metadata(&log).is_ok_and(|meta| meta.len() > APP_LOG_BYTES) {
            let old = with_suffix(&log, ".1");
            let capped = std::fs::copy(&log, &old)
                .and_then(|_| OpenOptions::new().write(true).open(&log))
                .and_then(|file| file.set_len(0));
            if let Err(e) = capped {
                self.event(
                    EventKind::Error,
                    Some(&id),
                    format!("cannot cap {}: {e}", log.display()),
                );
            }
        }
    }

    fn restart_due(inner: &Inner) -> bool {
        inner.store.state().txn.is_none()
            && matches!(inner.live, Live::Backoff { until, .. } if Instant::now() >= until)
    }

    /// Decide the rollback of a pending switch that missed its deadline.
    fn decide_overdue(&self, inner: &mut Inner) {
        if let Some(Txn::Pending(pending)) = &inner.store.state().txn
            && now_secs() >= self.deadline_secs(pending)
        {
            self.decide_rollback(inner, "missed confirm deadline");
        }
    }

    /// If the app exited without being asked to, take it out and decide what follows where the facts are: the pending
    /// generation gets no retries and its rollback is decided (so no confirm slips in), any
    /// other is restarted after a backoff or, after too many failed starts, given up. Returns the process group, which
    /// the caller ends once it has released `inner`.
    fn reap(&self, inner: &mut Inner) -> Option<u32> {
        let status = inner
            .running_mut()
            .and_then(|a| a.child.try_wait().ok().flatten())?;
        let app = inner.take_running()?;
        let ran = app.started.elapsed();
        let m = self.manifest();
        let pgid = app.child.id();
        let exit = LastExit::new(Some(status), false);
        // The crash count and the decision to roll back are one save. A reason decided earlier (a failed health
        // check) stays the reason.
        let (failed_starts, outcome) = self.update(&mut inner.store, |state| {
            let pending = state
                .pending()
                .is_some_and(|p| p.generation == app.generation);
            let decided = match state.find_mut(&app.generation) {
                Some(g) => {
                    let decided = after_exit(
                        g.failed_starts,
                        ran,
                        pending,
                        m.min_uptime,
                        m.max_failed_starts,
                    );
                    g.failed_starts = decided.0;
                    decided
                }
                None => (0, outcome(0, pending, m.max_failed_starts)),
            };
            if pending {
                state.decide_rollback(&format!("exited before confirm ({})", exit.describe()));
            }
            decided
        });
        let mut message = format!("{} after {:.1}s", exit.describe(), ran.as_secs_f64());
        match outcome {
            // The next supervision pass carries the rollback out.
            Outcome::RollBack => {}
            Outcome::GiveUp => inner.live = Live::Failed,
            Outcome::Restart { after } => {
                inner.live = Live::Backoff {
                    until: Instant::now() + after,
                    at: format_time(now_secs() + after.as_secs()),
                };
                message = format!("{message}; restarting in {}s", after.as_secs());
            }
        }
        inner.last_exit = Some(exit);
        self.event(EventKind::Exited, Some(&app.generation), message);
        if outcome == Outcome::GiveUp {
            self.event(
                EventKind::Failed,
                Some(&app.generation),
                format!("{failed_starts} failed starts in a row; not restarting"),
            );
        }
        Some(pgid)
    }

    fn is_running(&self, pid: u32) -> bool {
        self.inner().running().is_some_and(|a| a.child.id() == pid)
    }

    /// Start generation `id` on the live port. The caller holds `inner`, so nothing sees a half-started app.
    pub(super) fn start_live(self: &Arc<Self>, inner: &mut Inner, id: &str, why: Why) -> R<()> {
        if self.shutting_down() {
            return Err(err(ErrorCode::StartFailed, "the daemon is shutting down"));
        }
        let m = self.manifest();
        let g = inner
            .store
            .state()
            .find(id)
            .cloned()
            .ok_or_else(|| not_found(id))?;
        let vars = self.vars(m.port, &m.home, &g.dir, id);
        let env = self.app_env(&m, &vars, "live");
        // Private like the one the daemon's start creates, should it have been removed since.
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&m.home)
            .map_err(|e| {
                err(
                    ErrorCode::StartFailed,
                    format!("cannot create the home {} for {id}: {e}", m.home.display()),
                )
            })?;
        let child = process::spawn(
            &vars.expand_command(&m.start),
            &g.dir,
            &env,
            &self.paths.app_log(id),
            &format!("start {id} live port {}", m.port),
            Some(&self.paths.app_pid()),
        )
        .map_err(|e| err(ErrorCode::StartFailed, format!("cannot start {id}: {e}")))?;
        let pid = child.id();
        // Its exit wakes the supervisor to reap it, and ends the health check's wait between two tries.
        let (tell_health, exited) = mpsc::channel();
        let daemon = self.clone();
        process::on_exit(&child, move || {
            daemon.wake.notify();
            // The health check may be over already.
            let _ = tell_health.send(());
        });

        if why == Why::Fresh {
            if inner.last_started.as_deref() != Some(id) {
                inner.restarts = 0;
            }
        } else {
            inner.restarts += 1;
        }
        inner.last_started = Some(id.into());
        inner.live = Live::Running(App::new(child, id));
        self.update(&mut inner.store, |state| {
            if why != Why::Crash
                && let Some(g) = state.find_mut(id)
            {
                g.failed_starts = 0;
            }
        });
        self.point_current_link(&g.dir);
        self.event(
            EventKind::Started,
            Some(id),
            format!("pid {pid} on port {}", m.port),
        );

        // Health never confirms: only `confirm` makes a generation permanent. With `rollback_on_unhealthy` a pending
        // generation that gives up is rolled back by the supervisor without waiting for its deadline. The thread
        // takes only what it reads, so a manifest replaced by a reload is not kept alive for the whole check.
        let check = m.health_check(&vars);
        let (interval, timeout, roll_back) =
            (m.health.interval, m.health.timeout, m.rollback_on_unhealthy);
        let code = vars.code;
        let daemon = self.clone();
        let id = id.to_string();
        thread::spawn(move || {
            let result = process::wait_healthy(
                &check,
                &code,
                &env,
                interval,
                timeout,
                || daemon.is_running(pid),
                &exited,
            );
            match result {
                Ok(()) => {
                    daemon.event(EventKind::Healthy, Some(&id), "health check passed");
                }
                Err(e) if daemon.judge_unhealthy(&id, pid, roll_back) => {
                    daemon.event(EventKind::Unhealthy, Some(&id), e);
                }
                Err(_) => {}
            }
        });
        Ok(())
    }

    /// `start_live` of the permanent generation `id`, giving the app up when it cannot start: the failure is logged
    /// and nothing retries it. A daemon on its way out starts nothing, which is no failure of the generation: its
    /// next start runs it. Says whether the start was left to that next start.
    pub(super) fn start_or_give_up(
        self: &Arc<Self>,
        inner: &mut Inner,
        id: &str,
        why: Why,
    ) -> bool {
        let Err(e) = self.start_live(inner, id, why) else {
            return false;
        };
        if self.shutting_down() {
            self.event(
                EventKind::Daemon,
                Some(id),
                format!(
                    "{id} not started: the daemon is shutting down, and its next start runs {id}"
                ),
            );
            return true;
        }
        inner.live = Live::Failed;
        self.event(EventKind::Failed, Some(id), e.message);
        false
    }

    /// Whether process `pid` of generation `id` is still the live one when its health check gives up, i.e. whether
    /// `unhealthy` is news. If so and `roll_back` is set, the rollback of the pending generation is decided in the same
    /// section that looked at the process (unless the switch has changed since: confirmed, already rolling back), and
    /// the caller logs `unhealthy` only afterwards, so a confirm never finds the failure known but the rollback not
    /// decided. The next supervision pass carries the rollback out.
    fn judge_unhealthy(&self, id: &str, pid: u32, roll_back: bool) -> bool {
        let mut inner = self.inner();
        // A process that has exited is not news either, though the supervisor may not have reaped it yet: its exit
        // ended the check, and `reap` judges it with its own reason. `try_wait` keeps the status for `reap`.
        if inner
            .running_mut()
            .is_none_or(|app| app.child.id() != pid || !matches!(app.child.try_wait(), Ok(None)))
        {
            return false;
        }
        if roll_back
            && matches!(&inner.store.state().txn, Some(Txn::Pending(pending)) if pending.generation == id)
        {
            self.decide_rollback(&mut inner, "health check failed");
        }
        true
    }

    /// Decide to roll back the pending switch for `reason` (`State::decide_rollback`), and save the decision. The
    /// caller holds `inner` from learning why, so no confirm fits in between. The save is bookkeeping: the decision
    /// stands in memory, and a daemon that restarts rolls a pending switch back either way.
    pub(super) fn decide_rollback(&self, inner: &mut Inner, reason: &str) {
        if matches!(inner.store.state().txn, Some(Txn::Pending(_))) {
            self.update(&mut inner.store, |state| state.decide_rollback(reason));
        }
    }

    /// `<state_dir>/current` -> the live worktree, replaced atomically.
    fn point_current_link(&self, code: &Path) {
        let tmp = self.paths.root.join("current.tmp");
        let _ = std::fs::remove_file(&tmp);
        if std::os::unix::fs::symlink(code, &tmp).is_ok() {
            let _ = std::fs::rename(&tmp, self.paths.current_link());
        }
    }

    /// Stop an app Fawkes owns and record that Fawkes asked for it.
    pub(super) fn stop_app(&self, mut app: App) {
        let m = self.manifest();
        let status = process::stop_group(app.child.id(), m.stop_timeout, Some(&mut app.child));
        self.forget_pid_file();
        self.inner().last_exit = Some(LastExit::new(status, true));
    }

    /// The app's group is gone: its pid file names nothing a later daemon must stop. Best effort, since `leftover`
    /// never mistakes a stale file for a live app.
    fn forget_pid_file(&self) {
        let _ = std::fs::remove_file(self.paths.app_pid());
    }

    /// The same for the install or test that just ended.
    pub(super) fn forget_step_pid_file(&self) {
        let _ = std::fs::remove_file(self.paths.step_pid());
    }

    pub(super) fn process(&self, inner: &Inner) -> Process {
        let (state, generation, pid, started_at, uptime) = match &inner.live {
            Live::Running(app) => (
                ProcessState::Running,
                Some(app.generation.clone()),
                Some(app.child.id()),
                Some(app.started_at.clone()),
                Some(tenths(app.started.elapsed())),
            ),
            Live::Stopped | Live::Backoff { .. } | Live::Failed => (
                if matches!(inner.live, Live::Failed) {
                    ProcessState::Failed
                } else {
                    ProcessState::Stopped
                },
                inner.store.state().current().map(String::from),
                None,
                None,
                None,
            ),
        };
        Process {
            state,
            generation,
            pid,
            port: self.manifest().port,
            started_at,
            uptime,
            restarts: inner.restarts,
            last_exit: inner.last_exit.clone(),
            restart_at: match &inner.live {
                Live::Backoff { at, .. } => Some(at.clone()),
                _ => None,
            },
        }
    }

    pub(super) fn status(&self) -> Status {
        let inner = self.inner();
        Status {
            name: self.manifest().name.clone(),
            daemon_pid: std::process::id(),
            permanent: inner.store.state().permanent.clone(),
            current: inner.store.state().current().map(String::from),
            pending: inner.store.state().pending().cloned(),
            step: inner.store.state().step(),
            busy: inner.busy.clone().or_else(|| inner.moving.clone()),
            process: self.process(&inner),
        }
    }

    /// The generations with the end of their app logs. The files are read once `inner` is released: `history` is
    /// also in every `subscribe` snapshot, and a disk read must not hold up a transaction.
    pub(super) fn history(&self) -> History {
        let generations = self.inner().store.state().generations.clone();
        History {
            generations: generations
                .into_iter()
                .map(|generation| HistoryEntry {
                    log_tail: log_tail(&self.paths.app_log(&generation.id)),
                    generation,
                })
                .collect(),
            failed_install_tail: log_tail(&self.paths.failed_install_log()),
        }
    }
}

/// The most lines `history` shows of a log.
const TAIL_LINES: usize = 50;
/// The most bytes `history` reads of a log; only the end of the file is read.
const TAIL_BYTES: u64 = 8192;

/// The last lines of a log (at most `TAIL_LINES`, from the last `TAIL_BYTES`), or None when the file is missing,
/// unreadable or empty. A cut in the middle of a line drops that partial line.
fn log_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let from = lines.len().saturating_sub(TAIL_LINES);
    let tail = lines[from..].join("\n");
    (!tail.is_empty()).then_some(tail)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{KillOnDrop, scratch_daemon};
    use super::*;
    use crate::protocol::{GenState, Generation, Pending, Step};
    use std::path::PathBuf;

    const MIN_UPTIME: f64 = 10.0;
    const SHORT: Duration = Duration::from_secs(1);

    fn restart(seconds: u64) -> Outcome {
        Outcome::Restart {
            after: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn crashes_back_off_by_doubling_up_to_the_cap() {
        let mut failed_starts = 0;
        let mut waits = Vec::new();
        for _ in 0..8 {
            let (count, decided) = after_exit(failed_starts, SHORT, false, MIN_UPTIME, 100);
            failed_starts = count;
            waits.push(decided);
        }
        let expected: Vec<Outcome> = [1, 2, 4, 8, 16, 30, 30, 30].map(restart).to_vec();
        assert_eq!(waits, expected);
        assert_eq!(failed_starts, 8);
    }

    #[test]
    fn a_long_run_resets_the_count_and_waits_one_second() {
        let ran = Duration::from_secs_f64(MIN_UPTIME);
        assert_eq!(after_exit(4, ran, false, MIN_UPTIME, 5), (0, restart(1)));
    }

    #[test]
    fn the_last_allowed_failed_start_gives_up() {
        assert_eq!(after_exit(1, SHORT, false, MIN_UPTIME, 3), (2, restart(2)));
        assert_eq!(
            after_exit(2, SHORT, false, MIN_UPTIME, 3),
            (3, Outcome::GiveUp)
        );
    }

    #[test]
    fn a_pending_generation_always_rolls_back() {
        for (failed_starts, ran) in [(0, SHORT), (5, SHORT), (0, Duration::from_secs(60))] {
            let (_, decided) = after_exit(failed_starts, ran, true, MIN_UPTIME, 3);
            assert_eq!(decided, Outcome::RollBack);
        }
    }

    /// The supervisor sleeps exactly until its next timer, with no polling interval to round it to; the wall-clock
    /// deadline is looked at again at least every `WALL_CLOCK_LOOK`, so a clock that jumps is noticed within it.
    #[test]
    fn the_supervisor_sleeps_until_its_next_timer_and_looks_at_the_wall_clock_every_second() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_millis(1_000_000_600);
        let deadline_in = |seconds: u64| Some(1_000_000 + seconds);
        let ms = Duration::from_millis;

        assert_eq!(sleep_for(None, None, now, wall), None, "idle: no timer");
        assert_eq!(
            sleep_for(Some(now + ms(1500)), None, now, wall),
            Some(ms(1500))
        );
        // The deadline is a whole second, 400 ms after `wall`: the sleep ends on it, not a poll after it.
        assert_eq!(sleep_for(None, deadline_in(1), now, wall), Some(ms(400)));
        assert_eq!(
            sleep_for(Some(now + ms(300)), deadline_in(1), now, wall),
            Some(ms(300)),
            "the earlier timer"
        );
        assert_eq!(
            sleep_for(None, deadline_in(120), now, wall),
            Some(WALL_CLOCK_LOOK),
            "a far deadline is looked at again within a second"
        );
        // The clock jumped past the deadline during a sleep, or a backoff ran out: no sleep at all.
        assert_eq!(
            sleep_for(None, deadline_in(0), now, wall),
            Some(Duration::ZERO)
        );
        assert_eq!(
            sleep_for(None, deadline_in(1), now, wall + Duration::from_secs(3600)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            sleep_for(Some(now), None, now + ms(5), wall),
            Some(Duration::ZERO)
        );
        // A clock set back an hour puts the deadline an hour away, and it is still looked at every second.
        assert_eq!(
            sleep_for(None, deadline_in(1), now, wall - Duration::from_secs(3600)),
            Some(WALL_CLOCK_LOOK)
        );
    }

    /// `rollback_on_unhealthy`: the call that finds the failing process still live decides the rollback itself, so
    /// by the time its caller logs `unhealthy` a confirm already finds the rollback decided. The end-to-end test
    /// cannot see the difference: the decision would win the race against a client reading the event either way.
    #[test]
    fn an_unhealthy_pending_generation_has_its_rollback_decided_before_the_event_is_logged() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let daemon = scratch_daemon(&dir);
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let _reaper = KillOnDrop(pid);
        {
            let mut inner = daemon.inner();
            inner.live = Live::Running(App::new(child, "g2"));
            inner
                .store
                .modify(|state| {
                    state.txn = Some(Txn::Pending(Pending {
                        generation: "g2".into(),
                        previous: Some("g1".into()),
                        deadline: format_time(now_secs() + 120),
                    }));
                    Ok(())
                })
                .unwrap();
        }

        assert!(
            !daemon.judge_unhealthy("g2", pid + 1, true),
            "not the live process"
        );
        assert_eq!(daemon.inner().store.state().rolling_back(), None);
        assert!(daemon.judge_unhealthy("g2", pid, true));
        assert_eq!(
            daemon.inner().store.state().rolling_back(),
            Some("health check failed")
        );
    }

    /// The app's exit ends its health check at once, possibly before the supervisor has reaped it: a crash is for
    /// `reap` to judge, with its own reason, so the check that gave up is no news and decides no rollback.
    #[test]
    fn a_health_check_ended_by_a_crash_leaves_the_rollback_to_the_reap() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = scratch_daemon(temp.path());
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        // Exited, not reaped: as the live app is between its exit and the supervisor's pass.
        process::exited(&child).recv().unwrap();
        {
            let mut inner = daemon.inner();
            inner.live = Live::Running(App::new(child, "g2"));
            inner
                .store
                .modify(|state| {
                    state.txn = Some(Txn::Pending(Pending {
                        generation: "g2".into(),
                        previous: Some("g1".into()),
                        deadline: format_time(now_secs() + 120),
                    }));
                    Ok(())
                })
                .unwrap();
        }

        assert!(!daemon.judge_unhealthy("g2", pid, true));
        assert_eq!(daemon.inner().store.state().rolling_back(), None);
        assert!(
            daemon
                .inner()
                .running_mut()
                .is_some_and(|app| matches!(app.child.try_wait(), Ok(Some(_)))),
            "the exit is still there for `reap`"
        );
    }

    /// `status` says what the transaction step is: `pending` stays set while a switch is rolled back, so a client
    /// that read only it would offer a confirm that can no longer happen.
    #[test]
    fn status_names_the_transaction_step() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = scratch_daemon(temp.path());
        let pending = || Pending {
            generation: "g2".into(),
            previous: Some("g1".into()),
            deadline: "t".into(),
        };
        let rolling_back = |restore_failed| Txn::RollingBack {
            pending: pending(),
            reason: "unhealthy".into(),
            restore_failed,
        };
        let cases = [
            (None, None, false),
            (
                Some(Txn::Switching {
                    generation: "g2".into(),
                    previous: Some("g1".into()),
                }),
                Some(Step::Switching),
                false,
            ),
            (Some(Txn::Pending(pending())), Some(Step::Pending), true),
            (Some(rolling_back(false)), Some(Step::RollingBack), true),
            (Some(rolling_back(true)), Some(Step::RestoreFailed), true),
        ];
        for (txn, step, has_pending) in cases {
            let label = format!("{txn:?}");
            daemon
                .inner()
                .store
                .modify(|state| {
                    state.txn = txn;
                    Ok(())
                })
                .unwrap();
            let status = daemon.status();
            assert_eq!(status.step, step, "{label}");
            assert_eq!(status.pending.is_some(), has_pending, "{label}");
        }
    }

    /// `confirm` is the commit point, so each way it refuses must leave the switch as it was: a confirm that lands
    /// between the deadline and the supervisor's rollback, one for a generation that is not the pending one, and
    /// one for a pending generation that does not run.
    #[test]
    fn confirm_refuses_a_late_a_wrong_a_stopped_and_a_missing_switch() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = scratch_daemon(temp.path());
        let record = |id: &str, state: GenState| Generation {
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
            restore_abandoned_at: None,
        };
        // g1 is permanent and g2 is pending with its deadline `seconds_from_now` (negative: passed), or no switch.
        let set_txn = |seconds_from_now: Option<i64>| {
            daemon
                .inner()
                .store
                .modify(|state| {
                    state.generations = vec![
                        record("g1", GenState::Permanent),
                        record("g2", GenState::Current),
                    ];
                    state.permanent = Some("g1".into());
                    state.txn = seconds_from_now.map(|seconds| {
                        Txn::Pending(Pending {
                            generation: "g2".into(),
                            previous: Some("g1".into()),
                            deadline: format_time(now_secs().saturating_add_signed(seconds)),
                        })
                    });
                    Ok(())
                })
                .unwrap();
        };
        let refusal = |id: Option<&str>| {
            let error = daemon.confirm(id).unwrap_err();
            (error.code, error.message)
        };
        let assert_pending = || {
            let inner = daemon.inner();
            assert_eq!(
                inner.store.state().pending().map(|p| p.generation.as_str()),
                Some("g2")
            );
            assert_eq!(
                inner.store.state().find("g2").unwrap().state,
                GenState::Current
            );
            assert_eq!(inner.store.state().permanent.as_deref(), Some("g1"));
        };

        set_txn(None);
        let (code, message) = refusal(None);
        assert_eq!(code, ErrorCode::InvalidState);
        assert!(message.contains("no switch is pending"), "{message}");

        // Past its deadline, even though the supervisor has not rolled it back yet.
        set_txn(Some(-5));
        let (code, message) = refusal(None);
        assert_eq!(code, ErrorCode::InvalidState);
        assert!(message.contains("missed its confirm deadline"), "{message}");
        assert_pending();

        set_txn(Some(120));
        let (code, _) = refusal(Some("g9"));
        assert_eq!(code, ErrorCode::NotFound);
        // The permanent generation is already confirmed: a retried confirm gets its record and changes nothing.
        let record = daemon.confirm(Some("g1")).unwrap();
        assert_eq!(
            (record.id.as_str(), record.state),
            ("g1", GenState::Permanent)
        );
        assert_pending();

        // Nothing runs (`Live::Stopped`): only a generation that runs can vouch for itself.
        for id in [None, Some("g2")] {
            let (code, message) = refusal(id);
            assert_eq!(code, ErrorCode::InvalidState);
            assert!(
                message.contains("g2 is not running, so it cannot be confirmed"),
                "{message}"
            );
        }
        assert_pending();
    }

    /// The live generation's `app.log` over the cap is copied to `app.log.1` and emptied; at the cap, or with no
    /// live process, it is left alone.
    #[test]
    fn an_app_log_over_the_cap_moves_to_app_log_1_and_starts_over() {
        let temp = tempfile::tempdir().unwrap();
        let daemon = scratch_daemon(temp.path());
        let log = daemon.paths.app_log("g1");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let full = "x".repeat(usize::try_from(APP_LOG_BYTES).unwrap());

        std::fs::write(&log, format!("{full}y")).unwrap();
        daemon.cap_app_log();
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            APP_LOG_BYTES + 1,
            "nothing is live: nothing is capped"
        );

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _reaper = KillOnDrop(child.id());
        daemon.inner().live = Live::Running(App::new(child, "g1"));
        std::fs::write(&log, &full).unwrap();
        daemon.cap_app_log();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), full, "at the cap");
        assert!(!with_suffix(&log, ".1").exists());

        std::fs::write(&log, format!("{full}y")).unwrap();
        daemon.cap_app_log();
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        assert_eq!(
            std::fs::read_to_string(with_suffix(&log, ".1")).unwrap(),
            format!("{full}y")
        );
    }

    #[test]
    fn a_log_tail_is_the_last_lines_within_the_byte_bound() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.log");
        assert_eq!(log_tail(&path), None, "missing file");

        std::fs::write(&path, "").unwrap();
        assert_eq!(log_tail(&path), None, "empty file");

        std::fs::write(&path, "one\ntwo\n").unwrap();
        assert_eq!(log_tail(&path).as_deref(), Some("one\ntwo"));

        let many = "line\n".repeat(120).replacen("line", "first", 1) + "last\n";
        let tail = log_tail(&path_with(&path, &many)).unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line");
        assert_eq!(lines[TAIL_LINES - 1], "last");

        // Longer than the byte bound: only the end is read, and the line it cuts through is dropped.
        let long = "row\n".repeat(5000) + "end\n";
        assert!(long.len() as u64 > TAIL_BYTES * 2);
        let tail = log_tail(&path_with(&path, &long)).unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[TAIL_LINES - 1], "end");

        let partial = format!(
            "{}\nwhole\nlast\n",
            "x".repeat(usize::try_from(TAIL_BYTES).unwrap())
        );
        assert_eq!(
            log_tail(&path_with(&path, &partial)).as_deref(),
            Some("whole\nlast")
        );

        std::fs::write(&path, b"ok \xff\xfe bytes\n").unwrap();
        assert_eq!(
            log_tail(&path).as_deref(),
            Some("ok \u{fffd}\u{fffd} bytes")
        );
    }

    fn path_with(path: &Path, content: &str) -> std::path::PathBuf {
        std::fs::write(path, content).unwrap();
        path.to_path_buf()
    }
}
