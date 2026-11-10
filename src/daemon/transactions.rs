//! The transactions (stage, test, switch, confirm, rollback) and the pruning of old generations.

use super::{Daemon, Inner, Live, Slot, Why, lock, tenths};
use crate::manifest::Manifest;
use crate::process;
use crate::protocol::{
    DaemonAction, Error, ErrorCode, EventKind, Facts, GenState, Generation, Out, Pending,
    PruneResult, R, RollbackCause, RollbackResult, StageResult, StorageOutcome, SwitchResult,
    TestInfo, describe_status, err, invalid_state, not_found,
};
use crate::state::{self, State, Txn, allowed, now, now_secs, parse_time, refusal};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

/// The two ways out of a failed restore, for the end of an error message: retry it, or give it up.
fn restore_way_out(permanent: Option<&str>) -> String {
    let start = permanent.map_or_else(
        || "leaves the app stopped (there is no permanent generation)".into(),
        |id| format!("starts {id}"),
    );
    format!(
        "`fawkes rollback` retries the restore, or `fawkes rollback --keep-storage` {start} on the storage as it is (it may already be migrated)"
    )
}

/// Refuse a command that acts on the live generation (start, stop, restart, switch, confirm) while a rollback's
/// restore has failed and awaits `rollback`.
pub(super) fn refuse_failed_restore(state: &State) -> R<()> {
    match &state.txn {
        Some(Txn::RollingBack {
            pending,
            restore_failed: true,
            ..
        }) => Err(invalid_state(format!(
            "{} was rolled back but its storage could not be restored; {}",
            pending.generation,
            restore_way_out(state.permanent.as_deref())
        ))
        .about(&pending.generation)),
        _ => Ok(()),
    }
}

/// What `test` needs of generation `id`: that it exists and may be tested.
pub(super) fn check_test<'a>(state: &'a State, id: &str) -> R<&'a Generation> {
    let g = state.find(id).ok_or_else(|| not_found(id))?;
    if !allowed(g.state, GenState::Tested) {
        return Err(refusal(g, GenState::Tested));
    }
    Ok(g)
}

/// What `switch` needs of the state and of generation `id`: no failed restore, no switch in flight, a tested `id`.
/// Gives the permanent generation the switch leaves.
pub(super) fn check_switch(state: &State, id: &str) -> R<Option<String>> {
    refuse_failed_restore(state)?;
    if let Some(txn) = &state.txn {
        return Err(invalid_state(format!(
            "{} is still waiting for confirm; `fawkes confirm` or `fawkes rollback` first",
            txn.generation()
        ))
        .about(txn.generation()));
    }
    let g = state.find(id).ok_or_else(|| not_found(id))?;
    if !allowed(g.state, GenState::Current) {
        return Err(refusal(g, GenState::Current));
    }
    Ok(state.permanent.clone())
}

/// What `rollback` needs of the state: a switch to roll back, pending or already being rolled back (a retry after a
/// failed restore). Gives it.
pub(super) fn check_rollback(state: &State) -> R<&Pending> {
    match &state.txn {
        Some(Txn::Pending(pending) | Txn::RollingBack { pending, .. }) => Ok(pending),
        _ => Err(invalid_state(
            "no switch is pending, so there is nothing to roll back; to go back past a confirm, `test` and `switch` to a retired generation",
        )),
    }
}

/// What `rollback --keep-storage` needs of the state: a rollback whose restore failed. Gives the switch and the
/// reason it was rolled back, as text and as a code.
pub(super) fn check_abandon(state: &State) -> R<(Pending, String, Option<RollbackCause>)> {
    match &state.txn {
        Some(Txn::RollingBack {
            pending,
            reason,
            cause,
            restore_failed: true,
        }) => Ok((pending.clone(), reason.clone(), *cause)),
        _ if check_rollback(state).is_ok() => Err(invalid_state(
            "--keep-storage only gives up a restore that failed, and none has; `fawkes rollback` rolls the pending switch back first",
        )),
        _ => Err(invalid_state(
            "--keep-storage only gives up a restore that failed, and none has: no switch is pending, so there is nothing to roll back",
        )),
    }
}

/// What `prune` needs of the manifest: a `keep`. Gives it.
pub(super) fn check_prune(m: &Manifest) -> R<u32> {
    m.keep.ok_or_else(|| {
        invalid_state(
            "pruning needs `keep = N` in the manifest; without it every generation is kept",
        )
    })
}

/// Removes a directory when dropped, on every way out of the function that made it. Best effort: a directory that
/// cannot be removed is no reason to change the answer.
struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}

/// The directory a storage hook works on besides the live home: a switch's snapshot, or a test's home.
enum Place {
    Snapshot(PathBuf),
    TestHome(PathBuf),
}

/// How long the quick git calls (rev-parse, prune, remove) get.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    git_within(repo, args, GIT_TIMEOUT)
}

/// Run git in `repo` for at most `timeout`, so a hanging git can neither hold the step slot for ever nor outlive
/// its group; its stdout, trimmed, on success and its stderr on failure.
fn git_within(repo: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let finished = process::run_bounded(&mut command, timeout, Duration::ZERO, "git", |_| {})?;
    if finished.status.success() {
        Ok(finished.stdout.trim().to_string())
    } else {
        Err(finished.stderr.trim().to_string())
    }
}

impl Daemon {
    pub(super) fn stage(self: &Arc<Self>, out: &mut Out, git_ref: &str) -> R<StageResult> {
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
                    "`{git_ref}` is not a commit, branch or tag in {}{}; push or fetch it into that repository",
                    m.repo.display(),
                    if e.is_empty() {
                        String::new()
                    } else {
                        format!(" ({e})")
                    }
                ),
            )
        })?;
        // An agent that retries a stage must not build twice: the newest generation of this commit that was staged and
        // is not live or rolled back stands for it. The step slot is held, so no other stage adds one meanwhile.
        let existing = self
            .inner()
            .store
            .state()
            .generations
            .iter()
            .rev()
            .find(|g| {
                g.commit == commit
                    && matches!(
                        g.state,
                        GenState::Staged
                            | GenState::Tested
                            | GenState::TestFailed
                            | GenState::Retired
                    )
            })
            .cloned();
        if let Some(generation) = existing {
            self.say(
                out,
                &generation.id,
                format!(
                    "{} is already staged as {}; nothing to build",
                    generation.short_commit(),
                    generation.id
                ),
            );
            return Ok(StageResult {
                generation,
                existing: true,
            });
        }
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
                    rollback_cause: None,
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
            self.event_with(
                EventKind::Error,
                Some(&id),
                format!(
                    "stage of {git_ref} failed: {}{}",
                    e.message,
                    removed.err().map_or(String::new(), |e| format!("; {e}"))
                ),
                Facts {
                    error_code: Some(e.code),
                    ..Facts::default()
                },
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
        Ok(StageResult {
            generation: record,
            existing: false,
        })
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
        let generation_dir = self.paths.generation(id);
        std::fs::create_dir_all(&generation_dir).map_err(|e| {
            err(
                ErrorCode::Internal,
                format!(
                    "cannot create {} to stage {id}: {e}",
                    generation_dir.display()
                ),
            )
        })?;
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
            m.install_timeout,
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
                if let Err(e) = clone_directory(&from, &to, m.install_timeout) {
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
        // Fawkes's own environment, so package managers find their real caches.
        self.run_step(
            m,
            &format!("install for {id}"),
            &command,
            &code,
            &[],
            &self.paths.install_log(id),
            &self.paths.step_pid(),
            m.install_timeout,
        )
        .map_err(|e| {
            err(
                ErrorCode::InstallFailed,
                format!("{e}; see {}", self.paths.failed_install_log().display()),
            )
            .with_log(self.paths.failed_install_log())
        })
    }

    /// Run the install or a storage hook to its end, for at most `timeout` (`process::run_bounded`, stopped like
    /// the app on a timeout): the start wrapper records its group in `pid_file` for a later daemon, and the group
    /// joins those `shutdown` kills while it runs.
    #[expect(
        clippy::too_many_arguments,
        reason = "the install and each hook differ in every one of them"
    )]
    fn run_step(
        &self,
        m: &Manifest,
        what: &str,
        command: &str,
        cwd: &Path,
        env: &process::Env,
        log: &Path,
        pid_file: &Path,
        timeout: Duration,
    ) -> Result<(), String> {
        let mut shell = process::shell(
            command,
            cwd,
            env,
            log,
            &format!("{what}: {command}"),
            Some(pid_file),
        )
        .map_err(|e| format!("cannot run {what}: {e}"))?;
        let mut group = None;
        let finished = process::run_bounded(&mut shell, timeout, m.stop_timeout, what, |pgid| {
            group = Some(pgid);
            lock(&self.step_pgids).push(pgid);
        });
        // Best effort: `leftover` never mistakes a stale file for a live step.
        let _ = std::fs::remove_file(pid_file);
        if let Some(pgid) = group {
            lock(&self.step_pgids).retain(|p| *p != pgid);
        }
        let finished = finished?;
        if finished.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{what} failed ({})",
                describe_status(finished.status)
            ))
        }
    }

    /// Run the storage hook `name` for generation `id` (DESIGN.md → Hooks) from the permanent generation's worktree,
    /// or the repository before the first confirm: never from a generation that may still roll back, so code under
    /// suspicion never decides how the storage is copied or restored. `{home}` is the live home; `place` is the
    /// hook's own directory, `{snapshot}` or `{test_home}`. A non-zero exit or a timeout is `storage_failed`.
    fn run_hook(
        &self,
        m: &Manifest,
        name: &str,
        command: &str,
        id: &str,
        port: u16,
        place: Place,
    ) -> R<()> {
        let permanent = self.inner().store.state().permanent.clone();
        let cwd = permanent.map_or_else(|| m.repo.clone(), |p| self.paths.code(&p));
        let mut vars = self.vars(port, &m.home, &cwd, id);
        let (phase, pid_file) = match place {
            Place::Snapshot(dir) => {
                vars.snapshot = Some(dir);
                ("live", self.paths.hook_pid())
            }
            Place::TestHome(dir) => {
                vars.test_home = Some(dir);
                ("test", self.paths.step_pid())
            }
        };
        let env = self.app_env(m, &vars, phase);
        let log = self.paths.hook_log(id);
        self.run_step(
            m,
            &format!("the {name} hook"),
            &vars.expand_command(command),
            &cwd,
            &env,
            &log,
            &pid_file,
            m.hooks.timeout,
        )
        .map_err(|e| {
            err(
                ErrorCode::StorageFailed,
                format!("{e}; see {}", log.display()),
            )
            .about(id)
            .with_log(&log)
        })
    }

    /// The snapshot of a switch to `id`: the `snapshot` hook into a `snapshot/` made afresh, or none without the
    /// hook. A snapshot an earlier switch to the same generation took (a retired one switched to again) goes either
    /// way: it must neither mix with this one nor be what a `restore` added later puts back.
    fn snapshot(&self, out: &mut Out, m: &Manifest, id: &str) -> R<()> {
        let dir = self.paths.snapshot(id);
        let fresh = match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ if m.hooks.snapshot.is_some() => std::fs::create_dir_all(&dir),
            _ => Ok(()),
        };
        fresh.map_err(|e| {
            err(
                ErrorCode::StorageFailed,
                format!("cannot make {} afresh: {e}", dir.display()),
            )
            .about(id)
        })?;
        let Some(command) = &m.hooks.snapshot else {
            return Ok(());
        };
        self.say(out, id, "snapshotting storage");
        self.run_hook(m, "snapshot", command, id, m.port, Place::Snapshot(dir))
    }

    /// Start a test: the start wrapper records it in `step.pid` for a later daemon, and its group joins those
    /// `shutdown` kills.
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

    /// End the test `child` together with everything left in its group, and forget it.
    fn end_step(&self, child: &mut Child, stop_timeout: Duration) {
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
            check_test(inner.store.state(), id)?.clone()
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
                .about(id)
            })?;
        let test_home = self.paths.test_home(id);
        let _ = std::fs::remove_dir_all(&test_home);
        let test_home_guard = RemoveOnDrop(&test_home);
        std::fs::create_dir_all(&test_home).map_err(|e| {
            err(
                ErrorCode::StorageFailed,
                format!(
                    "cannot create {} for the test of {id}: {e}",
                    test_home.display()
                ),
            )
            .about(id)
        })?;
        // Without hooks the test home stays empty: never the live one, which the test must not touch.
        let with = match &m.hooks.test_copy {
            Some(test_copy) => {
                self.say(out, id, "copying the storage for the test");
                self.run_hook(
                    &m,
                    "test_copy",
                    test_copy,
                    id,
                    port,
                    Place::TestHome(test_home.clone()),
                )?;
                "a copy of the storage"
            }
            None => "an empty home",
        };

        self.say(
            out,
            id,
            format!("starting {id} on spare port {port} with {with}"),
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
                .about(id)
                .with_log(&log)
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
        drop(test_home_guard);
        // Our test process is gone, so a port that is still taken was taken by someone else, most likely between
        // the probe and the app's bind: not the generation's fault, so its state and last result stay as they were.
        if result.is_err() && !port_free(port) {
            let message = format!(
                "spare port {port} was taken by another process during the test of {id}; {id} is unchanged, run `fawkes test {id}` again"
            );
            self.event_with(
                EventKind::Error,
                Some(id),
                &message,
                Facts {
                    error_code: Some(ErrorCode::PortInUse),
                    ..Facts::default()
                },
            );
            return Err(err(ErrorCode::PortInUse, message).about(id));
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
        let facts = Facts {
            port: Some(port),
            seconds: Some(info.seconds),
            ..Facts::default()
        };
        match result {
            Ok(()) => {
                self.event_with(
                    EventKind::Tested,
                    Some(id),
                    format!("test passed on port {port} in {}s", info.seconds),
                    facts,
                );
                Ok(record)
            }
            Err(e) => {
                self.event_with(EventKind::TestFailed, Some(id), &e, facts);
                Err(err(
                    ErrorCode::HealthFailed,
                    format!(
                        "{id} failed its test: {e}; see {}; fix the ref and stage it again, or `fawkes test {id}` to retry",
                        log.display()
                    ),
                )
                .about(id)
                .with_log(&log))
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
            check_switch(inner.store.state(), id)?
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

        if let Err(e) = self.snapshot(out, &m, id) {
            resume(self);
            return Err(e.with_hint(&unchanged));
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
            let message = match self.rollback_pending(
                out,
                &format!("start failed: {}", e.message),
                Some(RollbackCause::StartFailed),
            ) {
                Ok(_) => format!("{}; {id} is rolled back", e.message),
                Err(rollback) => format!(
                    "{}; rolling {id} back failed too: {}",
                    e.message, rollback.message
                ),
            };
            return Err(Error { message, ..e });
        }
        drop(inner);
        self.event_with(
            EventKind::Switched,
            Some(id),
            format!(
                "switched from {}; confirm before {deadline}",
                previous.as_deref().unwrap_or("nothing")
            ),
            Facts {
                previous: previous.clone(),
                deadline: Some(deadline.clone()),
                ..Facts::default()
            },
        );
        Ok(SwitchResult {
            generation: id.into(),
            previous,
            deadline,
        })
    }

    /// What `confirm` needs of the state, once the generation is not permanent already: a pending switch that is
    /// `id` (any when none is named), within its deadline, whose process runs and has not exited. Gives the switch.
    /// It logs nothing, since `status` runs it for every subscriber's snapshot: an unreadable deadline counts as
    /// passed here, and the supervisor, which rolls that switch back, reports it.
    pub(super) fn check_confirm(inner: &mut Inner, id: Option<&str>) -> R<Pending> {
        refuse_failed_restore(inner.store.state())?;
        match &inner.store.state().txn {
            Some(Txn::RollingBack {
                pending, reason, ..
            }) => {
                return Err(invalid_state(format!(
                    "{} is being rolled back ({reason}) and can no longer be confirmed",
                    pending.generation
                ))
                .about(&pending.generation));
            }
            Some(Txn::Pending(pending))
                if parse_time(&pending.deadline).is_none_or(|deadline| now_secs() >= deadline) =>
            {
                return Err(invalid_state(format!(
                    "{} missed its confirm deadline ({}) and is being rolled back",
                    pending.generation, pending.deadline
                ))
                .about(&pending.generation));
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
                ))
                .about(id),
                None => not_found(id),
            });
        }
        // Only a generation that runs can vouch for itself. One whose process just exited is about to be rolled back
        // by the supervisor; making it permanent would keep crashed code.
        match inner.running_mut() {
            Some(app) if app.generation == pending.generation => {
                if let Some(status) = app.exit_status() {
                    return Err(invalid_state(format!(
                        "{} exited ({}) before it was confirmed and is being rolled back",
                        pending.generation,
                        describe_status(status)
                    ))
                    .about(&pending.generation));
                }
            }
            _ => {
                return Err(invalid_state(format!(
                    "{} is not running, so it cannot be confirmed; `fawkes start` it and confirm again, or `fawkes rollback`",
                    pending.generation
                ))
                .about(&pending.generation));
            }
        }
        Ok(pending)
    }

    /// Confirm the pending generation, or answer for the permanent one; the flag says whether a generation became
    /// permanent, which is what a prune follows.
    pub(super) fn confirm(&self, id: Option<&str>) -> R<(Generation, bool)> {
        let mut inner = self.inner();
        // An app confirms itself by name. If the answer was lost and it asks again, the generation is already
        // permanent: that is a true answer whatever else is going on, so it comes before every refusal.
        if let Some(id) = id
            && inner.store.state().permanent.as_deref() == Some(id)
            && let Some(record) = inner.store.state().find(id)
        {
            return Ok((record.clone(), false));
        }
        let pending = Self::check_confirm(&mut inner, id)?;
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
        self.event_with(
            EventKind::Confirmed,
            Some(&pending.generation),
            format!("{} is permanent ({by})", pending.generation),
            Facts {
                named: Some(id.is_some()),
                ..Facts::default()
            },
        );
        Ok((record, true))
    }

    /// Roll the pending generation back (the caller holds the rollback slots). A rollback already decided, or
    /// retried after a failed restore, keeps its first reason, cause and time; `reason` and `cause` apply when none is
    /// decided yet.
    pub(super) fn rollback_pending(
        self: &Arc<Self>,
        out: &mut Out,
        reason: &str,
        cause: Option<RollbackCause>,
    ) -> R<RollbackResult> {
        let m = self.manifest();
        let (id, reason, cause, app) = {
            let mut inner = self.inner();
            check_rollback(inner.store.state())?;
            // Saved with the generation's `rolled_back` state before anything is stopped. Only a best-effort
            // save: rolling back is the safe direction and a disk that still says pending rolls back at the next
            // start, so a full disk must not stop it.
            self.decide_rollback(&mut inner, reason, cause);
            let Some(Txn::RollingBack {
                pending,
                reason,
                cause,
                ..
            }) = &inner.store.state().txn
            else {
                return Err(err(
                    ErrorCode::Internal,
                    "the rollback was decided but the state does not say so",
                ));
            };
            let (id, reason, cause) = (pending.generation.clone(), reason.clone(), *cause);
            (id, reason, cause, inner.take_running())
        };
        self.say(out, &id, format!("rolling back {id}: {reason}"));
        if let Some(app) = app {
            self.stop_app(app);
        }
        // Every time: whether the storage goes back is not the hook's to decide (only `--keep-storage` gives it up).
        let storage = if m.hooks.restore.is_some() {
            StorageOutcome::Restored
        } else {
            StorageOutcome::Nothing
        };
        let restored = match &m.hooks.restore {
            Some(restore) => {
                self.say(out, &id, "restoring storage");
                let snapshot = Place::Snapshot(self.paths.snapshot(&id));
                self.run_hook(&m, "restore", restore, &id, m.port, snapshot)
            }
            None => Ok(()),
        };

        let mut inner = self.inner();
        let permanent = inner.store.state().permanent.clone();
        let facts = |storage| Facts {
            to: permanent.clone(),
            reason: Some(reason.clone()),
            cause,
            storage: Some(storage),
            ..Facts::default()
        };
        if let Err(e) = restored {
            // Starting the permanent generation on storage in an unknown state could do more harm. The step stays
            // in state.json, so `rollback` or the next daemon start retries the restore (the code is not retried:
            // the generation is rolled_back for good).
            self.update(&mut inner.store, State::fail_restore);
            inner.live = Live::Failed;
            self.event_with(
                EventKind::RolledBack,
                Some(&id),
                format!("{reason}; storage restore failed"),
                facts(StorageOutcome::RestoreFailed),
            );
            return Err(Error {
                message: format!(
                    "{id} was rolled back but its storage could not be restored: {}; the app stays stopped; {}",
                    e.message,
                    restore_way_out(permanent.as_deref())
                ),
                ..e
            });
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
            self.event_with(
                EventKind::RolledBack,
                Some(&id),
                format!("{reason}; state.json could not be written"),
                facts(storage),
            );
            return Err(err(
                e.code,
                format!(
                    "{id} was rolled back but {}; the app stays stopped and `fawkes rollback` retries",
                    e.message
                ),
            )
            .about(&id));
        }
        self.event_with(
            EventKind::RolledBack,
            Some(&id),
            format!(
                "{reason}; back to {}",
                permanent.as_deref().unwrap_or("nothing")
            ),
            facts(storage),
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
            cause,
        })
    }

    /// `rollback --keep-storage`: give up the restore of a rollback that failed (the caller holds the rollback
    /// slots) and start the permanent generation on the storage as it is. The generation is `rolled_back` already; the
    /// step ends and the generation records when its restore was given up. Only the operator decides this, because
    /// the storage may be one the failed generation already migrated.
    pub(super) fn abandon_restore(self: &Arc<Self>) -> R<RollbackResult> {
        let mut inner = self.inner();
        let (pending, reason, cause) = check_abandon(inner.store.state())?;
        let id = pending.generation;
        let permanent = inner.store.state().permanent.clone();
        self.modify(&mut inner.store, |state| {
            state.txn = None;
            if let Some(g) = state.find_mut(&id) {
                g.restore_abandoned_at = Some(now());
            }
            Ok(())
        })?;
        self.event_with(
            EventKind::RolledBack,
            Some(&id),
            format!(
                "{reason}; restore given up on request, storage kept as it was: {} now runs on storage that {id} may already have migrated",
                permanent.as_deref().unwrap_or("nothing")
            ),
            Facts {
                to: permanent.clone(),
                reason: Some(reason.clone()),
                cause,
                storage: Some(StorageOutcome::Kept),
                ..Facts::default()
            },
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
            cause,
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
        let keep = check_prune(&m)?;
        let doomed = self
            .inner()
            .store
            .state()
            .prunable(usize::try_from(keep).unwrap_or(usize::MAX));
        let mut removed = Vec::new();
        for id in doomed {
            self.say(out, &id, format!("removing {id}"));
            if let Err(e) = self.remove_generation(&m, &id) {
                self.event_with(
                    EventKind::Error,
                    Some(&id),
                    format!("prune: {e}"),
                    Facts {
                        error_code: Some(ErrorCode::Internal),
                        ..Facts::default()
                    },
                );
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
            self.event_with(
                EventKind::Pruned,
                None,
                format!("removed {} (keep = {keep})", removed.join(", ")),
                Facts {
                    removed: Some(removed.clone()),
                    ..Facts::default()
                },
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
        let cleaned_up = || Facts {
            action: Some(DaemonAction::CleanedUp),
            ..Facts::default()
        };
        for id in staging {
            match self.remove_generation(&m, &id) {
                Ok(()) => self.event_with(
                    EventKind::Daemon,
                    Some(&id),
                    format!("removed {id}, whose stage the end of the last daemon cut short"),
                    cleaned_up(),
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
                    Ok(()) => self.event_with(
                        EventKind::Daemon,
                        None,
                        format!("removed {}, which no generation owns", dir.path().display()),
                        cleaned_up(),
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
    .about(id)
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
    command
        .args(flags)
        .arg(from)
        .arg(to)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let finished = process::run_bounded(&mut command, timeout, Duration::ZERO, "cp", |_| {})?;
    if finished.status.success() {
        Ok(())
    } else {
        Err(format!("cp failed ({})", describe_status(finished.status)))
    }
}
