//! The happy path and the safety net, against the real binary and its socket:
//! stage → test → switch → confirm, the lifecycle commands, then a switch that is never confirmed and rolls back.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic fails the test, which is what a helper that cannot proceed should do"
)]

mod common;
use common::Harness;
use rustix::process::{
    Pid, Signal, kill_process, kill_process_group, test_kill_process, test_kill_process_group,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

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
    std::thread::sleep(Duration::from_millis(1500));
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
    assert_eq!(h.status()["step"], "pending");

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
    make_permanent(&h, "v1", "g1");

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
    h.edit_manifest(
        r#"start = "node app.js""#,
        r#"start = 'if [ "$(cat VERSION)" = v2 ] && [ "$FAWKES_PHASE" = live ]; then cp {state}/state.json {home}/at-start.json; kill -9 $PPID; fi; exec node app.js'"#,
    );
    h.commit("v1", "confirm");
    h.commit("v2", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
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
        let pid = read_pid(format!("{dir}/background.pid"));
        test_kill_process(pid).is_ok()
    };

    make_permanent(&h, "v1", "g1");
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
    // Waited for, not read once: the killed process stays a zombie until init reaps it, which a loaded machine delays.
    h.wait("g2's background process to be gone", 5, || !alive("g2"));
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
    make_permanent(&h, "v1", "g1");
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

/// An app that confirms itself without naming its generation and without looking at `FAWKES_PHASE` runs `confirm`
/// during its own test too, while another generation is pending: the CLI sends the caller's `FAWKES_GENERATION`, so
/// the confirm is refused and the pending generation still has to vouch for itself.
#[test]
fn a_bare_confirm_from_a_test_run_cannot_confirm_another_pending_generation() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.commit("v2", "never");
    h.commit("v3", "confirm-always");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    h.ok(&["switch", "g2", "--deadline", "120"]);

    h.ok(&["stage", "v3"]);
    h.ok(&["test", "g3"]);
    let answer = std::fs::read_to_string(h.dir.join("s/generations/g3/code/confirm.json")).unwrap();
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["code"], "invalid_state", "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap()
            .contains("g2 is the one waiting for confirm"),
        "{answer}"
    );
    let status = h.status();
    assert_eq!(
        (
            status["permanent"].as_str(),
            status["pending"]["generation"].as_str()
        ),
        (Some("g1"), Some("g2")),
        "{status}"
    );
}

/// With `rollback_on_unhealthy`, a pending generation whose health check gives up rolls back at once, long before
/// its confirm deadline.
#[test]
fn rollback_on_unhealthy_rolls_back_without_waiting_for_the_deadline() {
    let mut h = Harness::new("rollback_on_unhealthy = true");
    h.commit("v1", "confirm");
    h.commit("v2", "unhealthy-live");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    shorten_the_health_timeout(&h);
    let switched = std::time::Instant::now();
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("g2's failed health check to roll it back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    // The 3 s health timeout, then the stop and the restore.
    assert!(switched.elapsed().as_secs() < 10);
    assert_eq!(h.generation("g2")["rollback_reason"], "health check failed");
    assert_eq!(h.status()["permanent"], "g1");
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
}

/// The worktrees git knows of in the harness's repository, the repository itself included.
fn worktrees(h: &Harness) -> usize {
    let output = common::git()
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
    let pid = read_pid(h.dir.join("s/install.pid"));
    assert!(
        test_kill_process_group(pid).is_err(),
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
        make_permanent(&h, version, id);
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
    make_permanent(&h, "v1", "g1");
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
    let mut h = Harness::new("");
    h.start_daemon();
    let mut stream = h.connect();
    stream.write_all(br#"{"command": "log", "#).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    stream.write_all(b"\"args\": {\"limit\": 1000}}\n").unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(answer["ok"], true, "{answer}");
}

/// The daemon's answer to a raw request that `send` writes from a thread of its own, since the daemon may answer, and
/// close, before all of it is written.
fn answer_to(h: &Harness, send: impl FnOnce(&mut UnixStream) + Send + 'static) -> Value {
    let stream = h.connect();
    let mut writer = stream.try_clone().unwrap();
    let sender = std::thread::spawn(move || send(&mut writer));
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    sender.join().unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("no answer ({e}): {line:?}"))
}

/// The request bounds of the socket protocol: 64 KiB, and one 5 s deadline for the whole line however it trickles.
#[test]
fn a_request_too_long_or_too_slow_is_refused_with_usage() {
    let mut h = Harness::new("");
    h.start_daemon();

    // 64 KiB and one byte, the newline included.
    let padding = "x".repeat(64 * 1024 - r#"{"command": "status", "args": {"pad": ""}}"#.len());
    let request = format!("{{\"command\": \"status\", \"args\": {{\"pad\": \"{padding}\"}}}}\n");
    assert_eq!(request.len(), 64 * 1024 + 1);
    let answer = answer_to(&h, move |stream| {
        // Refused once the daemon has stopped reading; the answer says why.
        let _ = stream.write_all(request.as_bytes());
    });
    assert_eq!(answer["error"]["code"], "usage", "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("longer than"),
        "{answer}"
    );
    // One byte less is a request like any other.
    let padding = &padding[1..];
    let request = format!(r#"{{"command": "status", "args": {{"pad": "{padding}"}}}}"#);
    let answer = answer_to(&h, move |stream| writeln!(stream, "{request}").unwrap());
    assert_eq!(answer["ok"], true, "{answer}");

    // A byte every 500 ms keeps every read well within the deadline, but not the request.
    let started = Instant::now();
    let answer = answer_to(&h, |stream| {
        for byte in br#"{"command": "status"}"# {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    let took = started.elapsed();
    assert_eq!(answer["error"]["code"], "usage", "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("within 5 s"),
        "{answer}"
    );
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(8),
        "answered after {took:?}"
    );
}

fn mode(path: impl AsRef<std::path::Path>) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// What other users must not reach: the socket, the state directory (also when it existed with a looser mode), the
/// `run/` links and a home the daemon creates.
#[test]
fn the_socket_state_dir_run_dir_and_a_new_home_are_private() {
    let mut h = Harness::new("");
    std::fs::create_dir(h.dir.join("s")).unwrap();
    std::fs::set_permissions(h.dir.join("s"), std::fs::Permissions::from_mode(0o755)).unwrap();
    h.start_daemon();
    assert_eq!(mode(h.dir.join("s/fawkes.sock")), 0o600);
    assert_eq!(mode(h.dir.join("s")), 0o700);
    // The harness points FAWKES_HOME at its directory.
    assert_eq!(mode(h.dir.join("run")), 0o700);
    assert_eq!(mode(h.dir.join("home")), 0o700);
}

/// A state directory that is a symlink is refused before anything is done to it: its owner, or whoever owns the
/// directory the link is in, could point it at a directory of their own and run code as the daemon's user.
#[test]
fn a_symlinked_state_dir_is_refused() {
    let h = Harness::new("");
    let real = h.dir.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&real, h.dir.join("s")).unwrap();
    let (code, stderr) = h.run_second_daemon();
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("is a symbolic link"), "{stderr}");
    assert_eq!(mode(&real), 0o755);
    assert!(
        std::fs::read_dir(&real).unwrap().next().is_none(),
        "the daemon wrote into the link's target"
    );
}

/// A daemon whose stderr reader has gone (`fawkes daemon 2>&1 | tee log`, then `tee` dies) supervises on: stderr is
/// only a copy of the event log.
#[test]
fn a_daemon_whose_stderr_is_gone_keeps_supervising() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    h.start_daemon_with_stderr(writer);
    make_permanent(&h, "v1", "g1");
    h.signal_daemon(Signal::HUP);
    h.wait("the reload to be logged", 10, || {
        !messages_starting(&h, "daemon", "manifest reloaded").is_empty()
    });
    assert_eq!(h.status()["process"]["state"], "running");
}

/// A client whose reader has gone (`fawkes status | head -0`) still exits by the contract: the daemon's answer decides
/// the exit code, and a followed log simply ends.
#[test]
fn a_client_whose_stdout_is_gone_exits_by_its_answer() {
    let mut h = Harness::new("");
    h.start_daemon();
    // Nothing is pending, so the confirm is refused.
    for (args, code) in [
        (&["status"][..], 0),
        (&["log", "--follow"], 0),
        (&["confirm"], 1),
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let status = h.cli().args(args).stdout(writer).status().unwrap();
        assert_eq!(status.code(), Some(code), "{args:?}: {status}");
    }
}

/// A daemon that dies mid-stage leaves its install running; the next daemon stops it before it removes the cut-short
/// generation, so nothing of the stage outlives the daemon that started it. Tests are recorded the same way.
#[test]
fn a_daemon_killed_during_a_stage_leaves_no_install_running() {
    let mut h = Harness::new(r"install = '''echo $$ > {state}/install.pid; exec sleep 600'''");
    h.commit("v1", "confirm");
    h.start_daemon();
    let mut stage = h
        .cli()
        .args(["stage", "v1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid_file = h.dir.join("s/install.pid");
    h.wait("the install to run", 10, || {
        std::fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n'))
    });
    let pid = read_pid(&pid_file);
    h.kill_daemon();
    stage.wait().unwrap();

    h.start_daemon();
    let alive = test_kill_process_group(pid).is_ok();
    if alive {
        // The group is the orphaned install this test started.
        let _ = kill_process_group(pid, Signal::KILL);
    }
    assert!(!alive, "the install outlived the daemon that started it");
    assert_eq!(h.ok(&["history"])["generations"], serde_json::json!([]));
}

/// A switch that fails while the permanent generation waits out a crash backoff keeps that restart: the live app
/// comes back as it would have without the switch. Checked through `restart_at`, which the status shows while the
/// backoff lasts, before and after the switch.
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
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    // A storage file that is not a database: the next switch cannot snapshot it.
    std::fs::write(h.dir.join("home/other.sqlite"), "not a database").unwrap();

    // Three crashes within min_uptime: the third restart waits 4 s (after 1 s and 2 s), the window the switch lands in.
    for _ in 0..2 {
        h.crash_app();
        h.wait("the crash restart", 10, || {
            h.status()["process"]["state"] == "running"
        });
    }
    h.crash_app();
    let backoff = h.status()["process"].clone();
    assert!(
        backoff["restart_at"].is_string(),
        "no backoff to switch in: {backoff}"
    );
    let (code, error) = h.fk(&["switch", "g2"]);
    assert_eq!(
        (code, error["code"].as_str()),
        (1, Some("storage_failed")),
        "{error}"
    );
    let backoff = h.status()["process"].clone();
    assert!(
        backoff["restart_at"].is_string(),
        "the refused switch dropped the backoff: {backoff}"
    );
    h.wait("g1 to be restarted after its backoff", 10, || {
        h.status()["process"]["state"] == "running"
    });
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.generation("g2")["state"], "tested");
}

/// A test works on a copy of the storage, which does not outlive the test: not when the test fails before the app
/// starts either (here the copy itself fails).
#[test]
fn a_test_that_fails_before_its_app_starts_leaves_no_copy_of_the_storage() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    std::fs::create_dir_all(h.dir.join("home")).unwrap();
    std::fs::write(h.dir.join("home/app.sqlite"), "not a database").unwrap();

    refused(&h, &["test", "g1"], "storage_failed", "app.sqlite");
    assert!(!h.dir.join("s/generations/g1/test-home").exists());
    assert_eq!(h.generation("g1")["state"], "staged");
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

/// An idle daemon sleeps until something happens, then acts at once: a crash of the app is reaped and restarted when
/// its backoff ends, SIGHUP reloads, SIGTERM stops the app and the daemon. A lost wake-up would wait for the next
/// timer, ten seconds away or none at all.
#[test]
fn an_idle_daemon_wakes_at_once_for_a_crash_a_reload_and_a_shutdown() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    let mut follow = h.follow(&["-n", "0"]);
    let settle = || std::thread::sleep(Duration::from_millis(500));
    settle();

    let killed = Instant::now();
    kill_process_group(read_pid(h.dir.join("s/app.pid")), Signal::KILL).unwrap();
    let exited = follow.until("the crash", 5, |e| e["kind"] == "exited");
    let noticed = killed.elapsed();
    assert!(
        noticed < Duration::from_secs(4),
        "the crash was noticed after {noticed:?}"
    );
    assert!(
        exited["message"]
            .as_str()
            .unwrap()
            .ends_with("restarting in 1s"),
        "{exited}"
    );
    follow.until("the restart", 5, |e| e["kind"] == "started");
    let restarted = killed.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(5)).contains(&restarted),
        "the 1 s backoff ended after {restarted:?}"
    );
    let pid = h.status()["process"]["pid"].as_i64().unwrap();
    let app = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    settle();

    let signalled = Instant::now();
    h.signal_daemon(Signal::HUP);
    follow.until("the reload", 5, |e| {
        e["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("manifest reloaded"))
    });
    let reloaded = signalled.elapsed();
    assert!(
        reloaded < Duration::from_secs(4),
        "SIGHUP was acted on after {reloaded:?}"
    );
    settle();

    let signalled = Instant::now();
    h.signal_daemon(Signal::TERM);
    assert!(
        h.daemon_exits_within(Duration::from_secs(5)),
        "the daemon ignored SIGTERM; daemon log:\n{}",
        h.daemon_log()
    );
    let stopped = signalled.elapsed();
    assert!(stopped < Duration::from_secs(4), "SIGTERM took {stopped:?}");
    assert!(
        test_kill_process_group(app).is_err(),
        "the app outlived the daemon"
    );
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
    make_permanent(&h, "v1", "g1");
    // The first crash comes after a run longer than min_uptime, so it is not a failed start.
    h.wait("g1 to outlive min_uptime", 10, || {
        h.status()["process"]["uptime"]
            .as_f64()
            .is_some_and(|uptime| uptime > 1.5)
    });

    let marker = h.dir.join("home/crash");
    std::fs::write(&marker, "").unwrap();
    h.crash_app();
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
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    let blocker = h.dir.join("s/state.json.tmp");

    // The switch: its first intent cannot be written, so g1 is never stopped and g2 is still only tested.
    std::fs::create_dir(&blocker).unwrap();
    refused(
        &h,
        &["switch", "g2"],
        "internal",
        "g2 stays tested and nothing changed",
    );
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.status()["pending"], Value::Null);
    assert_eq!(h.generation("g2")["state"], "tested");
    // A stage whose record cannot be written fails before its worktree exists.
    refused(&h, &["stage", "v1"], "internal", "nothing was staged");
    assert!(!h.dir.join("s/generations/g3").exists());
    std::fs::remove_dir(&blocker).unwrap();

    // The confirm: g2 stays pending and keeps running.
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));
    std::fs::create_dir(&blocker).unwrap();
    refused(
        &h,
        &["confirm", "g2"],
        "internal",
        "g2 is still pending, confirm again",
    );
    assert_eq!(h.status()["pending"]["generation"], "g2");
    assert_eq!(h.generation("g2")["state"], "current");
    assert_eq!(h.get().as_deref(), Some("v2"));

    // The end of the rollback: the storage is restored, but nothing starts while the switch is still on disk.
    refused(
        &h,
        &["rollback"],
        "internal",
        "the app stays stopped and `fawkes rollback` retries",
    );
    let status = h.status();
    assert_eq!(status["process"]["state"], "failed", "{status}");
    assert_eq!(status["pending"]["generation"], "g2", "{status}");
    assert_eq!(status["step"], "restore_failed", "{status}");
    assert_eq!(h.generation("g2")["state"], "rolled_back");
    assert_eq!(h.get(), None);
    std::fs::remove_dir(&blocker).unwrap();

    // A restore that fails: the snapshot is out of reach. The switch stays pending and blocks everything but
    // `rollback`.
    let snapshot = h.dir.join("s/generations/g2/snapshot");
    let moved = h.dir.join("s/snapshot-moved");
    std::fs::rename(&snapshot, &moved).unwrap();
    // Both messages name the way out, and the permanent generation it starts.
    let way_out = "`fawkes rollback` retries the restore, or `fawkes rollback --keep-storage` starts g1 on the storage as it is (it may already be migrated)";
    refused(&h, &["rollback"], "storage_failed", way_out);
    let status = h.status();
    assert_eq!(status["process"]["state"], "failed", "{status}");
    assert_eq!(status["pending"]["generation"], "g2", "{status}");
    for args in [&["start"][..], &["confirm"], &["switch", "g1"]] {
        refused(&h, args, "invalid_state", way_out);
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

    let mut stream = h.connect();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(
        stream,
        r#"{{"command": "subscribe", "args": {{"since": {middle}}}}}"#
    )
    .unwrap();
    let mut lines = BufReader::new(&stream).lines();
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
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    let daemon_pid =
        Pid::from_raw(i32::try_from(h.status()["daemon_pid"].as_i64().unwrap()).unwrap()).unwrap();
    // The progress event is read as it comes (a late follow gets it replayed), not by polling the log with a process
    // per look: g1 leaves within 1.5 s.
    let mut follow = h.follow(&[]);
    let (code, answer) = std::thread::scope(|scope| {
        let switch = scope.spawn(|| h.fk(&["switch", "g2"]));
        // g1 takes 1.5 s to leave after SIGTERM: the window for the daemon's own SIGTERM.
        follow.until("the switch to stop g1", 10, |e| {
            e["kind"] == "progress" && e["message"] == "stopping g1"
        });
        // The pid is the daemon this test started.
        kill_process(daemon_pid, Signal::TERM).unwrap();
        switch.join().unwrap()
    });
    assert_ne!(code, 0, "{answer}");
    if let Some(message) = answer["message"].as_str() {
        assert_eq!(answer["code"], "busy", "{answer}");
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

/// SIGTERM reaches the daemon while a requested rollback stops the pending generation: the rollback finishes, but a
/// daemon on its way out starts nothing. It says so, without a `failed` event for the healthy permanent generation
/// and without an ok answer that claims it runs; the next daemon start runs it.
#[test]
fn a_daemon_stopped_during_a_rollback_leaves_the_permanent_generation_to_its_next_start() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.commit("v2", "slow-stop-never");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));

    let mut follow = h.follow(&[]);
    let (code, answer) = std::thread::scope(|scope| {
        let rollback = scope.spawn(|| h.fk(&["rollback"]));
        // g2 takes 1.5 s to leave after SIGTERM: the window for the daemon's own SIGTERM.
        follow.until("the rollback to stop g2", 10, |e| {
            e["kind"] == "progress"
                && e["message"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("rolling back g2"))
        });
        h.signal_daemon(Signal::TERM);
        rollback.join().unwrap()
    });
    assert_eq!(
        (code, answer["code"].as_str()),
        (1, Some("busy")),
        "{answer}"
    );
    assert!(
        answer["message"]
            .as_str()
            .unwrap()
            .contains("shutting down"),
        "{answer}"
    );
    h.wait("the daemon to end", 15, || h.fk(&["status"]).0 == 3);
    h.kill_daemon();

    h.start_daemon();
    h.wait("v1 to serve again", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(messages(&h, "failed"), Vec::<String>::new());
    assert_eq!(
        messages_starting(&h, "daemon", "g1 not started").len(),
        1,
        "{:?}",
        messages(&h, "daemon")
    );
    assert_eq!(h.generation("g2")["state"], "rolled_back");
    assert_eq!(h.status()["pending"], Value::Null);
}

/// A daemon that dies for good (SIGKILL, no cleanup) in the middle of a switch leaves `switching` in state.json. Its
/// successor must say so, leave the target `tested` (it never ran) and serve the permanent generation again.
#[test]
fn a_daemon_killed_during_a_switch_recovers_to_the_permanent_generation() {
    let mut h = Harness::new("");
    h.commit("v1", "slow-stop");
    h.commit("v2", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    let mut follow = h.follow(&[]);
    let (code, answer) = std::thread::scope(|scope| {
        let switch = scope.spawn(|| h.fk(&["switch", "g2"]));
        // g1 takes 1.5 s to leave after SIGTERM: the window for the kill.
        follow.until("the switch to stop g1", 10, |e| {
            e["kind"] == "progress" && e["message"] == "stopping g1"
        });
        h.signal_daemon(Signal::KILL);
        switch.join().unwrap()
    });
    assert_ne!(code, 0, "{answer}");
    h.kill_daemon();
    let state: Value =
        serde_json::from_slice(&std::fs::read(h.dir.join("s/state.json")).unwrap()).unwrap();
    assert_eq!(state["txn"]["step"], "switching", "{state}");
    assert_eq!(state["txn"]["generation"], "g2", "{state}");

    h.start_daemon();
    h.wait("v1 to serve again", 15, || h.get().as_deref() == Some("v1"));
    let cut_short = messages(&h, "daemon")
        .into_iter()
        .filter(|message| message.contains("cut short"))
        .collect::<Vec<_>>();
    assert_eq!(cut_short.len(), 1, "{cut_short:?}");
    assert!(cut_short[0].contains("g2"), "{cut_short:?}");
    let status = h.status();
    assert_eq!(
        (status["permanent"].as_str(), status["pending"].clone()),
        (Some("g1"), Value::Null),
        "{status}"
    );
    assert_eq!(status["process"]["state"], "running", "{status}");
    assert_eq!(status["process"]["generation"], "g1", "{status}");
    assert_eq!(h.generation("g2")["state"], "tested");
    assert!(!h.versions().contains(&"v2".to_string()));
}

/// A daemon that dies after a restore failed leaves `rolling_back` in state.json with the app stopped. Its successor
/// retries the restore, finishes the rollback with the reason it already had and serves the permanent generation.
#[test]
fn a_daemon_restart_retries_a_failed_restore_and_finishes_the_rollback() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.commit("v2", "never");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));
    assert_eq!(h.versions(), ["v1", "v2"]);

    let snapshot = h.dir.join("s/generations/g2/snapshot");
    let moved = h.dir.join("s/snapshot-moved");
    std::fs::rename(&snapshot, &moved).unwrap();
    refused(&h, &["rollback"], "storage_failed", "could not be restored");
    let status = h.status();
    assert_eq!(status["process"]["state"], "failed", "{status}");
    assert_eq!(status["pending"]["generation"], "g2", "{status}");

    h.kill_daemon();
    let state: Value =
        serde_json::from_slice(&std::fs::read(h.dir.join("s/state.json")).unwrap()).unwrap();
    assert_eq!(state["txn"]["step"], "rolling_back", "{state}");
    assert_eq!(state["txn"]["restore_failed"], true, "{state}");
    std::fs::rename(&moved, &snapshot).unwrap();

    h.start_daemon();
    h.wait("v1 to serve again", 15, || h.get().as_deref() == Some("v1"));
    let status = h.status();
    assert_eq!(
        (status["permanent"].as_str(), status["pending"].clone()),
        (Some("g1"), Value::Null),
        "{status}"
    );
    assert_eq!(status["process"]["state"], "running", "{status}");
    assert_eq!(status["process"]["generation"], "g1", "{status}");
    let g2 = h.generation("g2");
    assert_eq!(g2["state"], "rolled_back", "{g2}");
    // The first rollback's reason stays; the restart only finished it.
    assert_eq!(g2["rollback_reason"], "rollback requested", "{g2}");
    // v2's row is gone and g1's start after the restart is the second.
    assert_eq!(h.versions(), ["v1", "v1"]);
}

/// A crash backoff shows as `restart_at` while `state` stays `stopped`, and `stop` during it cancels the restart and
/// answers ok: the app stays stopped once the backoff has passed.
#[test]
fn stop_during_a_crash_backoff_cancels_the_restart_and_answers_ok() {
    let mut h = Harness::new("max_failed_starts = 5");
    // Two failed starts in a row: the second restart waits 2 s, the window `stop` lands in.
    h.edit_manifest("min_uptime = 1", "min_uptime = 60");
    h.commit("v1", "confirm");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    assert_eq!(h.status()["process"]["restart_at"], Value::Null);

    h.crash_app();
    h.wait("the first crash restart", 10, || {
        h.status()["process"]["state"] == "running"
    });
    h.crash_app();
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
    std::thread::sleep(Duration::from_millis(3000));
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
    make_permanent(&h, "v1", "g1");
    h.ok(&["stage", "v2"]);
    h.ok(&["test", "g2"]);

    // Nothing has failed: neither with a switch pending nor without one.
    refused(
        &h,
        &["rollback", "--keep-storage"],
        "invalid_state",
        "only gives up a restore that failed",
    );
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("v2 to serve", 10, || h.get().as_deref() == Some("v2"));
    refused(
        &h,
        &["rollback", "--keep-storage"],
        "invalid_state",
        "only gives up a restore that failed",
    );
    assert_eq!(h.status()["pending"]["generation"], "g2");

    std::fs::remove_dir_all(h.dir.join("s/generations/g2/snapshot")).unwrap();
    refused(&h, &["rollback"], "storage_failed", "could not be restored");
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
    refused(&h, &["rollback"], "invalid_state", "nothing to roll back");
}

/// The messages of the events of `kind`, oldest first, that start with `prefix`.
fn messages_starting(h: &Harness, kind: &str, prefix: &str) -> Vec<String> {
    messages(h, kind)
        .into_iter()
        .filter(|message| message.starts_with(prefix))
        .collect()
}

/// Stage `version` as generation `id`, test it, switch to it and wait for the app to confirm itself.
/// Reload the manifest with a 3 s health timeout, so that an unhealthy pending generation rolls back soon. Done only
/// once the generations have passed their tests and g1 is permanent: on a loaded machine a node app can take longer
/// than 3 s to start, and would then fail its test or roll back.
fn shorten_the_health_timeout(h: &Harness) {
    h.edit_manifest("timeout = 20", "timeout = 3");
    h.signal_daemon(Signal::HUP);
    h.wait("the reload to be logged", 10, || {
        messages_starting(h, "daemon", "manifest reloaded")
            .iter()
            .any(|message| message.contains("health"))
    });
}

fn make_permanent(h: &Harness, version: &str, id: &str) {
    h.ok(&["stage", version]);
    h.ok(&["test", id]);
    h.ok(&["switch", id]);
    h.wait(&format!("{id} to confirm itself"), 15, || {
        h.status()["permanent"] == id
    });
}

/// `fawkes <args>` fails with exit code 1, error `code`, and a message that says `says`.
fn refused(h: &Harness, args: &[&str], code: &str, says: &str) {
    let (exit, error) = h.fk(args);
    assert_eq!(
        (exit, error["code"].as_str()),
        (1, Some(code)),
        "{args:?}: {error}"
    );
    let message = error["message"].as_str().unwrap();
    assert!(message.contains(says), "{args:?}: {message}");
}

/// The pid a process wrote to `path`.
fn read_pid(path: impl AsRef<std::path::Path>) -> Pid {
    Pid::from_raw(
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap()
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
    h.signal_daemon(Signal::HUP);
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
    h.signal_daemon(Signal::HUP);
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
        h.signal_daemon(Signal::HUP);
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
    shorten_the_health_timeout(&h);

    let mut follow = h.follow(&[]);
    h.ok(&["switch", "g2", "--deadline", "120"]);
    follow.until("g2's unhealthy event", 20, |e| {
        e["kind"] == "unhealthy" && e["generation"] == "g2"
    });
    let mut raw = h.connect();
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
    std::thread::sleep(Duration::from_millis(500));
    assert!(log() == snapshot, "the daemon was not quiet");

    let mut follow = h.follow(&[]);
    let replay: Vec<Value> = (0..20)
        .map(|_| {
            follow
                .next(Duration::from_secs(10))
                .expect("a replayed event")
        })
        .collect();
    assert_eq!(replay, newer_tail(&snapshot, 20));
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

#[test]
fn a_usage_error_exits_2_and_help_exits_0() {
    // None of these reach a daemon, so no harness is needed.
    let run = |args: &[&str]| {
        let output = common::fawkes().args(args).output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        (output.status.code(), stdout)
    };
    for args in [
        &[][..],
        &["help"],
        &["frobnicate"],
        &["status", "--keep-storage"],
        &["switch", "g1", "--deadline", "soon"],
        &["confirm", "g1", "g2"],
        &["stage"],
    ] {
        assert_eq!(run(args).0, Some(2), "{args:?}");
    }
    // Under `--json` the usage error is the error object on stdout, like every other failure.
    for args in [&["--json", "status", "--bogus"][..], &["stage", "--json"]] {
        let (code, stdout) = run(args);
        assert_eq!(code, Some(2), "{args:?}");
        let error: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(error["code"], "usage", "{stdout}");
        assert!(
            error["message"].as_str().unwrap().contains("Usage: fawkes"),
            "{stdout}"
        );
    }
    for args in [&["--help"][..], &["switch", "--help"], &["status", "-h"]] {
        let (code, stdout) = run(args);
        assert_eq!(code, Some(0), "{args:?}");
        assert!(stdout.contains("Usage: fawkes"), "{stdout}");
    }
}

// ------------------------------------------------ failing tests, health commands, retired generations, startup

/// The sample app's `crash` mode crashes while `{state}/crash` exists, so the same generation fails its test and later
/// passes it.
fn crash_while_marked(h: &Harness) -> std::path::PathBuf {
    h.edit_manifest(
        r#"APP_HOME = "{home}""#,
        "APP_HOME = \"{home}\"\nCRASH_WHILE = \"{state}/crash\"",
    );
    let marker = h.dir.join("s/crash");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "").unwrap();
    marker
}

/// A generation whose test fails is `test_failed` and cannot be switched to, until a test of it passes.
#[test]
fn a_failing_test_leaves_the_generation_test_failed_until_a_retest_passes() {
    let mut h = Harness::new("");
    let marker = crash_while_marked(&h);
    h.commit("v1", "crash");
    h.start_daemon();
    h.ok(&["stage", "v1"]);

    refused(&h, &["test", "g1"], "health_failed", "g1 failed its test");
    let g1 = h.generation("g1");
    assert_eq!(g1["state"], "test_failed");
    assert_eq!(g1["test"]["passed"], false);
    assert_eq!(messages(&h, "test_failed").len(), 1);
    refused(
        &h,
        &["switch", "g1"],
        "invalid_state",
        "g1 failed its last test",
    );
    assert_eq!(h.status()["process"]["state"], "stopped");

    std::fs::remove_file(&marker).unwrap();
    let g1 = h.ok(&["test", "g1"]);
    assert_eq!(g1["state"], "tested");
    assert_eq!(g1["test"]["passed"], true);
    assert_eq!(messages(&h, "tested").len(), 1);
    // `tested → tested` is allowed too.
    assert_eq!(h.ok(&["test", "g1"])["state"], "tested");
    h.ok(&["switch", "g1"]);
}

/// A test that cannot get a spare port, or whose spare port another process takes, blames no one: the generation stays
/// as it was.
#[test]
fn a_test_without_a_free_spare_port_or_with_a_stolen_one_leaves_the_generation_unchanged() {
    let mut h = Harness::new("");
    // While `{state}/squat` exists, the start command has a listener of its own session take the spare port and
    // then fails, so the port is still taken once the test's process group is gone.
    let squat = h.dir.join("squat.js");
    std::fs::write(
        &squat,
        r"const fs = require('node:fs');
if (process.argv[2] === 'listen') {
  require('node:net').createServer().listen(Number(process.argv[3]), '127.0.0.1', () =>
    fs.writeFileSync(process.argv[4], String(process.pid)));
  // Should the test fail before it ends the listener, the listener ends itself.
  setTimeout(() => process.exit(), 60000);
} else {
  require('node:child_process').spawn(process.execPath, [__filename, 'listen', ...process.argv.slice(2)],
    { detached: true, stdio: 'ignore' }).unref();
  setInterval(() => { if (fs.existsSync(process.argv[3])) process.exit(3); }, 50);
}
",
    )
    .unwrap();
    h.edit_manifest(
        r#"start = "node app.js""#,
        &format!(
            r#"start = "if [ -e {{state}}/squat ]; then exec node {} {{port}} {{state}}/squat.pid; fi; exec node app.js""#,
            squat.display()
        ),
    );
    h.commit("v1", "confirm");
    h.start_daemon();
    let staged = h.ok(&["stage", "v1"]);
    std::fs::write(h.dir.join("s/squat"), "").unwrap();

    refused(
        &h,
        &["test", "g1"],
        "port_in_use",
        "was taken by another process",
    );
    assert_eq!(h.generation("g1"), staged, "the stolen port blamed g1");
    assert!(
        messages(&h, "error")
            .iter()
            .any(|message| message.contains("was taken by another process")),
    );
    assert_eq!(messages(&h, "test_failed").len(), 0);
    // The listener is the one process this test has to end, by the pid it wrote.
    let listener = read_pid(h.dir.join("s/squat.pid"));
    kill_process(listener, Signal::KILL).unwrap();
    std::fs::remove_file(h.dir.join("s/squat")).unwrap();
    h.wait("the listener to release the spare port", 10, || {
        std::net::TcpListener::bind(("127.0.0.1", h.port + 1)).is_ok()
    });

    let held: Vec<_> = (h.port + 1..=h.port + 9)
        .map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).unwrap())
        .collect();
    refused(&h, &["test", "g1"], "start_failed", "no free spare port");
    assert_eq!(h.generation("g1"), staged);
    drop(held);
    assert_eq!(h.ok(&["test", "g1"])["state"], "tested");
}

/// `health.command` is run with `{port}` expanded and the app's `[env]` and FAWKES_* variables, in the test and for a
/// pending switch: a passing one lets the upgrade through, one that keeps failing rolls it back.
#[test]
fn a_health_command_checks_the_test_and_the_pending_switch() {
    let mut h = Harness::new("rollback_on_unhealthy = true");
    h.edit_manifest(
        r#"url = "http://127.0.0.1:{port}/""#,
        r#"command = 'curl -fsS http://127.0.0.1:{port}/ && test "$APP_PORT" = {port} && test -n "$FAWKES_PHASE"'"#,
    );
    h.commit("v1", "confirm");
    h.commit("v2", "unhealthy-live");
    h.start_daemon();
    make_permanent(&h, "v1", "g1");
    assert_eq!(h.get().as_deref(), Some("v1"));

    h.ok(&["stage", "v2"]);
    assert_eq!(h.ok(&["test", "g2"])["state"], "tested");
    shorten_the_health_timeout(&h);
    h.ok(&["switch", "g2", "--deadline", "120"]);
    h.wait("the failing health command to roll g2 back", 20, || {
        h.generation("g2")["state"] == "rolled_back"
    });
    assert_eq!(h.generation("g2")["rollback_reason"], "health check failed");
    assert_eq!(h.status()["permanent"], "g1");
}

/// A retired generation is switched back to like any tested one; a missing ref is `git_failed`; `prune` without `keep`
/// is `invalid_state`; and `reuse` directories are cloned from the repository, then from the permanent generation.
#[test]
fn a_retired_generation_is_switched_back_to_and_stage_and_prune_refuse_what_they_cannot_do() {
    let mut h = Harness::new(r#"reuse = ["node_modules"]"#);
    h.commit("v1", "confirm");
    h.commit("v2", "confirm");
    h.start_daemon();
    // Untracked, so only `reuse` brings it into a worktree.
    let modules = h.dir.join("repo/node_modules");
    std::fs::create_dir(&modules).unwrap();
    std::fs::write(modules.join("from-repo"), "").unwrap();
    let code = |id: &str| {
        h.dir
            .join("s/generations")
            .join(id)
            .join("code/node_modules")
    };

    h.ok(&["stage", "v1"]);
    assert!(
        code("g1").join("from-repo").exists(),
        "no permanent generation: cloned from the repository"
    );
    h.ok(&["test", "g1"]);
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });
    std::fs::write(code("g1").join("from-g1"), "").unwrap();
    std::fs::remove_dir_all(&modules).unwrap();

    make_permanent(&h, "v2", "g2");
    assert!(
        code("g2").join("from-g1").exists(),
        "cloned from the permanent generation's worktree"
    );
    assert_eq!(h.generation("g1")["state"], "retired");

    refused(
        &h,
        &["stage", "nope"],
        "git_failed",
        "`nope` is not a commit",
    );
    assert_eq!(
        h.ok(&["history"])["generations"].as_array().unwrap().len(),
        2
    );
    refused(&h, &["prune"], "invalid_state", "keep = N");
    assert_eq!(
        h.ok(&["history"])["generations"].as_array().unwrap().len(),
        2
    );

    refused(&h, &["switch", "g1"], "invalid_state", "retired");
    assert_eq!(h.ok(&["test", "g1"])["state"], "tested");
    h.ok(&["switch", "g1"]);
    h.wait("g1 to confirm itself again", 15, || {
        h.status()["permanent"] == "g1"
    });
    h.wait("v1 to serve", 10, || h.get().as_deref() == Some("v1"));
    assert_eq!(h.generation("g2")["state"], "retired");
}

/// What the next daemon start does about what the last one left: a second daemon of the same name is refused without
/// harming the first, an unreadable `state.json` is recovered from its copy and reported, and a generation directory
/// no record owns is removed.
#[test]
fn a_daemon_start_refuses_a_namesake_recovers_state_json_and_removes_a_stray_directory() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);

    // Another state_dir, the same name and the same fawkes_home: the run link already answers.
    let d = h.dir.display();
    let manifest = std::fs::read_to_string(h.dir.join("fawkes.toml")).unwrap();
    let other = h.dir.join("other.toml");
    std::fs::write(
        &other,
        manifest
            .replace(&format!("{d}/s\""), &format!("{d}/s2\""))
            .replace(&format!("{d}/home\""), &format!("{d}/home2\"")),
    )
    .unwrap();
    let output = common::fawkes()
        .args(["daemon", "--manifest"])
        .arg(&other)
        .env("FAWKES_HOME", &h.dir)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("another daemon named t already runs"),
        "{stderr}"
    );
    assert!(
        !h.dir.join("s2/fawkes.sock").exists(),
        "the refused daemon left its socket behind"
    );
    assert_eq!(
        std::fs::read_link(h.dir.join("run/t.sock")).unwrap(),
        std::fs::canonicalize(h.dir.join("s/fawkes.sock")).unwrap(),
        "the first daemon's link was replaced"
    );
    assert_eq!(h.status()["name"], "t");

    h.kill_daemon();
    std::fs::write(h.dir.join("s/state.json"), "{ not json").unwrap();
    std::fs::create_dir(h.dir.join("s/generations/g99")).unwrap();
    h.start_daemon();
    assert_eq!(h.generation("g1")["state"], "staged");
    assert!(
        messages(&h, "error")
            .iter()
            .any(|message| message.contains("recovered from")),
        "{}",
        h.daemon_log()
    );
    assert!(!h.dir.join("s/generations/g99").exists());
    assert!(
        messages(&h, "daemon")
            .iter()
            .any(|message| message.contains("g99") && message.contains("no generation owns"))
    );
}

/// A daemon that dies or is stopped while a test runs leaves no test process on the spare port: SIGKILL (the next
/// daemon ends it through `step.pid`) and SIGTERM (the daemon ends it itself on its way out).
#[test]
fn a_daemon_killed_or_stopped_during_a_test_leaves_the_spare_port_free() {
    let mut h = Harness::new("");
    // A check that never passes keeps the test running, with its app listening on the spare port.
    h.edit_manifest(
        r#"url = "http://127.0.0.1:{port}/""#,
        r#"command = "test -e {state}/ready""#,
    );
    h.commit("v1", "confirm");
    h.start_daemon();
    h.ok(&["stage", "v1"]);
    let spare = h.port + 1;
    let listens = || std::net::TcpStream::connect(("127.0.0.1", spare)).is_ok();
    let free = || std::net::TcpListener::bind(("127.0.0.1", spare)).is_ok();
    let start_test = |h: &Harness| {
        let test = h
            .cli()
            .args(["test", "g1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        h.wait("the test app to listen", 10, listens);
        test
    };

    let mut test = start_test(&h);
    h.kill_daemon();
    test.wait().unwrap();
    h.start_daemon();
    h.wait("the next daemon to end the test left behind", 10, free);
    assert_eq!(h.generation("g1")["state"], "staged");

    let mut test = start_test(&h);
    h.signal_daemon(Signal::TERM);
    assert!(h.daemon_exits_within(Duration::from_secs(10)));
    // The daemon SIGKILLs the test's group and exits without waiting for the kernel to reap it.
    h.wait("the test app to leave the spare port", 10, free);
    test.wait().unwrap();
}

/// `--json` keeps stdout to one JSON document and puts the progress of a step on stderr, one event object per line;
/// without it the answer is text.
#[test]
fn json_output_is_one_document_on_stdout_and_progress_lines_on_stderr() {
    let mut h = Harness::new("");
    h.commit("v1", "confirm");
    h.start_daemon();

    for args in [["stage", "v1"], ["switch", "g1"]] {
        if args[0] == "switch" {
            h.ok(&["test", "g1"]);
        }
        let output = h.cli().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let result: Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            result["id"].as_str().or(result["generation"].as_str()),
            Some("g1")
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.trim().is_empty(), "{args:?} narrated nothing");
        for line in stderr.lines() {
            let progress: Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("{args:?} wrote a line to stderr that is not JSON ({e}): {line}")
            });
            assert_eq!(progress["kind"], "progress", "{line}");
            assert_eq!(progress["generation"], "g1", "{line}");
        }
    }
    h.wait("g1 to confirm itself", 15, || {
        h.status()["permanent"] == "g1"
    });

    let output = common::fawkes()
        .arg("--manifest")
        .arg(h.dir.join("fawkes.toml"))
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(serde_json::from_str::<Value>(&text).is_err(), "{text}");
    for word in [
        "t  daemon pid",
        "permanent g1",
        "current g1",
        "process running",
    ] {
        assert!(text.contains(word), "{word:?} is not in:\n{text}");
    }
}
