//! The transactions (stage, test, switch, confirm, rollback) and the pruning of old generations.

use super::{Daemon, Live, Slot, Why, lock, tenths};
use crate::manifest::Manifest;
use crate::process;
use crate::protocol::{
    Error, ErrorCode, EventKind, GenState, Generation, LastExit, Out, Pending, PruneResult, R,
    RollbackResult, SwitchResult, TestInfo, err, invalid_state, not_found,
};
use crate::state::{self, State, Txn, allowed, now, now_secs, refusal};
use crate::storage;
use std::io::Read;
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

/// Refuse a command that acts on the live generation (start, stop, restart, switch, confirm) while a rollback's
/// restore has failed and awaits `rollback`.
pub(super) fn refuse_failed_restore(state: &State) -> R<()> {
    match &state.txn {
        Some(Txn::RollingBack {
            pending,
            restore_failed: true,
            ..
        }) => Err(invalid_state(format!(
            "{} was rolled back but its storage could not be restored; `fawkes rollback` retries the restore",
            pending.generation
        ))),
        _ => Ok(()),
    }
}

/// How long the quick git calls (rev-parse, prune, remove) get.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    git_within(repo, args, GIT_TIMEOUT)
}

/// Run git in `repo` for at most `timeout`; its stdout, trimmed, on success and its stderr on failure.
fn git_within(repo: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    let (status, stdout, stderr) = run_bounded(command, timeout, "git")?;
    if status.success() {
        Ok(stdout.trim().to_string())
    } else {
        Err(stderr.trim().to_string())
    }
}

/// Read `pipe` to its end on a thread: a child that writes more than a pipe holds would otherwise wait for a reader
/// that only reads once it has exited.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(mut pipe) = pipe {
            // A read error ends the capture; what came before is still the useful part.
            let _ = pipe.read_to_end(&mut text);
        }
        String::from_utf8_lossy(&text).into_owned()
    })
}

/// Run `command` in its own process group for at most `timeout`, so a hanging child (a git hook, a huge copy) can
/// neither hold the step slot for ever nor outlive its group: on a timeout the whole group is ended, and a child
/// that left processes behind takes them with it. Gives its exit status, stdout and stderr.
fn run_bounded(
    mut command: Command,
    timeout: Duration,
    what: &str,
) -> Result<(ExitStatus, String, String), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot run {what}: {e}"))?;
    let pgid = child.id();
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let Some(status) = process::wait_until(&mut child, Instant::now() + timeout)
        .map_err(|e| format!("{what}: {e}"))?
    else {
        // The readers are left to finish on their own: a group stuck in the kernel keeps its pipes open.
        process::stop_group(pgid, 0.0, Some(&mut child));
        return Err(format!("{what} timed out after {}s", timeout.as_secs()));
    };
    // Whatever the child left running would keep the pipes open, and the readers waiting.
    process::stop_group(pgid, 0.0, Some(&mut child));
    let output = |reader: thread::JoinHandle<String>| reader.join().unwrap_or_default();
    Ok((status, output(stdout), output(stderr)))
}

impl Daemon {
    pub(super) fn stage(self: &Arc<Self>, out: &mut Out, git_ref: &str) -> R<Generation> {
        let _busy = self.begin("stage", Slot::Step)?;
        let m = self.manifest();
        let commit = git(
            &m.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                // A ref starting with `-` is a name, never an option.
                "--end-of-options",
                &format!("{git_ref}^{{commit}}"),
            ],
        )
        .map_err(|e| {
            err(
                ErrorCode::GitFailed,
                format!(
                    "`{git_ref}` is not a commit, branch or tag in {}{}",
                    m.repo.display(),
                    if e.is_empty() {
                        String::new()
                    } else {
                        format!(" ({e})")
                    }
                ),
            )
        })?;
        // Recorded before the slow work, so a stage cut short by a daemon restart shows in history and the next
        // daemon start cleans it up. An intent: the id it spends must be on disk before any worktree carries it, or
        // a daemon that dies now would hand the same id out again.
        let (id, reuse_from) = {
            let mut inner = self.inner();
            self.modify(&mut inner.store, |state| {
                state.next_id += 1;
                let id = format!("g{}", state.next_id);
                let reuse_from = state
                    .permanent
                    .as_ref()
                    .map_or(m.repo.clone(), |p| self.paths.code(p));
                state.generations.push(Generation {
                    id: id.clone(),
                    git_ref: git_ref.into(),
                    commit: commit.clone(),
                    state: GenState::Staging,
                    dir: self.paths.code(&id),
                    staged_at: now(),
                    test: None,
                    switched_at: None,
                    confirmed_at: None,
                    rolled_back_at: None,
                    rollback_reason: None,
                    failed_starts: 0,
                    restore_abandoned_at: None,
                });
                Ok((id, reuse_from))
            })
            .map_err(|e| e.with_hint("nothing was staged"))?
        };
        if let Err(e) = self.build(out, &m, &id, git_ref, &commit, &reuse_from) {
            // A failed install's log outlives the generation, for the agent to read.
            let _ = std::fs::rename(self.paths.install_log(&id), self.paths.failed_install_log());
            let removed = self.remove_generation(&m, &id);
            self.event(
                EventKind::Error,
                Some(&id),
                format!(
                    "stage of {git_ref} failed: {}{}",
                    e.message,
                    removed.err().map_or(String::new(), |e| format!("; {e}"))
                ),
            );
            return Err(e);
        }
        let mut inner = self.inner();
        let record = self.update(&mut inner.store, |state| {
            state.transition(&id, GenState::Staged).map(|g| g.clone())
        })?;
        self.event(
            EventKind::Staged,
            Some(&id),
            format!("{git_ref} staged as {id}"),
        );
        Ok(record)
    }

    /// The slow part of `stage`: the worktree, the reused directories and the install.
    fn build(
        &self,
        out: &mut Out,
        m: &Manifest,
        id: &str,
        git_ref: &str,
        commit: &str,
        reuse_from: &Path,
    ) -> R<()> {
        let code = self.paths.code(id);
        std::fs::create_dir_all(self.paths.generation(id))?;
        self.say(
            out,
            id,
            format!(
                "checking out {} at {}",
                git_ref,
                &commit[..commit.len().min(8)]
            ),
        );
        let code_arg = code.to_string_lossy().into_owned();
        let install_timeout = Duration::from_secs_f64(m.install_timeout);
        // The repository's hooks never run here: the generation's own install is the place for setup, and a
        // hanging `post-checkout` would hold the step slot.
        git_within(
            &m.repo,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--detach",
                &code_arg,
                commit,
            ],
            install_timeout,
        )
        .map_err(|e| {
            err(
                ErrorCode::GitFailed,
                format!("cannot create the worktree for {id}: {e}"),
            )
        })?;

        for dir in &m.reuse {
            let (from, to) = (reuse_from.join(dir), code.join(dir));
            if from.exists() && !to.exists() {
                self.say(
                    out,
                    id,
                    format!("cloning {dir} from {}", reuse_from.display()),
                );
                if let Err(e) = clone_directory(&from, &to, install_timeout) {
                    self.say(
                        out,
                        id,
                        format!("cloning {dir} failed ({e}); install starts without it"),
                    );
                    let _ = std::fs::remove_dir_all(&to);
                }
            }
        }

        let Some(install) = &m.install else {
            return Ok(());
        };
        let vars = self.vars(m.port, &m.home, &code, id);
        let command = vars.expand_command(install);
        self.say(out, id, format!("installing: {command}"));
        self.run_install(m, &command, &code, &self.paths.install_log(id))
            .map_err(|e| {
                err(
                    ErrorCode::InstallFailed,
                    format!(
                        "install for {id} failed ({e}); see {}",
                        self.paths.failed_install_log().display()
                    ),
                )
            })
    }

    /// Run the install command in the worktree, in its own process group and with Fawkes's own environment (so
    /// package managers find their real caches), for at most `install_timeout` seconds.
    fn run_install(
        &self,
        m: &Manifest,
        command: &str,
        code: &Path,
        log: &Path,
    ) -> Result<(), String> {
        let mut child = self
            .spawn_step(command, code, &[], log, &format!("install: {command}"))
            .map_err(|e| e.to_string())?;
        let until = Instant::now() + Duration::from_secs_f64(m.install_timeout);
        let result = match process::wait_until(&mut child, until) {
            Ok(Some(status)) if status.success() => Ok(()),
            Ok(Some(status)) => Err(status.to_string()),
            Err(e) => Err(e.to_string()),
            Ok(None) => Err(format!("timed out after {}s", m.install_timeout)),
        };
        // A timed-out install still runs, and a finished one may have left processes in its group: nothing of it
        // outlives the stage.
        self.end_step(&mut child, m.stop_timeout);
        result
    }

    /// Start an install or a test: the start wrapper records it in `step.pid` for a later daemon, and its group joins
    /// those `shutdown` kills.
    fn spawn_step(
        &self,
        command: &str,
        cwd: &Path,
        env: &process::Env,
        log: &Path,
        header: &str,
    ) -> std::io::Result<Child> {
        let child = process::spawn(command, cwd, env, log, header, Some(&self.paths.step_pid()))?;
        lock(&self.step_pgids).push(child.id());
        Ok(child)
    }

    /// End the install or test `child` together with everything left in its group, and forget it.
    fn end_step(&self, child: &mut Child, stop_timeout: f64) {
        let pgid = child.id();
        process::stop_group(pgid, stop_timeout, Some(child));
        self.forget_step_pid_file();
        lock(&self.step_pgids).retain(|p| *p != pgid);
    }

    /// Remove generation `id` for good: its worktree, its directory (snapshot and logs included) and its record.
    /// The caller makes sure nothing uses it.
    fn remove_generation(&self, m: &Manifest, id: &str) -> Result<(), String> {
        let code = self.paths.code(id);
        if code.exists() {
            // Best effort: the directory goes below either way, and `worktree prune` forgets a worktree whose
            // directory is gone.
            let _ = git(
                &m.repo,
                &["worktree", "remove", "--force", &code.to_string_lossy()],
            );
        }
        if let Err(e) = std::fs::remove_dir_all(self.paths.generation(id))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!(
                "cannot remove {}: {e}",
                self.paths.generation(id).display()
            ));
        }
        let _ = git(&m.repo, &["worktree", "prune"]);
        let mut inner = self.inner();
        self.update(&mut inner.store, |state| {
            state.generations.retain(|g| g.id != id);
        });
        Ok(())
    }

    pub(super) fn test(self: &Arc<Self>, out: &mut Out, id: &str) -> R<Generation> {
        let _busy = self.begin(&format!("test {id}"), Slot::Step)?;
        let m = self.manifest();
        let g = {
            let inner = self.inner();
            let g = inner.store.state().find(id).ok_or_else(|| not_found(id))?;
            if !allowed(g.state, GenState::Tested) {
                return Err(refusal(g, GenState::Tested));
            }
            g.clone()
        };
        let port = (m.spare_ports[0]..=m.spare_ports[1])
            .find(|&p| p != m.port && port_free(p))
            .ok_or_else(|| {
                err(
                    ErrorCode::StartFailed,
                    format!(
                        "no free spare port in {}..{}",
                        m.spare_ports[0], m.spare_ports[1]
                    ),
                )
            })?;
        let test_home = self.paths.test_home(id);
        let _ = std::fs::remove_dir_all(&test_home);
        std::fs::create_dir_all(&test_home)?;
        storage::copy(&m.home, &m.storage, &test_home)
            .map_err(|e| err(ErrorCode::StorageFailed, e))?;

        self.say(
            out,
            id,
            format!("starting {id} on spare port {port} with a copy of the storage"),
        );
        let started = Instant::now();
        let vars = self.vars(port, &test_home, &g.dir, id);
        let env = self.app_env(&m, &vars, "test");
        let log = self.paths.app_log(id);
        let mut child = self
            .spawn_step(
                &vars.expand_command(&m.start),
                &g.dir,
                &env,
                &log,
                &format!("start {id} test port {port}"),
            )
            .map_err(|e| {
                err(
                    ErrorCode::StartFailed,
                    format!("cannot start {id} for its test: {e}"),
                )
            })?;

        let check = m.health_check(&vars);
        let exited = process::exited(&child);
        let result = process::wait_healthy(
            &check,
            &g.dir,
            &env,
            m.health.interval,
            m.health.timeout,
            || matches!(child.try_wait(), Ok(None)),
            &exited,
        );
        self.end_step(&mut child, m.stop_timeout);
        let _ = std::fs::remove_dir_all(&test_home);
        // Our test process is gone, so a port that is still taken was taken by someone else, most likely between
        // the probe and the app's bind: not the generation's fault, so its state and last result stay as they were.
        if result.is_err() && !port_free(port) {
            let message = format!(
                "spare port {port} was taken by another process during the test of {id}; {id} is unchanged, run `fawkes test {id}` again"
            );
            self.event(EventKind::Error, Some(id), &message);
            return Err(err(ErrorCode::PortInUse, message));
        }

        let passed = result.is_ok();
        let info = TestInfo {
            port,
            passed,
            at: now(),
            seconds: tenths(started.elapsed()),
            message: result
                .as_ref()
                .map_or_else(Clone::clone, |()| "healthy".into()),
        };
        let mut inner = self.inner();
        let record = self.update(&mut inner.store, |state| {
            let to = if passed {
                GenState::Tested
            } else {
                GenState::TestFailed
            };
            state.transition(id, to).map(|g| {
                g.test = Some(info.clone());
                g.clone()
            })
        })?;
        match result {
            Ok(()) => {
                self.event(
                    EventKind::Tested,
                    Some(id),
                    format!("test passed on port {port} in {}s", info.seconds),
                );
                Ok(record)
            }
            Err(e) => {
                self.event(EventKind::TestFailed, Some(id), &e);
                Err(err(
                    ErrorCode::HealthFailed,
                    format!(
                        "{id} failed its test: {e}; see {}; fix the ref and stage it again, or `fawkes test {id}` to retry",
                        log.display()
                    ),
                ))
            }
        }
    }

    pub(super) fn switch(
        self: &Arc<Self>,
        out: &mut Out,
        id: &str,
        deadline: Option<u64>,
    ) -> R<SwitchResult> {
        let _busy = self.begin(&format!("switch {id}"), Slot::Both)?;
        let m = self.manifest();
        let unchanged = format!("{id} stays tested and nothing changed");
        let previous = {
            let inner = self.inner();
            refuse_failed_restore(inner.store.state())?;
            if let Some(txn) = &inner.store.state().txn {
                return Err(invalid_state(format!(
                    "{} is still waiting for confirm; `fawkes confirm` or `fawkes rollback` first",
                    txn.generation()
                )));
            }
            let g = inner.store.state().find(id).ok_or_else(|| not_found(id))?;
            if !allowed(g.state, GenState::Current) {
                return Err(refusal(g, GenState::Current));
            }
            inner.store.state().permanent.clone()
        };

        // The intent goes to disk before the old app is stopped: a daemon that dies from here on finds the switch
        // cut short at its next start and runs the permanent generation again.
        {
            let mut inner = self.inner();
            self.modify(&mut inner.store, |state| {
                state.txn = Some(Txn::Switching {
                    generation: id.into(),
                    previous: previous.clone(),
                });
                Ok(())
            })
            .map_err(|e| e.with_hint(&unchanged))?;
        }

        // Stop first: no write of the old generation can land after the snapshot. A crash restart the old generation
        // was waiting for is held back too, and given back if the switch fails before its point of no return.
        let (old, backoff) = {
            let mut inner = self.inner();
            let backoff = match inner.live {
                Live::Backoff { until, ref at } => Some((until, at.clone())),
                _ => None,
            };
            (inner.take_running(), backoff)
        };
        let had_app = old.is_some();
        if let Some(app) = old {
            self.say(out, id, format!("stopping {}", app.generation));
            self.stop_app(app);
        }
        // The switch did not happen: no step in flight, and the old generation runs again.
        let resume = |daemon: &Arc<Self>| {
            let mut inner = daemon.inner();
            daemon.update(&mut inner.store, |state| state.txn = None);
            let Some(previous) = &previous else { return };
            // A daemon on its way out does not start what it is about to stop; its next start does.
            if had_app && !daemon.shutting_down() {
                daemon.start_or_give_up(&mut inner, previous, Why::Fresh);
            } else if let Some((until, at)) = backoff {
                // The backoff and the failed-start count stay as the crash left them.
                inner.live = Live::Backoff { until, at };
            }
        };

        self.say(out, id, "snapshotting storage");
        if let Err(e) = storage::snapshot(&m.home, &m.storage, &self.paths.snapshot(id)) {
            resume(self);
            return Err(err(ErrorCode::StorageFailed, e).with_hint(&unchanged));
        }

        // The request and the manifest both bound the deadline by MAX_DEADLINE, so this never saturates; saturating
        // only so that no arithmetic can panic halfway through a switch.
        let deadline =
            state::format_time(now_secs().saturating_add(1 + deadline.unwrap_or(m.deadline)));
        self.say(
            out,
            id,
            format!(
                "starting {id} on port {}; confirm before {deadline}",
                m.port
            ),
        );
        // The intent goes to disk before the side effect: once a process of the new generation exists, state.json
        // already says the switch is pending, so a daemon that dies from here on rolls it back at its next start.
        let mut inner = self.inner();
        // `shutdown` waits for this switch and `start_live` refuses from then on: the target would be rolled back
        // for good without ever running. Up to here it is still a switch that did not happen.
        if self.shutting_down() {
            drop(inner);
            resume(self);
            return Err(err(
                ErrorCode::Busy,
                format!(
                    "the daemon is shutting down; {id} stays tested and nothing changed, switch again once it has started"
                ),
            ));
        }
        let switched = self.modify(&mut inner.store, |state| {
            state.transition(id, GenState::Current)?;
            state.txn = Some(Txn::Pending(Pending {
                generation: id.into(),
                previous: previous.clone(),
                deadline: deadline.clone(),
            }));
            Ok(())
        });
        if let Err(e) = switched {
            drop(inner);
            resume(self);
            return Err(e.with_hint(&unchanged));
        }
        if let Err(e) = self.start_live(&mut inner, id, Why::Fresh) {
            drop(inner);
            // The ordinary rollback undoes the recorded switch and starts the permanent generation again.
            let message = match self.rollback_pending(out, &format!("start failed: {}", e.message))
            {
                Ok(_) => format!("{}; {id} is rolled back", e.message),
                Err(rollback) => format!(
                    "{}; rolling {id} back failed too: {}",
                    e.message, rollback.message
                ),
            };
            return Err(err(e.code, message));
        }
        drop(inner);
        self.event(
            EventKind::Switched,
            Some(id),
            format!(
                "switched from {}; confirm before {deadline}",
                previous.as_deref().unwrap_or("nothing")
            ),
        );
        Ok(SwitchResult {
            generation: id.into(),
            previous,
            deadline,
        })
    }

    pub(super) fn confirm(&self, id: Option<&str>) -> R<Generation> {
        let mut inner = self.inner();
        refuse_failed_restore(inner.store.state())?;
        match &inner.store.state().txn {
            Some(Txn::RollingBack {
                pending, reason, ..
            }) => {
                return Err(invalid_state(format!(
                    "{} is being rolled back ({reason}) and can no longer be confirmed",
                    pending.generation
                )));
            }
            Some(Txn::Pending(pending)) if now_secs() >= self.deadline_secs(pending) => {
                return Err(invalid_state(format!(
                    "{} missed its confirm deadline ({}) and is being rolled back",
                    pending.generation, pending.deadline
                )));
            }
            _ => {}
        }
        let Some(Txn::Pending(pending)) = inner.store.state().txn.clone() else {
            return Err(match id {
                Some(id) => inner
                    .store
                    .state()
                    .find(id)
                    .map_or_else(|| not_found(id), |g| refusal(g, GenState::Permanent)),
                None => invalid_state("no switch is pending; there is nothing to confirm"),
            });
        };
        if let Some(id) = id
            && id != pending.generation
        {
            return Err(match inner.store.state().find(id) {
                Some(g) if g.state == GenState::RolledBack => refusal(g, GenState::Permanent),
                Some(g) => invalid_state(format!(
                    "{id} is {}, not pending; {} is the one waiting for confirm",
                    g.state.as_str(),
                    pending.generation
                )),
                None => not_found(id),
            });
        }
        // Only a generation that runs can vouch for itself. One whose process just exited is about to be rolled back
        // by the supervisor; making it permanent would keep crashed code.
        match inner.running_mut() {
            Some(app) if app.generation == pending.generation => {
                if let Ok(Some(status)) = app.child.try_wait() {
                    return Err(invalid_state(format!(
                        "{} exited ({}) before it was confirmed and is being rolled back",
                        pending.generation,
                        LastExit::new(Some(status), false).describe()
                    )));
                }
            }
            _ => {
                return Err(invalid_state(format!(
                    "{} is not running, so it cannot be confirmed; `fawkes start` it and confirm again, or `fawkes rollback`",
                    pending.generation
                )));
            }
        }
        // Applied to a copy and kept only once it is on disk: a confirm the daemon could not write is not one, and
        // the switch stays pending (a daemon restart would roll it back).
        let record = self
            .modify(&mut inner.store, |state| {
                if let Some(old) = state.permanent.clone() {
                    state.transition(&old, GenState::Retired)?;
                }
                let record = state
                    .transition(&pending.generation, GenState::Permanent)?
                    .clone();
                state.permanent = Some(pending.generation.clone());
                state.txn = None;
                Ok(record)
            })
            .map_err(|e| {
                e.with_hint(format!(
                    "{} is still pending, confirm again",
                    pending.generation
                ))
            })?;
        // Apps confirm by name (the CLI sends their `FAWKES_GENERATION`), so a confirm that names none came from
        // elsewhere, an operator most likely: the log says which it was.
        let by = if id.is_some() {
            "the confirm named it"
        } else {
            "the confirm named no generation"
        };
        self.event(
            EventKind::Confirmed,
            Some(&pending.generation),
            format!("{} is permanent ({by})", pending.generation),
        );
        Ok(record)
    }

    /// Roll the pending generation back (the caller holds the rollback slots). A rollback already decided, or
    /// retried after a failed restore, keeps its first reason and time; `reason` applies when none is decided yet.
    pub(super) fn rollback_pending(
        self: &Arc<Self>,
        out: &mut Out,
        reason: &str,
    ) -> R<RollbackResult> {
        let m = self.manifest();
        let (id, reason, app) = {
            let mut inner = self.inner();
            // Saved with the generation's `rolled_back` state before anything is stopped. Only a best-effort
            // save: rolling back is the safe direction and a disk that still says pending rolls back at the next
            // start, so a full disk must not stop it.
            self.decide_rollback(&mut inner, reason);
            let Some(Txn::RollingBack {
                pending, reason, ..
            }) = &inner.store.state().txn
            else {
                return Err(invalid_state(
                    "no switch is pending, so there is nothing to roll back; to go back past a confirm, `test` and `switch` to a retired generation",
                ));
            };
            let (id, reason) = (pending.generation.clone(), reason.clone());
            (id, reason, inner.take_running())
        };
        self.say(out, &id, format!("rolling back {id}: {reason}"));
        if let Some(app) = app {
            self.stop_app(app);
        }
        let restored = storage::restore(&m.home, &self.paths.snapshot(&id));

        let mut inner = self.inner();
        let permanent = inner.store.state().permanent.clone();
        if let Err(e) = restored {
            // Starting the permanent generation on storage in an unknown state could do more harm. The step stays
            // in state.json, so `rollback` or the next daemon start retries the restore (the code is not retried:
            // the generation is rolled_back for good).
            self.update(&mut inner.store, State::fail_restore);
            inner.live = Live::Failed;
            self.event(
                EventKind::RolledBack,
                Some(&id),
                format!("{reason}; storage restore failed"),
            );
            return Err(err(
                ErrorCode::StorageFailed,
                format!(
                    "{id} was rolled back but its storage could not be restored: {e}; the app stays stopped"
                ),
            ));
        }
        // Until the end of the switch is on disk the app stays stopped and the step stays, like a failed restore:
        // `rollback` or the next daemon start finishes it (restoring the same snapshot again is harmless).
        if let Err(e) = self.modify(&mut inner.store, |state| {
            state.txn = None;
            Ok(())
        }) {
            // The save that just failed is reported; this one only records that the restore awaits a retry.
            let _ = inner.store.update(State::fail_restore);
            inner.live = Live::Failed;
            self.event(
                EventKind::RolledBack,
                Some(&id),
                format!("{reason}; state.json could not be written"),
            );
            return Err(err(
                e.code,
                format!(
                    "{id} was rolled back but {}; the app stays stopped and `fawkes rollback` retries",
                    e.message
                ),
            ));
        }
        self.event(
            EventKind::RolledBack,
            Some(&id),
            format!(
                "{reason}; back to {}",
                permanent.as_deref().unwrap_or("nothing")
            ),
        );
        if let Some(permanent) = &permanent
            && self.start_or_give_up(&mut inner, permanent, Why::Fresh)
        {
            return Err(left_to_next_start(&id, permanent));
        }
        Ok(RollbackResult {
            from: id,
            to: permanent,
            reason,
        })
    }

    /// `rollback --keep-storage`: give up the restore of a rollback that failed (the caller holds the rollback
    /// slots) and start the permanent generation on the storage as it is. The generation is `rolled_back` already; the
    /// step ends and the generation records when its restore was given up. Only the operator decides this, because
    /// the storage may be one the failed generation already migrated.
    pub(super) fn abandon_restore(self: &Arc<Self>) -> R<RollbackResult> {
        let mut inner = self.inner();
        let Some(Txn::RollingBack {
            pending,
            reason,
            restore_failed: true,
        }) = inner.store.state().txn.clone()
        else {
            return Err(invalid_state(
                "--keep-storage only gives up a restore that failed, and none has; `fawkes rollback` first",
            ));
        };
        let id = pending.generation;
        let permanent = inner.store.state().permanent.clone();
        self.modify(&mut inner.store, |state| {
            state.txn = None;
            if let Some(g) = state.find_mut(&id) {
                g.restore_abandoned_at = Some(now());
            }
            Ok(())
        })?;
        self.event(
            EventKind::RolledBack,
            Some(&id),
            format!(
                "{reason}; restore given up on request, storage kept as it was: {} now runs on storage that {id} may already have migrated",
                permanent.as_deref().unwrap_or("nothing")
            ),
        );
        if let Some(permanent) = &permanent
            && self.start_or_give_up(&mut inner, permanent, Why::Fresh)
        {
            return Err(left_to_next_start(&id, permanent));
        }
        Ok(RollbackResult {
            from: id,
            to: permanent,
            reason,
        })
    }
}

// ------------------------------------------------------------ pruning

impl Daemon {
    /// Remove the oldest generations so that the manifest's `keep` remain (`State::prunable` says which). It takes
    /// the step slot, so no stage, test or switch uses a generation while it goes.
    pub(super) fn prune(&self, out: &mut Out) -> R<PruneResult> {
        let _busy = self.begin("prune", Slot::Step)?;
        let m = self.manifest();
        let keep = m.keep.ok_or_else(|| {
            invalid_state(
                "pruning needs `keep = N` in the manifest; without it every generation is kept",
            )
        })?;
        let doomed = self
            .inner()
            .store
            .state()
            .prunable(usize::try_from(keep).unwrap_or(usize::MAX));
        let mut removed = Vec::new();
        for id in doomed {
            self.say(out, &id, format!("removing {id}"));
            if let Err(e) = self.remove_generation(&m, &id) {
                self.event(EventKind::Error, Some(&id), format!("prune: {e}"));
                return Err(err(
                    ErrorCode::Internal,
                    format!(
                        "{e}; removed before it: {}; `fawkes prune` tries again",
                        if removed.is_empty() {
                            "none".into()
                        } else {
                            removed.join(", ")
                        }
                    ),
                ));
            }
            removed.push(id);
        }
        if !removed.is_empty() {
            self.event(
                EventKind::Pruned,
                None,
                format!("removed {} (keep = {keep})", removed.join(", ")),
            );
        }
        let kept: Vec<String> = self
            .inner()
            .store
            .state()
            .generations
            .iter()
            .map(|g| g.id.clone())
            .collect();
        Ok(PruneResult { removed, kept })
    }

    /// After a confirm, prune in the background once no stage or test holds the step slot; the confirm itself never
    /// waits for it.
    pub(super) fn prune_soon(self: &Arc<Self>) {
        if self.manifest().keep.is_none() {
            return;
        }
        let daemon = self.clone();
        thread::spawn(move || {
            while !daemon.shutting_down() {
                match daemon.prune(&mut Out::none()) {
                    Err(e) if e.code == ErrorCode::Busy => sleep(Duration::from_secs(1)),
                    // Reported as an event already, and the next confirm or `fawkes prune` tries again.
                    _ => return,
                }
            }
        });
    }

    /// At startup: remove what a daemon that died mid-stage left behind, a `staging` record with its directory or a
    /// generation directory without a record, and the worktrees git still lists for them.
    pub(super) fn sweep_generations(&self) {
        let m = self.manifest();
        let staging: Vec<String> = {
            let inner = self.inner();
            inner
                .store
                .state()
                .generations
                .iter()
                .filter(|g| g.state == GenState::Staging)
                .map(|g| g.id.clone())
                .collect()
        };
        for id in staging {
            match self.remove_generation(&m, &id) {
                Ok(()) => self.event(
                    EventKind::Daemon,
                    Some(&id),
                    format!("removed {id}, whose stage the end of the last daemon cut short"),
                ),
                Err(e) => self.event(
                    EventKind::Error,
                    Some(&id),
                    format!("cannot remove {id}: {e}"),
                ),
            };
        }
        let dirs = std::fs::read_dir(self.paths.root.join("generations"))
            .into_iter()
            .flatten();
        for dir in dirs.flatten() {
            let name = dir.file_name().to_string_lossy().into_owned();
            if self.inner().store.state().find(&name).is_none() {
                match std::fs::remove_dir_all(dir.path()) {
                    Ok(()) => self.event(
                        EventKind::Daemon,
                        None,
                        format!("removed {}, which no generation owns", dir.path().display()),
                    ),
                    Err(e) => self.event(
                        EventKind::Error,
                        None,
                        format!("cannot remove {}: {e}", dir.path().display()),
                    ),
                };
            }
        }
        // Best effort: a worktree git still lists only costs a line in `git worktree list`.
        let _ = git(&m.repo, &["worktree", "prune"]);
    }
}

// ------------------------------------------------------ small helpers

/// The answer to a rollback of `id` that ended while the daemon was shutting down: done, but `permanent` was not
/// started, so an ok answer saying it runs would be wrong.
fn left_to_next_start(id: &str, permanent: &str) -> Error {
    err(
        ErrorCode::Busy,
        format!(
            "{id} is rolled back, but the daemon is shutting down, so {permanent} was not started; the next daemon start runs it"
        ),
    )
}

/// Whether nothing listens on `port`, on the loopback address or on all addresses: an app may bind either.
fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok() && TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// Copy-on-write clone of a directory (APFS `clonefile` on macOS, reflinks elsewhere), for at most `timeout`.
fn clone_directory(from: &Path, to: &Path, timeout: Duration) -> Result<(), String> {
    let flags: &[&str] = if cfg!(target_os = "macos") {
        &["-cR"]
    } else {
        &["-R", "--reflink=auto"]
    };
    let mut command = Command::new("cp");
    command.args(flags).arg(from).arg(to);
    let (status, _, _) = run_bounded(command, timeout, "cp")?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cp {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hanging_command_is_ended_with_everything_it_left_running() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("leader.pid");
        // The shell starts a member in the background, then records its pid (its group's id) and waits: a written
        // file means the group has the member to end. A second leaves the shell ample time to get there under load.
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(r#"sleep 30 & echo $$ > "$0"; wait"#)
            .arg(&pid_file);

        let started = Instant::now();
        let result = run_bounded(command, Duration::from_secs(1), "sleeper");
        let waited = started.elapsed();

        let message = result.expect_err("a command that outlives its timeout");
        assert!(message.contains("timed out"), "{message}");
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
        // The shell leads its group and the background member joined it.
        let pgid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !process::group_alive(pgid),
            "a member of the group survived"
        );
    }
}
