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
