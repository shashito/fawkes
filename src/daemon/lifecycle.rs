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
            ));
        }
        inner.store.state().current().map(String::from).ok_or_else(|| {
            invalid_state(
                "there is no generation to run yet; `fawkes stage`, `test` and `switch` one first",
            )
        })
    }

    pub(super) fn start_command(self: &Arc<Self>) -> R<Process> {
        let _busy = self.begin("start", Slot::Live)?;
        let mut inner = self.inner();
        let id = Self::current_generation(&inner)?;
        if let Some(app) = inner.running() {
            return Err(invalid_state(format!(
                "{id} is already running (pid {})",
                app.child.id()
            )));
        }
        self.start_live(&mut inner, &id, Why::Fresh)?;
        Ok(self.process(&inner))
    }

    pub(super) fn stop_command(self: &Arc<Self>, out: &mut Out) -> R<Process> {
        let _busy = self.begin("stop", Slot::Live)?;
        let app = {
            let mut inner = self.inner();
            let id = Self::current_generation(&inner)?;
            let scheduled = matches!(inner.live, Live::Backoff { .. });
            let Some(app) = inner.take_running_alive()? else {
                if !scheduled {
                    return Err(invalid_state(format!(
                        "{id} is not running; `fawkes start` starts it"
                    )));
                }
                // `take_running_alive` cancelled the scheduled crash restart: that is what was asked for.
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
            let id = Self::current_generation(&inner)?;
            (id, inner.take_running_alive()?)
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
