# Fawkes code review (Claude Opus 5.5)

Reviewed at commit `57b914b` on 2026-11-03, by Claude Opus 5.5 (`anthropic/claude-opus-5-5`, high thinking). This
is `main` with phases 1 and 2 of [PLAN.md](PLAN.md) landed on top of `bfcac2d`, the commit that
[01-review.md](01-review.md) and [02-review.md](02-review.md) reviewed. Every file in `src/` and `tests/` was read in
full, plus `README.md`, `DESIGN.md`, `CLAUDE.md`, `PLAN.md`, `Cargo.toml` and `examples/sample/app.js`. Every
file:line below was read at `57b914b`; none is copied from the earlier reviews. Two findings (M1, M2) were also
shown with a throwaway integration test (port block 4900/4910), which was deleted afterwards. Its output is quoted
under each finding.

## Commands run

| Command | Result |
|---|---|
| `cargo fmt --check` | clean, exit 0, no output |
| `cargo clippy --all-targets -- -D warnings` | clean, exit 0 |
| `cargo test` | exit 0: 20 unit tests pass (4.00 s), 10 integration tests pass (12.37 s), 0 ignored |
| `cargo clippy --all-targets -- -W clippy::pedantic` | exit 0, 54 distinct warnings: 48 in the binary, 4 more in its unit tests, 2 in `tests/core.rs` |

The pedantic warnings by kind: 8 `usize → u16` casts (all in `tui/draw.rs`), 6 long literals, 5 redundant closures,
4 `format!` appended to a `String`, 3 needless pass-by-value, 3 float equality comparisons (the `reload` field
diff), 2 unused `self`, 4 functions over 100 lines (`Daemon::run` daemon.rs:256, `Daemon::test` daemon.rs:1423,
`draw_apps` draw.rs:406, a test in tests/core.rs:9), 2 implicit borrows as raw pointers (draw.rs:125), and single casts
`u64 → i64` (state.rs:26, draw.rs:120), `f64 → u64` (draw.rs:136) and `u32 → i32` (tests/common/mod.rs:239). The
`pgid as i32` casts that review 1 flagged in `process.rs` are gone. None of the warnings is a bug.

## Verdict

Phases 1 and 2 are really done. All 20 PLAN.md rows are fixed in the code, and every test the plan asked for exists
and passes. The two decisions built in phase 2 (`rollback_on_unhealthy`, `keep` and `prune`) work and are tested.
The core of the transaction holds up when read adversarially. The intent is written before the side effect
(daemon.rs:1609-1630). A confirm cannot slip between a crash and its rollback (daemon.rs:1100-1104, 1663-1716). A
failed restore keeps the switch pending and starts nothing (daemon.rs:1782-1799). The end of a rollback is
written before the permanent generation starts again (daemon.rs:1800-1834). I found no path that leaves an
upgrade neither permanent nor rolled back, and none that starts or rolls back a `rolled_back` generation a second
time.

What remains is at the edges of the promise, not in it:

| Severity | Count |
|---|---|
| High: can break the transactional guarantee | 0 |
| Medium: supervision or recovery gaps, shown by a test | 2 |
| Low: races with benign outcomes, misleading answers, doc drift | 14 |

The two Medium findings are bugs in the supervision around the promise. **M1:** a daemon crash leaves its test and
install processes running, and since phase 1 every panic is such a crash. **M2:** a switch that fails during a crash
backoff leaves the live app down with no restart scheduled. Both are small to fix, and both should be fixed and
tested before phase 3 rewrites the code they live in (section 6).

---

## 1. The plan, row by row

"Fixed" means the change the row asks for is in the code at the lines cited and, where the plan asked for a test,
the test exists and passes. A ⚠ marks a fix that works as asked but exposes or leaves a problem, listed in section 2.

### Phase 1

| # | Status | Evidence at `57b914b` |
|---|---|---|
| 1.1 Panic policy | **Fixed** ⚠ | `panic = "abort"` for dev and release (Cargo.toml:16-23). `log_panics` appends an `error` event through its own file handle, then runs the default hook (daemon.rs:505-519, state.rs:363-368). It is installed before the socket exists (daemon.rs:271). `lock()` aborts on a poisoned mutex instead of recovering (daemon.rs:35-37). ⚠ The row's premise, "a dead watcher thread becomes a daemon restart, which the startup recovery already handles", is not true for test and install processes (M1). Two `unwrap_or_else(\|e\| e.into_inner())` remain in the TUI (tui.rs:508, 563); they are dead code under abort. |
| 1.2 `--deadline` range | **Fixed** | `protocol::deadline_arg` refuses anything outside `1..=MAX_DEADLINE` with `usage` (protocol.rs:33-49, manifest.rs:64). It runs in `dispatch` before `switch` is called (daemon.rs:887), so nothing has stopped yet. The addition saturates (daemon.rs:1599-1600). The manifest `deadline` has the same bound (manifest.rs:241-246). Tested in tests/core.rs:148-165, with the live app checked to be still up. |
| 1.3 Manifest validation | **Fixed** | `Manifest::problems` (manifest.rs:217-283) covers every field the row names, plus `keep`, `install_timeout`, `reuse` and an upper bound of 86400 s (`MAX_SECONDS`, manifest.rs:68). That bound keeps `Duration::from_secs_f64` and `Instant + Duration` from panicking. A SIGHUP reload goes through `Manifest::load`, so it validates too (daemon.rs:399-406). Unit tests at manifest.rs:456-580. |
| 1.4 Durable writes | **Fixed** | `write_durably`: temp file, `sync_all`, rename, directory `sync_all` (state.rs:206-218). `State::save` writes `state.json` and then `.bak` (state.rs:251-255). `load` falls back to `.bak` and says so (state.rs:223-247), and the daemon turns that into an `error` event (daemon.rs:320-322). `snapshot/index.json` is written durably, last (storage.rs:64-67). The intent saves go through `persist`, which fails the step (daemon.rs:576-584): switch daemon.rs:1622, confirm :1734-1742, end of rollback :1804-1817. |
| 1.5 Intent before spawn | **Fixed** | `pending` and `Current` are persisted on a copy, the copy is adopted, and only then does `start_live` spawn (daemon.rs:1609-1630). A spawn failure goes through the ordinary `rollback_pending` (daemon.rs:1631-1643). Tested by `a_switch_records_its_intent_before_the_new_generation_starts` (tests/core.rs:198-243): the new app copies `state.json` at its first instruction and kills the daemon. |
| 1.6 Safe SIGHUP reload | **Fixed** ⚠ | `Manifest::reload` refuses `name`, `repo`, `state_dir`, `home` and `storage`, refuses `port` while the app runs, and names what changed (manifest.rs:288-344). The daemon holds `inner` across the check and the swap (daemon.rs:407-425). ⚠ "While the app runs" is `inner.app.is_some()`, which is false in the middle of a switch or restart (L4). |
| 1.7 `flock` | **Fixed** | `lock_state_dir` takes `NonBlockingLockExclusive` on `<state_dir>/lock`, mode 0600, held for `run`'s life as `_lock` (daemon.rs:485-502, 270). `bind` never takes over a socket that answers (daemon.rs:523-555). Tested: a second daemon is refused (tests/core.rs:143-146). |
| Phase 1 low items | **Fixed** | `--end-of-options` (daemon.rs:1213). The `current()` doc comment sits on `current()` (state.rs:290-296). Daemon `usage` errors exit 2 (main.rs:240-246). Checked pgids that refuse 0 and 1 (process.rs:54-59). signal-hook installed first (daemon.rs:258-265). The `umask` comment (daemon.rs:548-552). An `error` event for an unreadable deadline (daemon.rs:587-600). |
| Phase 1 tests | **Present** | kill -9 during a pending switch (tests/core.rs:131-192), the crash between spawn and save (:198), manifest validation (manifest.rs:456), the huge `--deadline` (:148-165). |

### Phase 2

| # | Status | Evidence at `57b914b` |
|---|---|---|
| 2.1 confirm vs crash | **Fixed** | `reap` decides where the facts are. For the pending generation it records the rollback reason in `inner.rollback` in the same critical section that takes the child (daemon.rs:1076-1105). A missed deadline is recorded the same way (`decide_rollback`, daemon.rs:1059-1069), and so is a failed health check (`roll_back_unhealthy`, daemon.rs:768-778). `confirm` refuses on a recorded rollback (daemon.rs:1663-1668), on a passed deadline (:1669-1676), and when the pending generation is not running or has exited (:1699-1716). Tested by `a_confirm_racing_a_crash_of_the_pending_generation_is_refused` (tests/core.rs:299), whose assertion is weak (§5). |
| 2.2 reap outside the lock | **Fixed** ⚠ | `tick` takes the Live slot (daemon.rs:961-966). It reaps and decides under `inner` (:967-974), then ends the stragglers' group after releasing it (:977-980). `stop_group` waits after SIGKILL until the group is gone, bounded by `KILL_WAIT` (process.rs:136-166). The old 2 s loop in `reap` is gone. ⚠ The wait for the leader after SIGKILL is unbounded (L7). |
| 2.3 Back-pressure | **Fixed** | 2 s write timeout on every client (protocol.rs:14, 73-80), and a client that times out once gets nothing more (`gone`, protocol.rs:69, 90-100). A `sync_channel(1024)` per subscriber, dropped on `try_send` failure (daemon.rs:129-144). `take(64 KiB)` on the request (daemon.rs:858-869). The TUI's read timeout covers only `start` and `confirm` (tui.rs:586-590); the comment there says why the slow actions go without. Unit tests at daemon.rs:1990-2005 and protocol.rs:136-160. |
| 2.4 install | **Fixed** ⚠ | `run_install` spawns in its own group, tracks the pgid in `step_pgids`, enforces `install_timeout` and ends the group afterwards (daemon.rs:1357-1393). Shutdown kills tracked groups (daemon.rs:440-442). Tested by `a_failing_or_hanging_install_leaves_nothing_behind` (tests/core.rs:383-431). ⚠ The tracking lives only in memory, so a daemon crash orphans the install (M1). |
| 2.5 Startup guard | **Fixed** | `Published` removes the socket, and the run link while it still points at our socket, on every return from `run` (daemon.rs:160-180, 268-273). |
| 2.6 Stale app kill | **Fixed** ⚠ | The start wrapper writes `$$` to `app.pid` before it `exec`s the command (process.rs:16-50). `leftover` accepts the pid only if the process started at most 60 s before the file's mtime and its group is alive (process.rs:105-125, start time via `proc_pidinfo` at process.rs:68-79 and `/proc` at :81-103). The file is removed when the group is gone (daemon.rs:789-800, 979). There is a unit test (process.rs:279-319), and tests/core.rs:131 and :198 exercise it end to end. ⚠ A group whose leader died is never found (L5). The Linux branch has no test. |
| 2.7 Logs bounded | **Fixed** | An in-memory ring of 1000 events (state.rs:304-402). `events.jsonl` rotates at 10 MiB (state.rs:310, 386-393). The live `app.log` is capped by copy and truncate every 100 watcher passes (daemon.rs:349-360 in `run`, `cap_app_log` daemon.rs:1029-1048). Unit test at state.rs:490-521. |
| 2.8 Stage cleanup | **Fixed** ⚠ | `staging` is recorded before the slow work (daemon.rs:1232-1256). A failure removes the record, directory and worktree (daemon.rs:1258-1272, `remove_generation` :1396-1421). Startup sweeps `staging` records and directories nobody owns, then runs `git worktree prune` (daemon.rs:1911-1955). ⚠ The sweep deletes the directory of a stage whose install is still running, orphaned by the crash (M1). |
| 2.9 `state_dir` 0700 | **Fixed** | `create_private_dir` creates with 0700 and tightens an existing directory (daemon.rs:451-461). |
| 2.10 Spare port | **Fixed** | `port_free` probes `127.0.0.1` and `0.0.0.0` (daemon.rs:1960-1963). A failed test whose port is taken afterwards is `port_in_use` and leaves the generation unchanged (daemon.rs:1491-1499). There is no test for that path. |
| 2.11 Templates | **Fixed** | One pass (manifest.rs:118-149). Values are shell-quoted in commands only (manifest.rs:109-116, 152-160). Unit tests run the result through `/bin/sh` (manifest.rs:390-405). DESIGN.md documents the quoting. |
| 2.12 `parse_time` | **Fixed** | Only the canonical form round-trips (state.rs:45-50), tested at state.rs:524-536. One overflow sits before the check, unreachable today (L9). |
| 2.13 TUI | **Fixed** | `tzset` once, before any thread, with a `SAFETY` comment (tui.rs:612-620). `width_of` measures with `Span::width` (draw.rs:183-187). |
| Decision 2 `rollback_on_unhealthy` | **Built** | manifest.rs:43-45. The health thread calls `roll_back_unhealthy` (daemon.rs:751-760, 768-778). Tested at tests/core.rs:342-365. |
| Decision 3 `keep` / `prune` | **Built** | `State::prunable` (state.rs:271-288), `prune` and `prune_soon` (daemon.rs:1843-1907), unit test state.rs:453-487, integration test tests/core.rs:436-491. |
| Phase 2 tests | **Present** | The confirm/crash race (tests/core.rs:299). A subscriber that never reads (daemon.rs:1990, protocol.rs:136). A failing install (tests/core.rs:383). Busy semantics (tests/core.rs:496). |

**Tally:** 20 of 20 rows fixed (7 in phase 1, 13 in phase 2). 0 partial, 0 not fixed. Six carry a ⚠, and two of
those lead to the Medium findings below (1.1, 2.4 and 2.8 to M1). No fix made the promise weaker.

---

## 2. New findings

Ordered by what they can do to the promise first, then by severity.

### M1. A daemon crash leaves its test and install processes running (Medium)

`src/daemon.rs:159`, `:1374`, `:1477`, `:440-442`, `:1911-1955`

The process groups of tests and installs exist only in `step_pgids`, an in-memory `Vec`. Only a graceful `shutdown`
kills them (daemon.rs:440-442). After a crash (kill -9, OOM, and since 1.1 every panic) the next daemon knows
nothing of them. Startup handles a leftover *live* app through `app.pid` (daemon.rs:326-333), but not these. The
startup sweep then deletes the directory of the cut-short stage while its install is still running in it.

Shown with a throwaway test: a `slow-test` generation under test, the daemon SIGKILLed, a new daemon started. Then a
`stage` whose install hangs, the daemon SIGKILLed again, a new daemon started:

```
ORPHAN TEST APP on 4901 after daemon SIGKILL: Some("v2")
ORPHAN TEST APP on 4901 after the next daemon started: Some("v2")
ORPHAN INSTALL pid 19563 alive after restart: true; generations ["g1", "g2"]; g3 dir exists: false
```

**Why it matters:**
- DESIGN.md says the daemon start "recovers whatever was in flight", and PLAN.md 1.1 rests on that premise.
- Each crash during a test leaks a process that holds a spare port for good, so after `spare_ports` crashes `test`
  fails with `no free spare port`.
- The orphaned test app keeps `FAWKES_SOCKET` and `FAWKES_GENERATION` and talks to the new daemon. An app that
  confirms on boot without checking `FAWKES_PHASE` would confirm whatever is pending.
- An orphaned install keeps writing into a worktree that no longer exists.

**Fix:** `process::spawn` already has the pid-file wrapper. Give tests and installs a pid file each, for example
`generations/<id>/step.pid`, or one `<state_dir>/step.pid` since one step runs at a time. Have startup stop it
through `process::leftover` before `sweep_generations`, the way it stops `app.pid`. About 15 lines. Core test: kill
-9 the daemon during a `slow-test` test, start a new one, and the spare port is free.

### M2. A switch that fails during a crash backoff leaves the live app down (Medium)

`src/daemon.rs:1567-1586`, `:1590`, `:1624`

`switch` clears `restart_at` and takes `app` (daemon.rs:1567-1571). It only restarts the previous generation on
failure when there was a process to stop (`had_app`, daemon.rs:1573, 1578-1586). If the permanent generation has
just crashed and waits for its backoff restart, `app` is `None`. A switch that then fails at the snapshot (:1588-1595)
or at writing `state.json` (:1622-1629) has cancelled the restart and starts nothing. `status` says `stopped`, and
nothing brings the app back until someone runs `fawkes start`.

Shown with a throwaway test. `bad.sqlite`, a storage file that is not a database, makes the snapshot fail. `GET
/crash` crashes g1, and the switch comes inside its 1 s backoff:

```
SWITCH during backoff: 1 {"code":"storage_failed","message":"cannot copy …/bad.sqlite to …/g2/snapshot/1.sqlite: file is not a database; g2 stays tested and nothing changed"}
PROCESS 4 s later: {"generation":"g1",…,"state":"stopped",…}
GET: None
```

**Why it matters:** DESIGN.md ("Switch") promises that a switch that fails before step 4 "starts the previous
generation again". The error message says "nothing changed", but the app that should have been restarted after 1 s
stays down. This is the supervisor's core job, outside the transaction.

**Fix:** decide whether the previous generation should run from the process *and* the backoff:
`let should_run = inner.app.is_some() || inner.restart_at.take().is_some();`. `resume` then uses `should_run`, and
an `app_failed` generation stays down as it should. Phase 3.3's `Live` enum makes this state explicit; until then,
a core test reproduces it as above.

### Low

| # | Where | Issue and fix |
|---|---|---|
| L1 | daemon.rs:1182-1197, 1165-1180 | **`restart` racing an unreaped crash retries the pending generation.** `restart_command` takes `inner.app` without checking `try_wait`. A crash in the ≤100 ms before `tick` reaps is recorded as a *requested* stop (`stop_app` → `LastExit::new(status, true)`, daemon.rs:789-795), and the pending generation is started again. DESIGN.md says "the pending generation gets no retries: its first crash rolls it back". The promise holds, because it must still confirm while running. `stop` has the same race, with a deadline rollback as the outcome. Fix: in both, `try_wait` the taken app; if it has exited, put it back and answer `busy`, so `reap` handles it. |
| L2 | daemon.rs:1170-1174, 802-830 | **`stop` during a crash backoff cancels the restart but answers `invalid_state`** ("not running; `fawkes start` starts it"): a side effect on the error path, and an agent concludes that nothing happened. `status` cannot show a backoff either: `process.state` is `stopped`, the same as after a requested stop. Fix: answer `ok` when a scheduled restart was cancelled; make the backoff visible in phase 3.3 (a protocol change for DESIGN.md). |
| L3 | daemon.rs:692-695, 1631-1643, 428-436 | **SIGTERM during a switch rolls back a target that never ran,** with reason `start failed: the daemon is shutting down`. `shutdown` waits for the switch (daemon.rs:433), and the switch then reaches `start_live`, which refuses. The target is `rolled_back` for good, and a stage and test are lost to a routine `launchctl` restart. Fix: check `shutting_down()` before `persist` (daemon.rs:1622) and give up the switch there. The target stays `tested`, and the next start runs the permanent generation. |
| L4 | daemon.rs:409, 696 | **A reload can change `port` in the middle of a switch or restart.** `app.is_some()` is false between stopping the old process and starting the new one, and `start_live` reads the manifest afresh, so the switch announces one port and starts on another. Fix: also refuse `port` while `inner.moving` is set. |
| L5 | process.rs:105-125 | **A leftover group whose leader died is never stopped.** `leftover` needs `start_time(pid)` of the leader. With `start = "npm start"` and a leader that exits while the daemon is dead, the rest of the group lives on. If that group is the pending generation's, the startup rollback restores storage underneath it and the permanent generation cannot bind the port. This needs two failures at once. Fix: when the leader is gone but `group_alive(pgid)` holds and the file is newer than the boot time, stop the group (a pgid cannot be reused while its group has members). |
| L6 | daemon.rs:1512-1515, manifest.rs:353, 360 | **`expect` in daemon paths.** `test` does `.expect("generations are never removed")`, which stopped being true in phase 2: prune, failed stages and the sweep remove generations. It holds only because `test` owns the step slot. Under abort, a future change there kills the daemon mid-test, which is M1. Fix: `.ok_or_else(\|\| not_found(id))?` (the test process is already stopped by then). `state_dir()`'s `expect` and `health_check`'s `unreachable!` encode invariants of `load`; phase 3 can make them types (a resolved `state_dir: PathBuf`, an enum `Check` in the manifest). |
| L7 | process.rs:136-138, 157-161 | **The wait after SIGKILL is not bounded for the leader.** The comment says the wait is bounded because of uninterruptible sleep, but `child.wait()` on the leader blocks until it is reaped. A leader in D state stalls `stop`, `switch` or `rollback` for as long as it is stuck. Fix: poll `try_wait` inside the `KILL_WAIT` loop. |
| L8 | daemon.rs:238-251, 1305, 1966-1983 | **`git` and `cp` run with no timeout and no process group.** `git worktree add` runs the repository's `post-checkout` hook, and a hanging hook (or a `cp -cR` of a huge `node_modules`) holds the step slot for ever and outlives shutdown. Fix: run them through `process::spawn` with a bound, or pass `-c core.hooksPath=/dev/null` to `worktree add`. |
| L9 | state.rs:53-69 | **`parse_fields` can overflow before the canonical check.** `h * 3600` with an hour field near `i64::MAX` panics in a debug build, which is an abort. It is unreachable today: deadlines come from the daemon's own writes, and a daemon start rolls back any pending switch before parsing one. Fix: reject fields longer than four digits, or use `checked_mul`. |
| L10 | daemon.rs:751-760 | **A confirm can fit between "unhealthy" and its rollback.** The health thread learns of the failure in `wait_healthy`, logs, and then records the rollback under a new lock (`roll_back_unhealthy`). DESIGN.md says such a rollback is recorded "in the same step that learns of it, so a confirm never slips in between". The outcome is benign, since the generation is running when it confirms, but the sentence overclaims. Fix: say so in DESIGN.md, or check and record inside `is_running`'s lock. |
| L11 | main.rs:187-257 | **The CLI has no timeouts.** A daemon that accepts and never answers hangs `fawkes confirm`, which the app itself calls, for ever. The TUI already bounds `start` and `confirm` (tui.rs:586-590). Fix: the same read timeout for `confirm`, `status`, `history`, `log` and `start`. |
| L12 | tui.rs:508, 563 | Poison recovery left in the TUI (PLAN 1.1 said to drop it): dead code under `panic = "abort"`. Use a plain `lock().unwrap_or_else(\|_\| abort())` as the daemon does, or `expect` with a reason (this is not daemon code). |
| L13 | (design) daemon.rs:1781-1799 | **A restore that can never succeed has no way out.** It is retried by `fawkes rollback` and every daemon start, and refuses `start`, `switch` and `confirm` (daemon.rs:1130-1134, 1551-1556, 1658-1662). A deleted or corrupt `snapshot/` leaves the app down until someone edits `state.json` by hand. Fix: phase 3.2 should define an explicit operator exit (a decision for the user: for example `rollback --keep-storage`, which records the restore as abandoned and starts the permanent generation). |
| L14 | tests/core.rs:246, 546, tui.rs:977 | Comments say "Found in review" and "Found while bounding request reads". CLAUDE.md asks comments never to mention a review or a previous version; :546 was added in phase 2. Rephrase them as the behaviour they pin. |

---

## 3. CLAUDE.md invariants

- **`unwrap`/`expect` in daemon paths:** `daemon.rs:1515` (`expect`, with a stale justification, L6), `manifest.rs:353`
  (`expect`) and `manifest.rs:360` (`unreachable!`), both reached from the daemon. `lock()` aborts on poison
  (daemon.rs:35-37); that is deliberate and documented. There is no other `unwrap`, `expect` or `panic!` outside
  `#[cfg(test)]` in `src/`.
- **A lock held across a slow step:** none for an install, a health check or a process stop. Every stop runs after
  `inner` is released: `stop_command` daemon.rs:1167-1177, `restart_command` :1184-1191, `switch` :1567-1576,
  `rollback_pending` :1755-1768, `tick` :967-980, `shutdown` :437-439. `inner` *is* held across a spawn
  (`start_live`, daemon.rs:692-731) and across durable saves: four `F_FULLFSYNC`s per `State::save`, about 9-14 ms
  on this machine (a Python emulation of `save`, 20 runs: median 9.4 ms, max 13.8 ms). That is acceptable, but
  DESIGN.md's "`confirm` is always answered" now means "after the current save". **Lock order:** the daemon.rs
  header says `inner → manifest → log → hub` (daemon.rs:4-6), CLAUDE.md says `inner → log → hub`, and neither
  mentions `step_pgids`, which is only ever taken alone. Update CLAUDE.md to match the code.
- **`// SAFETY:`:** every `unsafe` block in `src/` has one (process.rs:72, 75, 101; draw.rs:121, 123; tui.rs:618).
  The `unsafe extern "C" { fn tzset(); }` declaration (tui.rs:612-615) has a doc comment but no `SAFETY:` for the
  signature. In the tests, `libc::kill` and `libc::killpg` at tests/common/mod.rs:239 and :252, and `libc::kill` at
  tests/core.rs:416, have none. :252 (79816c7) and core.rs:416 (aace336) were added in phase 2, which breaks "any
  unsafe block you add or touch".
- **`let _ =` on results that matter:** none. All 20 are best-effort cleanups or timeouts that fail only for a zero
  duration (protocol.rs:75, daemon.rs:857), and most say so. The one worth a second look, `storage.rs:49`
  (`remove_dir_all` of an old snapshot), is safe because the index is rewritten and a rollback trusts only the index.
- **Process-group signalling:** the app, the test, the install and the health command each run in their own group
  (`process_group(0)`, process.rs:48, 211), and every signal goes to the group through rustix's
  `kill_process_group` (process.rs:60-66). The exceptions are `git` and `cp`, which run in the daemon's own group
  with no bound (L8).
- **Error codes** are still compared as strings (`e.code == "busy"`, daemon.rs:1901). Phase 3.1 replaces that.

---

## 4. DESIGN.md versus the code

| # | DESIGN.md says | The code does |
|---|---|---|
| D1 | "Panics … the daemon start below recovers whatever was in flight" | not tests or installs (M1) |
| D2 | Switch: "A switch whose `state.json` cannot be written fails before step 4 and starts the previous generation again" | not when the previous generation was in a crash backoff (M2) |
| D3 | Crashes: "The pending generation gets no retries: its first crash rolls it back" | a `restart` in the 100 ms before the reap retries it (L1) |
| D4 | Confirm: a failed health check's rollback is recorded "in the same step that learns of it" | two lock sections, daemon.rs:753-757 (L10) |
| D5 | Reload: "`port` does not change while the app runs" | it can change mid-switch or mid-restart (L4) |
| D6 | Daemon start 5: "If `app.pid` names an app … stop its process group" | only while the group's leader lives (L5) |
| D7 | Protocol table: `log` with `follow` "instead streams every new event" | it first replays the newest `limit` (default 20) events (daemon.rs:900-907) |
| D8 | Socket protocol: a long request is `usage` | so is a request that takes over 5 s to arrive (daemon.rs:857, 874); undocumented |
| D9 | `status`: "`busy` names the transaction step in progress" | it also names lifecycle and supervisor steps (`start`, `stop`, `reap`, `restart`), `busy.or(moving)` at daemon.rs:837; the TUI section's `⟳ <step>` relies on that |
| D10 | `process.state` is `running`, `stopped` or `failed` | a crash backoff shows as `stopped` and cannot be told apart (L2) |
| D11 | Crates: libc is for "`localtime_r` for the TUI's local clock" | also `proc_pidinfo` (macOS) and `sysconf` (Linux) for the start time (process.rs:68-103); `tzset` is declared by hand (tui.rs:612) |
| D12 | On-disk layout | `install-failed.log` is listed between `logs/install.log` and `logs/app.log`, so `app.log` appears to live under it; it belongs at the `<state_dir>` level, before `generations/` |
| D13 | Stage: "Ids are never reused" | true unless the `staging` save fails and the daemon then dies (daemon.rs:1255 is a best-effort `save`): the next stage gets the same id; negligible, but the sentence is absolute |
| D14 | README "Status": "The first prototype covers `stage`, `switch`, and rollback after a missed confirm deadline" | out of date: test, confirm, lifecycle, prune, reload and the TUI are built |
| D15 | CLAUDE.md lock order `inner → log → hub` | daemon.rs:4-6 has `manifest` in it (section 3) |

The DESIGN.md `Lifecycle`, `Prune`, `Rollback`, manifest, CLI, error-code and TUI sections otherwise match the code
exactly. All 11 error codes in DESIGN.md are the ones the daemon produces, with no extra, and the column drop order
(draw.rs:457-468) matches "Narrow terminals".

---

## 5. Tests

**What exists is good.** Ten end-to-end tests cover the happy path, the missed deadline, the crash before confirm
(whole group), a kill -9 during a pending switch, the crash between intent and spawn, the confirm/crash race,
`rollback_on_unhealthy`, failing and hanging installs, pruning, and busy semantics. 20 unit tests cover validation,
templates, time, durable load, the event log, the hub, the client write timeout, the leftover check, a WAL snapshot
and the TUI. The suite passed in 16 s here.

**Recovery paths still untested,** highest value first. The first four are exactly the code phase 3.2 and 3.3
rewrite:

1. A restore failure: `failed` in `status`, `start` refused, `fawkes rollback` retries and recovers
   (daemon.rs:1781-1799).
2. `state.json` unwritable during a switch, a confirm and the end of a rollback (the three `persist` paths). A
   read-only state directory, or a `state.json.tmp` that is a directory, reproduces it.
3. A crash loop of the permanent generation: backoff, then `failed` after `max_failed_starts`, then `start` clears
   it. Also `stop` during the backoff (L2) and a switch during it (M2).
4. A daemon crash during a test or an install (M1).
5. Manual `fawkes rollback` is covered once (tests/core.rs:481); `rollback` with nothing pending is not.
6. SIGHUP reload end to end: only `Manifest::reload` is unit-tested, not the swap under `inner`.
7. `subscribe` and `log --follow` replay and streaming; the TUI reader against a fake socket.
8. The startup sweep of a cut-short `staging` generation; the `.bak` fallback's `error` event at daemon start;
   `port_in_use`; a switch whose spawn fails (the immediate rollback); the panic hook's event.
9. Linux: `start_time` from `/proc` and `cp --reflink`. There is no CI.

**Flaky or weak:**

- **tests/core.rs:84-89 can fail under load.** `--deadline 2` gives g2 two to three seconds. The test waits up to
  5 s for v2's storage row, then asserts `pending.generation == "g2"`. If node takes about 2 s to start (a busy
  lander), the rollback deletes the row before the wait sees it, or wins the race against the `status` call.
  Commit 516ddc4 fixed the same pattern in the kill -9 test but not here. Fix: `--deadline 5`, and wait for v2 to
  *serve*.
- **tests/core.rs:323 accepts any refusal.** `confirm["code"] == "invalid_state"` also holds for the refusal *after*
  the rollback finished ("g2 was rolled back … stage the ref again"), which the old code produced too. To prove the
  race is closed, assert that the message is one of the two new refusals ("exited … before it was confirmed" or
  "is being rolled back").
- **tests/core.rs:361** `switched.elapsed() < 20` is implied by the `wait(…, 20, …)` before it, so it adds nothing.
  Bound it well below the 120 s deadline instead (say `< 10`), which is what the test means.
- **tests/core.rs:496-547** depends on a 2 s margin: a 4 s deadline against a test that listens after 6 s. It is
  fine today; it becomes the next flake if the sample app starts more slowly.
- **tests/core.rs:286** still uses `pgrep -f "sleep 621"` (phase 4 replaces it with the pid from `status`).

---

## 6. Phase 3: does the plan still fit?

Mostly yes, with one change in order and three additions.

1. **Fix M1 and M2 first, as their own small branch with a core test each.** They are real bugs in today's
   supervision, independent of the refactor, and each is a few lines. Folding them into 3.2/3.3 would make the
   refactor change behaviour and structure at once.
2. **Move phase 4's missing integration tests for these paths ahead of 3.2 and 3.3:** restore failure → `rollback`
   retries, crash loop → backoff → `failed` → `start`, and the `persist` failure paths. PLAN.md says phase 3 runs
   "after phases 1–2 have their tests", but the code 3.2 (`state.txn`, replacing `rolling_back`/`restore_failed()`)
   and 3.3 (the `Live` enum, replacing `app`/`app_failed`/`restart_at`) rewrite is the code with no tests (§5, items
   1-4). Without them, the refactor has nothing to hold it to the current behaviour.
3. **Additions to the rows:**
   - **3.2** should cover the step processes, so a restart knows what to stop (the durable form of M1's fix). It
     should also define the operator exit from a restore that can never succeed (L13, a user decision), and keep
     decision 1a (a daemon start rolls back).
   - **3.3** must keep a backoff across a failed switch (M2) and decide whether `status` shows it (L2, D10). That
     is a protocol change, so it belongs in DESIGN.md with 3.1.
   - **3.4**'s transition table should include `staging`, removal by `prune` and by a failed stage, and the
     restore-retry self-edge of `rolled_back`. L6's `expect` goes with it.
4. **3.5** should keep the two kinds of save distinct: best-effort bookkeeping (`save`, 9 call sites) and the intent
   that fails the step (`persist`, 3 call sites). Today there are 12 hand-written saves, not 14. A single `modify`
   that hides whether a failure counts would undo 1.4.
5. **3.6 is more urgent than when it was planned:** `daemon.rs` is 2,007 lines (1,461 at `bfcac2d`). Do it as the
   *first* commit of phase 3, a pure move with no behaviour change, so the semantic commits of 3.2-3.5 are
   reviewable file by file.
6. **3.1** still fits. Clients index `serde_json::Value` with string keys in about 127 places (main.rs 52, tui.rs 19,
   draw.rs 56, by `grep -c '\["'`). The typed `Status` should carry whatever 3.3 decides about the backoff. **3.7**
   is unchanged.

The Low findings L1, L3, L4, L5, L7, L8, L11 and L14, and the DESIGN.md drift D7-D15, fit in whichever phase touches
those lines. D12 and D14 (layout and README) are one doc commit. The flaky deadline at tests/core.rs:84 should go in
the M1/M2 branch, because the lander runs that test on every landing.

## 7. Suggested order of work

1. M1 and M2 with core tests, plus the tests/core.rs:84 deadline and the sharper assertion at :323. Small.
2. The integration tests for restore failure, crash loop and backoff, and `persist` failures (from phase 4).
3. Phase 3, starting with the 3.6 split, then 3.1, then 3.2-3.5 together, then 3.7.
4. The rest of phase 4 (dynamic ports, lints, CI, docs), alongside.
