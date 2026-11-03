//! The happy path and the safety net, against the real binary and its socket:
//! stage → test → switch → confirm, the lifecycle commands, then a switch that is never confirmed and rolls back.

mod common;
use common::Harness;
use serde_json::Value;

#[test]
fn upgrade_confirms_and_a_missed_deadline_rolls_back_code_and_storage() {
    let mut h = Harness::new(4800, "");
    h.commit("v1", "confirm");
    h.commit("v2", "never");
    h.start_daemon();

    // Nothing runs before the first switch.
    let status = h.status();
    assert_eq!(status["process"]["state"], "stopped");
    assert_eq!(status["permanent"], Value::Null);

    // stage → test → switch → the app confirms itself.
    let g1 = h.ok(&["stage", "v1"]);
    assert_eq!(
        (g1["id"].as_str(), g1["state"].as_str()),
        (Some("g1"), Some("staged"))
    );
    let g1 = h.ok(&["test", "g1"]);
    assert_eq!(g1["state"], "tested");
    assert_eq!(g1["test"]["passed"], true);
    assert!(
        h.versions().is_empty(),
        "a test works on a copy of the storage, never the live one"
    );

    let switched = h.ok(&["switch", "g1"]);
    assert_eq!(
        (
            switched["generation"].as_str(),
            switched["previous"].clone()
        ),
        (Some("g1"), Value::Null)
    );
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    assert_eq!(h.generation("g1")["state"], "permanent");
    assert!(h.generation("g1")["confirmed_at"].is_string());
    assert_eq!(h.status()["pending"], Value::Null);
    assert_eq!(h.status()["process"]["state"], "running");
    assert_eq!(h.get().as_deref(), Some("v1"));

    // Lifecycle: stop stays stopped and is not a crash; start and restart run the same generation.
    let stopped = h.ok(&["stop"]);
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["last_exit"]["requested"], true);
    assert_eq!(h.get(), None);
    assert_eq!(h.fk(&["stop"]).1["code"], "invalid_state");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(
        h.status()["process"]["state"],
        "stopped",
        "a requested stop is not restarted"
    );

    let started = h.ok(&["start"]);
    assert_eq!(
        (started["state"].as_str(), started["generation"].as_str()),
        (Some("running"), Some("g1"))
    );
    h.wait("v1 to answer after start", 10, || {
        h.get().as_deref() == Some("v1")
    });
    let old_pid = h.status()["process"]["pid"].clone();
    let restarted = h.ok(&["restart"]);
    assert_eq!(restarted["restarts"], 1);
    assert_ne!(restarted["pid"], old_pid);
    h.wait("v1 to answer after restart", 10, || {
        h.get().as_deref() == Some("v1")
    });
    assert_eq!(h.status()["process"]["restarts"], 1);

    // A second generation that never confirms: the 2 s deadline passes and everything goes back.
    assert_eq!(h.ok(&["stage", "v2"])["id"], "g2");
    assert_eq!(h.ok(&["test", "g2"])["state"], "tested");
    let switched = h.ok(&["switch", "g2", "--deadline", "2"]);
    assert_eq!(switched["previous"], "g1");
    h.wait("v2's row in the storage", 5, || {
        h.versions().contains(&"v2".to_string())
    });
    assert_eq!(h.status()["pending"]["generation"], "g2");

    h.wait("the deadline to roll g2 back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    let g2 = h.generation("g2");
    assert!(
        g2["rollback_reason"].as_str().unwrap().contains("deadline"),
        "reason: {g2}"
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert!(
        !h.versions().contains(&"v2".to_string()),
        "v2's storage row must be gone: {:?}",
        h.versions()
    );
    let status = h.status();
    assert_eq!(
        (status["permanent"].as_str(), status["pending"].clone()),
        (Some("g1"), Value::Null)
    );
    assert_eq!(status["process"]["generation"], "g1");
    assert_eq!(
        status["process"]["restarts"], 0,
        "restarts count per generation since it became current"
    );

    // A rolled-back generation is final.
    let (code, error) = h.fk(&["switch", "g2"]);
    assert_eq!(code, 1);
    assert_eq!(error["code"], "invalid_state");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("g2 was rolled back") && message.contains("stage the ref again"),
        "{message}"
    );
}

/// The daemon dies (SIGKILL) while a switch awaits confirm: its next start ends the app it left behind, restores the
/// storage and runs the permanent generation again. Before that, a switch with an out-of-range deadline is refused
/// before anything stops, and a second daemon for the same state directory is refused.
#[test]
fn a_daemon_killed_during_a_pending_switch_rolls_it_back_at_its_next_start() {
    let mut h = Harness::new(4820, "");
    h.commit("v1", "confirm");
    h.commit("v2", "never");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });

    let (code, stderr) = h.run_second_daemon();
    assert_eq!(code, 1, "a second daemon must refuse to start: {stderr}");
    assert!(stderr.contains("another daemon"), "{stderr}");
    assert_eq!(h.get().as_deref(), Some("v1"));

    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    for deadline in ["0", "99999999999999"] {
        let (code, error) = h.fk(&["switch", "g2", "--deadline", deadline]);
        assert_eq!(
            (code, error["code"].as_str()),
            (2, Some("usage")),
            "--deadline {deadline}: {error}"
        );
    }
    assert_eq!(h.generation("g2")["state"], "tested");
    assert_eq!(
        h.get().as_deref(),
        Some("v1"),
        "a refused switch never stops the live app"
    );

    h.ok(&["switch", "g2"]);
    h.wait("v2's row in the storage", 5, || {
        h.versions().contains(&"v2".to_string())
    });
    h.kill_daemon();
    assert_eq!(
        h.get().as_deref(),
        Some("v2"),
        "the app outlives a killed daemon"
    );

    h.start_daemon();
    h.wait("the restarted daemon to roll g2 back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert_eq!(h.generation("g2")["rollback_reason"], "daemon_restart");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert!(
        !h.versions().contains(&"v2".to_string()),
        "v2's storage row must be gone: {:?}",
        h.versions()
    );
    let status = h.status();
    assert_eq!(
        (status["permanent"].as_str(), status["pending"].clone()),
        (Some("g1"), Value::Null)
    );
}

/// The new generation's first process starts only once state.json says the switch is pending, so a daemon that dies
/// right after spawning it still rolls the switch back at its next start.
#[test]
fn a_switch_records_its_intent_before_the_new_generation_starts() {
    let mut h = Harness::new(4830, "");
    // v2, started live, keeps a copy of the state.json it finds and kills the daemon, its parent, on the spot.
    let manifest = h.dir.join("fawkes.toml");
    let text = std::fs::read_to_string(&manifest).unwrap().replace(
        r#"start = "node app.js""#,
        r#"start = 'if [ "$(cat VERSION)" = v2 ] && [ "$FAWKES_PHASE" = live ]; then cp {state}/state.json {home}/at-start.json; kill -9 $PPID; exit 1; fi; exec node app.js'"#,
    );
    std::fs::write(&manifest, text).unwrap();
    h.commit("v1", "confirm");
    h.commit("v2", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    // Whether the answer reaches the client before the daemon dies is a race; the state on disk is not.
    h.fk(&["switch", "g2"]);
    h.wait("v2 to kill the daemon", 10, || {
        h.dir.join("home/at-start.json").exists() && h.fk(&["status"]).0 == 3
    });
    let at_start: Value =
        serde_json::from_str(&std::fs::read_to_string(h.dir.join("home/at-start.json")).unwrap())
            .unwrap();
    assert_eq!(at_start["pending"]["generation"], "g2", "{at_start}");
    let g2 = at_start["generations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == "g2")
        .unwrap();
    assert_eq!(g2["state"], "current");

    h.kill_daemon();
    h.start_daemon();
    h.wait("the restarted daemon to roll g2 back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert_eq!(h.generation("g2")["rollback_reason"], "daemon_restart");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.status()["permanent"], "g1");
}

/// Found in review: a crash only reaped the group leader, so processes the app had started lived on, unsupervised,
/// while the rollback restored the storage under them.
#[test]
fn a_crash_before_confirm_ends_the_whole_process_group_before_rolling_back() {
    let mut h = Harness::new(4810, "");
    let manifest = h.dir.join("fawkes.toml");
    let text = std::fs::read_to_string(&manifest).unwrap().replace(
        r#"start = "node app.js""#,
        r#"start = "sleep 6$(cut -c2 VERSION)1 & node app.js""#,
    );
    std::fs::write(&manifest, text).unwrap();
    h.commit("v1", "confirm");
    h.commit("v2", "crash-live");
    h.start_daemon();
    let alive = |pattern: &str| {
        std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .is_ok_and(|o| !o.stdout.is_empty())
    };

    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    h.ok(&["switch", "g2"]);
    h.wait("g2's crash to roll it back", 15, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert!(
        h.generation("g2")["rollback_reason"]
            .as_str()
            .unwrap()
            .contains("exited before confirm")
    );
    assert!(
        !alive("sleep 621"),
        "g2's background process outlived its crash"
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert!(
        alive("sleep 611"),
        "g1 runs again, with its own background process"
    );
}

/// A confirm that arrives just after the pending generation crashed must not make the crashed code permanent: the
/// crash decides, and the confirm is refused.
#[test]
fn a_confirm_racing_a_crash_of_the_pending_generation_is_refused() {
    let mut h = Harness::new(4840, "");
    h.commit("v1", "confirm");
    h.commit("v2", "crash-confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    h.ok(&["switch", "g2"]);

    let answer = h.dir.join("home/confirm.json");
    let mut confirm = Value::Null;
    h.wait("the late confirm to be answered", 15, || {
        confirm = std::fs::read_to_string(&answer)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        !confirm.is_null()
    });
    assert_eq!(confirm["code"], "invalid_state", "{confirm}");
    h.wait("g2's crash to roll it back", 15, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert!(
        h.generation("g2")["rollback_reason"]
            .as_str()
            .unwrap()
            .contains("exited before confirm"),
        "{}",
        h.generation("g2")
    );
    assert_eq!(h.status()["permanent"], "g1");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
}

/// With `rollback_on_unhealthy`, a pending generation whose health check gives up rolls back at once, long before
/// its confirm deadline.
#[test]
fn rollback_on_unhealthy_rolls_back_without_waiting_for_the_deadline() {
    let mut h = Harness::new(4850, "rollback_on_unhealthy = true");
    h.edit_manifest("timeout = 20", "timeout = 3");
    h.commit("v1", "confirm");
    h.commit("v2", "unhealthy-live");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    let switched = std::time::Instant::now();
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("g2's failed health check to roll it back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert!(switched.elapsed().as_secs() < 20);
    assert_eq!(h.generation("g2")["rollback_reason"], "health check failed");
    assert_eq!(h.status()["permanent"], "g1");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
}
