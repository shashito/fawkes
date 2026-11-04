//! The happy path and the safety net, against the real binary and its socket:
//! stage → test → switch → confirm, the lifecycle commands, then a switch that is never confirmed and rolls back.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic fails the test, which is what a helper that cannot proceed should do"
)]

mod common;
use common::Harness;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

#[test]
fn upgrade_confirms_and_a_missed_deadline_rolls_back_code_and_storage() {
    let mut h = Harness::new("");
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

    // A second generation that never confirms: the 5 s deadline passes and everything goes back.
    assert_eq!(h.ok(&["stage", "v2"])["id"], "g2");
    assert_eq!(h.ok(&["test", "g2"])["state"], "tested");
    let switched = h.ok(&["switch", "g2", "--deadline", "5"]);
    assert_eq!(switched["previous"], "g1");
    // v2 writes its row before it listens, so serving means both.
    h.wait("v2 to serve", 5, || h.get().as_deref() == Some("v2"));
    assert!(h.versions().contains(&"v2".to_string()));
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
    let mut h = Harness::new("");
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
    // v2 writes its row before it listens, so serving means both.
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));
    assert!(h.versions().contains(&"v2".to_string()));
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

/// The new generation's first process starts only once state.json says the switch is pending, and it is on record
/// before it runs, so a daemon that dies right after spawning it still stops it and rolls the switch back at its next
/// start.
#[test]
fn a_switch_records_its_intent_before_the_new_generation_starts() {
    let mut h = Harness::new("");
    // v2, started live, keeps a copy of the state.json it finds, kills the daemon, its parent, on the spot, and runs on.
    let manifest = h.dir.join("fawkes.toml");
    let text = std::fs::read_to_string(&manifest).unwrap().replace(
        r#"start = "node app.js""#,
        r#"start = 'if [ "$(cat VERSION)" = v2 ] && [ "$FAWKES_PHASE" = live ]; then cp {state}/state.json {home}/at-start.json; kill -9 $PPID; fi; exec node app.js'"#,
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
    assert_eq!(at_start["txn"]["step"], "pending", "{at_start}");
    assert_eq!(at_start["txn"]["generation"], "g2", "{at_start}");
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

/// A crash ends the app's whole process group, not only its leader: processes the app started must not live on,
/// unsupervised, while the rollback restores the storage under them.
#[test]
fn a_crash_before_confirm_ends_the_whole_process_group_before_rolling_back() {
    let mut h = Harness::new("");
    // Each start leaves a background process in the app's group, and its pid in the generation's code directory.
    h.edit_manifest(
        r#"start = "node app.js""#,
        r#"start = "sleep 600 & echo $! > background.pid; node app.js""#,
    );
    h.commit("v1", "confirm");
    h.commit("v2", "crash-live");
    h.start_daemon();
    let alive = |id: &str| {
        let dir = h.generation(id)["dir"].as_str().unwrap().to_string();
        let pid: i32 = std::fs::read_to_string(format!("{dir}/background.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only asks whether the process exists; nothing is sent.
        unsafe { libc::kill(pid, 0) == 0 }
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
    assert!(!alive("g2"), "g2's background process outlived its crash");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert!(
        alive("g1"),
        "g1 runs again, with its own background process"
    );
}

/// A confirm that arrives just after the pending generation crashed must not make the crashed code permanent: the
/// crash decides, and the confirm is refused.
#[test]
fn a_confirm_racing_a_crash_of_the_pending_generation_is_refused() {
    let mut h = Harness::new("");
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
    // Refused because of the crash, not merely because the rollback had already finished.
    let message = confirm["message"].as_str().unwrap();
    assert!(
        message.contains("before it was confirmed") || message.contains("is being rolled back"),
        "{message}"
    );
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
    let mut h = Harness::new("rollback_on_unhealthy = true");
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

/// The worktrees git knows of in the harness's repository, the repository itself included.
fn worktrees(h: &Harness) -> usize {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(h.dir.join("repo"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

/// A stage whose install fails, or hangs past `install_timeout`, leaves nothing behind: no generation record, no
/// directory, no worktree, no process.
#[test]
fn a_failing_or_hanging_install_leaves_nothing_behind() {
    let mut h = Harness::new(
        r#"install = '''case "$(cat MODE)" in fails) exit 1;; hangs) echo $$ > {state}/install.pid; exec sleep 600;; esac'''
install_timeout = 1"#,
    );
    h.commit("v1", "fails");
    h.commit("v2", "hangs");
    h.commit("v3", "confirm");
    h.start_daemon();

    let (code, error) = h.fk(&["stage", "v1"]);
    assert_eq!(
        (code, error["code"].as_str()),
        (1, Some("install_failed")),
        "{error}"
    );
    let (code, error) = h.fk(&["stage", "v2"]);
    assert_eq!(
        (code, error["code"].as_str()),
        (1, Some("install_failed")),
        "{error}"
    );
    assert!(
        error["message"].as_str().unwrap().contains("timed out"),
        "{error}"
    );
    let pid: i32 = std::fs::read_to_string(h.dir.join("s/install.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(
        // SAFETY: signal 0 only asks whether the process group exists.
        unsafe { libc::kill(-pid, 0) },
        0,
        "the hanging install's process group outlived its timeout"
    );
    for id in ["g1", "g2"] {
        assert!(
            !h.dir.join("s/generations").join(id).exists(),
            "{id}'s directory survived its failed stage"
        );
    }
    assert_eq!(h.ok(&["history"])["generations"], serde_json::json!([]));
    assert_eq!(worktrees(&h), 1);

    assert_eq!(h.ok(&["stage", "v3"])["id"], "g3", "ids are never reused");
    assert_eq!(worktrees(&h), 2);
}

/// `keep = 2`: `prune` removes the oldest generations beyond two, never the permanent one, the pending one or the
/// one it rolls back to, and a confirm prunes by itself.
#[test]
fn prune_keeps_the_newest_generations_and_never_the_ones_a_rollback_needs() {
    let mut h = Harness::new("keep = 2");
    h.commit("v1", "confirm");
    h.commit("v2", "confirm");
    h.commit("v3", "never");
    h.start_daemon();
    let ids = |h: &Harness| -> Vec<String> {
        h.ok(&["history"])["generations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["id"].as_str().unwrap().to_string())
            .collect()
    };
    for (version, id) in [("v1", "g1"), ("v2", "g2")] {
        h.ok(&["stage", version]);
        h.ok(&["test", id]);
        h.ok(&["switch", id]);
        h.wait("the new generation to confirm itself", 15, || {
            h.status()["permanent"] == id
        });
    }
    h.ok(&["stage", "v3"]);
    h.ok(&["test", "g3"]);
    h.ok(&["switch", "g3", "--deadline", "120"]);
    h.ok(&["stage", "v1"]);
    assert_eq!(ids(&h), ["g1", "g2", "g3", "g4"]);

    let pruned = h.ok(&["prune"]);
    assert_eq!(
        pruned["removed"],
        serde_json::json!(["g1", "g4"]),
        "{pruned}"
    );
    assert_eq!(ids(&h), ["g2", "g3"]);
    for id in ["g1", "g4"] {
        assert!(
            !h.dir.join("s/generations").join(id).exists(),
            "{id} survived"
        );
    }
    assert!(h.dir.join("s/generations/g3/snapshot/index.json").exists());
    assert_eq!(worktrees(&h), 3);

    // The rollback still has everything it needs.
    h.ok(&["rollback"]);
    h.wait("v2 to serve again", 10, || h.get().as_deref() == Some("v2"));

    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g5"]);
    h.ok(&["switch", "g5"]);
    h.wait("the confirm of g5 to prune g2", 15, || {
        ids(&h) == ["g3", "g5"]
    });
    assert_eq!(worktrees(&h), 3);
}

/// One transaction step at a time, but an automatic rollback never waits for a stage or test: `stop` during a
/// switch and `stage` during a test are `busy`, and a deadline that passes during a test still rolls back.
#[test]
fn busy_steps_refuse_others_but_never_delay_an_automatic_rollback() {
    let mut h = Harness::new("");
    h.commit("v1", "slow-stop");
    h.commit("v2", "never");
    h.commit("v3", "slow-test");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    std::thread::scope(|scope| {
        let switch = scope.spawn(|| h.fk(&["switch", "g2", "--deadline", "4"]));
        h.wait("the switch to stop g1", 5, || {
            h.status()["busy"] == "switch g2"
        });
        let (code, error) = h.fk(&["stop"]);
        assert_eq!((code, error["code"].as_str()), (1, Some("busy")), "{error}");
        assert_eq!(switch.join().unwrap().0, 0);
    });

    h.ok(&["stage", "v3"]);
    std::thread::scope(|scope| {
        let test = scope.spawn(|| h.fk(&["test", "g3"]));
        h.wait("the test of g3 to start", 5, || {
            h.status()["busy"] == "test g3"
        });
        let (code, error) = h.fk(&["stage", "v1"]);
        assert_eq!((code, error["code"].as_str()), (1, Some("busy")), "{error}");
        h.wait("g2's deadline to roll it back", 10, || {
            h.generation("g2")["state"] == "rolled_back"
        });
        assert_eq!(
            h.status()["busy"],
            "test g3",
            "the rollback ran while the test went on"
        );
        assert_eq!(test.join().unwrap().0, 0);
    });
    assert_eq!(
        h.generation("g2")["rollback_reason"],
        "missed confirm deadline"
    );
    assert_eq!(h.status()["permanent"], "g1");
}

/// An accepted socket must block even where it inherits the listener's non-blocking mode (macOS): a request that
/// arrives in two pieces is answered rather than refused as "no request received", and a reply larger than the
/// socket buffer waits for the client instead of failing at once.
#[test]
fn a_request_that_arrives_in_pieces_is_answered() {
    use std::io::{BufRead, Write};
    let mut h = Harness::new("");
    h.start_daemon();
    let mut stream = std::os::unix::net::UnixStream::connect(h.dir.join("s/fawkes.sock")).unwrap();
    stream.write_all(br#"{"command": "log", "#).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    stream.write_all(b"\"args\": {\"limit\": 1000}}\n").unwrap();
    let mut line = String::new();
    std::io::BufReader::new(&stream)
        .read_line(&mut line)
        .unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(answer["ok"], true, "{answer}");
}

/// A daemon that dies mid-stage leaves its install running; the next daemon stops it before it removes the cut-short
/// generation, so nothing of the stage outlives the daemon that started it. Tests are recorded the same way.
#[test]
fn a_daemon_killed_during_a_stage_leaves_no_install_running() {
    let mut h = Harness::new(r"install = '''echo $$ > {state}/install.pid; exec sleep 600'''");
    h.commit("v1", "confirm");
    h.start_daemon();
    let mut stage = std::process::Command::new(common::BIN)
        .args(["--json", "--manifest"])
        .arg(h.dir.join("fawkes.toml"))
        .args(["stage", "v1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid_file = h.dir.join("s/install.pid");
    h.wait("the install to run", 10, || {
        std::fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n'))
    });
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    h.kill_daemon();
    stage.wait().unwrap();

    h.start_daemon();
    // SAFETY: signal 0 only asks whether the process group exists.
    let alive = unsafe { libc::kill(-pid, 0) } == 0;
    if alive {
        // SAFETY: the group is the orphaned install this test started.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
    assert!(!alive, "the install outlived the daemon that started it");
    assert_eq!(h.ok(&["history"])["generations"], serde_json::json!([]));
}

/// A switch that fails while the permanent generation waits out a crash backoff keeps that restart: the live app
/// comes back as it would have without the switch.
#[test]
fn a_switch_that_fails_during_a_crash_backoff_keeps_the_restart() {
    let mut h = Harness::new("max_failed_starts = 5");
    h.edit_manifest("min_uptime = 1", "min_uptime = 60");
    h.edit_manifest(
        r#"storage = ["app.sqlite"]"#,
        r#"storage = ["app.sqlite", "other.sqlite"]"#,
    );
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
    // A storage file that is not a database: the next switch cannot snapshot it.
    std::fs::write(h.dir.join("home/other.sqlite"), "not a database").unwrap();

    // Two crashes within min_uptime: the second restart waits 2 s, the window the switch lands in.
    let crash = |h: &Harness| {
        let pid = h.status()["process"]["pid"].to_string();
        let killed = std::process::Command::new("kill")
            .args(["-9", &pid])
            .status()
            .unwrap();
        assert!(killed.success());
        h.wait("the crash to be reaped", 5, || {
            h.status()["process"]["state"] == "stopped"
        });
    };
    crash(&h);
    h.wait("the first crash restart", 10, || {
        h.status()["process"]["state"] == "running"
    });
    crash(&h);
    let (code, error) = h.fk(&["switch", "g2"]);
    assert_eq!(
        (code, error["code"].as_str()),
        (1, Some("storage_failed")),
        "{error}"
    );
    h.wait("g1 to be restarted after its backoff", 10, || {
        h.status()["process"]["state"] == "running"
    });
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.generation("g2")["state"], "tested");
}

/// The messages of the events of `kind`, oldest first.
fn messages(h: &Harness, kind: &str) -> Vec<String> {
    h.ok(&["log", "-n", "1000"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .map(|event| event["message"].as_str().unwrap().to_string())
        .collect()
}

/// The permanent generation crash-loops: a crash after a long run restarts it in 1 s, each failed start in a row then
/// backs off 1, 2 s, …, the third gives it up (`failed`, not restarted), and `start` clears that.
#[test]
fn a_crash_looping_permanent_generation_backs_off_then_fails_until_started() {
    let mut h = Harness::new("");
    // While `{home}/crash` exists, every live start exits at once.
    h.edit_manifest(
        r#"start = "node app.js""#,
        r#"start = "if [ -e {home}/crash ]; then exit 3; fi; exec node app.js""#,
    );
    h.commit("v1", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    // The first crash comes after a run longer than min_uptime, so it is not a failed start.
    h.wait("g1 to outlive min_uptime", 10, || {
        h.status()["process"]["uptime"]
            .as_f64()
            .is_some_and(|uptime| uptime > 1.5)
    });

    let marker = h.dir.join("home/crash");
    std::fs::write(&marker, "").unwrap();
    let pid = h.status()["process"]["pid"].to_string();
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    h.wait("g1 to be given up", 20, || {
        h.status()["process"]["state"] == "failed"
    });
    let exits = messages(&h, "exited");
    let backoffs: Vec<&str> = exits
        .iter()
        .map(|message| message.split_once("; ").map_or("", |(_, then)| then))
        .collect();
    assert_eq!(
        backoffs,
        [
            "restarting in 1s",
            "restarting in 1s",
            "restarting in 2s",
            ""
        ],
        "{exits:?}"
    );
    assert!(exits[0].starts_with("signal 9 after"), "{exits:?}");
    assert!(
        exits[1..]
            .iter()
            .all(|message| message.starts_with("exit status 3 after")),
        "{exits:?}"
    );
    assert_eq!(
        messages(&h, "failed"),
        ["3 failed starts in a row; not restarting"]
    );
    assert_eq!(h.generation("g1")["failed_starts"], 3);
    assert_eq!(h.get(), None);

    std::fs::remove_file(&marker).unwrap();
    let started = h.ok(&["start"]);
    assert_eq!(
        (started["state"].as_str(), started["generation"].as_str()),
        (Some("running"), Some("g1"))
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.generation("g1")["failed_starts"], 0);
    assert_eq!(h.status()["permanent"], "g1");
}

/// The three steps whose intent must reach state.json, with state.json unwritable (a directory where its temp file
/// goes): a switch fails before it stops anything, a confirm fails and stays pending, and a rollback
/// restores the storage but keeps the switch pending with the app stopped. A restore that fails does the same. Each
/// time `fawkes rollback` retries, and once it gets through the permanent generation serves on its own storage.
#[test]
fn failed_writes_and_restores_keep_the_switch_pending_until_a_rollback_gets_through() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.commit("v2", "never");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    let blocker = h.dir.join("s/state.json.tmp");
    let refused = |args: &[&str], code: &str, says: &str| {
        let (exit, error) = h.fk(args);
        assert_eq!(
            (exit, error["code"].as_str()),
            (1, Some(code)),
            "{args:?}: {error}"
        );
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(says), "{args:?}: {message}");
    };

    // The switch: its first intent cannot be written, so g1 is never stopped and g2 is still only tested.
    std::fs::create_dir(&blocker).unwrap();
    refused(
        &["switch", "g2"],
        "internal",
        "g2 stays tested and nothing changed",
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.status()["pending"], Value::Null);
    assert_eq!(h.generation("g2")["state"], "tested");
    // A stage whose record cannot be written fails before its worktree exists.
    refused(&["stage", "v1"], "internal", "nothing was staged");
    assert!(!h.dir.join("s/generations/g3").exists());
    std::fs::remove_dir(&blocker).unwrap();

    // The confirm: g2 stays pending and keeps running.
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));
    std::fs::create_dir(&blocker).unwrap();
    refused(
        &["confirm", "g2"],
        "internal",
        "g2 is still pending, confirm again",
    );
    assert_eq!(h.status()["pending"]["generation"], "g2");
    assert_eq!(h.generation("g2")["state"], "current");
    assert_eq!(h.get().as_deref(), Some("v2"));

    // The end of the rollback: the storage is restored, but nothing starts while the switch is still on disk.
    refused(
        &["rollback"],
        "internal",
        "the app stays stopped and `fawkes rollback` retries",
    );
    let status = h.status();
    assert_eq!(status["process"]["state"], "failed", "{status}");
    assert_eq!(status["pending"]["generation"], "g2", "{status}");
    assert_eq!(h.generation("g2")["state"], "rolled_back");
    assert_eq!(h.get(), None);
    std::fs::remove_dir(&blocker).unwrap();

    // A restore that fails: the snapshot is out of reach. The switch stays pending and blocks everything but
    // `rollback`.
    let snapshot = h.dir.join("s/generations/g2/snapshot");
    let moved = h.dir.join("s/snapshot-moved");
    std::fs::rename(&snapshot, &moved).unwrap();
    refused(&["rollback"], "storage_failed", "could not be restored");
    let status = h.status();
    assert_eq!(status["process"]["state"], "failed", "{status}");
    assert_eq!(status["pending"]["generation"], "g2", "{status}");
    for args in [&["start"][..], &["confirm"], &["switch", "g1"]] {
        refused(
            args,
            "invalid_state",
            "`fawkes rollback` retries the restore",
        );
    }
    assert_eq!(h.get(), None);
    std::fs::rename(&moved, &snapshot).unwrap();

    let rolled = h.ok(&["rollback"]);
    assert_eq!(
        (rolled["from"].as_str(), rolled["to"].as_str()),
        (Some("g2"), Some("g1")),
        "{rolled}"
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    // g1's first start and its start after the rollback: v2's row is gone and the rows before it are back.
    assert_eq!(h.versions(), ["v1", "v1"]);
    let status = h.status();
    assert_eq!(
        (status["permanent"].as_str(), status["pending"].clone()),
        (Some("g1"), Value::Null)
    );
    assert_eq!(status["process"]["state"], "running");
    // The first rollback's reason stays; the retries only finished it.
    assert_eq!(h.generation("g2")["rollback_reason"], "rollback requested");
}

/// Every event carries a `seq`, and `subscribe {since}` replays exactly the events after it, in order, before the
/// `status` and `history` lines.
#[test]
fn subscribe_since_replays_the_events_after_a_sequence_number() {
    use std::io::{BufRead, Write};
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);

    let events = h.ok(&["log", "-n", "1000"])["events"].clone();
    let seqs: Vec<u64> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.len() >= 4, "{events}");
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    let middle = seqs[seqs.len() / 2];

    let mut stream = std::os::unix::net::UnixStream::connect(h.dir.join("s/fawkes.sock")).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    writeln!(
        stream,
        r#"{{"command": "subscribe", "args": {{"since": {middle}}}}}"#
    )
    .unwrap();
    let mut lines = std::io::BufReader::new(&stream).lines();
    let mut replayed = Vec::new();
    let first_other = loop {
        let line: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        match line.get("event") {
            Some(event) => replayed.push(event["seq"].as_u64().unwrap()),
            None => break line,
        }
    };
    let later: Vec<u64> = seqs.iter().copied().filter(|&n| n > middle).collect();
    assert_eq!(replayed, later);
    assert!(first_other.get("status").is_some(), "{first_other}");
    let next: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert!(next.get("history").is_some(), "{next}");
}

/// SIGTERM reaches the daemon while a switch is stopping the old app: the target never ran, so the switch is given up
/// (the target stays tested and the answer says why), and the next daemon start runs the permanent generation.
#[test]
fn a_daemon_stopped_during_a_switch_leaves_the_target_tested() {
    let mut h = Harness::new("");
    h.commit("v1", "slow-stop");
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

    let daemon_pid = i32::try_from(h.status()["daemon_pid"].as_i64().unwrap()).unwrap();
    let (code, answer) = std::thread::scope(|scope| {
        let switch = scope.spawn(|| h.fk(&["switch", "g2"]));
        // g1 takes 1.5 s to leave after SIGTERM: the window for the daemon's own SIGTERM.
        h.wait("the switch to stop g1", 10, || {
            messages(&h, "progress").contains(&"stopping g1".to_string())
        });
        // SAFETY: the pid is the daemon this test started.
        unsafe { libc::kill(daemon_pid, libc::SIGTERM) };
        switch.join().unwrap()
    });
    assert_ne!(code, 0, "{answer}");
    if let Some(message) = answer["message"].as_str() {
        assert!(message.contains("shutting down"), "{answer}");
        assert!(message.contains("g2 stays tested"), "{answer}");
    }
    h.wait("the daemon to end", 15, || h.fk(&["status"]).0 == 3);
    h.kill_daemon();

    h.start_daemon();
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    let status = h.status();
    assert_eq!(
        (status["permanent"].as_str(), status["pending"].clone()),
        (Some("g1"), Value::Null),
        "{status}"
    );
    assert_eq!(status["process"]["state"], "running", "{status}");
    assert_eq!(h.generation("g2")["state"], "tested");
    assert!(!h.versions().contains(&"v2".to_string()));
}

/// A crash backoff shows as `restart_at` while `state` stays `stopped`, and `stop` during it cancels the restart and
/// answers ok: the app stays stopped once the backoff has passed.
#[test]
fn stop_during_a_crash_backoff_cancels_the_restart_and_answers_ok() {
    use std::io::Write;
    let mut h = Harness::new("max_failed_starts = 5");
    // Two failed starts in a row: the second restart waits 2 s, the window `stop` lands in.
    h.edit_manifest("min_uptime = 1", "min_uptime = 60");
    h.commit("v1", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    assert_eq!(h.status()["process"]["restart_at"], Value::Null);

    let crash = |h: &Harness| {
        h.wait("the app to listen", 10, || h.get().is_some());
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", h.port)).unwrap();
        stream
            .write_all(b"GET /crash HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        h.wait("the crash to be reaped", 5, || {
            h.status()["process"]["state"] == "stopped"
        });
    };
    crash(&h);
    h.wait("the first crash restart", 10, || {
        h.status()["process"]["state"] == "running"
    });
    crash(&h);
    let process = h.status()["process"].clone();
    assert_eq!(process["state"], "stopped", "{process}");
    let restart_at = process["restart_at"].as_str().expect("a scheduled restart");
    // `2026-11-03T14:00:01Z`
    let shape: Vec<bool> = restart_at.chars().map(|c| c.is_ascii_digit()).collect();
    assert!(
        restart_at.len() == 20 && restart_at.ends_with('Z') && shape[..4].iter().all(|d| *d),
        "{restart_at}"
    );

    let stopped = h.ok(&["stop"]);
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    assert_eq!(stopped["restart_at"], Value::Null, "{stopped}");
    assert!(
        messages(&h, "stopped")
            .last()
            .is_some_and(|message| message.contains("scheduled restart")),
    );
    // Past the 2 s backoff: nothing restarted the app.
    std::thread::sleep(std::time::Duration::from_millis(3000));
    let process = h.status()["process"].clone();
    assert_eq!(process["state"], "stopped", "{process}");
    assert_eq!(process["restart_at"], Value::Null, "{process}");
    // Stopped and not scheduled: still refused.
    let (code, error) = h.fk(&["stop"]);
    assert_eq!((code, error["code"].as_str()), (1, Some("invalid_state")));
}

/// A restore that can never succeed (its snapshot is gone) blocks the app for good; `rollback --keep-storage` is the
/// operator's way out, and only for that case. It ends the step, records that the restore was given up and starts the
/// permanent generation on the storage as it is.
#[test]
fn rollback_keep_storage_gives_up_a_restore_that_failed() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.commit("v2", "never");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    let refused = |args: &[&str], code: &str, says: &str| {
        let (exit, error) = h.fk(args);
        assert_eq!(
            (exit, error["code"].as_str()),
            (1, Some(code)),
            "{args:?}: {error}"
        );
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(says), "{args:?}: {message}");
    };

    // Nothing has failed: neither with a switch pending nor without one.
    refused(
        &["rollback", "--keep-storage"],
        "invalid_state",
        "only gives up a restore that failed",
    );
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));
    refused(
        &["rollback", "--keep-storage"],
        "invalid_state",
        "only gives up a restore that failed",
    );
    assert_eq!(h.status()["pending"]["generation"], "g2");

    std::fs::remove_dir_all(h.dir.join("s/generations/g2/snapshot")).unwrap();
    refused(&["rollback"], "storage_failed", "could not be restored");
    assert_eq!(h.status()["process"]["state"], "failed");
    assert_eq!(h.get(), None);

    let rolled = h.ok(&["rollback", "--keep-storage"]);
    assert_eq!(
        (
            rolled["from"].as_str(),
            rolled["to"].as_str(),
            rolled["reason"].as_str()
        ),
        (Some("g2"), Some("g1"), Some("rollback requested")),
        "{rolled}"
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    let status = h.status();
    assert_eq!(status["pending"], Value::Null, "{status}");
    assert_eq!(status["permanent"], "g1", "{status}");
    assert_eq!(status["process"]["state"], "running", "{status}");
    let g2 = h.generation("g2");
    assert_eq!(g2["state"], "rolled_back", "{g2}");
    assert!(g2["restore_abandoned_at"].is_string(), "{g2}");
    assert_eq!(h.generation("g1")["restore_abandoned_at"], Value::Null);
    assert!(
        messages(&h, "rolled_back")
            .last()
            .is_some_and(|message| message.contains("may already have migrated")),
    );
    // Nothing is pending any more.
    refused(&["rollback"], "invalid_state", "nothing to roll back");
}

/// The messages of the events of `kind`, oldest first, that start with `prefix`.
fn messages_starting(h: &Harness, kind: &str, prefix: &str) -> Vec<String> {
    messages(h, kind)
        .into_iter()
        .filter(|message| message.starts_with(prefix))
        .collect()
}

/// Make a generation `id` of `version` permanent, in a daemon that has none yet.
fn make_permanent(h: &Harness, version: &str, id: &str) {
    h.ok(&["stage", version]);
    h.ok(&["test", id]);
    h.ok(&["switch", id]);
    h.wait("the generation to confirm itself", 15, || {
        h.status()["permanent"] == id
    });
}

/// SIGHUP applies what may change while the daemon runs and the next start uses it. A manifest that changes a fixed
/// field is refused whole, the hot field edited along with it included.
#[test]
fn sighup_reloads_hot_fields_and_refuses_a_manifest_that_changes_a_fixed_one() {
    let mut h = Harness::new("");
    // The app's start command records what it was given, so the next start shows which manifest it ran under.
    h.edit_manifest(
        r#"start = "node app.js""#,
        r#"start = "echo $MARK > $APP_HOME/mark; exec node app.js""#,
    );
    h.edit_manifest(
        r#"APP_HOME = "{home}""#,
        "APP_HOME = \"{home}\"\nMARK = \"one\"",
    );
    h.commit("v1", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    let mark = || std::fs::read_to_string(h.dir.join("home/mark")).unwrap();
    assert_eq!(mark().trim(), "one");

    h.edit_manifest(r#"MARK = "one""#, r#"MARK = "two""#);
    h.edit_manifest("interval = 0.1", "interval = 0.2");
    h.signal_daemon(libc::SIGHUP);
    h.wait("the reload to be logged", 10, || {
        !messages_starting(&h, "daemon", "manifest reloaded").is_empty()
    });
    assert_eq!(
        messages_starting(&h, "daemon", "manifest reloaded"),
        ["manifest reloaded, changed: health, env"]
    );
    assert_eq!(
        mark().trim(),
        "one",
        "the running app is not touched by a reload"
    );
    h.ok(&["restart"]);
    // The start command writes the mark before the app listens.
    h.wait("v1 to answer after the restart", 10, || {
        h.get().as_deref() == Some("v1")
    });
    assert_eq!(mark().trim(), "two", "the next start runs with the new env");

    // A refused field refuses the whole manifest: `MARK` stays what it was.
    h.edit_manifest(r#"MARK = "two""#, r#"MARK = "three""#);
    h.edit_manifest(
        r#"storage = ["app.sqlite"]"#,
        r#"storage = ["app.sqlite", "other.sqlite"]"#,
    );
    h.signal_daemon(libc::SIGHUP);
    h.wait("the refusal to be logged", 10, || {
        !messages_starting(&h, "error", "manifest not reloaded").is_empty()
    });
    let refused = messages_starting(&h, "error", "manifest not reloaded");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains("storage can only change with a daemon restart"),
        "{refused:?}"
    );
    h.ok(&["restart"]);
    h.wait("v1 to answer after the restart", 10, || {
        h.get().as_deref() == Some("v1")
    });
    assert_eq!(
        mark().trim(),
        "two",
        "the old manifest stays whole after a refused reload"
    );
    assert_eq!(
        messages_starting(&h, "daemon", "manifest reloaded").len(),
        1
    );
}

/// A reload cannot move `port` in the window of a switch between stopping the old app and starting the new one, when
/// no app is in `live`: the switch announced the old port and the new generation serves on it.
#[test]
fn a_reload_cannot_change_the_port_while_a_switch_has_stopped_the_old_app() {
    let mut h = Harness::new("");
    h.commit("v1", "slow-stop");
    h.commit("v2", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    // Edited now, applied only by the SIGHUP, so nothing is left to do inside g1's 1.5 s stop.
    let port = h.port.to_string();
    h.edit_manifest(
        &format!("spare_ports = [{}, {}]", h.port + 1, h.port + 9),
        &format!("spare_ports = [{}, {}]", h.port + 1, h.port + 8),
    );
    h.edit_manifest(
        &format!("port = {port}\n"),
        &format!("port = {}\n", h.port + 9),
    );
    let mut follow = h.follow(&[]);
    std::thread::scope(|scope| {
        let switch = scope.spawn(|| h.fk(&["switch", "g2"]));
        follow.until("the switch to stop g1", 15, |e| {
            e["message"] == "stopping g1"
        });
        h.signal_daemon(libc::SIGHUP);
        let refused = follow.until("the reload to be refused", 15, |e| e["kind"] == "error");
        let message = refused["message"].as_str().unwrap();
        assert!(
            message.starts_with("manifest not reloaded")
                && message.contains("port cannot change while switch g2 is in progress"),
            "{message}"
        );
        assert_eq!(switch.join().unwrap().0, 0);
    });

    let started = format!("pid {} on port {port}", h.status()["process"]["pid"]);
    h.wait("v2 to serve on the old port", 15, || {
        h.get().as_deref() == Some("v2")
    });
    assert_eq!(h.status()["process"]["port"], h.port);
    assert!(messages(&h, "started").contains(&started));
    assert_eq!(
        messages_starting(&h, "daemon", "manifest reloaded"),
        Vec::<String>::new()
    );
}

/// With `rollback_on_unhealthy`, a confirm sent the moment `unhealthy` appears is refused as one for a generation
/// being rolled back, and g2 ends rolled back. That the decision comes before the event is pinned by the unit test
/// in `daemon/supervisor.rs`; a client reading the event is too slow to see the order.
#[test]
fn a_confirm_sent_at_the_unhealthy_event_finds_the_rollback_decided() {
    let mut h = Harness::new("rollback_on_unhealthy = true");
    h.edit_manifest("timeout = 20", "timeout = 3");
    h.edit_manifest("stop_timeout = 5", "stop_timeout = 3");
    // g2 leaves a process that ignores SIGTERM in its group, so its rollback takes the whole stop timeout and a
    // confirm has time to arrive while the rollback is under way.
    h.edit_manifest(
        r#"start = "node app.js""#,
        r#"start = "if [ $FAWKES_GENERATION = g2 ] && [ $FAWKES_PHASE = live ]; then (trap '' TERM; sleep 4) & fi; exec node app.js""#,
    );
    h.commit("v1", "confirm");
    h.commit("v2", "unhealthy-live");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    let mut follow = h.follow(&[]);
    // Connected beforehand: the daemon accepts only every 20 ms, and the confirm must go out with the event.
    let mut raw = h.connect();
    h.ok(&["switch", "g2", "--deadline", "120"]);
    follow.until("g2's unhealthy event", 20, |e| {
        e["kind"] == "unhealthy" && e["generation"] == "g2"
    });
    writeln!(
        raw,
        r#"{{"command": "confirm", "args": {{"generation": "g2"}}}}"#
    )
    .unwrap();
    let mut answer = String::new();
    BufReader::new(&raw).read_line(&mut answer).unwrap();
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"]["code"], "invalid_state", "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("is being rolled back (health check failed)"),
        "{message}"
    );

    h.wait("g2 to be rolled back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert_eq!(h.generation("g2")["rollback_reason"], "health check failed");
    assert_eq!(h.status()["permanent"], "g1");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
}

/// `log --follow` replays the newest 20 events (or `-n`), then prints each new one once as it happens, one JSON event
/// per line.
#[test]
fn log_follow_replays_the_newest_events_then_streams_new_ones_once() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    // More than the 20 a follow replays by default.
    while h.ok(&["log", "-n", "1000"])["events"]
        .as_array()
        .unwrap()
        .len()
        < 30
    {
        h.ok(&["stop"]);
        h.ok(&["start"]);
    }
    h.wait("the health check to pass", 10, || {
        h.ok(&["log", "-n", "1"])["events"][0]["kind"] == "healthy"
    });
    // Quiet: nothing is logged between the picture of the log and the follow.
    let log = || h.ok(&["log", "-n", "1000"])["events"].clone();
    let snapshot = log();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(log() == snapshot, "the daemon was not quiet");
    let all = snapshot.as_array().unwrap();

    let mut follow = h.follow(&[]);
    let replay: Vec<Value> = (0..20)
        .map(|_| {
            follow
                .next(Duration::from_secs(10))
                .expect("a replayed event")
        })
        .collect();
    assert_eq!(replay, all[all.len() - 20..]);
    assert!(
        follow.line(Duration::from_millis(500)).is_none(),
        "the replay is the newest 20 and nothing else"
    );

    // New events as they happen: exactly the ones after the replay, in order, none twice.
    h.ok(&["stop"]);
    h.ok(&["start"]);
    let mut followed = Vec::new();
    while followed
        .last()
        .is_none_or(|e: &Value| e["kind"] != "healthy")
    {
        followed.push(
            follow
                .next(Duration::from_secs(15))
                .expect("the restart and its health check"),
        );
    }
    assert!(
        follow.line(Duration::from_millis(500)).is_none(),
        "nothing arrives twice"
    );
    // Every event the daemon logged after the replay, none left out and in order.
    let newer: Vec<Value> = log()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["seq"].as_u64() > replay[19]["seq"].as_u64())
        .cloned()
        .collect();
    assert_eq!(followed, newer);
    let seqs: Vec<u64> = newer.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    assert!(followed.iter().any(|e| e["kind"] == "started"));

    // `-n` bounds the replay.
    let mut few = h.follow(&["-n", "3"]);
    let last_three: Vec<Value> = (0..3)
        .map(|_| few.next(Duration::from_secs(10)).expect("a replayed event"))
        .collect();
    assert_eq!(last_three, newer_tail(&log(), 3));
    assert!(few.line(Duration::from_millis(500)).is_none());
}

/// The last `n` events of a log.
fn newer_tail(log: &Value, n: usize) -> Vec<Value> {
    let all = log.as_array().unwrap();
    all[all.len() - n..].to_vec()
}
