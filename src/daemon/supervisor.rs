//! The supervision loop: reaping and restarting the app, the health check, and the process and status reports.

use super::{APP_LOG_BYTES, App, Daemon, Inner, Live, Slot, Why};
use crate::process;
use crate::protocol::{
    ErrorCode, EventKind, History, LastExit, Out, Process, ProcessState, R, Status, err, not_found,
};
use crate::state::{Txn, format_time, now, now_secs, parse_time};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

impl Daemon {
    /// One supervision pass: reap the app, roll back a pending switch that is dead, overdue or unhealthy, restart a
    /// crash. Each of them moves the live process, so the pass holds the live slot like `stop` or `restart` do: no
    /// start, stop, switch or rollback runs while it cleans up after a crash. A stage or test never delays it.
    pub(super) fn tick(self: &Arc<Self>) {
        let Some(step) = self.due() else { return };
        // Busy: the step holding the slot deals with the live process itself, or the next pass does.
        let Ok(_busy) = self.begin(step, Slot::Live) else {
            return;
        };
        let (stragglers, rollback) = {
            let mut inner = self.inner();
            if self.shutting_down() {
                return;
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
            if let Err(e) = self.rollback_pending(&mut Out::none(), &reason) {
                self.event(EventKind::Error, None, e.message);
            }
            return;
        }
        let mut inner = self.inner();
        if Self::restart_due(&inner)
            && let Some(id) = inner.store.state().permanent.clone()
            && let Err(e) = self.start_live(&mut inner, &id, Why::Crash)
        {
            inner.live = Live::Failed;
            self.event(EventKind::Failed, Some(&id), e.message);
        }
    }

    /// What a supervision pass has to do, named for `busy`; None in the usual case, so an idle pass takes no slot.
    fn due(&self) -> Option<&'static str> {
        let mut inner = self.inner();
        if self.shutting_down() {
            return None;
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
            Some("rollback")
        } else if exited {
            Some("reap")
        } else if Self::restart_due(&inner) {
            Some("restart")
        } else {
            None
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
            let mut old = log.clone().into_os_string();
            old.push(".1");
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
        let vars = self.vars(m.port, &m.home, &g);
        let env = self.app_env(&m, &vars, "live");
        std::fs::create_dir_all(&m.home)?;
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

        if why == Why::Fresh {
            if inner.last_started.as_deref() != Some(id) {
                inner.restarts = 0;
            }
        } else {
            inner.restarts += 1;
        }
        inner.last_started = Some(id.into());
        inner.live = Live::Running(App {
            child,
            generation: id.into(),
            started: Instant::now(),
            started_at: now(),
        });
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
        // generation that gives up is rolled back by the supervisor without waiting for its deadline.
        let daemon = self.clone();
        let id = id.to_string();
        thread::spawn(move || {
            let check = m.health_check(&vars);
            let result = process::wait_healthy(
                &check,
                &vars.code,
                &env,
                m.health.interval,
                m.health.timeout,
                || daemon.is_running(pid),
            );
            match result {
                Ok(()) => {
                    daemon.event(EventKind::Healthy, Some(&id), "health check passed");
                }
                Err(e) if daemon.judge_unhealthy(&id, pid, m.rollback_on_unhealthy) => {
                    daemon.event(EventKind::Unhealthy, Some(&id), e);
                }
                Err(_) => {}
            }
        });
        Ok(())
    }

    /// Whether process `pid` of generation `id` is still the live one when its health check gives up, i.e. whether
    /// `unhealthy` is news. If so and `roll_back` is set, the rollback of the pending generation is decided in the same
    /// section that looked at the process (unless the switch has changed since: confirmed, already rolling back), and
    /// the caller logs `unhealthy` only afterwards, so a confirm never finds the failure known but the rollback not
    /// decided. The next supervision pass carries the rollback out.
    fn judge_unhealthy(&self, id: &str, pid: u32, roll_back: bool) -> bool {
        let mut inner = self.inner();
        if inner.running().is_none_or(|app| app.child.id() != pid) {
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
                Some((app.started.elapsed().as_secs_f64() * 10.0).round() / 10.0),
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
            busy: inner.busy.clone().or_else(|| inner.moving.clone()),
            process: self.process(&inner),
        }
    }

    pub(super) fn history(&self) -> History {
        History {
            generations: self.inner().store.state().generations.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Hub;
    use super::*;
    use crate::manifest::Manifest;
    use crate::protocol::Pending;
    use crate::state::{EventLog, Paths, Store};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

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

    /// A daemon over a scratch state directory, with no socket and no supervision thread.
    fn scratch_daemon(dir: &Path) -> Daemon {
        let manifest_path = dir.join("fawkes.toml");
        std::fs::write(
            &manifest_path,
            "name = \"app\"\nrepo = \"repo\"\nstate_dir = \"state\"\nhome = \"home\"\nport = 4790\n\
             spare_ports = [4791, 4795]\nstart = \"node app.js\"\n[health]\nurl = \"http://127.0.0.1:{port}/\"\n",
        )
        .unwrap();
        let manifest = Manifest::load(&manifest_path).unwrap();
        let paths = Paths {
            root: dir.join("state"),
        };
        std::fs::create_dir_all(&paths.root).unwrap();
        let (store, _) = Store::load(paths.state_json()).unwrap();
        Daemon {
            shutdown: Arc::new(AtomicBool::new(false)),
            reload: Arc::new(AtomicBool::new(false)),
            manifest_path,
            manifest: Mutex::new(Arc::new(manifest)),
            socket: paths.socket(),
            bin: PathBuf::new(),
            inner: Mutex::new(Inner {
                store,
                live: Live::Stopped,
                last_exit: None,
                last_started: None,
                restarts: 0,
                busy: None,
                moving: None,
            }),
            log: Mutex::new(EventLog::open(paths.events()).unwrap()),
            paths,
            hub: Hub(Mutex::new(Vec::new())),
            step_pgids: Mutex::new(Vec::new()),
        }
    }

    /// `rollback_on_unhealthy`: the call that finds the failing process still live decides the rollback itself, so
    /// by the time its caller logs `unhealthy` a confirm already finds the rollback decided. The end-to-end test
    /// cannot see the difference: the decision would win the race against a client reading the event either way.
    #[test]
    fn an_unhealthy_pending_generation_has_its_rollback_decided_before_the_event_is_logged() {
        let dir = std::env::temp_dir().join(format!("fk-unhealthy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = scratch_daemon(&dir);
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        {
            let mut inner = daemon.inner();
            inner.live = Live::Running(App {
                child,
                generation: "g2".into(),
                started: Instant::now(),
                started_at: now(),
            });
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

        let mut app = daemon.inner().take_running().unwrap();
        app.child.kill().unwrap();
        app.child.wait().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
