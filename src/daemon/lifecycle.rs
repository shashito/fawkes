//! The `start`, `stop` and `restart` commands.

use super::transactions::refuse_failed_restore;
use super::{Daemon, Inner, Live, Slot, Why};
use crate::protocol::{ErrorCode, EventKind, Out, Process, R, err, invalid_state};
use crate::state::Txn;
use std::sync::Arc;

impl Daemon {
    pub(super) fn current_generation(inner: &Inner) -> R<String> {
        refuse_failed_restore(inner.store.state())?;
        if let Some(Txn::RollingBack {
            pending, reason, ..
        }) = &inner.store.state().txn
        {
            return Err(err(
                ErrorCode::Busy,
                format!(
                    "{} is being rolled back ({reason}); try again when the rollback has finished",
                    pending.generation
                ),
            )
            .about(&pending.generation));
        }
        inner.store.state().current().map(String::from).ok_or_else(|| {
            invalid_state(
                "there is no generation to run yet; `fawkes stage`, `test` and `switch` one first",
            )
        })
    }

    /// What `start` needs of the state: a current generation that is not already running. Gives its id.
    pub(super) fn check_start(inner: &Inner) -> R<String> {
        let id = Self::current_generation(inner)?;
        if let Some(app) = inner.running() {
            return Err(invalid_state(format!(
                "{id} is already running (pid {}); `fawkes restart` restarts it",
                app.child.id()
            ))
            .about(&id));
        }
        Ok(id)
    }

    /// What `stop` needs of the state: a current generation, and an app that runs and has not exited, or a crash
    /// restart to cancel. Gives its id.
    pub(super) fn check_stop(inner: &mut Inner) -> R<String> {
        let id = Self::current_generation(inner)?;
        inner.refuse_exited()?;
        if inner.running().is_none() && !matches!(inner.live, Live::Backoff { .. }) {
            return Err(
                invalid_state(format!("{id} is not running; `fawkes start` starts it")).about(&id),
            );
        }
        Ok(id)
    }

    /// What `restart` needs of the state: a current generation, whose app, if it runs, has not exited. Gives its id.
    pub(super) fn check_restart(inner: &mut Inner) -> R<String> {
        let id = Self::current_generation(inner)?;
        inner.refuse_exited()?;
        Ok(id)
    }

    pub(super) fn start_command(self: &Arc<Self>) -> R<Process> {
        let _busy = self.begin("start", Slot::Live)?;
        let mut inner = self.inner();
        let id = Self::check_start(&inner)?;
        self.start_live(&mut inner, &id, Why::Fresh)?;
        Ok(self.process(&inner))
    }

    pub(super) fn stop_command(self: &Arc<Self>, out: &mut Out) -> R<Process> {
        let _busy = self.begin("stop", Slot::Live)?;
        let app = {
            let mut inner = self.inner();
            let id = Self::check_stop(&mut inner)?;
            let Some(app) = inner.take_running() else {
                // `check_stop` found a scheduled crash restart and `take_running` cancelled it: that is what was
                // asked for.
                drop(inner);
                self.event(
                    EventKind::Stopped,
                    Some(&id),
                    "stopped on request; the scheduled restart after the crash is cancelled",
                );
                return Ok(self.process(&self.inner()));
            };
            app
        };
        self.say(out, &app.generation, format!("stopping {}", app.generation));
        let id = app.generation.clone();
        self.stop_app(app);
        self.event(EventKind::Stopped, Some(&id), "stopped on request");
        Ok(self.process(&self.inner()))
    }

    pub(super) fn restart_command(self: &Arc<Self>, out: &mut Out) -> R<Process> {
        let _busy = self.begin("restart", Slot::Live)?;
        let (id, app) = {
            let mut inner = self.inner();
            let id = Self::check_restart(&mut inner)?;
            (id, inner.take_running())
        };
        if let Some(app) = app {
            self.say(out, &id, format!("stopping {id}"));
            self.stop_app(app);
        }
        let mut inner = self.inner();
        self.start_live(&mut inner, &id, Why::Restart)?;
        Ok(self.process(&inner))
    }
}
