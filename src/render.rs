//! The client's text for what the daemon answers and streams: results, status, progress and event lines. Pure
//! functions from the typed records of `protocol` to strings; `--json` prints the daemon's own documents instead.

use crate::disk::{self, Space};
use crate::protocol::{
    AppState, DownResult, ErrorCode, Event, Events, FleetStatus, FleetView, GenState, Generation,
    History, Process, ProcessState, PruneResult, ReexecResult, ReloadResult, ReportResult, Request,
    RollbackResult, StageResult, Status, Step, SwitchResult,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// An optional field as text: `-` for null.
fn or_dash(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "-".into(), |v| v.to_string())
}

pub fn render_progress(event: &Event) -> String {
    format!(
        "{}: {}",
        or_dash(event.generation.as_deref()),
        event.message
    )
}

pub fn render_event(event: &Event) -> String {
    format!(
        "{}  {:<11} {:<4} {}",
        event.at,
        event.kind.as_str(),
        or_dash(event.generation.as_deref()),
        event.message
    )
}

/// An app's info in the order people read it: `url` first, then the rest by key.
fn ordered_info(info: &BTreeMap<String, String>) -> Vec<(&str, &str)> {
    info.get_key_value("url")
        .into_iter()
        .chain(info.iter().filter(|(key, _)| key.as_str() != "url"))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

/// The message of a `reported` event: the info as `key value` pairs.
pub fn render_info(info: &BTreeMap<String, String>) -> String {
    if info.is_empty() {
        return "info cleared".into();
    }
    ordered_info(info)
        .iter()
        .map(|(key, value)| format!("{key} {value}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `rows` as lines of `indent`, label and value, the values in one column at least `min_width` wide.
fn aligned_rows(rows: &[(&str, &str)], indent: &str, min_width: usize) -> Vec<String> {
    let width = rows
        .iter()
        .map(|(label, _)| label.len() + 1)
        .fold(min_width, usize::max);
    rows.iter()
        .map(|(label, value)| format!("{indent}{label:<width$}{value}"))
        .collect()
}

/// `rows` as lines of `indent` and cells, each column as wide as its widest cell and two spaces from the next; the last
/// column is not padded, so no line ends in spaces.
fn aligned_columns(rows: &[Vec<String>], indent: &str) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(column) {
                Some(widest) => *widest = (*widest).max(width),
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let last = row.len().saturating_sub(1);
            let cells = row.iter().enumerate().map(|(column, cell)| {
                if column == last {
                    cell.clone()
                } else {
                    format!("{cell:<width$}", width = widths[column])
                }
            });
            format!("{indent}{}", cells.collect::<Vec<_>>().join("  "))
        })
        .collect()
}

/// What `fawkes status --all` prints: the supervisor, then one line per app with its process, its daemon's version and
/// what the supervisor does with it. An app whose daemon does not answer says so in place of the process's state.
pub fn render_fleet(view: &FleetView) -> String {
    let head = match &view.supervisor {
        Some(s) => format!(
            "supervisor pid {}, fawkes {}, {} app{}",
            s.pid,
            s.version,
            s.apps.len(),
            if s.apps.len() == 1 { "" } else { "s" }
        ),
        None => {
            "no supervisor runs; `fawkes service install` or `fawkes up --all` starts one".into()
        }
    };
    let rows: Vec<Vec<String>> = view
        .apps
        .iter()
        .map(|app| {
            let supervised = app.supervisor.as_ref().map(|s| s.state);
            let state = match (&app.status, supervised, &app.error) {
                (Some(status), ..) => status.process.state.as_str(),
                (None, Some(AppState::Down), _) => "down",
                (None, _, Some(e)) if e.code == ErrorCode::DaemonUnreachable => "no daemon",
                (None, ..) => "no answer",
            };
            let status = app.status.as_ref();
            let process = status.map(|s| &s.process);
            let version = status.map(|s| s.version.as_str()).filter(|v| !v.is_empty());
            vec![
                app.name.clone(),
                state.into(),
                or_dash(process.and_then(|p| p.generation.as_deref())),
                or_dash(process.and_then(|p| p.pid).map(|pid| format!("pid {pid}"))),
                or_dash(process.map(|p| format!("port {}", p.port))),
                or_dash(version.map(|v| format!("fawkes {v}"))),
                or_dash(supervised.map(supervision)),
            ]
        })
        .collect();
    let lines = std::iter::once(head)
        .chain(fleet_disk_lines(view))
        .chain(aligned_columns(&rows, "  "));
    lines.collect::<Vec<_>>().join("\n")
}

/// One line per distinct volume size that has apps on a low disk, naming them: apps share a volume more often than not,
/// and one warning each would say the same thing. The least free of the apps on it is shown.
fn fleet_disk_lines(view: &FleetView) -> Vec<String> {
    let mut volumes: BTreeMap<u64, (u64, Vec<&str>)> = BTreeMap::new();
    for app in &view.apps {
        if let Some(disk) = app.status.as_ref().and_then(|s| s.disk).filter(|d| d.low) {
            let (free, names) = volumes.entry(disk.total).or_insert((disk.free, Vec::new()));
            *free = (*free).min(disk.free);
            names.push(&app.name);
        }
    }
    volumes
        .into_iter()
        .map(|(total, (free, names))| {
            format!(
                "disk low: {} ({})",
                Space { free, total }.describe(),
                names.join(", ")
            )
        })
        .collect()
}

/// What the supervisor does with an app, in a word: `supervised` when its daemon is the supervisor's child.
fn supervision(state: AppState) -> &'static str {
    match state {
        AppState::Running => "supervised",
        AppState::Attached => "attached",
        AppState::Backoff => "backoff",
        AppState::Down => "down",
        AppState::Broken => "broken",
        AppState::Unknown => "unknown",
    }
}

/// What `fawkes reexec --all` prints for the supervisor once it answers `status` on its new binary.
pub fn render_supervisor_reexec(r: &ReexecResult, s: &FleetStatus) -> String {
    let daemons = s
        .apps
        .iter()
        .filter(|app| app.state == AppState::Running)
        .count();
    format!(
        "the supervisor (pid {}) re-executed: fawkes {} → {}; {daemons} {}",
        r.pid,
        r.from,
        r.to,
        if daemons == 1 {
            "daemon keeps running"
        } else {
            "daemons keep running"
        }
    )
}

/// What `up` prints to its terminal after the live app turned healthy, so the operator sees where it is. Framed by a
/// blank line on each side, as one line of the daemon's stderr writer. `health_url` is the manifest's expanded health
/// URL when it checks one, the app's address when it reports none of its own.
pub fn render_up_block(s: &Status, app_log: &Path, health_url: Option<&str>) -> String {
    let generation = s.process.generation.as_deref();
    let standing = match (s.awaiting_confirm(), s.step, &s.pending) {
        (Some(pending), ..) if Some(pending.generation.as_str()) == generation => {
            format!("pending, confirm by {}", pending.deadline)
        }
        (None, Some(Step::RollingBack), Some(pending))
            if Some(pending.generation.as_str()) == generation =>
        {
            "rolling back".into()
        }
        _ => "permanent".into(),
    };
    let info = ordered_info(&s.process.info);
    let url = info
        .iter()
        .find(|(key, _)| *key == "url")
        .map(|(_, value)| *value)
        .or(health_url);
    let app_log = app_log.display().to_string();
    let state_dir = s.state_dir.display().to_string();
    let manifest = s.manifest.display();
    let next = format!(
        "fawkes status --manifest {manifest} · fawkes watch · fawkes down --manifest {manifest}"
    );
    let mut rows: Vec<(&str, &str)> = url.map(|url| ("url", url)).into_iter().collect();
    rows.extend(info.iter().filter(|(key, _)| *key != "url"));
    rows.extend([
        ("output", app_log.as_str()),
        ("state", state_dir.as_str()),
        ("next", next.as_str()),
    ]);
    let header = format!(
        "{}  {} {standing} · pid {} · port {}",
        s.name,
        or_dash(generation),
        or_dash(s.process.pid),
        s.process.port
    );
    let lines = std::iter::once(header).chain(aligned_rows(&rows, "  ", 9));
    format!("\n{}\n", lines.collect::<Vec<_>>().join("\n"))
}

fn render_generation(g: &Generation) -> String {
    format!(
        "{:<4} {:<12} {} @ {}",
        g.id,
        g.state.as_str(),
        g.git_ref,
        g.short_commit()
    )
}

fn render_process(p: &Process) -> String {
    let mut line = format!("{}  {}", p.state.as_str(), or_dash(p.generation.as_deref()));
    if p.state == ProcessState::Running {
        line = format!(
            "{line}  pid {}  port {}  up {}s",
            or_dash(p.pid),
            p.port,
            or_dash(p.uptime.map(|seconds| format!("{seconds:?}")))
        );
    }
    line = format!("{line}  restarts {}", p.restarts);
    if let Some(at) = &p.restart_at {
        line = format!("{line}  restart at {at}");
    }
    for row in aligned_rows(&ordered_info(&p.info), "  ", 8) {
        line = format!("{line}\n{row}");
    }
    if let Some(exit) = &p.last_exit {
        let who = if exit.requested {
            "requested"
        } else {
            "unrequested"
        };
        line = format!(
            "{line}\nlast exit: {} ({who}) at {}",
            exit.describe(),
            exit.at
        );
    }
    line
}

/// `pending` is also set while the switch is rolled back, so a confirm is asked for only when `step` allows one.
fn render_status(s: &Status) -> String {
    let note = if let Some(p) = s.awaiting_confirm() {
        format!("  (waiting for confirm until {})", p.deadline)
    } else {
        let switch = s.pending.as_ref();
        let generation = switch.map_or("the switch", |p| p.generation.as_str());
        match (s.step, switch) {
            (Some(Step::RollingBack), Some(p)) => format!(
                "  (rolling back {} to {})",
                p.generation,
                or_dash(p.previous.as_deref())
            ),
            (Some(Step::RestoreFailed), _) => format!(
                "\n{generation}'s storage could not be restored and the app is stopped; `fawkes rollback` retries it, `fawkes rollback --keep-storage` gives it up"
            ),
            (Some(step @ (Step::Switching | Step::Unknown)), _) => {
                format!("  (step: {})", step.as_str())
            }
            _ => String::new(),
        }
    };
    // An older daemon's status has none of these.
    let daemon = if s.version.is_empty() {
        String::new()
    } else {
        format!(
            "\ndaemon {}  manifest {}  state_dir {}  socket {}",
            s.version,
            s.manifest.display(),
            s.state_dir.display(),
            s.socket.display()
        )
    };
    // Only a low disk is worth a line.
    let disk = match s.disk {
        Some(disk) if disk.low => format!(
            "\ndisk low: {}; stage refuses below {}",
            Space {
                free: disk.free,
                total: disk.total
            }
            .describe(),
            disk::gigabytes(disk.min_free)
        ),
        _ => String::new(),
    };
    format!(
        "{}  daemon pid {}  busy: {}\npermanent {}  current {}{}\nactions: {}{}\nprocess {}{}",
        s.name,
        s.daemon_pid,
        or_dash(s.busy.as_deref()),
        or_dash(s.permanent.as_deref()),
        or_dash(s.current.as_deref()),
        note,
        if s.actions.is_empty() {
            "none".into()
        } else {
            s.actions.join(", ")
        },
        disk,
        render_process(&s.process),
        daemon
    )
}

pub fn render_switch(r: &SwitchResult) -> String {
    format!(
        "switched to {} (previous: {}); confirm before {} or it rolls back",
        r.generation,
        or_dash(r.previous.as_deref()),
        r.deadline
    )
}

/// What `fawkes reexec` prints once the daemon answers `status` on its new binary.
pub fn render_reexec(r: &ReexecResult, s: &Status) -> String {
    let app = &s.process;
    let generation = or_dash(app.generation.as_deref());
    let kept = match app.state {
        ProcessState::Running => format!("{generation} keeps running (pid {})", or_dash(app.pid)),
        ProcessState::Failed => format!("{generation} stays failed"),
        ProcessState::Stopped | ProcessState::Unknown => "no app runs".into(),
    };
    format!(
        "{}'s daemon (pid {}) re-executed: fawkes {} → {}; {kept}",
        s.name, r.pid, r.from, r.to
    )
}

/// What `fawkes wait` prints for a generation that was confirmed: still permanent, or retired by a later one.
pub fn render_decided(g: &Generation) -> String {
    let at = or_dash(g.confirmed_at.as_deref());
    if g.state == GenState::Retired {
        format!("{} was confirmed at {at} and has been retired since", g.id)
    } else {
        format!("{} is permanent (confirmed at {at})", g.id)
    }
}

/// The answer to `request` as text. A result this client cannot read (a newer daemon's) is shown as the JSON it is.
pub fn render_result(request: &Request, result: &Value) -> String {
    fn typed<T: serde::de::DeserializeOwned>(
        result: &Value,
        render: impl FnOnce(T) -> String,
    ) -> String {
        T::deserialize(result).map_or_else(|_| result.to_string(), render)
    }
    match request {
        Request::Stage { .. } => typed(result, |r: StageResult| {
            let g = &r.generation;
            let hint = format!("`fawkes test {}` tests it", g.id);
            if r.existing {
                format!(
                    "{} is already staged as {} ({}); nothing was built; {hint}",
                    g.short_commit(),
                    g.id,
                    g.git_ref
                )
            } else {
                format!(
                    "staged {} ({} @ {}); {hint}",
                    g.id,
                    g.git_ref,
                    g.short_commit()
                )
            }
        }),
        Request::Test { .. } => typed(result, |g: Generation| {
            format!(
                "{} passed its test on port {} in {}s; `fawkes switch {}` goes live",
                g.id,
                or_dash(g.test.as_ref().map(|t| t.port)),
                or_dash(g.test.as_ref().map(|t| format!("{:?}", t.seconds))),
                g.id
            )
        }),
        Request::Switch { .. } => typed(result, |r: SwitchResult| render_switch(&r)),
        Request::Confirm { .. } => {
            typed(result, |g: Generation| format!("{} is now permanent", g.id))
        }
        Request::Rollback { keep_storage } => typed(result, |r: RollbackResult| {
            format!(
                "rolled back {}; now running {} ({}){}",
                r.from,
                or_dash(r.to),
                r.reason,
                if *keep_storage {
                    "; storage kept as it was"
                } else {
                    ""
                }
            )
        }),
        Request::Prune => typed(result, |r: PruneResult| {
            if r.removed.is_empty() {
                "nothing to remove".into()
            } else {
                format!(
                    "removed {}; kept {}",
                    r.removed.join(", "),
                    r.kept.join(", ")
                )
            }
        }),
        Request::Start | Request::Stop | Request::Restart => {
            typed(result, |p: Process| render_process(&p))
        }
        Request::Status => typed(result, |s: Status| render_status(&s)),
        Request::History => typed(result, |h: History| {
            h.generations
                .iter()
                .map(|entry| &entry.generation)
                .map(|g| {
                    let reason = g
                        .rollback_reason
                        .as_ref()
                        .map_or_default(|s| format!("  ({s})"));
                    format!("{}{reason}", render_generation(g))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        Request::Log { .. } => typed(result, |e: Events| {
            e.events
                .iter()
                .map(render_event)
                .collect::<Vec<_>>()
                .join("\n")
        }),
        Request::Report { generation, .. } => typed(result, |r: ReportResult| {
            if r.info.is_empty() {
                format!("cleared the info of {generation}")
            } else {
                let keys: Vec<_> = ordered_info(&r.info).iter().map(|(key, _)| *key).collect();
                format!("reported {} for {generation}", keys.join(", "))
            }
        }),
        Request::Reload => typed(result, |r: ReloadResult| {
            if r.changed.is_empty() {
                "reloaded the manifest; nothing changed".into()
            } else {
                format!("reloaded the manifest; changed: {}", r.changed.join(", "))
            }
        }),
        Request::Down => typed(result, |r: DownResult| {
            let stopped = r
                .stopped
                .map_or_else(String::new, |id| format!("{id} and "));
            format!(
                "stopped {stopped}the daemon of {} (pid {})",
                r.name, r.daemon_pid
            )
        }),
        Request::Reexec => typed(result, |r: ReexecResult| {
            format!(
                "the daemon (pid {}) re-executes: fawkes {} → {}",
                r.pid, r.from, r.to
            )
        }),
        Request::Subscribe { .. } => result.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Pending;

    fn status_in(step: Option<Step>, with_pending: bool) -> Status {
        Status {
            name: "sample".into(),
            daemon_pid: 812,
            version: "0.3.0".into(),
            manifest: "/apps/sample/fawkes.toml".into(),
            state_dir: "/apps/sample/state".into(),
            socket: "/apps/sample/state/fawkes.sock".into(),
            permanent: Some("g1".into()),
            current: Some("g2".into()),
            pending: with_pending.then(|| Pending {
                generation: "g2".into(),
                previous: Some("g1".into()),
                deadline: "2030-01-01T00:00:00Z".into(),
            }),
            step,
            busy: None,
            actions: Vec::new(),
            disk: None,
            process: Process {
                state: ProcessState::Stopped,
                generation: Some("g2".into()),
                pid: None,
                port: 4790,
                started_at: None,
                uptime: None,
                restarts: 0,
                last_exit: None,
                restart_at: None,
                info: BTreeMap::new(),
            },
        }
    }

    /// `pending` stays set during a rollback, so only `step` says whether a confirm is still possible.
    #[test]
    fn status_text_asks_for_a_confirm_only_while_the_switch_awaits_one() {
        let text = |step, with_pending| render_status(&status_in(step, with_pending));
        let waits = "waiting for confirm until 2030-01-01T00:00:00Z";
        assert!(text(Some(Step::Pending), true).contains(waits));
        // An older daemon sends no step.
        assert!(text(None, true).contains(waits));
        assert!(!text(None, false).contains("confirm"));
        assert!(text(None, false).contains("\nactions: none\n"));
        let mut status = status_in(Some(Step::Pending), true);
        status.actions = vec!["stop".into(), "confirm g2".into()];
        assert!(render_status(&status).contains("\nactions: stop, confirm g2\n"));

        let rolling_back = text(Some(Step::RollingBack), true);
        assert!(!rolling_back.contains("confirm"), "{rolling_back}");
        assert!(
            rolling_back.contains("rolling back g2 to g1"),
            "{rolling_back}"
        );

        // Which daemon answered, on a line of its own; an older daemon's status has none of it.
        let mut status = status_in(None, false);
        assert!(
            render_status(&status).ends_with(
                "\ndaemon 0.3.0  manifest /apps/sample/fawkes.toml  state_dir /apps/sample/state  socket /apps/sample/state/fawkes.sock"
            ),
            "{}",
            render_status(&status)
        );
        status.version.clear();
        assert!(!render_status(&status).contains("manifest"));

        let failed = text(Some(Step::RestoreFailed), true);
        assert!(!failed.contains("confirm"), "{failed}");
        assert!(
            failed.contains("g2's storage could not be restored")
                && failed.contains("`fawkes rollback` retries it")
                && failed.contains("`fawkes rollback --keep-storage` gives it up"),
            "{failed}"
        );
    }

    #[test]
    fn a_low_disk_has_a_line_in_status_and_in_the_fleet() {
        use crate::protocol::{Disk, FleetAppView};
        let disk = |free: u64, low| Disk {
            free: free * 100_000_000,
            total: 2_450_000_000_000,
            low,
            min_free: 10_000_000_000,
        };
        let mut status = status_in(None, false);
        status.disk = Some(disk(150, false));
        assert!(!render_status(&status).contains("disk"));
        status.disk = Some(disk(92, true));
        assert!(
            render_status(&status).contains(
                "\nactions: none\ndisk low: 9.2 GB free of 2450.0 GB; stage refuses below 10.0 GB\nprocess "
            ),
            "{}",
            render_status(&status)
        );

        let app = |name: &str, status: Status| FleetAppView {
            name: name.into(),
            supervisor: None,
            error: None,
            status: Some(status),
        };
        let low = |free| Status {
            disk: Some(disk(free, true)),
            ..status_in(None, false)
        };
        let view = FleetView {
            supervisor: None,
            apps: vec![
                app("xana", low(92)),
                app("fine", status_in(None, false)),
                app("ultimate", low(90)),
            ],
        };
        let text = render_fleet(&view);
        assert_eq!(
            text.lines().nth(1),
            Some("disk low: 9.0 GB free of 2450.0 GB (xana, ultimate)"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 5, "{text}");
    }

    fn info(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn the_info_of_an_app_reads_url_first_then_by_key() {
        let both = info(&[
            ("remote", "https://r"),
            ("log", "/p/l"),
            ("url", "http://u"),
        ]);
        assert_eq!(
            render_info(&both),
            "url http://u · log /p/l · remote https://r"
        );
        assert_eq!(render_info(&info(&[("log", "/p/l")])), "log /p/l");
        assert_eq!(render_info(&BTreeMap::new()), "info cleared");

        let mut status = status_in(None, false);
        status.process.info = both;
        let text = render_status(&status);
        assert!(
            text.contains("restarts 0\n  url     http://u\n  log     /p/l\n  remote  https://r\n"),
            "{text}"
        );

        let cleared = render_result(
            &Request::Report {
                generation: "g3".into(),
                info: BTreeMap::new(),
            },
            &serde_json::json!({"info": {}}),
        );
        assert_eq!(cleared, "cleared the info of g3");
    }

    fn running_status(pending: bool) -> Status {
        let mut status = status_in(pending.then_some(Step::Pending), pending);
        status.name = "xana".into();
        status.manifest = "/h/xana.toml".into();
        status.state_dir = "/h/xana".into();
        status.process.state = ProcessState::Running;
        status.process.generation = Some("g3".into());
        status.process.pid = Some(71568);
        status.process.port = 4749;
        if let Some(p) = status.pending.as_mut() {
            p.generation = "g3".into();
        }
        status
    }

    #[test]
    fn reexec_says_which_binary_the_daemon_moved_to_and_what_runs() {
        let result = ReexecResult {
            pid: 812,
            from: "0.1.4".into(),
            to: "0.1.5".into(),
        };
        let mut status = running_status(false);
        status.process.generation = Some("g5".into());
        status.process.pid = Some(4657);
        assert_eq!(
            render_reexec(&result, &status),
            "xana's daemon (pid 812) re-executed: fawkes 0.1.4 → 0.1.5; g5 keeps running (pid 4657)"
        );
        status.process.state = ProcessState::Stopped;
        assert!(render_reexec(&result, &status).ends_with("; no app runs"));
    }

    /// One line for the supervisor, then one per app in columns; an app whose daemon does not answer says why in its
    /// state.
    #[test]
    fn the_fleet_is_a_line_for_the_supervisor_and_an_aligned_line_per_app() {
        use crate::protocol::{ErrorCode, FleetApp, FleetAppView, err};
        let supervised = |name: &str, state| FleetApp {
            name: name.into(),
            manifest: format!("/h/{name}.toml").into(),
            state,
            daemon_pid: None,
            restart_at: None,
            failures: 0,
        };
        let app =
            |name: &str, state: Option<AppState>, status: Option<Status>, code| FleetAppView {
                name: name.into(),
                supervisor: state.map(|state| supervised(name, state)),
                error: status.is_none().then(|| err(code, "m")),
                status,
            };
        let unreachable = ErrorCode::DaemonUnreachable;
        let mut view = FleetView {
            supervisor: Some(FleetStatus {
                pid: 812,
                version: "0.1.5".into(),
                socket: "/h/supervisor.sock".into(),
                apps: vec![
                    supervised("docs", AppState::Running),
                    supervised("ultimate", AppState::Down),
                    supervised("xana", AppState::Running),
                ],
            }),
            apps: vec![
                app("docs", Some(AppState::Running), None, ErrorCode::NoAnswer),
                app("sample", None, Some(status_in(None, false)), unreachable),
                app("ultimate", Some(AppState::Down), None, unreachable),
                app(
                    "xana",
                    Some(AppState::Running),
                    Some(running_status(false)),
                    unreachable,
                ),
            ],
        };
        assert_eq!(
            render_fleet(&view),
            [
                "supervisor pid 812, fawkes 0.1.5, 3 apps",
                "  docs      no answer  -   -          -          -             supervised",
                "  sample    stopped    g2  -          port 4790  fawkes 0.3.0  -",
                "  ultimate  down       -   -          -          -             down",
                "  xana      running    g3  pid 71568  port 4749  fawkes 0.3.0  supervised",
            ]
            .join("\n")
        );

        // Without a supervisor nothing is supervised, and a daemon that is not there is said apart from a silent one.
        view.supervisor = None;
        view.apps = vec![
            app("docs", None, None, ErrorCode::NoAnswer),
            app("ultimate", None, None, unreachable),
        ];
        assert_eq!(
            render_fleet(&view),
            [
                "no supervisor runs; `fawkes service install` or `fawkes up --all` starts one",
                "  docs      no answer  -  -  -  -  -",
                "  ultimate  no daemon  -  -  -  -  -",
            ]
            .join("\n")
        );
    }

    #[test]
    fn the_supervisor_s_reexec_says_how_many_daemons_keep_running() {
        let result = ReexecResult {
            pid: 812,
            from: "0.1.4".into(),
            to: "0.1.5".into(),
        };
        let mut status = FleetStatus {
            pid: 812,
            version: "0.1.5".into(),
            socket: "/h/supervisor.sock".into(),
            apps: Vec::new(),
        };
        assert_eq!(
            render_supervisor_reexec(&result, &status),
            "the supervisor (pid 812) re-executed: fawkes 0.1.4 → 0.1.5; 0 daemons keep running"
        );
        status.apps.push(crate::protocol::FleetApp {
            name: "xana".into(),
            manifest: "/h/xana.toml".into(),
            state: AppState::Running,
            daemon_pid: Some(4657),
            restart_at: None,
            failures: 0,
        });
        assert!(render_supervisor_reexec(&result, &status).ends_with("; 1 daemon keeps running"));
    }

    #[test]
    fn the_block_up_prints_says_where_the_app_is() {
        let log = Path::new("/h/xana/generations/g3/logs/app.log");
        let mut status = running_status(false);
        status.process.info = info(&[
            ("url", "http://127.0.0.1:4749"),
            ("remote", "https://xana.ts.net:4749"),
            ("log", "/p/supervisor.log"),
        ]);
        assert_eq!(
            render_up_block(&status, log, Some("http://127.0.0.1:4749/health")),
            "\nxana  g3 permanent · pid 71568 · port 4749\n\
             \x20 url      http://127.0.0.1:4749\n\
             \x20 log      /p/supervisor.log\n\
             \x20 remote   https://xana.ts.net:4749\n\
             \x20 output   /h/xana/generations/g3/logs/app.log\n\
             \x20 state    /h/xana\n\
             \x20 next     fawkes status --manifest /h/xana.toml · fawkes watch · fawkes down --manifest /h/xana.toml\n"
        );

        // Nothing reported: the health URL is the address, and it is the only row that is guessed.
        let bare = render_up_block(&running_status(false), log, Some("http://127.0.0.1:4749/"));
        assert!(
            bare.contains("\n  url      http://127.0.0.1:4749/\n  output   "),
            "{bare}"
        );
        // No URL anywhere: no row, not an empty one.
        let none = render_up_block(&running_status(false), log, None);
        assert!(!none.contains("url"), "{none}");
        assert!(none.contains("\n  output   /h/xana/generations/g3/logs/app.log\n"));

        // A switch that still awaits its confirm says so, with the time it is due by.
        let pending = render_up_block(&running_status(true), log, None);
        assert!(
            pending.starts_with(
                "\nxana  g3 pending, confirm by 2030-01-01T00:00:00Z · pid 71568 · port 4749\n"
            ),
            "{pending}"
        );
        // A rollback decided on it says that instead.
        let mut rolling_back = running_status(true);
        rolling_back.step = Some(Step::RollingBack);
        assert!(render_up_block(&rolling_back, log, None).contains("g3 rolling back · pid"));
    }

    fn generation(id: &str, state: &str, reason: Option<&str>) -> Value {
        serde_json::json!({
            "id": id, "ref": "main", "commit": "0123456789abcdef", "state": state, "dir": "/w/g",
            "staged_at": "t", "test": {"port": 4791, "passed": true, "at": "t", "seconds": 1.5, "message": "ok"},
            "switched_at": null, "confirmed_at": null, "rolled_back_at": null, "rollback_reason": reason,
            "failed_starts": 0
        })
    }

    fn process(state: &str, last_exit: &Value) -> Value {
        serde_json::json!({
            "state": state, "generation": "g3", "pid": 812, "port": 4790, "started_at": "t",
            "uptime": 12.5, "restarts": 1, "last_exit": last_exit, "restart_at": null
        })
    }

    /// Every request, in the order of the cases below; the match makes a new variant fail to compile until it is
    /// listed here.
    fn position(request: &Request) -> usize {
        match request {
            Request::Start => 0,
            Request::Stop => 1,
            Request::Restart => 2,
            Request::Stage { .. } => 3,
            Request::Test { .. } => 4,
            Request::Switch { .. } => 5,
            Request::Confirm { .. } => 6,
            Request::Prune => 7,
            Request::Rollback { .. } => 8,
            Request::Status => 9,
            Request::History => 10,
            Request::Log { .. } => 11,
            Request::Subscribe { .. } => 12,
            Request::Reload => 13,
            Request::Down => 14,
            Request::Report { .. } => 15,
            Request::Reexec => 16,
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one case per request")]
    fn each_result_is_rendered_as_a_sentence_with_its_key_facts() {
        let g = |id: &str| id.to_string();
        let exit = serde_json::json!({"at": "t", "code": null, "signal": 9, "requested": false});
        let event = serde_json::json!({
            "seq": 1, "at": "2030-01-01T00:00:00Z", "kind": "switched", "generation": "g3", "message": "went live"
        });
        let status = serde_json::json!({
            "name": "sample", "daemon_pid": 99, "permanent": "g2", "current": "g3",
            "pending": {"generation": "g3", "previous": "g2", "deadline": "2030-01-01T00:01:00Z"},
            "step": "pending", "busy": null, "process": process("running", &Value::Null)
        });
        let cases = [
            (
                Request::Start,
                process("running", &Value::Null),
                vec![
                    "running  g3",
                    "pid 812",
                    "port 4790",
                    "up 12.5s",
                    "restarts 1",
                ],
            ),
            (
                Request::Stop,
                process("stopped", &exit),
                vec![
                    "stopped  g3",
                    "restarts 1",
                    "last exit: signal 9 (unrequested)",
                ],
            ),
            (
                Request::Restart,
                process("failed", &Value::Null),
                vec!["failed  g3", "restarts 1"],
            ),
            (
                Request::Stage { git_ref: g("main") },
                generation("g3", "staged", None),
                vec!["staged g3", "main @ 01234567", "`fawkes test g3` tests it"],
            ),
            (
                Request::Test {
                    generation: g("g3"),
                },
                generation("g3", "tested", None),
                vec![
                    "g3 passed its test on port 4791 in 1.5s",
                    "`fawkes switch g3` goes live",
                ],
            ),
            (
                Request::Switch {
                    generation: g("g3"),
                    deadline: None,
                },
                serde_json::json!({"generation": "g3", "previous": "g2", "deadline": "2030-01-01T00:01:00Z"}),
                vec![
                    "switched to g3 (previous: g2)",
                    "confirm before 2030-01-01T00:01:00Z or it rolls back",
                ],
            ),
            (
                Request::Confirm { generation: None },
                generation("g3", "permanent", None),
                vec!["g3 is now permanent"],
            ),
            (
                Request::Prune,
                serde_json::json!({"removed": ["g1", "g2"], "kept": ["g3"]}),
                vec!["removed g1, g2", "kept g3"],
            ),
            (
                Request::Rollback { keep_storage: true },
                serde_json::json!({"from": "g3", "to": "g2", "reason": "deadline passed"}),
                vec![
                    "rolled back g3",
                    "now running g2 (deadline passed)",
                    "storage kept as it was",
                ],
            ),
            (
                Request::Status,
                status,
                vec![
                    "sample  daemon pid 99",
                    "permanent g2  current g3",
                    "waiting for confirm until 2030-01-01T00:01:00Z",
                    "process running  g3",
                ],
            ),
            (
                Request::History,
                serde_json::json!({"generations": [
                    generation("g2", "permanent", None),
                    generation("g3", "rolled_back", Some("unhealthy")),
                ]}),
                vec!["g2", "permanent", "g3", "rolled_back", "(unhealthy)"],
            ),
            (
                Request::Log {
                    limit: None,
                    since: None,
                    follow: false,
                },
                serde_json::json!({"events": [event]}),
                vec!["2030-01-01T00:00:00Z", "switched", "g3", "went live"],
            ),
            (
                Request::Reload,
                serde_json::json!({"changed": ["port", "env"]}),
                vec!["reloaded the manifest; changed: port, env"],
            ),
            (
                Request::Report {
                    generation: g("g3"),
                    info: BTreeMap::new(),
                },
                serde_json::json!({"info": {"log": "/p/x.log", "url": "http://x"}}),
                vec!["reported url, log for g3"],
            ),
            (
                Request::Down,
                serde_json::json!({"name": "sample", "daemon_pid": 99, "stopped": "g3"}),
                vec!["stopped g3 and the daemon of sample (pid 99)"],
            ),
            (
                Request::Subscribe {
                    limit: None,
                    since: None,
                },
                Value::Null,
                vec!["null"],
            ),
            (
                Request::Reexec,
                serde_json::json!({"pid": 99, "from": "0.1.4", "to": "0.1.5"}),
                vec!["the daemon (pid 99) re-executes: fawkes 0.1.4 → 0.1.5"],
            ),
        ];
        let mut seen: Vec<usize> = cases
            .iter()
            .map(|(request, ..)| position(request))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..cases.len()).collect::<Vec<_>>());
        for (request, result, words) in cases {
            let text = render_result(&request, &result);
            for word in words {
                assert!(text.contains(word), "{request:?}: {word:?} not in {text:?}");
            }
        }
    }

    #[test]
    fn the_variable_parts_of_a_rendered_result_follow_the_result() {
        let mut existing = generation("g3", "tested", None);
        existing["existing"] = serde_json::json!(true);
        let text = render_result(
            &Request::Stage {
                git_ref: "main".into(),
            },
            &existing,
        );
        assert!(
            text.contains("01234567 is already staged as g3")
                && text.contains("nothing was built")
                && text.contains("`fawkes test g3` tests it"),
            "{text}"
        );

        let rolled_back = serde_json::json!({"from": "g3", "to": null, "reason": "no previous"});
        let text = render_result(
            &Request::Rollback {
                keep_storage: false,
            },
            &rolled_back,
        );
        assert!(text.contains("now running -"), "{text}");
        assert!(!text.contains("storage kept"), "{text}");

        let nothing = serde_json::json!({"removed": [], "kept": ["g1"]});
        assert_eq!(
            render_result(&Request::Prune, &nothing),
            "nothing to remove"
        );

        // A result this client cannot read is shown as the JSON it is.
        let unknown = serde_json::json!({"from": "a newer daemon"});
        let text = render_result(
            &Request::Stage {
                git_ref: "main".into(),
            },
            &unknown,
        );
        assert_eq!(text, unknown.to_string());
    }
}
