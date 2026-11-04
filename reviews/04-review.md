# Fawkes code review 4 (Claude Opus 5.5)

Reviewed at commit `abe54f0` on 2026-11-04, by Claude Opus 5.5 (`claude-opus-5-5`, high effort), on `main` after
phases 1 to 4 of [PLAN.md](PLAN.md) and the commits that followed them (jiff timestamps, clap, the event-driven
daemon in `5cc84f9`, the health-check change in `abe54f0`). The review covers the whole codebase, not a diff: every
file in `src/` and `tests/`, plus `README.md`, `DESIGN.md`, `CLAUDE.md`, `Cargo.toml`, `Cargo.lock`,
`rust-toolchain.toml`, `clippy.toml` and `examples/`. No file outside `reviews/` was changed.

## How this review was made

- **Nine review areas**, one reviewer each, each reading its scope in full: transactional core, supervision and
  processes, Rust semantics and idioms, the DESIGN.md contract (protocol, CLI, manifest), TUI, security, test
  coverage and quality, external dependencies and build, code quality.
- **A /simplify pass in report-only mode** after that: its four angles (reuse, simplification, efficiency,
  altitude) over the whole codebase. It changed no code; its results are in section 6.
- **Adversarial verification.** A skeptic re-read every cited location at HEAD for each area's findings and tried to
  refute each one: already handled elsewhere, intended per DESIGN.md, wrong line, inflated severity. Of 187 raw
  findings, 5 were refuted (section 8) and 21 had their severity corrected. The rest are marked *confirmed* (checked
  by the skeptic) or *plausible* (the code is as cited but the impact was not fully shown).
- **A completeness critic** then looked for what the panel had missed (recent commit diffs, Linux-only paths, files
  nobody had read closely). Its findings were verified the same way and are merged in (ids `critic/X*`).
- **Probes, all outside the repository** (the session scratchpad, its own `CARGO_TARGET_DIR`): `eprintln!` into a
  closed pipe under `panic=abort` (M3), a paced SQLite backup against a concurrent writer (M2), a trickled request
  against `SO_RCVTIMEO` + `read_line` (M6), `create_private_dir` on a symlink (M5), a TestBackend render of the
  gone-selection screen (M9), the trimmed-feature build (L49), and the real debug binary with `FAWKES_SOCKET` set
  (M10). I re-read the code behind M1, M2, M3, M5, M6 and M4 myself at HEAD before writing this up.
- Duplicates reported by several reviewers are merged under one entry, which names every reviewer that found it
  (`area/id`, e.g. `transactions/T1`). 187 raw findings became 125 entries.

## 1. Commands run and verdict

| Command | Result |
|---|---|
| `cargo fmt --check` | clean, exit 0 |
| `cargo clippy --all-targets -- -D warnings` | clean, exit 0 (pedantic is on through `[lints.clippy]`) |
| `cargo test` | exit 0: 50 unit tests pass (4.03 s), 24 integration tests pass (16.09 s), 0 ignored |
| `cargo clippy --all-targets -- -W clippy::nursery` | ~194 warnings: 141 `use_self`, 29 `missing_const_for_fn`, 14 `significant_drop_tightening`, 5 `option_if_let_else`, 5 `or_fun_call` (`map_or`), 2 `derive_partial_eq_without_eq`, 2 `or_fun_call` (`unwrap_or`), single others. Which are worth adopting: I15 |
| `cargo tree -d` | one duplicate: `syn` 2.0.119 and 3.0.9 (I22) |
| `cargo tree -e normal` | 93 crates for the host (fawkes plus 92), 68 of them under ratatui |
| `cargo audit` / `cargo deny` / `cargo llvm-cov` | not installed, so advisories are unchecked (L50) and coverage was mapped by hand (section 4) |

### Verdict

The transactional promise holds. Read adversarially by two reviewers and the critic, no path leaves an upgrade
neither permanent nor rolled back on a single fault, and none rolls back a `rolled_back` generation a second time.
Intents are written before their side effects: `switching` before the old app stops, `pending` and `current`
together before the target spawns. Every confirm refusal is decided under the same `inner` hold as the commit.
`reap`, `decide_overdue` and `judge_unhealthy` record a rollback in the critical section that learns its cause. The
lock order holds on every path read, and slow steps run with `inner` released. The event-driven rewrite in
`5cc84f9` has no lost or spurious wakeup that I or the supervision reviewer could find. The code is clean: no
`unwrap`/`expect` in non-test code, `#[expect]` with reasons instead of `#[allow]`, and four `unsafe` blocks, all in macOS-only
`process.rs` code (two libc calls and the zeroed structs they fill), each with an accurate `SAFETY` comment.

Every 03-review and PLAN item the reviewers checked is fixed, except four that are still open: the `port_in_use`
test (03-review §5 item 8, now part of M12), the orphaned-test confirm hazard (03-review M1, widened in M4), the
`< 20` bound (L40) and cargo-deny (PLAN phase 4, L50).

| Severity | Count |
|---|---|
| High: can break the transactional guarantee, or a hole another local user can exploit | 0 |
| Medium: a real bug in supervision, protocol or TUI, a DESIGN.md violation, or a test gap on a critical path | 14 |
| Low | 61, plus 10 from /simplify |
| Info | 26, plus 14 from /simplify |

The medium findings, in the order I would fix them:

| # | Finding | Kind |
|---|---|---|
| M1 | A save that fails *after* `state.json` was renamed into place (the `.bak` write or the directory fsync) leaves memory on the old state while disk has the new intent. A confirm reported as failed can come back as permanent; with a second fault, code is permanent and storage rolled back. | transaction edge, double fault for the worst case |
| M2 | `fawkes test` copies the live database with a paced backup that restarts on every write by the live app: on a busy database above ~40 MB it never finishes, and holds the step slot. | supervision bug, reproduced |
| M3 | `Daemon::event` uses `eprintln!`, which panics, and so aborts the daemon, when stderr breaks (a dead `tee`, a closed terminal). It also writes stderr while holding `inner`. | supervision bug, reproduced |
| M4 | A test-phase instance that runs a bare `fawkes confirm` makes *another* pending generation permanent; DESIGN.md never tells apps to name themselves or check `FAWKES_PHASE`. | bypass of the mark-good gate |
| M5 | `state_dir` is followed through symlinks and never checked for ownership, while the shipped examples put it (and `home`) in `/tmp`: on multi-user macOS, another user can run code as the daemon's user. | security, needs a second account and the example layout |
| M6 | The documented 5 s request bound is per `read(2)`, not per request, and connections are unbounded: a trickling client holds a thread for days. | DESIGN.md violation, same-uid DoS |
| M7 | Snapshot and restore sleep 10 ms per 100 pages while the app is down: about 2.5 s of extra downtime per 100 MiB on every switch and rollback. | efficiency on the critical path, found by /simplify |
| M8 | With `--json`, several client-side failures print nothing on stdout, and `log --follow --json` ends with a stray `null`. | DESIGN.md violation |
| M9 | In the TUI, when the selected app's daemon leaves, the screen hides every other app and says "waiting for a daemon…". | TUI bug, reproduced |
| M10 | One integration test builds its CLI by hand without removing `FAWKES_SOCKET`, so under a supervised environment its `stage v1` reaches the live daemon. | test isolation, breaks CLAUDE.md's rule |
| M11 | No test restarts a daemon that finds `switching` or `rolling_back` on disk. | test gap, crash recovery |
| M12 | No test runs a *failing* `test`: `test_failed`, `health_failed`, `port_in_use` and "no free spare port" are untested. | test gap, first gate |
| M13 | Four of confirm's refusals, the deadline check among them, have no test. | test gap, commit point |
| M14 | `health.command` checks have no test at all. | test gap |

Below medium, the themes are: the hand-written HTTP health check reads the status line with no size or total-time
bound (L7); several "has the app exited" and "has the deadline passed" checks are written by hand at five or six
sites (L54, L56); one `LastExit` is formatted three different ways (L55); `EventKind` has no `#[serde(other)]`, so
the TUI drops events it does not know (L25); the status during a failed restore still reads as "waiting for confirm"
(L13); and ratatui's default features pull in a second date library (L49).

Housekeeping, seen in passing: an untracked `.README.md.swp` sits in the repository root, and `.gitignore` covers
neither editor swap files nor scratch review files, which CLAUDE.md says must not be committed.

---

## 2. Medium findings

#### M1. Store::modify reports a failed save and keeps the old in-memory state after state.json has already been replaced

*Medium* · confirmed · found by `transactions/T1`

Where: `src/state.rs:264-275`, `src/state.rs:306-312`, `src/state.rs:447-456`, `src/daemon/mod.rs:677-692`, `src/daemon/transactions.rs:670-689`

**Evidence.** `State::save` is `write_durably(path, &bytes)?; write_durably(&backup_path(path), &bytes)` (state.rs:310-311), and `write_durably` returns the directory fsync's error *after* `std::fs::rename(&tmp, path)?` (state.rs:269-274). `Store::modify` does `next.save(&self.path).map_err(Unchanged::Unsaved)?; self.state = next;` (state.rs:453-454), and its doc says "When `change` refuses or the save fails, nothing changed" (state.rs:449). If state.json was renamed into place but the `.bak` write fails (a leftover/unwritable `state.json.bak.tmp`, EIO, ENOSPC on the second file) or the directory fsync fails (some filesystems refuse fsync on a directory fd; speculation as to which), the new intent IS what a restarted daemon reads, while memory keeps the old state and the client is told it failed. confirm's comment relies on the opposite: "a confirm the daemon could not write is not one, and the switch stays pending (a daemon restart would roll it back)" (transactions.rs:670-671).

**Impact.** confirm: state.json says g2 permanent / g1 retired / txn null, memory says pending, the agent gets `internal` "g2 is still pending, confirm again". If the daemon then dies, the next start runs g2 as permanent although the client was told the confirm failed (a later `rollback` gets invalid_state). Double fault: after that partial save the deadline passes and the rollback's own saves fail (disk full): the rollback stops g2 and restores the pre-g2 snapshot, `modify(txn=None)` fails, and a daemon restart then reads state.json = g2 permanent and starts g2 on the restored storage, while events.jsonl says g2 was rolled back. Code is permanent and storage is rolled back: a mixed outcome. The same disagreement happens in stage (id spent on disk, not in memory), the Switching and Pending intents of switch, and abandon_restore; in those, the next `update` usually overwrites state.json and the two agree again.

**Fix.** Treat the save as done once the rename has happened: have `save` return Ok after state.json is in place and report a `.bak` (or directory-fsync) failure separately, e.g. `Result<Option<io::Error>, io::Error>`, which modify turns into an `error` event while still adopting `next`. In other words, memory must always equal what a restarted daemon would read. Add a unit test in state.rs that blocks only `state.json.bak.tmp` (a directory of that name) and asserts that modify's outcome and `Store::load` agree.

#### M2. The test copy of a live database can run forever: the incremental backup restarts on every write by the running app, and nothing bounds the loop

*Medium* · confirmed · found by `deps/DEP1`

Where: `src/storage.rs:27-31`, `src/storage.rs:88-95`, `src/daemon/transactions.rs:345`, `src/daemon/transactions.rs:370`

**Evidence.** `Backup::new(&from, &mut to).and_then(|b| b.run_to_completion(100, Duration::from_millis(10), None))` (storage.rs:29-30). `storage::copy` (storage.rs:88-95) is called from `test` (transactions.rs:370) while the live app keeps running and writing, and `test` holds `Slot::Step` (`let _busy = self.begin(..., Slot::Step)?`, transactions.rs:345). SQLite's backup docs say a backup restarts from page 1 when another connection or process changes the source between steps. rusqlite's `run_to_completion` (rusqlite-0.41.1/src/backup.rs:304-313) loops `More | Busy | Locked => thread::sleep(pause)` with no deadline. I reproduced it in a scratch crate with the same parameters (100 pages, 10 ms): an 8 MB WAL database with another connection inserting every 50 ms was not done after 579 steps and 8 s, and its progress never got past 500 of 2008 pages. `step(-1)` on the same database finished in one call. Snapshot and restore are not affected, because they run after `stop_app` (transactions.rs:516/533, 725/727).

**Impact.** A copy finishes only if pages/100 x (10 ms + step time) is shorter than the gap between the app's writes. At about one write per second that means under roughly 10k pages (about 40 MB). Above that, `fawkes test` never returns and the CLI waits forever, since `test` is not a quick request. The step slot stays taken, so `stage` and `test` answer `busy` until the daemon restarts. An app that holds a long write lock (`locking_mode=EXCLUSIVE`, or a long rollback-journal transaction) also makes it spin on `Busy` forever.

**Fix.** For `copy`, call `Backup::step(-1)` in one go, so the whole copy runs in a single read transaction (in WAL mode it does not block the app's writers). Retry `Busy`/`Locked` only until a deadline (for example `install_timeout` or a fixed bound), then return a `StorageFailed` message that says the database stayed locked. `VACUUM INTO` is an alternative. Add a unit test in storage.rs with a writer thread committing every few milliseconds during `copy`, and assert that it finishes within a time limit.

#### M3. Daemon::event writes with eprintln!, which panics (so the daemon aborts) when stderr breaks, and it runs while `inner` is held

*Medium* · confirmed · found by `rust/S1`

Where: `src/daemon/mod.rs:733-739`, `src/daemon/mod.rs:462-463`, `src/daemon/mod.rs:512-563`, `src/daemon/transactions.rs:609-690`, `src/daemon/supervisor.rs:323`, `Cargo.toml:35-39`

**Evidence.** `event()` starts with `eprintln!("{} {} {} {}", event.at, event.kind.as_str(), ...)` (mod.rs:733). std's `eprintln!` panics on any write error except EBADF (`failed printing to stderr: ...`). Under `panic = "abort"` that aborts the whole daemon. I checked this in the scratchpad: a 4-line program built with `-C panic=abort` that prints to a closed pipe exits 134 with `failed printing to stdout: Broken pipe (os error 32)`, and the same happens with stderr. `event()` is the only print in the daemon, and many callers hold `inner` while they call it: `confirm` (transactions.rs:609 to 690), `reap` (supervisor.rs:323) and `reload_manifest` (mod.rs:525 to 563).

**Impact.** Any stderr that stops accepting writes kills the supervisor at its next event: `fawkes daemon 2>&1 | tee log` after `tee` dies (EPIPE, verified mechanism), or a journald stream that breaks when journald restarts. A daemon started from a terminal that is then closed also aborts at its next event: it gets SIGHUP, which here means a reload (mod.rs:462), and `reload_manifest` logs an event to a hung-up tty, whose write probably fails with EIO (not tested on a real tty). The abort skips `shutdown`, so the app keeps running with no supervisor. Under an init system, the restart then rolls back any pending switch, even a healthy one. Run by hand, nothing enforces the confirm deadline until someone starts a new daemon. There is a second effect: a stderr that blocks (a flow-controlled terminal) stalls every thread that needs `inner`, because `event` writes while it is held. That includes `confirm` and the supervisor.

**Fix.** Treat stderr as best effort: `let _ = writeln!(std::io::stderr().lock(), ...);`, with a comment that the event log is the record. If possible, write to stderr after the caller has released `inner`, for example by returning the line or by printing from the hub. Add a unit test that points `event` at a broken pipe (the test can run with stderr redirected) and checks that it returns.

#### M4. `fawkes confirm` without a generation, called by a test-phase instance, makes a different pending generation permanent; DESIGN tells apps to call it but not to name themselves

*Medium* · confirmed · found by `contract/C3`, also `transactions/T4`, `security/S5` · still open from 03-review M1 (third bullet, for orphaned test apps)

Where: `DESIGN.md:109-115`, `DESIGN.md:305-307`, `src/main.rs:84-88`, `src/main.rs:136-138`, `src/daemon/transactions.rs:345`, `src/daemon/transactions.rs:379-380` …

**Evidence.** DESIGN.md:109-110: "`fawkes confirm [<gen>]`, normally called by the new generation itself once it knows it is healthy (Fawkes gives it `FAWKES_BIN`, `FAWKES_SOCKET` and `FAWKES_GENERATION`)". A test run gets the same variables: `let env = self.app_env(&m, &vars, "test")` (transactions.rs:380), which pushes `FAWKES_SOCKET` and `FAWKES_GENERATION` (mod.rs:805-806). `confirm(None)` takes whatever is pending (`let Some(Txn::Pending(pending)) = ... ` and the id check only runs `if let Some(id) = id`, transactions.rs:628-639), and the liveness check only asks whether the *pending* generation's process runs (transactions.rs:653-669). A test of g4 can run while g3 is pending (test holds only `Slot::Step`). The sample app avoids the trap (it passes `$FAWKES_GENERATION` and checks `phase === 'live'`, examples/sample/app.js:51-54), but DESIGN never asks an app to do either.

**Impact.** An app written to the DESIGN text ("once it knows it is healthy", `$FAWKES_BIN confirm`) confirms during its own test as well, and makes g3 permanent although g3 never vouched for itself. The outcome is still permanent-or-rolled-back, but the mark-good gate that justifies the whole transaction is bypassed. 03-review M1 mentioned the same hazard for orphaned test apps; the live-test case remains.

**Fix.** In the CLI, default `confirm`'s generation to `$FAWKES_GENERATION` when it is set (main.rs:136-138), so a generation can only confirm itself unless it names another. In DESIGN.md's Confirm section, say that the app confirms with `"$FAWKES_GENERATION"` and only when `FAWKES_PHASE=live`. Optionally refuse a confirm whose explicit generation is not the pending one (already done) and log which generation confirmed. Add a core test: a test-phase app that runs `confirm` with no argument while another generation is pending is refused.

#### M5. state_dir is followed through symlinks and never checked for ownership, and the shipped examples put state_dir and home in world-writable /tmp

*Medium* · confirmed · found by `security/S1`

Where: `src/daemon/mod.rs:598-606`, `src/daemon/mod.rs:353-354`, `src/manifest.rs:251-254`, `src/state.rs:44-46`, `src/daemon/supervisor.rs:352-360`, `src/daemon/transactions.rs:379-388` …

**Evidence.** `create_private_dir` is `DirBuilder::new().recursive(true).mode(0o700).create(dir).and_then(|()| std::fs::set_permissions(dir, Permissions::from_mode(0o700)))`. std's recursive create returns Ok when the path already `is_dir()`, and that check follows symlinks. `set_permissions` is chmod(2), which also follows them. Nothing compares the owner with the euid. A scratch probe of this exact code on a symlink `link -> real` returned `Ok(())` and chmodded `real` to 0700. `Paths { root }` keeps the unresolved path (`base.join(dir)`, manifest.rs:252), and so does every generation's `dir` in state.json. As a result each later `spawn` cwd (`g.dir`, supervisor.rs:354-356 and transactions.rs:384-386), `git worktree add`, snapshot, `test-home` and state.json write resolves the path again. The sample manifest and DESIGN.md's example use `state_dir = "/tmp/fawkes-sample"` and `home = "/tmp/sample-home"`, and `start_live` runs `std::fs::create_dir_all(&m.home)` with no check at all.

**Impact.** On a multi-user macOS host (macOS has no `fs.protected_symlinks`), another local account can pre-create `/tmp/fawkes-sample` as a symlink it owns. Before the victim's first daemon start (and again after each reboot cleans /tmp), the symlink points at any directory the victim owns, so the chmod succeeds and startup passes. Because the attacker owns the symlink in sticky /tmp, it can repoint it at an attacker-owned directory later. From then on: (1) the next `start_live` or `test` uses `<state_dir>/generations/gN/code` as cwd and runs `node app.js` from the attacker's tree, which is code execution as the victim; (2) the next switch writes its storage snapshot (a copy of the app's SQLite) and the next test its `test-home` copy into the attacker's directory. A pre-created `/tmp/sample-home` (or a symlink there) likewise gives the attacker the live database, or redirects where restores write. A pre-created plain directory is refused, because the chmod fails with EPERM. The default `~/.fawkes/<name>` is not affected, which is why this is medium rather than high: it needs a second local account (or a compromised service account) and the /tmp layout of the examples.

**Fix.** At startup, before the chmod: `symlink_metadata` the state_dir and refuse it if it is a symlink or if its `st_uid` is not `geteuid()` (rustix `fs::lstat` and `process::geteuid`). Then use `std::fs::canonicalize(state_dir)` as `Paths.root`, so swapping a path component later cannot redirect anything. Apply the same owner check (or a loud `error` event) to `home`. Move `examples/sample/fawkes.toml` and DESIGN.md's example off /tmp (keep paths short for the socket, e.g. the default `state_dir` and `home = "sample-home"`). In DESIGN.md → Daemon start, state that the directory must be owned by the daemon's user and is not followed through a symlink. Add a unit test that a symlinked or foreign-owned state_dir is refused.

#### M6. The 5 s request bound is per read(2), not per request: a trickling client holds a daemon thread for days, and connections are unbounded

*Medium* · confirmed · found by `security/S2`, also `contract/C5`, `supervision/S4`, `rust/S8`

Where: `src/daemon/server.rs:75-81`, `src/daemon/mod.rs:465-469`, `DESIGN.md:390`

**Evidence.** `stream.set_read_timeout(Some(Duration::from_secs(5)))` sets SO_RCVTIMEO, which bounds each `recv`. `BufReader::new(s.take(MAX_REQUEST)).read_line(&mut line)` keeps calling `recv` as long as each call gets a byte within 5 s. DESIGN.md:390 promises: "A request line longer than 64 KiB, or one that has not arrived whole within 5 s, is refused with `usage`." A scratch probe with the same setup and a 1 s timeout, fed one byte every 600 ms, returned `Ok(21)` after 12.1 s instead of refusing. The accept loop spawns an unbounded `thread::spawn(move || daemon.handle(stream))` per connection.

**Impact.** Any client of the socket (the owner's processes, including every app generation, which gets `FAWKES_SOCKET`) can hold a thread and a file descriptor for up to 64 KiB × 5 s ≈ 91 h per connection, and open as many such connections as it likes until the daemon runs out of fds or threads. Once it does, `accept` fails, and the `confirm` the pending generation needs cannot get through, so the switch rolls back at its deadline. The socket is 0600, so this is self-DoS (a buggy app or agent) rather than an attack by another user. It is still a mismatch with the documented contract.

**Fix.** Compute `let until = Instant::now() + 5 s` and read in a loop (`fill_buf`/`consume`, or 1 KiB reads into a `Vec`), calling `set_read_timeout(until - now)` before each read and refusing when the time is past. Optionally cap concurrent non-streaming connections (an `AtomicUsize` gate that answers `busy` past, say, 64). Add an integration test that trickles a request past 5 s and expects `usage`, and one that sends 64 KiB + 1 bytes.

#### M7. The storage snapshot and restore sleep 10 ms per 100 pages while the app is down

*Medium* · confirmed · found by `efficiency/E1`

Where: `src/storage.rs:29-31`, `src/storage.rs:70-84`, `src/daemon/transactions.rs:514-536`, `src/daemon/transactions.rs:724-727`

**Evidence.** `Backup::new(&from, &mut to).and_then(|b| b.run_to_completion(100, Duration::from_millis(10), None))` (storage.rs:30). In rusqlite 0.41.1, `run_to_completion` calls `thread::sleep(pause_between_pages)` after every `More` step (backup.rs:304-313). `switch` stops the old app first (`self.stop_app(app)`, transactions.rs:516) and only then calls `storage::snapshot` (:533). `rollback_pending` stops the target (:725) and then calls `storage::restore` (:727). Both run while no app serves and nothing else writes the database. The pause lets other connections in during an online backup, and in these two places there are none.

**Impact.** With 4 KiB pages, 100 pages is 400 KiB per step. A 100 MiB database adds about 2.5 s of pure sleep to every switch and every rollback, and a 1 GiB one adds about 26 s, all of it downtime of the live port. The cost grows linearly with the storage, and nothing is gained for it.

**Fix.** For `snapshot` and `restore`, where the app is stopped, copy in one step: `loop { match b.step(-1)? { Done => break, Busy | Locked => sleep(short), More => {} } }`. `run_to_completion` asserts pages_per_step > 0, so the loop calls `step` directly. Keep paced steps, or a much larger step, only for `storage::copy` in `test`, which reads the live database. Even there, under WAL a single `step(-1)` avoids the restart-on-write behaviour of a paced backup.

#### M8. With --json, several client-side failures leave stdout empty, and `log --follow` writes more than one document

*Medium* · confirmed · found by `contract/C2`

Where: `src/main.rs:191-196`, `src/main.rs:250-256`, `src/main.rs:259-262`, `src/main.rs:264-269`, `src/main.rs:324-331`, `src/main.rs:277-286` …

**Evidence.** DESIGN.md:364-365: "With `--json`, stdout carries exactly one JSON document, the `result` or the `error` object", and `daemon_unreachable` is "produced by the client only" (DESIGN.md:404), so client-made errors are meant to be JSON as well. Only the connect failure honours this (main.rs:239-244). These paths print plain text to stderr and nothing to stdout even with `--json`: a manifest that cannot be read (`eprintln!("error: {message}"); return ExitCode::from(EXIT_USAGE)`, main.rs:194-195); a write that fails (`"error: the daemon on {} closed the connection"`, exit 3, main.rs:251-255); `set_read_timeout` failing (main.rs:260); the 30 s timeout (`"error: the daemon did not answer in time"`, main.rs:267); and a daemon that closes without an answer (main.rs:327-331). For `log --follow --json` every event goes to stdout as its own document (main.rs:283, 294), and when the daemon ends the stream (shutdown, or a subscriber dropped for falling 1024 behind) the final `{"ok":true,"result":null}` prints a trailing `null` (main.rs:302-303).

**Impact.** An agent that runs `fawkes --json confirm` (the app's own confirm) or `fawkes --json status` and parses stdout gets an empty string, not an error object with a `code`, exactly in the cases a supervisor wrapper has to handle: a hung or dying daemon. Exit 3 for a dropped connection comes without the `daemon_unreachable` object that exit 3 otherwise carries.

**Fix.** Route every client-side failure through one helper that prints `{"code","message"}` on stdout under `--json` (text on stderr otherwise). Use `daemon_unreachable` for connect, write and closed-without-answer, and add a code for the timeout (for example `no_answer`; add it to `ErrorCode` and the DESIGN.md list, without renaming any). Document `log --follow --json` as a stream of event documents (one per line), and don't print the `null` result of an ended stream in either mode. A main.rs unit test can cover the timeout path with `--json`, next to `a_quick_command_gives_up_on_a_daemon_that_never_answers`.

#### M9. When the selected app leaves, the screen hides every other app and shows 'waiting for a daemon…'

*Medium* · confirmed · found by `tui/T1`

Where: `src/tui.rs:224-235`, `src/tui.rs:284-286`, `src/tui/draw.rs:44-56`, `src/tui/draw.rs:76-79`, `src/tui.rs:433-444`, `src/tui.rs:887-893`

**Evidence.** `Msg::Gone(key) => self.apps.retain(|a| a.key != key)` leaves `current = Some(<gone key>)`, so `current_index()` is None (by design: 'An app that left the list stays selected (nothing is)'). But `draw` treats `app.current() == None` as 'no apps at all': `} else { table = take(u16::MAX); }` and later `Paragraph::new(" waiting for a daemon…")`. I confirmed this with a scratch copy (not the repo) and a TestBackend probe: three apps, select `bbb`, then `Gone("bbb")`. The screen is the header ' fawkes  2 apps', then ' waiting for a daemon…', then an empty body. The two connected apps are not drawn. With `Focus::Generations`, `↓` goes to `select()`, which needs `current_index()`, so nothing happens. Only after a Tab do the arrows pick an app again.

**Impact.** In discovery mode, stopping the app you are looking at (from the CLI or another TUI) makes the whole screen look as if no daemon answers. The header still counts the other apps. Nothing on screen says to press Tab and then an arrow. Other apps' countdowns, including a pending confirm on another app, are hidden until the operator works this out.

**Fix.** In `draw`, use `app.apps.is_empty()` to decide whether to show 'waiting for a daemon…'. When the selection is gone but apps remain, draw the apps table with `selected = None` and a one-line detail such as '<key> stopped; ↑/↓ picks another app'. Let `move_selection` fall back to `step_app` while `current_index()` is None. First add a render test for the Gone-while-selected case: the existing assertion at tui.rs:891-893 checks the state but never draws it.

#### M10. One integration test runs the CLI without removing FAWKES_SOCKET, so its `stage` can reach a live daemon

*Medium* · confirmed · found by `tests/T1`, also `reuse/R1`

Where: `tests/core.rs:540-547`, `tests/common/mod.rs:245-254`, `src/main.rs:212-221`, `src/daemon/mod.rs:805`

**Evidence.** `a_daemon_killed_during_a_stage_leaves_no_install_running` builds its own client: ``` let mut stage = std::process::Command::new(common::BIN)     .args(["--json", "--manifest"])     .arg(h.dir.join("fawkes.toml"))     .args(["stage", "v1"]) ``` It has no `.env_remove("FAWKES_SOCKET")`. `Harness::cli` has one ("deaf to a `FAWKES_SOCKET` of the environment the tests run in"), and so does `a_usage_error_exits_2_and_help_exits_0` (core.rs:1382). `socket_path` checks `--socket`, then `FAWKES_SOCKET`, then the manifest, so `--manifest` does not win over the variable. I confirmed this with the real binary: `FAWKES_SOCKET=<scratch>/not-a-daemon.sock fawkes --json --manifest <scratch>/none.toml status` answered `daemon_unreachable` on the FAWKES_SOCKET path and exited 3.

**Impact.** Fawkes sets FAWKES_SOCKET for every app it supervises, and every process the app starts inherits it. `.ultimate/settings.json` runs `cargo test` as a lander check from inside Ultimate, the first user (DESIGN.md: examples/ultimate.toml). When Ultimate runs under Fawkes, this test sends `stage v1` to the live Ultimate daemon. That takes its step slot, and if the repo has a `v1` ref it runs `pnpm install` and records a generation. The test then times out waiting for `install.pid`. This breaks CLAUDE.md's rule: never point a test at a live app.

**Fix.** Make the harness the only way tests build a client: expose `Harness::cli()` (or a `Harness::spawn(args)` that returns the `Child`) and use it here. As defence in depth, also `env_remove("FAWKES_SOCKET")` in `daemon_command`, and say in CLAUDE.md that tests never build a `Command::new(BIN)` themselves.

#### M11. No test covers a daemon start that finds `switching` or `rolling_back` in state.json

*Medium* · confirmed · found by `tests/T2`, also `transactions/T2`

Where: `src/daemon/mod.rs:421-437`, `src/daemon/transactions.rs:519-530`, `tests/core.rs:908-951`

**Evidence.** Startup recovery is one `match txn`. Its `Some(Txn::Pending(_))` arm is pinned twice (core.rs:142, core.rs:204), and `None` runs in nearly every restart. `Some(Txn::Switching { .. })`, which logs "the switch to … was cut short" and starts the permanent generation, has no test. Neither does `Some(Txn::RollingBack { reason, .. })`, which retries a restore that failed or a rollback that was under way. The SIGTERM test does not reach the `Switching` arm, because the switch's `resume` clears the step before the daemon exits (`daemon.update(&mut inner.store, |state| state.txn = None)`), so the next start sees `None`. Both failed-restore tests (core.rs:769, 1012) retry with `fawkes rollback`, never with a daemon restart.

**Impact.** DESIGN.md (Daemon start 7, Rollback) promises that the next daemon start retries a failed restore and recovers a cut-short switch. These are the recovery paths after a daemon crash, the case where the transactional promise relies on code that runs only at startup. A regression there (a wrong reason, starting the app on unrestored storage, or rolling back a target that never ran) would pass the suite.

**Fix.** Add two core tests. (1) With a `slow-stop` g1, SIGKILL the daemon (`kill_daemon`) once "stopping g1" is logged, then start it again: expect the `daemon` event "cut short", g2 still `tested`, g1 serving, `pending` null. (2) Get to a failed restore (move `snapshot/` away as at core.rs:825-828), `kill_daemon`, put the snapshot back, `start_daemon`: expect g2 `rolled_back` with its first reason, v2's row gone, g1 serving. A third case could restart with the snapshot still missing: the daemon stays up, `status` shows `failed`, and an `error` event is logged.

#### M12. No test runs a failing `test`: `test_failed`, `health_failed`, `port_in_use` and "no free spare port" are all untested

*Medium* · confirmed · found by `tests/T3`

Where: `src/daemon/transactions.rs:356-366`, `src/daemon/transactions.rs:410-461`, `examples/sample/app.js:7`, `examples/sample/app.js:28`

**Evidence.** Every `test` call in tests/core.rs passes. Nothing in the suite contains `test_failed`, `health_failed`, `port_in_use` or `start_failed`. The sample app's `crash` mode ("exits with status 3 straight away, also in a test") is never committed by any test, so it is dead. The paths behind it have no test: `state.transition(id, GenState::TestFailed)`, the `TestFailed` event, `err(ErrorCode::HealthFailed, "… failed its test …")`, the `PortInUse` branch that leaves the generation unchanged, and the StartFailed "no free spare port". The `allowed` table test in state.rs covers the edges, but not the daemon's `test` command writing them.

**Impact.** The test step is the first safety gate of the transaction: a generation that fails it must not be switchable. A regression that records a failed test as `tested`, maps it to the wrong error code, or wrongly blames the generation in the port-in-use case would ship unnoticed. DESIGN.md's retest edges (`test_failed → tested`, `tested → tested`) also have no end-to-end test.

**Fix.** Add one core test: commit v1 `crash` and stage and test it. Expect exit 1 with `health_failed`, state `test_failed`, a `test_failed` event, and `switch g1` refused with `invalid_state` and "failed its last test". Make the failure come and go with a marker file in `{home}`, as the crash-loop test does, then `test g1` again and expect `tested`. For `port_in_use`, a `start` that in the test phase spawns a listener on `{port}` in a group of its own (and records its pid for cleanup) before exiting.

#### M13. Four of confirm's refusal branches have no test, the deadline check among them

*Medium* · confirmed · found by `tests/T4`

Where: `src/daemon/transactions.rs:620-625`, `src/daemon/transactions.rs:628-650`, `src/daemon/transactions.rs:653-669`, `src/daemon/supervisor.rs:656-684`

**Evidence.** Two refusals are pinned: confirm during a failed restore (core.rs:832-839) and confirm while rolling back (core.rs:1277; core.rs:312 accepts either message). These have no test: - `Some(Txn::Pending(pending)) if now_secs() >= self.deadline_secs(pending)` → "missed its confirm deadline" - `None => invalid_state("no switch is pending; there is nothing to confirm")` - the wrong-generation branch (`{id} is {state}, not pending`) - `"{} is not running, so it cannot be confirmed"` (a stopped pending generation)  DESIGN.md Rollback says "A confirm after the deadline is refused, even if the rollback has not begun yet". Lifecycle says a stopped pending generation "cannot be confirmed while stopped".

**Impact.** Confirm is the commit point. If the deadline check or the not-running check were dropped, a confirm landing in the up to 1 s gap between the deadline and the supervisor's `decide_overdue` would make a late generation permanent, and so would a confirm for a stopped one. No test would fail.

**Fix.** These are millisecond unit tests with the existing `scratch_daemon` in supervisor.rs. Set `Txn::Pending` with a deadline in the past and assert `confirm(None)` is `invalid_state` with "missed its confirm deadline". Do the same for no txn, for `Some("g9")`/`Some("g1")` while g2 is pending, and for `Live::Stopped` with g2 pending. End to end, `stop` a pending generation and `confirm` it (with `--deadline 120`).

#### M14. `health.command` checks have no test at all

*Medium* · confirmed · found by `tests/T5`, also `supervision/S1`

Where: `src/process.rs:311-334`, `src/manifest.rs:350-355`, `src/manifest.rs:382-384`, `tests/common/mod.rs:99-102`

**Evidence.** The harness manifest always uses `[health] url = …`, and no unit test calls `run_check` or `wait_healthy` with `Check::Command`. Untested: the command run in its own group with `[env]` and FAWKES_*, `COMMAND_TIMEOUT` of 10 s followed by `kill_group(child.id())`, and the shell-quoted expansion in `health_check` (`Check::Command(vars.expand_command(command))`). The manifest rule "[health] needs exactly one of url or command" is not among the `invalid_manifests_are_refused_naming_the_field` cases either.

**Impact.** DESIGN.md offers the command check as an equal alternative to the URL (`curl -fsS …`). A regression in that path, such as a hanging check that never times out, a group that leaks, or a wrong environment, would hold `test` until `health.timeout`, or keep a pending generation unhealthy with no test catching it.

**Fix.** Unit-test `run_check` with `exit 0`, `exit 1` and a command that starts `sleep 30 &` and then `sleep 30`; make the timeout injectable or a parameter so the test stays short, and assert the group is gone. Add one core test whose manifest uses `command = "curl -fsS http://127.0.0.1:{port}/"` (or `nc -z`) for test and live. Add the both/neither cases to the manifest table.

## 3. Low and info findings, by area

Each entry gives the verified severity, who found it and the verdict of its skeptic. Duplicates found by several reviewers are merged under the first one.

### 3.1 Transactions, state and storage

#### L1. A test's storage copy can race the supervisor's restore and copy a missing, empty or mixed storage

*Low* · plausible · found by `transactions/T3`

Where: `src/daemon/transactions.rs:346`, `src/daemon/transactions.rs:370-371`, `src/daemon/supervisor.rs:153`, `src/daemon/supervisor.rs:174-176`, `src/storage.rs:19-32`, `src/storage.rs:88-95`

**Impact.** A test that starts while an automatic rollback restores storage can fail with `storage_failed` (the file was removed between `exists()` and the open). It can also copy an empty database (opened in the window after the create and before the backup's first step). With several storage files it can copy some from before the restore and some from after. The generation can then be marked `tested` after running on storage that never existed. The live data itself is never at risk.

**Fix.** Have `test` hold the live slot for the duration of `storage::copy` only (a short `begin("test copy", Slot::Live)` or a dedicated storage mutex taken by copy, snapshot and restore). Otherwise, document in DESIGN.md that a test's storage copy may coincide with an automatic rollback.

**Verifier.** The cited code is correct. `test` takes only `Slot::Step` and the supervisor's automatic rollback takes only `Slot::Live`. Running them concurrently is intended (supervisor.rs:146-147, and the test busy_steps_refuse_others_but_never_delay_an_automatic_rollback). `restore` -> `backup_file` runs `remove_database(dst)` and then `Connection::open(dst)`, while `copy` checks `exists()` and then opens the source read-only, with no lock shared between them. So a copy taken during a restore can fail with storage_failed, or can read files from before and after the restore. That needs a test and an automatic rollback in the same instant, and I did not reproduce it. The empty-copy case depends on SQLite locking timing, and the dst write lock may make the reader see BUSY instead. Live data is never written by copy, so low is right.

#### L2. The snapshot treats `Path::exists() == false` as "absent", which a restore turns into deleting the live database

*Low* · confirmed · found by `rust/S6`

Where: `src/storage.rs:53-60`, `src/storage.rs:76-82`, `src/storage.rs:90`

**Impact.** Speculative, but the outcome is data loss. Suppose a storage path cannot be examined at snapshot time (a parent directory with transient permissions, an I/O error on a network mount) and becomes readable before the rollback. The restore then deletes the real database instead of putting it back. The snapshot should have failed (`storage_failed`, nothing changed), not recorded "absent".

**Fix.** Use `source.try_exists().map_err(|e| format!("cannot examine {}: {e}", source.display()))?`. Then only a real NotFound is recorded as absent, and any other error fails the switch before its point of no return. Use the same in `copy`.

#### L3. On reopen, the event log is dropped whole when the file is not valid UTF-8, and `seq` numbers then repeat

*Low* · confirmed · found by `rust/S5`

Where: `src/state.rs:517-526`, `src/state.rs:559-565`

**Impact.** One torn line empties the in-memory tail (`log`, the `subscribe` replay) for that whole file. `last_seq` then falls back to the rotated file's maximum, or 0, so new events reuse `seq` numbers that are still in `events.jsonl`. `replay(since)` and the TUI's resume check (net.rs:84) then compare against duplicated numbers. A partial line without a newline also absorbs the next appended event, so both are lost.

**Fix.** Read the bytes (`std::fs::read`), split them on `b'\n'` and parse each line with `serde_json::from_slice`, skipping bad lines, so a damaged line costs only itself. Before the first append, write a `\n` if the file does not end with one. Add a unit test with a file that holds a truncated multi-byte character between two valid lines.

#### L4. Failed-restore errors never point at `rollback --keep-storage`, and the rollback's own error does not say how to retry

*Low* · confirmed · found by `transactions/T5`

Where: `src/daemon/transactions.rs:742-747`, `src/daemon/transactions.rs:29-32`

**Impact.** An agent facing a snapshot that is gone retries `fawkes rollback` forever while the app stays down, because no message it receives mentions the documented way out. CLAUDE.md asks that error messages say what to do next.

**Fix.** Append "; `fawkes rollback` retries the restore, or `fawkes rollback --keep-storage` starts <permanent> on the storage as it is (it may already be migrated)" to both messages, and assert the hint in the failed-restore test.

#### L5. A test that fails before its process starts leaves `test-home/` (a copy of the live storage) behind

*Low* · confirmed · found by `contract/C11`

Where: `src/daemon/transactions.rs:367-395`, `src/daemon/transactions.rs:409`, `DESIGN.md:574`

**Impact.** A full copy of the app's databases stays on disk until the next test of that generation, or until the generation is pruned. The state dir is 0700, so this costs disk space, not confidentiality.

**Fix.** Remove `test_home` on every way out of `test`, for example with a small drop guard created right after `create_dir_all`.

#### L6. A SIGTERM during a rollback, or during startup recovery, logs `failed` for the permanent generation and answers the rollback ok although nothing was started

*Low* · confirmed · found by `critic/X4`

Where: `src/daemon/supervisor.rs:339-342`, `src/daemon/supervisor.rs:428-433`, `src/daemon/transactions.rs:771-791`, `src/daemon/transactions.rs:822-830`, `src/daemon/mod.rs:420-436`, `src/daemon/mod.rs:500-505` …

**Impact.** The outcome is benign, because the next daemon start runs the permanent generation, but the record is misleading. The event log, which agents read, gets a `failed` event for the healthy permanent generation with the message "the daemon is shutting down". `fawkes rollback` exits 0 and says it is back to g1 while nothing runs, and a TUI shows the app red and failed until the daemon is gone. A SIGTERM that arrives during a long startup rollback (stop_timeout plus restore) has the same effect.

**Fix.** Treat the shutdown refusal as a stop, not a failure. In `start_or_give_up`, leave `live` Stopped and log a `daemon` event ("not started: the daemon is shutting down; its next start runs g1") when `shutting_down()` holds, or check it at the call sites the way `resume` does. Mention the case in DESIGN's Rollback or shutdown text.

#### I1. restore does not sync the home directory before the end of the rollback is durably recorded

*Info* · plausible · found by `transactions/T6`

Where: `src/storage.rs:71-85`, `src/storage.rs:19-45`, `src/storage.rs:64-66`, `src/daemon/transactions.rs:727`, `src/daemon/transactions.rs:751-754`

**Impact.** On a filesystem that does not order metadata (most journaling filesystems, APFS and ext4 among them, do commit the unlink and create along with the later fsync, so this is theoretical), a power loss right after the rollback could leave state.json saying the rollback finished while home shows the failed generation's files or none.

**Fix.** After a successful restore, fsync each restored file's parent directory under `home` (open the directory and `sync_all`) before returning. This costs one call per storage directory.

**Verifier.** Accurate: restore unlinks and recreates the database files through SQLite and never fsyncs the home directory. Snapshot does sync its directory through write_durably(index.json). As far as I know, SQLite's unix VFS fsyncs the directory only for journal and WAL files it creates (isCreate with the main-journal or WAL type), not for a new main database file, which supports the speculation. The impact is theoretical on journaling filesystems, and the finding says so. A related observation: on macOS, SQLite's default commit sync is plain fsync, not F_FULLFSYNC, while Rust's sync_all uses F_FULLFSYNC, so the asymmetry also covers file contents. info is right.

### 3.2 Supervision and processes

#### L7. `health.timeout` is overshot by up to one interval plus one check, and a status line sent in slow pieces is never bounded

*Low* · confirmed · found by `supervision/S3`, also `rust/S9`, `security/S4`, `deps/DEP5`

Where: `src/process.rs:284-307`, `src/process.rs:345-362`, `src/process.rs:351-360`

**Impact.** DESIGN.md:276 says `timeout` is the "seconds until `test` gives up". With `timeout = 5, interval = 30`, the second try runs at about 30 s and `unhealthy` comes about 30 s late. The early rollback of `rollback_on_unhealthy` is delayed by the same amount. The slow-drip case requires a pathological app, but it would hold the step slot for ever during `test`.

**Fix.** Compute `left = give_up - now` and wait `interval.min(left)` between tries. Give `http_get` one deadline for the whole attempt: set each read timeout to the time remaining, and use `take(1024)` on the reader so a status line cannot grow without limit. Optionally pass the remaining time to `run_check` instead of the fixed `COMMAND_TIMEOUT`.

#### L8. `run_check` waits without a bound after SIGKILL, and leaves the rest of a successful check's group running

*Low* · confirmed · found by `supervision/S2` · still open from 03-review L7 (same pattern, different call site)

Where: `src/process.rs:325-333`

**Impact.** A health command stuck in uninterruptible sleep (an NFS or FUSE `stat`, say) blocks the health thread for ever. During `fawkes test` that thread is the step thread, so the step slot stays taken and every later stage, test and switch gets `busy` until the daemon restarts. A health command that backgrounds something (`curl … & wait -n`) leaves that process running after each passing check.

**Fix.** Replace both arms with `stop_group(child.id(), 0.0, Some(&mut child))`, as `run_bounded` does. This bounds the wait by `KILL_WAIT` and ends whatever the check left in its group.

#### L9. The leaderless-group branch of `leftover` can match a pgid reused after the app's group ended, and startup never removes a stale `app.pid`

*Low* · confirmed · found by `supervision/S5`, also `security/S6`, `supervision/S8` · still open from 03-review L5 (fixed for the common case; this residual edge is new)

Where: `src/process.rs:192-214`, `src/daemon/mod.rs:400-413`, `src/daemon/supervisor.rs:486-488`

**Impact.** In rare cases a new daemon SIGTERMs and then SIGKILLs an unrelated process group of the same user. This needs a stale `app.pid` (the daemon died and the app then ended unsupervised), a pid wrap-around that lands on that pid, and a leaderless group at the moment of the check. Another user's group is safe: `test_kill_process_group` gets EPERM, so `group_alive` is false. DESIGN.md:209-214 says a reused pid "is left alone", which this branch does not guarantee.

**Fix.** Remove `app.pid` at startup once `leftover` has been acted on, whether the group was stopped or the file was stale. This narrows the window to the gap between the daemon's death and its next start. For a stronger check, compare the members' start times against the file's mtime: on macOS `proc_listpids(PROC_PGRP_ONLY, pgid)` plus `proc_pidinfo`, on Linux the `/proc/*/stat` field 5. Alternatively, record a nonce in the app's environment and check it.

#### L10. `git`, `cp` and health commands are not tracked for shutdown or crash recovery, and `run_bounded` joins its readers without a bound

*Low* · confirmed · found by `supervision/S6` · still open from 03-review L8 (now bounded, but still not covered by shutdown or startup recovery)

Where: `src/daemon/transactions.rs:72-98`, `src/daemon/transactions.rs:212-224`, `src/daemon/transactions.rs:296-307`, `src/daemon/mod.rs:588-590`, `src/process.rs:314-324`

**Impact.** After SIGTERM or a crash in the middle of a stage, an orphaned `git` with a hanging hook, or a long `cp`, keeps running with no timeout, and the next daemon's sweep deletes the directory it writes into. The unbounded join can hold the step slot for ever despite the "bounded" promise in the doc comment (:69-71).

**Fix.** Register the `run_bounded` group in `step_pgids`, and give it the `step.pid` wrapper, or a pgid file of its own, so that shutdown and the next start stop it the way they stop an install. Join the readers with a deadline: hand each reader a channel and `recv_timeout` it, abandoning the reader after `KILL_WAIT`.

#### L11. Graceful shutdown does not stop a stage or test from spawning after `step_pgids` was drained

*Low* · confirmed · found by `supervision/S7`

Where: `src/daemon/mod.rs:569-593`, `src/daemon/transactions.rs:296-307`, `src/daemon/transactions.rs:345-395`

**Impact.** After a SIGTERM, a test app can survive the daemon, holding a spare port and carrying `FAWKES_SOCKET`/`FAWKES_GENERATION`. `step.pid` names it, so a daemon that starts again stops it. A daemon that is stopped for good (an uninstall, or a manual `fawkes daemon` run) leaves it running. The window is small: the time between the drain and the process exit, a save and an event.

**Fix.** In `spawn_step`, take `step_pgids`, check `shutting_down()` and refuse the spawn under that lock, and push the pgid before releasing it. `shutdown` then sets a "draining" state under the same lock, so a spawn either lands in the list before the drain or is refused.

#### L12. The run-link check in `register` is check-then-act: two same-name daemons that start together can both run, and one loses its link

*Low* · confirmed · found by `critic/X6`

Where: `src/daemon/server.rs:13-29`, `src/daemon/mod.rs:315-329`, `src/daemon/mod.rs:366`, `DESIGN.md:478`

**Impact.** DESIGN.md:478 promises that "Two daemons with the same `name` are refused at start". In this race both run, and A cannot be found by `fawkes tui` discovery for as long as it lives. If B stops first, it removes its link and neither daemon is listed. The outcome is benign but breaks the contract, and it is most likely when an init system starts several units at once.

**Fix.** Decide name uniqueness in the kernel, as the state lock does. Take a non-blocking `flock` on `<fawkes_home>/run/<name>.lock` before the check and hold it for the daemon's life, then the check and the relink cannot interleave. Add a test that starts two same-name daemons and expects exactly one to refuse.

#### I2. `Out::closed` cannot see a client close when the client sent anything after its request line, so the stream thread lives until the next write fails

*Info* · confirmed · found by `critic/X7`

Where: `src/protocol.rs:665-677`, `src/daemon/server.rs:76-81`, `src/daemon/server.rs:153-158`

**Impact.** On an idle daemon (no app, no events) a `subscribe` or `log --follow` client that writes a stray byte (a second newline, a keep-alive) and then disconnects leaves a daemon thread and a descriptor behind indefinitely. This adds to the unbounded-clients cost in supervision/S4. Only a same-user client can do it (socket 0600).

**Fix.** After the request line, treat any further input on a streaming connection as the end of it. For example, in `closed()` drain with a non-peeking `recv` and count EOF, or report `n > 0` as closed (the protocol allows one request per connection). Document the rule in DESIGN's socket section.

#### I3. Small style and simplification items in scope

*Info* · confirmed · found by `supervision/S10`

Where: `src/daemon/server.rs:78-81`, `src/daemon/supervisor.rs:468-474`, `src/daemon/supervisor.rs:495-515`, `src/daemon/mod.rs:74-75`, `src/daemon/supervisor.rs:656-684`, `src/daemon/mod.rs:370-383`

**Impact.** Readability and consistency only.

**Fix.** Fold these into the /simplify pass. None changes behaviour.

### 3.3 Protocol, CLI and the DESIGN.md contract

#### L13. status keeps `pending` (and `current`) on a generation that is already rolled back, so the CLI and TUI tell the operator to confirm during a rollback or a failed restore

*Low* · confirmed, severity medium → low by the verifier · found by `contract/C1`

Where: `src/state.rs:322-328`, `src/state.rs:411-416`, `src/daemon/supervisor.rs:533-543`, `src/daemon/supervisor.rs:504-514`, `src/main.rs:459-461`, `src/tui.rs:176-181` …

**Impact.** After a failed restore the operator needs `fawkes rollback` (or `--keep-storage`), but the screen and `fawkes status` say `waiting for confirm` / `confirm g2 within 00:00`, and an agent that reads `status.pending` concludes a switch awaits its confirm. This is the one failure state where misleading guidance costs the most: the app is down until someone acts.

**Fix.** Make the status say what the step is. Either publish `pending` only for `Txn::Pending` and add a `rollback` field (`{generation, reason, restore_failed}`) for `Txn::RollingBack`, or add a `step` field (`pending` | `rolling_back` | `restore_failed`) beside `pending`. Document it in DESIGN.md's `status` section (this is a protocol addition, so add a field, don't rename one). Then have the TUI show `✘ restore failed, b retries` instead of the countdown, and have the CLI text say `rolling back` or `restore failed; fawkes rollback retries`. Add a unit test on `status()` for each `Txn` variant.

**Verifier.** The code is as quoted. `pending()` returns the switch for `Txn::Pending | Txn::RollingBack`, and state.rs:322 documents that on purpose ("also while it is being rolled back or its restore failed"). tests/core.rs:818 and :831 pin `status.pending.generation == g2` after a failed restore. So publishing it is intended in the daemon, but the clients render it as a live switch. The CLI prints `(waiting for confirm until …)`. The TUI detail line shows `confirm g2 within 00:00`, and the footer puts `c confirm` first because it checks only `pending.is_some()`. The doc comment on the protocol `Pending` type ("A switch that waits for its confirm") is also inaccurate. This conflicts with DESIGN's TUI text, which shows the countdown "while a switch awaits confirm". I lowered it to low: the same screen shows the row as `✘ failed` in red and g2 as `rolled_back`. The countdown text "or it rolls back to g1" is close to the right next step, and pressing `c` gets `invalid_state` with the message "`fawkes rollback` retries the restore". The misleading hint therefore leads to the correct guidance at once. It persists only in the rare restore-failed state, and nothing about the transaction itself is wrong.

#### L14. An optional request argument of the wrong JSON type is silently read as absent

*Low* · confirmed · found by `rust/S4`, also `contract/C4`

Where: `src/protocol.rs:169-185`, `src/protocol.rs:263-265`, `src/protocol.rs:259-262`

**Impact.** `{"command":"confirm","args":{"generation":7}}` confirms whatever is pending, which defeats the "must be the pending generation" check an agent asked for. `{"keep_storage":"true"}` runs an ordinary rollback, which retries the restore, when the agent asked to give it up. `{"limit":-1}` silently becomes 20. These are typed-protocol holes in the one place where agents build JSON by hand.

**Fix.** Make a present non-null value of the wrong type a `usage` error, the way `deadline_arg` does: an `optional_text` that returns `R<Option<String>>`, or one small `#[derive(Deserialize)]` args struct per command, fed to `serde_json::from_value` (unknown keys stay ignored, null still means absent). Extend `a_bad_request_is_a_usage_error_with_its_message` to cover it.

#### L15. `From<std::io::Error> for protocol::Error` lets `?` drop the path and turn start failures into `internal`

*Low* · confirmed · found by `rust/S3`, also `contract/C6`, `quality/Q14`

Where: `src/protocol.rs:91-95`, `src/daemon/supervisor.rs:352`, `src/daemon/transactions.rs:198`, `src/daemon/transactions.rs:369`, `src/daemon/transactions.rs:579-590`

**Impact.** Example: an unwritable `home`. The switch rolls back and answers `{"code":"internal","message":"Permission denied (os error 13); g3 is rolled back"}`, which names neither the directory nor the step. Every other failure to start answers `start_failed`. `start_or_give_up` logs the same bare text as the `failed` event. CLAUDE.md asks for messages that say what is wrong and what to do next.

**Fix.** Delete the `From` impl, so the compiler rejects a bare `?` on an io error, and wrap these three calls the way the rest of the daemon does: `.map_err(|e| err(ErrorCode::StartFailed, format!("cannot create {}: {e}", m.home.display())))`, and `StorageFailed` or `Internal` with the path for the other two.

#### L16. The CLI's println! panics on a closed stdout, which breaks the exit-code contract (0/1/2/3)

*Low* · confirmed · found by `rust/S2`

Where: `src/main.rs:283-285`, `src/main.rs:294-297`, `src/main.rs:303-313`, `src/main.rs:25-28`

**Impact.** A common shell pipeline ends with a panic message and exit 134, not 0. Agents that branch on the documented exit codes (CLAUDE.md: 0 ok, 1 failed, 2 usage, 3 unreachable) see an undefined code.

**Fix.** In `client`, write through a locked `stdout()` with `writeln!`. On `ErrorKind::BrokenPipe`, return 0 for `--follow` and 1 otherwise, or restore SIGPIPE's default action at CLI startup (not in the daemon). A unit test can run `client` with stdout replaced by a closed pipe.

#### L17. DESIGN never lists the event kinds; `started`, `staged` and `pruned` appear nowhere

*Low* · confirmed · found by `contract/C7`

Where: `src/protocol.rs:373-393`, `DESIGN.md:396-401`, `DESIGN.md:517-519`

**Impact.** CLAUDE.md counts an event as part of the contract ("a field, an error code, a state, an exit code, an event"), and agents that follow `subscribe` or `log --follow` have to guess the vocabulary from the source.

**Fix.** Add an `Events` table under Socket protocol: each kind, when it is logged and what `generation` holds. `protocol.rs`'s `names_shown_to_people_are_the_names_on_the_wire` test already enumerates them.

#### L18. DESIGN's busy rules miss two cases the slots produce

*Low* · confirmed · found by `contract/C8`

Where: `DESIGN.md:20-23`, `src/daemon/mod.rs:174-195`, `src/daemon/mod.rs:761-783`, `src/daemon/lifecycle.rs:32`, `src/daemon/lifecycle.rs:46`, `src/daemon/lifecycle.rs:76` …

**Impact.** Doc drift only. An agent that retries on `busy` is fine, but the documented rules don't predict the answers.

**Fix.** Reword DESIGN.md:20-23: the lifecycle commands exclude each other, and an automatic rollback never waits for a stage or test. Say that `busy` names the transaction step first when both slots are taken.

#### L19. Manifest section: a timed-out `reuse` copy does not fail the stage, and unknown keys are refused but not documented

*Low* · confirmed · found by `contract/C9`

Where: `DESIGN.md:297-299`, `src/daemon/transactions.rs:232-248`, `src/daemon/transactions.rs:968-975`, `DESIGN.md:280-294`, `src/manifest.rs:57-59`, `src/manifest.rs:90-92`

**Impact.** Doc drift. The code's behaviour is the better one in both cases.

**Fix.** In DESIGN.md, say that a `reuse` copy that fails or times out is skipped and the install starts without it. Add "an unknown key" to the Validation list.

#### L20. Validation accepts port 0 and health URL ports above 65535

*Low* · confirmed · found by `contract/C10`

Where: `src/manifest.rs:385-392`, `src/manifest.rs:213-226`, `src/daemon/transactions.rs:963-965`, `DESIGN.md:288`, `DESIGN.md:293-294`

**Impact.** These are mistakes the validation exists to catch (DESIGN.md:232: "Validation catches mistakes"). They surface later as a test or switch that fails its health check with no hint why.

**Fix.** Refuse `port == 0` and a `spare_ports[0] == 0`. Parse the URL port as `u16` and refuse 0. Add cases to `invalid_manifests_are_refused_naming_the_field`.

#### L21. A relative FAWKES_HOME (or HOME) gives a relative state_dir: worktrees get created inside the repository, and the CLI and daemon resolve different sockets

*Low* · confirmed · found by `critic/X2`

Where: `src/manifest.rs:9-15`, `src/manifest.rs:22-23`, `src/manifest.rs:251-254`, `src/state.rs:72-77`, `src/daemon/transactions.rs:44-47`, `src/daemon/transactions.rs:197-223` …

**Impact.** Take `FAWKES_HOME=.fawkes` and a manifest with no `state_dir`. `stage` creates the worktree at `<repo>/.fawkes/<name>/generations/gN/code`, which is stray directories inside the user's working tree. The install then fails with ENOENT, because it runs in a `code` directory relative to the daemon's cwd that does not exist. Without an install, `test` fails with "cannot start gN for its test". A CLI or TUI run from another directory reports the daemon unreachable (exit 3), because it computes a different socket path. `run/` links go to a relative directory as well.

**Fix.** Make `fawkes_home()` absolute (`std::path::absolute`), or refuse a relative `FAWKES_HOME` with a message that says to use an absolute path. Add a manifest unit test that sets a relative FAWKES_HOME and asserts that `paths().root` is absolute. If the refusal is chosen, mention it in DESIGN's manifest section.

#### I4. Smaller inconsistencies between code comments, exit codes and DESIGN

*Info* · confirmed · found by `contract/C12`

Where: `src/protocol.rs:506-507`, `DESIGN.md:463-465`, `src/main.rs:168-175`, `src/main.rs:191-196`, `CLAUDE.md:62`

**Impact.** None at run time. These are small surprises for readers and agents.

**Fix.** Fix the doc comment. Decide whether a bad manifest is `usage` (2) for the daemon too, and note in DESIGN.md that `--json` does not apply to `daemon` and `tui`.

### 3.4 TUI

#### L22. Modifier keys and pasted text trigger unasked actions (Alt-c confirms, Ctrl-S starts, Ctrl-Z zooms)

*Low* · confirmed · found by `tui/T2`

Where: `src/tui.rs:300-306`, `src/tui.rs:351-354`, `src/tui.rs:358-419`, `src/tui.rs:321`, `src/tui.rs:339`

**Impact.** `c` (confirm), `t` (test) and `s` (start) never ask. Suppose an operator pastes a ref such as `feature/cache` without pressing `n` first. The paste pauses the pane (`f`), tests the selected generation (`t`), opens a restart dialog (`r`) that the next `e` cancels, and then confirms the pending switch (`c`). That makes the upgrade permanent before the operator decided to. A reflexive Ctrl-W or Ctrl-U in a prompt inserts letters into the ref instead of editing it.

**Fix.** In Normal mode, ignore key events that carry CONTROL or ALT, except Ctrl-C. In Stage and Search, accept only `KeyModifiers::NONE`/`SHIFT` characters. Optionally map Ctrl-U to clear and Ctrl-W to delete a word. Enable `crossterm::event::EnableBracketedPaste` after `try_init` and disable it on restore. Forward `Event::Paste` only to the Stage and Search buffers, and drop it in Normal mode. Add a key test for each case.

#### L23. Gaps in terminal restoration: the try_init error arm, the hidden cursor after a panic, SIGTERM, and a dead input thread

*Low* · confirmed · found by `tui/T3`

Where: `src/tui.rs:517-523`, `src/tui.rs:505-516`, `src/tui.rs:524-525`

**Impact.** After these rare paths the user's terminal is left unusable or the cursor invisible until they run `reset` or `tput cnorm`. Case (d) leaves a TUI that has to be killed from another terminal, which then hits case (c).

**Fix.** Call `ratatui::restore()` in the `try_init` error arm. After `try_init`, install a hook that wraps ratatui's and also writes `crossterm::cursor::Show`. When `event::read()` fails, make the input thread send a message that ends the loop, for example `Msg::Down` for the screen or a dedicated `Msg::InputClosed` that returns an error. Optionally use the existing signal-hook dependency to turn SIGTERM, SIGHUP and SIGINT into a quit message.

#### L24. A subscribe answered with an error line counts as 'up', and the daemon's message is dropped

*Low* · plausible · found by `tui/T4`

Where: `src/tui/net.rs:73-107`, `src/tui.rs:144`

**Impact.** A daemon that refuses `subscribe` (an older binary that is still running after an upgrade, or one that is shutting down) shows as connected and then 'disconnected, retrying: connection lost'. This repeats every second. For a discovered app, `seen` becomes true, so the app stays listed and keeps flapping. The real reason, written for the operator, never reaches the screen.

**Fix.** In `follow`, when the first parsed line is `Line::Failed { error, .. }`, return without sending `Up` and use `error.message` as the reason, so `Msg::Down` carries the daemon's own message. Add a FakeDaemon test.

**Verifier.** The code does what the finding says. `up = true` and `Msg::Up` are sent on the first line read, even one that does not parse. A `Line::Failed` is forwarded and then dropped at tui.rs:144, and the stream ends as (true, "connection lost"). I could not find a realistic trigger at HEAD, however. `Request::parse` is lenient (protocol.rs:182-185 reads `since` with `.get`, so an extra field is ignored). `stream()` never writes an error line, and server.rs sends no refusal while shutting down. Only a daemon built before `subscribe` existed (b78b0b8), or a malformed request, would answer with an error line. The defect is real, but its impact rests on that skew.

#### L25. An unknown event kind, generation state or process state silently drops the whole line, and a dropped status leaves a stale picture

*Low* · confirmed · found by `tui/T5`, also `altitude/A6`

Where: `src/tui/net.rs:75`, `src/protocol.rs:291-304`, `src/protocol.rs:375-393`, `src/protocol.rs:461-469`, `src/protocol.rs:52-54`

**Impact.** The TUI is documented to work against daemons started elsewhere, and a daemon keeps running after the binary is upgraded, so version skew between the two is ordinary. A newer daemon's new event kind would simply be missing from the pane. A single new `GenState` in `history` would freeze the generations list. A new `ProcessState` would freeze `status`, and the countdown would keep running on a pending switch that may already be confirmed. The link would still show as up, so nothing tells the operator the picture is stale.

**Fix.** Add `#[serde(other)] Unknown` to `EventKind`, `GenState` and `ProcessState`, as `ErrorCode` already has. This is decode-only, so the wire format is unchanged. Give each an `as_str` of `"unknown"` and a neutral color. At least count unreadable lines and show them in the detail rule, for example 'N unreadable lines from the daemon'. (This overlaps with the protocol dimension. It is reported here because the TUI is where the failure is silent.)

#### L26. Long error notices lose their actionable tail and disappear on the next key; long event messages are cut with no way to read them

*Low* · confirmed · found by `tui/T6`

Where: `src/tui/draw.rs:856-871`, `src/tui.rs:351-353`, `src/tui/draw.rs:820`

**Impact.** CLAUDE.md asks daemon messages to say 'what is wrong and what to do next', so the next step sits at the end, for example '…; run `fawkes test g3` first' or a pointer to `rollback`. On an 80-column terminal that tail is what gets cut. Pressing an arrow key erases the message for good, so the only way to read it is to run the command from the CLI.

**Fix.** When the notice is wider than the footer, wrap it into one or two lines above the footer, or keep the last error in the detail pane until the next action. Wrap event messages in the zoomed pane (`Paragraph::wrap(Wrap { trim: false })`).

#### L27. Progress events use Color::DarkGray (ANSI bright black), which disappears in common dark themes; CLAUDE.md's color list is out of date

*Low* · confirmed · found by `tui/T7`

Where: `src/tui/draw.rs:744`, `src/tui/draw.rs:912`, `src/tui/draw.rs:109`

**Impact.** In such themes the progress lines of a switch or stage are invisible or barely visible, and those lines are the ones an operator watches during an upgrade. Contributors who follow CLAUDE.md would treat Yellow as out of bounds, although DESIGN.md requires it for the countdown.

**Fix.** Draw progress lines with the existing `dim()` modifier instead of DarkGray. This matches 'grey' in DESIGN.md and follows the terminal's own foreground. Update CLAUDE.md's palette to include Yellow, plus Black-on-Yellow for the highlighted confirm hint if that stays.

#### L28. AGE is not 'since its last state change' for tested, test_failed or retired generations

*Low* · confirmed · found by `tui/T8`

Where: `src/tui/draw.rs:609-620`

**Impact.** A generation staged two days ago and tested a minute ago shows `2d`. A retired generation shows the age of its own confirm, not the moment it was replaced. The column that should show what just happened misleads, and the code disagrees with the contract.

**Fix.** Include `g.test.as_ref().map(|t| t.at.as_str())`, taking the latest of the parsed times rather than the first present. Either reword DESIGN.md (for example 'since it was staged, tested, switched, confirmed or rolled back') or record a `retired_at`. Per CLAUDE.md, ask which side is right before changing either.

#### L29. The rollback dialog is not bound to the generation it names; confirm is

*Low* · confirmed · found by `tui/T9`

Where: `src/tui.rs:390-411`, `src/tui.rs:383-388`, `src/protocol.rs:121-124`, `src/daemon/transactions.rs:700-722`

**Impact.** Suppose that while the dialog is open the pending switch is confirmed and someone else switches to g9. Pressing `y` then rolls back g9, a generation the dialog never named, and restores the storage snapshot. Writes made since g9's switch are discarded. Rolling back is the safe direction, so the transactional promise holds, but the action is not the one the operator approved.

**Fix.** Add an optional `generation` argument to `rollback`, mirroring `confirm` (`invalid_state` unless it is the pending one), and send it from the TUI. Update DESIGN.md as part of the same change. A TUI-only alternative is to cancel an open rollback dialog, with a notice, when a `status` line changes `pending.generation`.

#### I5. Unchecked u16 additions after a saturating clamp

*Info* · confirmed · found by `tui/T12`

Where: `src/tui/draw.rs:49`, `src/tui/draw.rs:52`, `src/tui/draw.rs:209`, `src/tui/draw.rs:280-285`, `src/tui/draw.rs:897`, `src/tui/draw.rs:935` …

**Impact.** Nothing in practice. The saturating helper suggests overflow is handled when it is not.

**Fix.** Use `saturating_add` and a fold with `saturating_add` in `rule`, `visible`, `keys_line` and `draw_dialog`, or leave it and drop the implied promise from `clamp_u16`'s doc comment.

#### I6. No scrollback in the events pane, and long app names are cut silently

*Info* · confirmed · found by `tui/T14`

Where: `src/tui.rs:21`, `src/tui/draw.rs:775-790`, `src/tui/draw.rs:468-470`

**Impact.** Older events of a long upgrade cannot be reviewed in the TUI. Two apps whose names share their first 24 characters look identical in the table, although the detail rule shows the full name.

**Fix.** Optional: while paused, let ↑/↓ or PgUp/PgDn scroll the pane, and end truncated names with '…'.

### 3.5 Security

#### L30. A rollback recreates the live database files with SQLite's default mode (0644 under umask 022), and `home` is created world-readable

*Low* · confirmed · found by `security/S3`

Where: `src/storage.rs:19-32`, `src/storage.rs:69-83`, `src/daemon/supervisor.rs:352`

**Impact.** An app that keeps its storage 0600 (for example tokens in SQLite) has it world-readable after its first rollback, and the -wal/-shm files the app creates afterwards inherit the same mode. Other local users can read it whenever they can traverse `home` (a home Fawkes created is 0755, and macOS user directories are group-traversable by `staff`). The protection DESIGN gives the copies (state_dir 0700, "it holds … copies of its storage") is lost on the original.

**Fix.** In `restore`, record the live file's mode (and owner, where it can be kept) before deleting it, or the snapshot source's mode at snapshot time in `index.json`, and `set_permissions` the restored file and its sidecars to it. Alternatively, restore into the existing file by opening it rather than deleting it first. Create a missing `home` with 0700 (`DirBuilderExt::mode`). A unit test in `storage.rs` can assert that the mode survives snapshot and restore.

#### L31. The test harness works in fixed, guessable /tmp paths that another local user can create first, which lets them write over the developer's files or run code as the developer

*Low* · confirmed, severity medium → low by the verifier · found by `critic/X1`

Where: `tests/common/mod.rs:21`, `tests/common/mod.rs:34`, `tests/common/mod.rs:38`, `tests/common/mod.rs:72-74`, `tests/common/mod.rs:84`, `tests/common/mod.rs:408`

**Impact.** This only matters on a shared machine (a multi-user dev box or a shared CI runner). Scenario 1: user B creates `/tmp/fawkes-test-ports` before anyone else (mode 0777) and puts a symlink `20000 -> /home/A/.bashrc` in it. User A's `cargo test` then calls `File::create`, which truncates A's file. Scenario 2: B creates `/tmp/fk20000` with mode 1777 (sticky, so A cannot delete B's entries) and puts `fawkes.toml` in it, or a symlink with that name. `remove_dir_all` fails without a word, and `std::fs::write` either follows the symlink or fails on the planted file. Alternatively B swaps the manifest between the write and `start_daemon`: `start` is an arbitrary shell command, and A's daemon runs it as A. Even with no attacker, when another user has already created `/tmp/fawkes-test-ports` with the default 0755 mode, every `Harness::new` panics with EACCES. security/S1 covers the shipped example manifests that use /tmp, not the harness.

**Fix.** Check that `PORT_LOCKS` and `/tmp/fk<port>` are directories owned by the current uid (`symlink_metadata` plus the uid and mode), and fail the test if they are not. Open the lock files with `OpenOptions::new().create(true).write(true)` (no truncation) and `custom_flags(libc::O_NOFOLLOW)`. Alternatively, create the world directory with `create_dir` (not `create_dir_all`) and skip the block when it already exists and belongs to someone else. Paths stay short either way.

**Verifier.** I checked the code at HEAD. Line 21 is the fixed `PORT_LOCKS = "/tmp/fawkes-test-ports"`. Line 34 is `create_dir_all(PORT_LOCKS)`, which accepts a directory someone else already made. Line 38 is `File::create(format!("{PORT_LOCKS}/{base}"))`, which truncates and follows a symlink. Lines 72-74 are the fixed `/tmp/fk{port}`, a `let _ = remove_dir_all` that drops any failure, and `create_dir_all`. Line 84 writes the manifest into that directory. Scenario 1 (a 0777 non-sticky lock directory with a symlink planted inside, so a lock file truncates the victim's file) works on macOS and on Linux. One part of scenario 2 is overstated: if the planted directory is 1777, it is sticky and world-writable, so Linux fs.protected_symlinks blocks following B's symlink there. macOS has no such protection. Swapping files in a planted 0777 directory between the write and the daemon start still lets B run code as A. CLAUDE.md asks for these paths on purpose (short socket paths), but the defect is real. I lowered the severity to low: this is test harness code, nothing ships with it, and it needs a shared multi-user machine where the developer runs `cargo test`. The `high` level in the scale is for holes in the product.

#### L32. No test pins the security invariants: socket mode 0600, state_dir 0700, and the 64 KiB and 5 s request bounds

*Low* · confirmed · found by `security/S7`

Where: `src/daemon/server.rs:55-60`, `src/daemon/mod.rs:598-606`, `src/daemon/server.rs:75-91`, `tests/core.rs:516-531`

**Impact.** A refactor of `bind` (for example moving it after the first thread spawn, where the process-wide umask would race) or of `create_private_dir` could silently expose the socket or the storage copies, and nothing would fail.

**Fix.** Add to an existing integration test: `metadata(h.dir/s/fawkes.sock).mode() & 0o777 == 0o600` and `metadata(state_dir).mode() & 0o777 == 0o700` (also when the directory existed with 0755 before). Add one test for a 64 KiB + 1 request and one for a trickled request, both expecting `usage`.

#### I7. The CLI and the daemon's stderr print daemon text raw, and the install command line is logged in events

*Info* · confirmed · found by `security/S8`

Where: `src/main.rs:313`, `src/main.rs:341-357`, `src/daemon/mod.rs:733-739`, `src/daemon/transactions.rs:116-127`, `src/daemon/transactions.rs:256`

**Impact.** A ref containing ESC sequences or newlines (possible only from a same-user client) can rewrite the terminal of `fawkes log` or `history` and forge lines in the launchd/journald stderr log. A token written into `install = "… --token=…"` ends up in events.jsonl (0700), in every subscriber's stream and in the daemon's stderr file, which launchd creates 0644 by default.

**Fix.** Escape control characters in the CLI's text rendering (e.g. `char::escape_debug` for any `char::is_control` except none), and keep the daemon's stderr one line per event. In DESIGN.md → Manifest, add one line: put secrets in `[env]` (never logged), not in `install`/`start`.

#### I8. `restore` trusts `index.json` paths without checking them again

*Info* · confirmed · found by `security/S9`

Where: `src/storage.rs:69-83`, `src/manifest.rs:202-210`

**Impact.** Only the daemon's user can write `index.json`, inside the 0700 state_dir, so this is not exploitable today. It is the one place where a corrupt or hand-edited file can turn a rollback into deleting files outside `home`.

**Fix.** Run `is_inside` (made `pub(crate)`) on `entry.path` and check `entry.file` against `^[0-9]+\.sqlite$` before acting. Refuse the index (`storage_failed`, restore stays failed) otherwise.

#### I9. The manifest's ownership and mode are never checked, and `~/.fawkes` and `run/` are created with the umask

*Info* · confirmed · found by `security/S10`

Where: `src/manifest.rs:230-234`, `src/daemon/server.rs:13-15`, `DESIGN.md:228-233`

**Impact.** A group- or world-writable manifest, or a manifest in such a directory, is code execution as the daemon's user at the next start or SIGHUP, with no warning. A 0755 `run/` lets other users list the supervised app names and their socket paths. The sockets themselves stay 0600 inside 0700 directories.

**Fix.** Like sshd's StrictModes: refuse (or log an `error` event for) a manifest that is not owned by the euid or is group/world-writable, and check the same for `repo`'s `.git` if cheap. Create `fawkes_home` and `run/` with 0700.

### 3.6 Rust semantics and idioms

#### L33. Manifest durations are f64 seconds, turned into Duration at six use sites with the panicking `Duration::from_secs_f64`

*Low* · confirmed · found by `rust/S7`, also `quality/Q7`

Where: `src/manifest.rs:34-53`, `src/manifest.rs:105-107`, `src/manifest.rs:405-416`, `src/process.rs:236`, `src/process.rs:284`, `src/process.rs:301` …

**Impact.** No bug today, but the safety of a daemon path depends on validation in another module. A future caller that passes a computed value (`2.0 * stop_timeout + 5.0`, mod.rs:575, already does) can abort the daemon mid-step.

**Fix.** Convert once, in `Manifest::load` after `problems()` passes, to `Duration` fields (`Duration::try_from_secs_f64(..)` mapped into a problem), and make `stop_group`, `wait_healthy` and `run_install` take `Duration`. Unrepresentable values then cannot reach a daemon path, and the six conversions and the comment can go.

#### L34. `let _ =` on fallible calls whose failure matters, with no comment saying why

*Low* · confirmed, severity low → info by the verifier · found by `rust/S11`, also `quality/Q16`

Where: `src/daemon/supervisor.rs:467-474`, `src/storage.rs:49`

**Impact.** If the symlink or rename fails, `<state_dir>/current` silently keeps pointing at the previous generation's worktree after a switch. Anything outside that follows `current` (scripts, the docs' on-disk layout) then reads stale code with no event logged. In `snapshot`, an old snapshot directory that cannot be removed leaves stray `N.sqlite` files. That is harmless (only `index.json` is trusted), but the reader has to work that out.

**Fix.** Log an `error` event when the `current` link cannot be replaced. Add a one-line why to `storage.rs:49` (only `index.json` is trusted, and every file it names is rewritten).

**Verifier.** The code is as quoted, and neither `let _ =` has a comment. DESIGN.md:568, however, describes `current` as a "symlink to the live worktree, for people", so the claimed impact of scripts reading stale code is overstated. 03-review (line 203) already reasons that the `remove_dir_all` in snapshot is safe because only the index is trusted. This is a minor gap against the CLAUDE.md comment rule, so info.

#### L35. The lock helper is duplicated in the TUI, and visibility mixes `pub` and `pub(super)`

*Low* · confirmed · found by `rust/S19`, also `quality/Q18`, `reuse/R5`, `simplification/S14`

Where: `src/tui/net.rs:46-49`, `src/tui/net.rs:124-127`, `src/daemon/mod.rs:43-45`

**Impact.** Minor: the poison policy (abort) is written down in three places, and `pub` signals an API this binary does not have.

**Fix.** Move `lock` to one crate-level helper and use it in tui/net.rs. Optionally turn on `unreachable_pub` (rustc lint) and use `pub(crate)` consistently.

#### I10. `stop_command` is deadlock-free only because of edition 2024's drop order for tail-expression temporaries

*Info* · confirmed · found by `rust/S13`

Where: `src/daemon/lifecycle.rs:46`, `src/daemon/lifecycle.rs:72`, `src/daemon/mod.rs:203-215`, `Cargo.toml:4`

**Impact.** No defect today. The crate is edition 2024, and every other site binds the guard to a local declared after `_busy`. But the correctness of this line is invisible, and the same pattern copied into a helper with an extra block or a `match` tail can silently become a self-deadlock.

**Fix.** Write it the way the other commands do: `let inner = self.inner(); Ok(self.process(&inner))`. Or add a one-line comment to `Busy` that no `inner` guard may outlive it.

#### I11. Unchecked arithmetic on persisted and request values, one case contradicting its own comment

*Info* · confirmed · found by `rust/S14`

Where: `src/daemon/supervisor.rs:41-45`, `src/daemon/supervisor.rs:377`, `src/daemon/transactions.rs:538-541`

**Impact.** None of them can be reached with valid input (the deadline is bounded at the protocol and the manifest, and counters would need billions of crashes). A hand-edited `state.json` with `failed_starts: 4294967295`, though, aborts a debug daemon on the next crash, and every crash after that.

**Fix.** `failed_starts.saturating_add(1)`, `restarts = restarts.saturating_add(1)`, and `now_secs().saturating_add(deadline.unwrap_or(m.deadline)).saturating_add(1)`, so the comment is true.

#### I12. Commit shortened by byte slicing in `build`, by chars in `Generation::short_commit`

*Info* · confirmed · found by `rust/S15`, also `reuse/R6`

Where: `src/daemon/transactions.rs:205`, `src/protocol.rs:355-357`

**Impact.** Safe today, because `git rev-parse` prints ASCII hex. The byte slice would panic on a non-ASCII string, and the two spellings of one concept can drift apart.

**Fix.** Use one helper, a free `short_commit(&str) -> &str` that `Generation::short_commit` also calls, or `commit.get(..8).unwrap_or(commit)`.

#### I13. Every event is deep-cloned once per subscriber and again per write

*Info* · confirmed · found by `rust/S16`, also `efficiency/E6`

Where: `src/daemon/mod.rs:245-247`, `src/daemon/mod.rs:743-748`, `src/protocol.rs:679-683`

**Impact.** Small, with a handful of subscribers. With the 1000-deep queues it is the main per-event cost, and it happens while `log` is held.

**Fix.** `Msg::Event(Arc<Event>)`, and a serialize-only borrowed line (for example `#[derive(Serialize)] struct EventLine<'a> { event: &'a Event }`) in `Out::event`.

#### I14. The TUI allocates in hot per-frame helpers

*Info* · confirmed · found by `rust/S17`, also `tui/T13`, `efficiency/E7`, `simplification/S13`

Where: `src/tui.rs:121-126`, `src/tui.rs:149-170`, `src/tui/draw.rs:780-789`, `src/tui/draw.rs:118-126`, `src/tui/draw.rs:824-832`

**Impact.** No visible problem at current sizes, but it is avoidable work on every redraw.

**Fix.** `fn name(&self) -> &str`. Use `self.history.iter().rev()` directly, or keep `rows()` returning an iterator. Match the filter on `kind`, `generation` and `message` without formatting the clock, or cache the formatted clock per event.

#### I15. clippy::nursery: adopt three lints, leave the rest off

*Info* · confirmed · found by `rust/S18`

Where: `src/manifest.rs:133`, `src/protocol.rs:100`, `src/daemon/transactions.rs:140`, `src/manifest.rs:250`, `src/state.rs:273`, `src/tui/draw.rs:392` …

**Impact.** Turning on the whole group would add churn and nudge lock scopes in a dangerous direction. The three cheap lints catch real eager allocations and missing `Eq`.

**Fix.** Add `derive_partial_eq_without_eq`, `or_fun_call` and `literal_string_with_formatting_args` to `[lints.clippy]`. Do not enable `nursery` as a group, and never `significant_drop_tightening`, for the reason above.

### 3.7 Tests

#### L36. The unhealthy-confirm race test pre-connects inside the daemon's 5 s request timeout, and its comment gives an outdated reason

*Low* · confirmed · found by `tests/T6`

Where: `tests/core.rs:1258-1270`, `src/daemon/server.rs:75-91`, `src/daemon/mod.rs:465-480`

**Impact.** With other suites in parallel (CLAUDE.md expects that), the request misses its 5 s window. The test then fails with `answer["error"]["code"] == "usage"` ("no request received"), which looks like a protocol bug rather than load. The comment also misleads the next reader about why the connection is made early.

**Fix.** Connect right after the `unhealthy` event: accept is now immediate, so pre-connecting buys nothing. Or keep the connection and write the request at once, with the line split so that only the newline waits. Remove the 20 ms sentence.

#### L37. Tight wall-clock upper bounds in the idle-wake test include CLI subprocess latency

*Low* · plausible · found by `tests/T7`

Where: `tests/core.rs:639-690`

**Impact.** Under load from parallel suites, the 1 s bounds can be exceeded without any lost wake-up, and the test flakes. Bounds of 3-5 s would still tell a wake-up from the 10 s timer.

**Fix.** Raise the upper bounds to about 4 s (the backoff window to 1 s..5 s). Keep the lower bound on the backoff, which pins the 1 s wait.

**Verifier.** The bounds are as quoted: `noticed < 1s`, `restarted` in 1..2 s, `reloaded < 1s`, `stopped < 2s`. A correction: `stopped` is measured with `daemon_exits_within` (try_wait polling), not through `log --follow`. The other three do go through the follow subprocess. The `started` event is logged right after spawn, so the 2 s bound has about 1 s of slack. The flake risk under parallel load is real but not observed, so plausible, low.

#### L38. The switch-during-backoff test never checks that the switch landed inside the backoff

*Low* · confirmed · found by `tests/T8`

Where: `tests/core.rs:597-612`, `src/daemon/transactions.rs:505-530`

**Impact.** On a slow machine the test quietly stops covering the M2 fix from 03-review. A regression in the backoff-restore branch would then pass.

**Fix.** Assert `status.process.restart_at` is non-null just before the switch, and again right after the refused switch (the backoff is kept). Or widen the window: a third failed start waits 4 s with `max_failed_starts = 5`.

#### L39. The SIGTERM-during-switch test never checks the `busy` code, and skips its message checks when no answer arrives

*Low* · confirmed · found by `tests/T9`

Where: `tests/core.rs:932-936`, `src/daemon/transactions.rs:556-565`

**Impact.** A change that answered `start_failed` or `internal` here, which agents would read as a failed switch rather than a retry, passes as long as the message text matches.

**Fix.** Inside the `if let`, also `assert_eq!(answer["code"], "busy")`. Optionally count how often the answer is missing; if it usually arrives, consider flushing the final line before `shutdown` lets `run` return.

#### L40. `switched.elapsed() < 20` adds nothing after a 20 s wait

*Low* · confirmed · found by `tests/T10` · still open from 03-review §5 (third flaky/weak bullet)

Where: `tests/core.rs:342-347`

**Impact.** A rollback that took 15 s, such as one decided only at a later pass, would still pass.

**Fix.** Bound it to what the test means, for example `< 10` (3 s health timeout plus stop and restore).

#### L41. Harness::drop ends the app's group but not a test or install left in `step.pid`

*Low* · confirmed · found by `tests/T11`

Where: `tests/common/mod.rs:387-409`, `tests/core.rs:553-562`

**Impact.** A failing run leaves a 10-minute process behind. It is harmless for ports (the next claim probes them), but it is exactly the leak the test checks for, and it piles up across reruns.

**Fix.** In Drop, do the same `kill_process_group` for `s/step.pid` as for `s/app.pid`, with the same `pid > 1` guard.

#### L42. Harness git commands inherit the user's global git config and any GIT_* environment

*Low* · plausible · found by `tests/T12`

Where: `tests/common/mod.rs:123-141`, `tests/common/mod.rs:235-243`, `src/daemon/transactions.rs:45-48`

**Impact.** Commits can be slowed or broken by the user's hooks, and in the hook scenario the harness writes into the real repo.

**Fix.** Add `-c core.hooksPath=/dev/null` and `.env_remove` for `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE`, or set `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1`. Apply the same to `daemon_command`, whose git children inherit the test's environment.

**Verifier.** The harness `git` passes only user.name/email and the gpgsign `-c` options, sets `current_dir(repo)`, and neither removes nor overrides GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE or core.hooksPath. `daemon_command` and the daemon's `git_within` (`git -C repo`) inherit the environment too, and only `worktree add` disables hooks. If GIT_DIR were set, as when cargo test runs from a git hook, `git init`, `add .`, `commit` and `tag` would act on the real repository with the temp dir as work tree. That scenario is speculation and was not reproduced, so plausible, low.

#### L43. TUI tests miss discovery, the App-side resume path, the event cap, the Gone-while-selected screen and the request timeout

*Low* · confirmed · found by `tui/T10`, also `tests/T13`, `tests/T14`

Where: `src/tui/net.rs:25-55`, `src/tui/net.rs:111-135`, `src/tui.rs:128-134`, `src/tui.rs:257-278`, `src/tui.rs:784-899`, `src/tui/net.rs:186-190`

**Impact.** Regressions in which apps appear or disappear, in event continuity after a reconnect, and in the Gone handling would go unnoticed. T1 is an example of a bug these gaps let through.

**Fix.** Add FakeDaemon tests for a discovered reader, covering never-answered, link-gone and socket-still-there cases. Add App tests for `Up(resumed=true)` keeping events and for the cap with `frozen`. Add a render test for Gone-while-selected. Split the long key test by behavior. Consider `tempfile::Builder::new().tempdir_in("/tmp")` for short unique socket directories.

#### L44. Startup and protocol edges promised by DESIGN.md have no test

*Low* · confirmed · found by `tests/T15` · still open from 03-review §5 item 8 (panic hook, .bak event)

Where: `src/daemon/server.rs:13-31`, `src/daemon/server.rs:35-46`, `src/daemon/server.rs:84-91`, `src/daemon/mod.rs:393-412`, `src/daemon/transactions.rs:935-954`, `tests/core.rs:533-534`

**Impact.** Each is small, but all are written into DESIGN.md (Socket protocol, Daemon start 3-6), and the step.pid path for tests is the fix for 03-review M1.

**Fix.** Cheap additions to existing tests: send 70 KiB without a newline and expect `usage`; `run_second_daemon` with another state_dir and the same `name` (and with `--socket` of the first); corrupt `s/state.json` between `kill_daemon` and `start_daemon` and expect an `error` event saying "recovered from"; `mkdir s/generations/g99` and expect it gone; SIGKILL the daemon during a `slow-test` test and expect the spare port free after restart.

#### L45. Several documented features have no test: `reuse`, `git_failed`, prune without `keep`, switching back to a retired generation

*Low* · confirmed · found by `tests/T16`

Where: `src/daemon/transactions.rs:104-128`, `src/daemon/transactions.rs:232-249`, `src/daemon/transactions.rs:841-845`, `src/daemon/transactions.rs:968-982`, `src/daemon/supervisor.rs:237-255`

**Impact.** These are smaller paths, but switching back to a retired generation is part of the generation diagram and the operator's way out of a bad confirm. A refusal or state bug there would surface only in production.

**Fix.** Extend `prune_keeps_the_newest_generations…` or the first test: after g2 is permanent, assert g1 is `retired`, `test g1`, `switch g1`, and expect v1 to serve and confirm. Add `stage nope` expecting `git_failed`, `prune` without keep expecting `invalid_state`, and a `reuse = ["node_modules"]` stage expecting the directory in the new worktree.

#### L46. Manifest validation table misses several rules

*Low* · confirmed · found by `tests/T17`

Where: `src/manifest.rs:382-392`, `src/manifest.rs:519-594`, `src/manifest.rs:57-58`

**Impact.** A typo in a manifest key or a reversed range is the most likely real mistake. Losing `deny_unknown_fields`, or the collection of all problems, would go unnoticed.

**Fix.** Add those four cases. For the last, assert that a manifest with two problems names both fields in one error.

#### L47. CLI text output and `--json` progress lines are never exercised

*Low* · confirmed · found by `tests/T18`, also `quality/Q19`

Where: `src/main.rs:341-496`, `src/main.rs:505-606`

**Impact.** Text mode is what people read. A wrong field in a rendered sentence, or a JSON progress line on stdout (which would break the "exactly one JSON document" promise), would pass.

**Fix.** Add a unit test that feeds `render_result` typed results for each `Request` variant. Add one core test that runs `stage` and `switch` with `--json`, checks that stderr is all JSON lines with `event` objects and stdout is exactly one document, and runs `status` without `--json` once.

#### L48. Crash helpers are duplicated, and two tests signal through the external `kill` binary

*Low* · confirmed · found by `tests/T20`, also `quality/Q17`, `reuse/R9`, `altitude/A12`, `simplification/S16`

Where: `tests/core.rs:586-596`, `tests/core.rs:715-720`, `tests/core.rs:965-974`

**Impact.** Small maintenance cost, and less clear failure messages ("kill -9 null").

**Fix.** Add `Harness::crash_app()` (rustix `kill_process` on the status pid, then wait for `stopped`) and use it in all three tests.

#### I16. Further untested supervision and server paths

*Info* · confirmed · found by `supervision/S11` · still open from 03-review §5 item 9 (Linux, no CI)

Where: `src/daemon/supervisor.rs:237-255`, `src/daemon/server.rs:84-87`, `src/daemon/mod.rs:569-593`, `src/process.rs:133-149`, `src/process.rs:57-74`

**Impact.** These paths could regress silently. The log cap and the request limit are both resource bounds that the earlier reviews asked for.

**Fix.** Unit-test `cap_app_log` with a small `APP_LOG_BYTES` (`cfg(test)`) and `handle` with an oversized request over `UnixStream::pair()`. Add a core test that SIGTERMs during a `slow-test` test and asserts the spare port is free. Add a `process.rs` test that `exited(&child).recv()` is followed by `try_wait() == Ok(Some(_))`.

#### I17. No test loads the example manifests that DESIGN.md quotes as the contract

*Info* · confirmed · found by `critic/X8`

Where: `examples/sample/fawkes.toml:1-24`, `examples/ultimate.toml:1-19`, `tests/common/mod.rs:17`

**Impact.** No defect today. A future validation rule (for example a fix for contract/C10, or a renamed key) could make a documented example invalid without any test failing. Users copy these files first.

**Fix.** Add a `manifest.rs` unit test that runs `Manifest::load` on both example files, and on the DESIGN sample extracted into a fixture if that is practical, so the documented manifests cannot drift from the validator.

#### I18. Unit-test environment assumptions: IPv6 loopback required, short-lived children not killed on failure, hand-kept enum lists

*Info* · confirmed · found by `tests/T21` · still open from 03-review §5 item 9 (Linux branches)

Where: `src/process.rs:477-482`, `src/daemon/supervisor.rs:694-728`, `src/daemon/mod.rs:858-867`, `src/protocol.rs:884-898`, `src/protocol.rs:933-950`

**Impact.** A spurious failure on IPv6-less runners, and silent coverage loss when variants are added.

**Fix.** Skip the `[::1]` case when the bind fails with `AddrNotAvailable`. Build the variant lists in the tests from an exhaustive `match` helper, so a new variant fails to compile until it is listed.

### 3.8 Dependencies, build and tooling

#### L49. ratatui, rusqlite and toml keep default features fawkes never uses; ratatui's pull in the `time` crate next to jiff

*Low* · confirmed · found by `deps/DEP2`

Where: `Cargo.toml:11`, `Cargo.toml:12`, `Cargo.toml:17`, `src/tui/draw.rs:12-16`, `src/tui.rs:13-14`, `src/manifest.rs:233`

**Impact.** Nine extra crates are compiled and have to be audited, and a second date-time library (`time`) ships beside jiff without being used. It costs build time and supply-chain surface, and nothing breaks.

**Fix.** Trim the three declarations as above and list the chosen features in DESIGN.md → Crates, as the entries for clap and jiff already do. Keep `layout-cache`, since the TUI redraws every 250 ms.

#### L50. No advisory, licence or source checks, and the lander's cargo commands are not `--locked`

*Low* · confirmed · found by `deps/DEP3`

Where: `Cargo.lock`, `Cargo.toml:7-17`, `.ultimate/settings.json:12-14`

**Impact.** Nothing reports a RustSec advisory, a yanked crate or a new licence. A plain `cargo audit` on this Cargo.lock would also flag crates that are never built, and that noise teaches people to ignore it. Without `--locked`, a change to Cargo.toml can update Cargo.lock silently during a check.

**Fix.** Add a `deny.toml` (ask before installing cargo-deny, as CLAUDE.md requires) with `[graph] targets = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]`, `[advisories]` on, `[bans] multiple-versions = "deny"` with a `skip` for syn 2, `[sources] unknown-registry = "deny"`, `unknown-git = "deny"`, and a licence allow-list. The compiled crates are all MIT, Apache-2.0, Unlicense, Zlib, BSL-1.0 or Unicode-3.0, and the bundled SQLite is public domain. Add `--locked` to the lander's clippy and test commands. cargo-vet is probably more process than a single-maintainer tool needs.

#### L51. `rust-version` is described as two different things, and neither is enforced

*Low* · confirmed · found by `deps/DEP4`

Where: `Cargo.toml:5`, `rust-toolchain.toml:1-4`, `README.md:50-51`

**Impact.** One of the two documents goes stale with the next stable release, and a promised floor that nothing checks can quietly break. Edition 2024's MSRV-aware resolver ("Locking 221 packages to highest Rust 1.99 compatible versions") is the only part that actually uses the value.

**Fix.** Pick one meaning. If it is a floor, add a lander or CI check `cargo +1.99 check --all-targets --locked`, or pin `channel = "1.99.1"`. If it is only the version under test, say that in rust-toolchain.toml too. Either way, the dependencies alone would allow a much lower floor if wider support ever matters.

#### L52. No CI, and the Linux-only code paths, crash recovery's leftover check among them, are never compiled or run in this setup

*Low* · confirmed · found by `critic/X5`

Where: `src/process.rs:134-149`, `src/process.rs:176-186`, `src/process.rs:196-214`, `src/daemon/transactions.rs:968-973`

**Impact.** `leftover` decides whether a crashed daemon's app or step is stopped at the next start (DESIGN Daemon start, step 5). A regression in the Linux branch would ship unnoticed: a compile error, wrong field arithmetic, or a `/proc` format assumption. On Linux it shows up either as an app left running beside the new one (port taken, storage restored under a live writer), or as a crash recovery that never stops anything. busybox `cp` (Alpine) also lacks `--reflink`, and nothing would show it.

**Fix.** Add a minimal CI (or a lander step) that runs `cargo clippy --all-targets -- -D warnings` and `cargo test` on Linux, or at least `cargo check --target x86_64-unknown-linux-gnu --all-targets`. Add a unit test for `start_time`'s field parsing on a fixed sample `/proc/<pid>/stat` line, including a command name with spaces and `)`.

#### I19. libc is a dependency on every platform but only macOS code uses it, and other Unix targets fail with an unclear error

*Info* · confirmed · found by `deps/DEP6`

Where: `Cargo.toml:10`, `src/process.rs:117-136`, `src/process.rs:152-176`, `src/process.rs:196-215`

**Impact.** No defect. The manifest suggests libc is used everywhere, and a FreeBSD build fails with "cannot find function `start_time`" instead of a clear message.

**Fix.** Move it to `[target.'cfg(target_os = "macos")'.dependencies] libc = "0.2.191"`. Add `#[cfg(not(any(target_os = "macos", target_os = "linux")))] compile_error!("fawkes supports macOS and Linux")`, or fallbacks that return None.

#### I20. std's `File::try_lock`/`lock` (stable since 1.89, flock on Unix) can replace rustix's `flock`

*Info* · confirmed · found by `deps/DEP7`

Where: `src/daemon/mod.rs:28`, `src/daemon/mod.rs:620-627`, `tests/common/mod.rs:4`, `tests/common/mod.rs:39`, `DESIGN.md:597-598`

**Impact.** No defect. It would be one fewer third-party API on the locking path, which the transactional promise depends on.

**Fix.** Optional: use `file.try_lock()` and match `TryLockError::WouldBlock`. Use `File::lock()` in the test harness. Update the rustix line in DESIGN.md → Crates.

#### I21. `panic = "abort"` works as the comment says; two things follow from it

*Info* · confirmed · found by `deps/DEP8`

Where: `Cargo.toml:32-39`, `src/daemon/mod.rs:42-45`, `src/tui.rs:517`

**Impact.** No defect. Debug and release builds differ on overflow, which goes against the stated policy of ending the process on a broken invariant.

**Fix.** Consider `overflow-checks = true` under `[profile.release]` (it costs little in a daemon that is mostly syscalls). Optionally note in the `lock()` comment that the abort arm can run under `cargo test`.

#### I22. Tree size and the one duplicate (syn 2 and syn 3) come from ratatui and can't be fixed from fawkes

*Info* · confirmed · found by `deps/DEP9`

Where: `Cargo.toml:11`, `Cargo.toml:16`, `Cargo.lock`

**Impact.** One extra syn compile, and the TUI accounts for most of the dependency surface of the daemon binary, which is the same binary. As far as I know, none of the compiled crate versions has a RustSec advisory. I could not check that offline.

**Fix.** Accept the syn duplicate and skip it in the cargo-deny config. Don't count on signal-hook's trimmed defaults. If debug-build test time matters, `[profile.dev.package.libsqlite3-sys] opt-level = 2` builds the bundled SQLite optimized: it compiles at -O0 in dev today, which slows the backups the tests run.

### 3.9 Code quality and maintainability

#### L53. state.rs is five modules in one, and protocol.rs and state.rs depend on each other

*Low* · confirmed · found by `quality/Q2`

Where: `src/state.rs:1`, `src/state.rs:3`, `src/state.rs:14-40`, `src/state.rs:469-612`, `src/protocol.rs:6`, `src/protocol.rs:441-448`

**Impact.** A circular module dependency (legal in Rust, but it hides layering: the wire types depend on the persistence module for a clock), and a 1036-line file whose parts have nothing in common except being on disk.

**Fix.** Create `src/time.rs` (`now_secs`, `now`, `format_time`, `parse_time`) that both protocol and state import, which breaks the cycle; move `EventLog`, `rotated_path`, `open_append`, `remember`, `EVENT_LOG_BYTES`, `RECENT_EVENTS` and their two tests to `src/events.rs`. state.rs keeps Txn/State/allowed/refusal/Store/write_durably.

#### L54. The "has the live app exited?" check is hand-written at six sites with different polarities, guarding two invariants

*Low* · confirmed · found by `quality/Q4`, also `simplification/S8`, `altitude/A4`

Where: `src/daemon/mod.rs:159-160`, `src/daemon/supervisor.rs:196-198`, `src/daemon/supervisor.rs:276-278`, `src/daemon/supervisor.rs:444-446`, `src/daemon/transactions.rs:653-655`, `src/daemon/supervisor.rs:257-260` …

**Impact.** An `Err(_)` from try_wait counts as alive at some sites and as exited at others (446 treats Err as exited, 160 as alive); a future edit that flips one silently changes which path handles a crash.

**Fix.** Add `impl App { fn exit_status(&mut self) -> Option<ExitStatus> { self.child.try_wait().ok().flatten() } fn has_exited(&mut self) -> bool }` and use it at every site, deciding the Err case once with a comment. Move `current_generation` and `restart_due` to `impl Inner`, and add `Inner::state(&self) -> &State`.

#### L55. How a process ended is rendered three ways, and the TUI imports a helper from the binary root

*Low* · confirmed · found by `quality/Q5`, also `rust/S10`, `reuse/R3`, `simplification/S1`, `altitude/A9`

Where: `src/protocol.rs:450-456`, `src/main.rs:383-395`, `src/tui/draw.rs:171-179`, `src/tui/draw.rs:7`, `src/main.rs:337-339`, `src/daemon/transactions.rs:203-205` …

**Impact.** The event log says `exit status 3`, `fawkes status` says `exit 3`, the TUI says `exit 3`/`SIGKILL`: an agent matching text sees three spellings of one fact, and the TUI depends on main.rs.

**Fix.** Give `LastExit` one `fn how(&self) -> String` (with the signal names from draw.rs) used by the daemon's events, `render_process` and the TUI, and move `or_dash` next to it (protocol.rs or a small `render.rs`). Use `short_commit` in `build`.

#### L56. Deadlines are parsed from strings at many sites with different fallbacks, and seconds are formatted four ways

*Low* · confirmed · found by `quality/Q8`, also `simplification/S6`, `efficiency/E10`, `altitude/A5`

Where: `src/main.rs:376`, `src/main.rs:416`, `src/daemon/transactions.rs:447`, `src/daemon/supervisor.rs:309`, `src/tui/draw.rs:630`

**Impact.** The outcome is consistent today only because `tick` calls `decide_overdue` after `due`; a reader has to verify that across three functions. Users see `up 65.2s`, `in 2s`, `after 2.0s` for the same kind of value.

**Fix.** Add `Pending::deadline_secs(&self) -> Option<u64>` and decide the unreadable case in one place (the supervisor), which the other sites call. Use one `{:.1}` format for every seconds value shown to people (CLI, events, TUI).

#### L57. Timing and size literals inline in daemon and TUI code; the log default 20 lives in four places

*Low* · confirmed · found by `quality/Q9`

Where: `src/daemon/server.rs:33`, `src/daemon/server.rs:75`, `src/daemon/server.rs:125`, `src/daemon/server.rs:154`, `src/daemon/mod.rs:486`, `src/daemon/mod.rs:574-575` …

**Impact.** The 5 s request-read bound and 500 ms stream check are contract-relevant (DESIGN.md mentions both) but have no name to grep for; the default log limit can drift between server and docs.

**Fix.** Add named constants with a one-line why: `REQUEST_READ_TIMEOUT`, `MAX_SOCKET_PATH`, `CLOSED_CHECK`, `ACCEPT_RETRY`, `SHUTDOWN_GRACE`, `PRUNE_RETRY`, `GROUP_POLL`, `HTTP_TIMEOUT`, `BACKUP_PAGES`/`BACKUP_PAUSE`; put `pub const DEFAULT_LOG_LIMIT: u64 = 20` in protocol.rs and use it in server.rs and the tests.

#### L58. Comments that refer to earlier versions or are now false

*Low* · confirmed · found by `quality/Q10`, also `tests/T19`, `altitude/A7`, `efficiency/E11`

Where: `src/tui.rs:864`, `tests/core.rs:1259`, `src/state.rs:775-777`, `src/state.rs:815`

**Impact.** CLAUDE.md forbids comments mentioning a previous version; the 20 ms one is also wrong, so a reader looking for the race the test guards against looks in the wrong place.

**Fix.** tui.rs:864: `// Upper-case letters and digits are no keys; `q` quits.` core.rs:1259: say why the connection is opened early (so the confirm leaves the moment the event is read, without a connect in between). state.rs: rename to `pre_txn_state` / `a_state_json_without_txn_loads_its_pending_switch_as_one` and describe the shape (`pending` instead of `txn`), not the phase.

#### L59. Inaccurate doc comments and names; CLAUDE.md lists fewer colours and locks than the code uses

*Low* · confirmed · found by `quality/Q11`, also `rust/S12`, `quality/Q13`, `simplification/S11`

Where: `src/tui/draw.rs:249`, `src/tui/draw.rs:408-413`, `src/tui.rs:423`, `tests/core.rs:1-2`, `src/tui/draw.rs:744`, `src/tui/draw.rs:912` …

**Impact.** Small, but these are the comments a future change reads first; CLAUDE.md drift means a contributor following it would reject the existing yellow countdown.

**Fix.** draw.rs:249: `/// One column of a table. When space runs out the highest `rank` is hidden first; rank 0 is always shown.` Rename `Row6` to `AppRow`. tui.rs:423: `/// A request on the current app (stop, restart, prune) after a y/N dialog.` Rewrite core.rs's `//!` as a list of areas. Update CLAUDE.md's colour list (Yellow, DarkGray, Black-on-Yellow) and lock order (`wake` last).

#### L60. event_color's "(requested)" guard can never be false and ties the TUI to daemon message text

*Low* · confirmed · found by `quality/Q12`, also `tui/T11`, `simplification/S2`, `altitude/A8`

Where: `src/tui/draw.rs:741-742`, `src/daemon/supervisor.rs:309`, `src/daemon/supervisor.rs:323`

**Impact.** Dead branch with a misleading comment; if a daemon message ever contained the word, a crash would silently turn uncoloured.

**Fix.** `EventKind::Exited => Color::Red,` with the comment "`exited` is always unrequested; a requested stop is `stopped`".

#### L61. Abbreviated type and variable names and positional tuple variants, against CLAUDE.md's full-word rule

*Low* · confirmed · found by `quality/Q15`

Where: `src/daemon/mod.rs:75`, `src/daemon/mod.rs:219`, `src/state.rs:102`, `src/tui.rs:32-45`, `src/tui.rs:84`, `src/tui/draw.rs:43` …

**Impact.** Mostly readability; the positional bool in `Msg::Up` is the one that can be passed wrong without a compiler error.

**Fix.** Use named fields for TUI messages (`Up { key, socket, resumed }`). Rename `Why` → `StartReason`, `Msg` → `Message`/`Notice`, `Cmd` → `Command`, `Col` → `Column`, `m` → `manifest`, `mmss` → `minutes_seconds`. `Txn` → `Transaction` is safe for state.json because the enum is internally tagged by `step`, but keep the type name stable if renaming feels risky.

#### I23. transactions.rs mixes subprocess plumbing and generation housekeeping with the promise-critical switch/confirm/rollback

*Info* · plausible, severity low → info by the verifier · found by `quality/Q1`

Where: `src/daemon/transactions.rs:37-98`, `src/daemon/transactions.rs:960-982`, `src/daemon/transactions.rs:833-958`, `src/process.rs:25-55`, `src/process.rs:314-334`

**Impact.** The ~370 lines a reviewer must read to check the transactional promise are buried among ~600 lines of git/cp/prune plumbing, and a change to how children are isolated (group, stdin, env) has to be made in three places that can drift.

**Fix.** Move `git`, `git_within`, `drain`, `run_bounded`, `clone_directory` and `port_free` into `process.rs` (or a new `daemon/external.rs`), with one `fn shell(command: &str, cwd: &Path, env: &Env) -> Command` builder that `spawn`, `run_check` and `run_bounded` share. Move stage/build/run_install/spawn_step/end_step/remove_generation/prune/prune_soon/sweep_generations into `daemon/generations.rs`. transactions.rs then holds refuse_failed_restore, switch, confirm, rollback_pending and abandon_restore (~430 lines).

**Verifier.** The layout is as described: git/git_within/drain/run_bounded sit at 37-98, port_free/clone_directory at 960-982 and pruning at 833-958. But the duplication evidence is wrong. run_bounded runs `git`/`cp` directly with piped stdout/stderr. It is not a `/bin/sh` setup. process::spawn writes to a log file with an optional pid-file wrapper, and run_check sends output to null. The only code all three share is `.stdin(Stdio::null())` and `.process_group(0)`, so a shared `shell()` builder would cover two of the three and save a few lines. The module doc (line 1) lists stage and test as transactions, so moving stage/build/test into a 'generations' module goes against the file's stated scope. What is left is an opinion about file layout, not a defect.

#### I24. Several functions are 90-140 lines with closures that carry control flow; they split along obvious seams

*Info* · plausible, severity low → info by the verifier · found by `quality/Q3`

Where: `src/daemon/transactions.rs:464-606`, `src/daemon/transactions.rs:345-462`, `src/daemon/mod.rs:336-445`, `src/daemon/supervisor.rs:339-425`, `clippy.toml:3-4`

**Impact.** The recovery and switch paths, where an early return matters most, are the hardest to read top to bottom; the threshold bump means the next long function is not flagged.

**Fix.** switch: extract `fn switchable(&self, id) -> R<Option<String>>` (473-487), turn the closure into `struct HeldBack { previous, had_app, backoff }` with `fn resume(self, daemon: &Arc<Daemon>)`, and `fn go_live(...)` for 552-591. test: `fn spare_port(m: &Manifest) -> R<u16>` and `fn record_test(&self, id, info, passed) -> R<Generation>`. run: `fn recover(self: &Arc<Self>) -> Result<(), String>` for 397-437. start_live: `fn spawn_health_check(self: &Arc<Self>, ...)` for 395-423. Then lower the clippy threshold back toward 100.

**Verifier.** The lengths and the `resume` closure (519-530, called on three error exits) are as described. However, clippy.toml documents the threshold as a deliberate choice: 'The daemon's transaction steps read top to bottom, in the order they happen'. The finding contradicts a documented design decision and does not show a defect. Whether splitting switch into HeldBack/go_live makes the promise easier to check is a matter of taste. One real nit: the clippy.toml comment says 'the longest is 138 lines', which may be out of date.

#### I25. The CLI and the TUI each implement the send-request/read-until-answer loop with the same timeout and messages

*Info* · plausible, severity low → info by the verifier · found by `quality/Q6`, also `reuse/R4`

Where: `src/main.rs:226-332`, `src/tui/net.rs:137-167`

**Impact.** A protocol change to the final line (a new `Line` variant, a different timeout rule) has to be made twice, and the two already differ: main.rs `break`s on a non-timeout read error, net.rs returns it.

**Fix.** Add to protocol.rs `pub fn call(socket: &Path, request: &Request, timeout: Duration, mut on_line: impl FnMut(&str, &Line)) -> Result<Line, CallError>` that owns connect/write/timeout/termination; `client` keeps only rendering and exit codes, `net::request` becomes a few lines.

**Verifier.** Both loops connect, write, set a read timeout for quick requests, read lines and stop at Done/Failed, and they share the two error strings. The CLI loop also has to pass raw JSON through for --json, handle unreadable events, route follow mode to stdout or stderr and map Usage to exit 2. A shared `call` would need the raw line, the parsed line and the error kind, so the saving is small. The evidence says both use REQUEST_TIMEOUT, but the CLI takes a `timeout` parameter. The two loops handle a non-timeout read error differently (break vs return), and both outcomes are benign.

#### I26. Adding a socket command means editing eight hand-kept places

*Info* · confirmed · found by `quality/Q20`

Where: `src/protocol.rs:152-189`, `src/protocol.rs:193-209`, `src/protocol.rs:215-224`, `src/protocol.rs:233-257`, `src/protocol.rs:847-881`

**Impact.** Exhaustive matches catch a forgotten arm in most of them, but `parse` uses string arms with a catch-all, so a new variant that is serialized but never parsed compiles.

**Fix.** Keep the hand-written parser (its error texts are part of the contract), but add one test that builds every `Request` variant and asserts `Request::parse(&serde_json::to_string(&r)) == r` and that `command()` is unique, so a missing `parse` arm fails a test.

---

## 4. Test coverage map

No coverage tool is installed, so this map was built by reading every test (50 unit, 24 integration) against
DESIGN.md. "e2e" is `tests/core.rs`, "unit" a `#[cfg(test)]` module. Test names are cut to their leading words.

### Commands

| Command | Pinned by | Gap |
|---|---|---|
| `daemon` start, a second daemon refused | e2e (every test); `a_daemon_killed_during_a_pending_switch…` (flock) | same `name` run link, socket path too long (L44, L12) |
| `start` | e2e `upgrade_confirms…`, `a_crash_looping…`; refused during a failed restore | already running, no generation yet, `busy` during a rollback |
| `stop` / `restart` | e2e `upgrade_confirms…`, `busy_steps…`, `stop_during_a_crash_backoff…`; unit `taking_the_app_for_stop_or_restart…` | none |
| `stage` | e2e many; `a_failing_or_hanging_install…`; `a_daemon_killed_during_a_stage…` | `git_failed`, `reuse` (L45) |
| `test` | passing tests only | failing test, retest, `port_in_use`, no free spare port (**M12**) |
| `switch` | e2e many; `--deadline` out of range; unwritable intent; `rolled_back` refused | switch to `staged`/`test_failed`, switch while pending, spawn failure → rollback, back to `retired` (L45) |
| `confirm` | e2e by the app; refused in `a_confirm_racing_a_crash…`, `a_confirm_sent_at_the_unhealthy_event…`, during a failed restore | deadline passed, nothing pending, wrong id, pending not running (**M13**); bare confirm from a test (**M4**) |
| `rollback` / `--keep-storage` | e2e `prune_keeps…`, `failed_writes…`, `rollback_keep_storage…` | none |
| `prune` | e2e `prune_keeps…`; unit `prune_takes_the_oldest…` | without `keep`, `busy`, the `prune_soon` retry |
| `status` / `history` / `log` / `subscribe` | e2e everywhere; `log_follow_replays…`, `subscribe_since…`; unit field order | CLI text output (L47) |
| `tui` | unit tui.rs (4), tui/net.rs (3) | discovery, disconnected/busy/failed render, gone selection (L43, **M9**) |
| usage / `--help` | e2e `a_usage_error_exits_2_and_help_exits_0` | the 64 KiB and 5 s request bounds (L32, **M6**) |

### Daemon start recovery (`txn` on disk)

| Step on disk | Pinned by |
|---|---|
| none | most restarts |
| `switching` | **none** (**M11**: the SIGTERM test clears the step before the daemon exits) |
| `pending` | `a_daemon_killed_during_a_pending_switch…`, `a_switch_records_its_intent…` |
| `rolling_back`, `restore_failed` included | **none** (**M11**) |
| leftover app (`app.pid`) | both pending-switch tests; unit `a_leftover_is_only…`, `a_group_whose_leader_has_exited…` |
| leftover install / test (`step.pid`) | install: `a_daemon_killed_during_a_stage…`; test: **none** (L44) |
| `state.json.bak` fallback | unit `load_falls_back…` only |

### Generation edges

Every edge of `allowed` is pinned by the 8x8 unit table. End to end: `staging → staged`, `→ removed` (failed stage
and at the next start), `tested → current → permanent`, `current → rolled_back` (deadline, crash, unhealthy,
`daemon_restart`, manual; not spawn failure), and `rolled_back` final. **Never driven end to end:** `→ test_failed`,
`test_failed → tested`, `retired → tested → current`, and `permanent → retired` is never asserted.

### Error codes and events

| Code | Pinned by |
|---|---|
| `usage`, `invalid_state`, `busy`, `install_failed`, `storage_failed`, `internal` | e2e |
| `not_found` | unit only |
| `git_failed`, `health_failed`, `start_failed`, `port_in_use` | **none** (M12, L45) |
| `daemon_unreachable` | exit code 3 in e2e, string in unit |
| unknown future code | unit |

Event kinds asserted by kind in e2e: `daemon`, `progress`, `started`, `healthy`, `unhealthy`, `exited`, `failed`,
`stopped`, `rolled_back`, `error`. Emitted but never asserted: `staged`, `tested`, `switched`, `confirmed`, `pruned`.
Never emitted by any test: `test_failed`.

### Restart policy

`after_exit` is fully unit-tested: doubling 1, 2, 4, 8, 16 to the 30 s cap, the reset after `min_uptime` (at the
boundary), giving up at `max_failed_starts`, a pending generation always rolling back. `sleep_for` is unit-tested
with both clocks injected. End to end: `failed` cleared by `start`, `restart_at` during a backoff, stop cancelling
it, a backoff kept across a failed switch (weakly, L38). Not tested: `failed` cleared by `restart`, by `switch` or
by a daemon start.

### Test quality

- **Flake risks:** L36 (a pre-connect inside the 5 s request window, with a comment about the 20 ms accept loop that
  `5cc84f9` removed) and L37 (tight wall-clock bounds that include CLI process start-up).
- **Weak assertions:** L38, L39, L40.
- **Isolation:** M10 (`FAWKES_SOCKET`), L42 (the harness's git inherits the user's global config and `GIT_*`), L31
  (fixed `/tmp` paths another local user can create first).
- **Cleanup:** `Harness::drop` ends the app's group but not a test or install recorded in `step.pid` (L41).
- **Good:** waits are condition polls that print the daemon log on failure, ports are claimed by `flock`, pids come
  from files, never from `pgrep`; the pure policy functions are tested in milliseconds.

---

## 5. External dependencies

The list is short and matches DESIGN.md → Crates one for one. All 221 locked packages come from crates.io with
checksums, with no git or path sources, and every compiled crate has a permissive licence. Keeping the HTTP health
check, the thread-based concurrency and the error types hand-written holds up: a crate would bring in more code
than it removes. The bounds the hand-written health check lacks (L7) are a few lines, not a reason to take a crate.

| Crate | Used for | Assessment |
|---|---|---|
| clap 4.6.7 | CLI parsing | Defaults trimmed (`std, derive, help, usage, error-context`). Good. |
| jiff 0.2.39 | timestamps, the TUI's local clock | Trimmed to the system tzdb, no serde. Replaced unsafe `tzset`/`localtime_r`. Good. |
| libc 0.2.191 | four macOS-only `unsafe` calls in `process.rs` | Needed there; it is a dependency on every platform, and an unsupported Unix fails with an unclear error (I19). |
| ratatui 0.30.2 | TUI | 68 of the 93 crates. Its default features pull in `time` (beside jiff), its macros and the calendar widget, none of them used (L49). It is the source of the `syn` duplicate, and it turns signal-hook's defaults back on (I22). |
| rusqlite 0.41.1 (`bundled`, `backup`) | snapshot, restore, test copy | Used through the online backup API, as CLAUDE.md requires. The paced `run_to_completion(100, 10 ms)` is the cause of M2 and M7. The default `cache` feature is unused (L49). Bundled SQLite 3.54.1 compiles at `-O0` in dev, which slows the backups the tests run (I22). |
| rustix 1.1.6 | poll, a peeking recv, `waitid(WNOWAIT)`, `killpg`, umask, flock, the clock tick | Used only where std has nothing, except `flock`: std's `File::try_lock` (stable since 1.89) could replace it (I20). |
| serde 1.0.229, serde_json 1.0.154 | protocol, state, events | `deny_unknown_fields` on the manifest but not on `state.json`, which is right. `EventKind` lacks the `#[serde(other)]` that `ErrorCode` has (L25). |
| signal-hook 0.3.18 | self-pipe for SIGTERM/SIGINT/SIGHUP | Right tool (rustix has no handler API). `default-features = false` has no effect, because crossterm enables them (I22). |
| toml 1.1.8 | manifest | Only `toml::from_str` is called; the default `display` feature is unused (L49). |
| tempfile 3.27.0 (dev) | unit-test scratch directories | Good. |

Build and tooling: `panic = "abort"` works as its comment says, and the integration tests really run the abort
binary (I21). Two consequences: M3 (any implicit panic, such as `eprintln!` failing, ends the daemon) and the TUI's
hidden cursor after a panic (L23). `rust-version = "1.99"` is called "the floor" in `rust-toolchain.toml` and "the
release it is developed and tested on" in README.md, and the floating `stable` channel means nothing builds on 1.99
(L51). There is no CI, and the Linux-only paths (`/proc` parsing in `leftover`, `cp --reflink`) are never compiled
on this macOS setup (L52). No advisory checks, and the lander runs `cargo clippy`/`cargo test` without `--locked`
(L50). From the reviewers' knowledge, and unverified offline: no compiled version has a RustSec advisory.

---

## 6. /simplify, report only

The four /simplify angles (reuse, simplification, efficiency, altitude) ran over the whole of `src/` and `tests/` at HEAD, as a review rather than a diff, and **no code was changed**. Each finding was checked by a skeptic like the review findings. Where a /simplify finding is the same as a review finding, it is listed once in sections 2-3 and only pointed to here.

### Reuse

#### P1. `test` and `switch` repeat the find + allowed + refusal check that State::transition already does

*Low* · confirmed · found by `reuse/R2`

Where: `src/daemon/transactions.rs:350-353`, `src/daemon/transactions.rs:482-485`, `src/state.rs:345-349`

**Impact.** The rule "may this generation become X, and if not, why" lives in three places. A later change to the precondition (for example a new refusal case) has to be made in all three, or `test` and `switch` will accept a request that `transition` then refuses halfway through the step.

**Fix.** Add `State::can_become(&self, id, to) -> R<&Generation>` (find + allowed + refusal). Call it from `transition` before it mutates, and from `test` and `switch` as `let g = inner.store.state().can_become(id, GenState::Tested)?.clone();`.

#### P2. The `/bin/sh -c` builder (cwd, env, stdin null, own process group) is written twice in process.rs, and the log append opener duplicates state::open_append

*Low* · confirmed · found by `reuse/R7`, also `simplification/S7`

Where: `src/process.rs:36-54`, `src/process.rs:314-323`, `src/daemon/transactions.rs:77-82`, `src/state.rs:495-497`

**Impact.** The process-group rule, a CLAUDE.md invariant ("the app runs in its own process group"), is restated at every spawn site. A new spawn path that copies one of the two and drops `process_group(0)` would leave a child that `kill_group` cannot reach.

**Fix.** Add `fn shell(command: &str, cwd: &Path, env: &Env) -> Command` in process.rs. It returns `/bin/sh -c command` with cwd, env, null stdin and `process_group(0)`, and `spawn` and `run_check` only set stdout and stderr on it. Make `state::open_append` `pub(crate)` and use it for the log in `spawn`.

#### P3. Parsing a pid file is written four times

*Info* · plausible, severity low → info by the verifier · found by `reuse/R10`

Where: `src/process.rs:203-207`, `src/daemon/transactions.rs:1008-1012`, `tests/common/mod.rs:400-404`, `tests/core.rs:1101-1111`

**Impact.** There is one file format and four readers. The harness's Drop re-derives the `pid > 1` guard that `process::group` and `read_pid` each handle their own way.

**Fix.** In the crate, add `process::read_pid(path) -> Option<u32>` and use it in `leftover` and the transactions test. In the tests, move `read_pid` from core.rs into tests/common (as `Option<Pid>`, which `Pid::from_raw` already returns as None for 0) and use it in `Harness::drop`.

**Verifier.** All four readers exist. Only one is production code (`leftover`); the transactions one is a unit test. The integration tests cannot call crate code, so at best this becomes two pairs, each a one-line `read_to_string().trim().parse()`. `Harness::drop` cannot reuse core.rs's `read_pid`, which unwraps (a panic in Drop), and it still needs its own `pid > 1` guard, because `Pid::from_raw(1)` is Some and process group 1 must never be signalled. The gain is marginal. Info.

#### P4. Hand-written try_wait/sleep polling loops where a helper exists

*Info* · confirmed, severity low → info by the verifier · found by `reuse/R11`

Where: `src/daemon/mod.rs:843-849`, `tests/common/mod.rs:183-194`, `tests/common/mod.rs:389-398`

**Impact.** Several timing loops do the same thing. The daemon test polls every 10 ms while the event-driven helper sits one module away, and the harness has two copies of "wait up to N for the daemon to exit".

**Fix.** daemon/mod.rs: `assert!(process::wait_until(&mut crashed, Instant::now() + Duration::from_secs(5)).unwrap().is_some());`. tests/common: give `Drop` a call to the same logic as `daemon_exits_within`, for example a free `fn exits_within(child: &mut Child, within) -> bool` that both use.

**Verifier.** daemon/mod.rs:845 polls `try_wait` every 10 ms. `process::wait_until` (process.rs:87-98) reaps through `try_wait` in the same way, so the Child caches the status as this test needs, and it can replace the loop. supervisor.rs:740 is not equivalent: it waits for the exit unreaped (`exited().recv()`), so it is not a candidate and should not be cited as the same thing. In tests/common, `daemon_exits_within` and `Drop` duplicate the deadline loop, but Drop must not panic (it uses `.ok().flatten()`), so a shared helper has to avoid `unwrap`. Test-only, info.

#### P5. An Event with `seq: 0, at: now()` is built field by field in three places

*Info* · confirmed · found by `reuse/R12`

Where: `src/daemon/mod.rs:635-644`, `src/daemon/mod.rs:726-732`, `src/state.rs:637-645`

**Impact.** The rule "seq is assigned by the log, so start at 0" is spelled out at every construction. It is small, but a new field on `Event` has to be added to every literal.

**Fix.** Add `Event::new(kind, generation: Option<&str>, message: impl Into<String>) -> Event` in protocol.rs (seq 0, at `now()`). Use it in `Daemon::event`, in `log_panics` and in the state test.

#### P6. A fresh Generation record is written out with all thirteen fields in the daemon and again in the state tests

*Info* · confirmed · found by `reuse/R13`, also `simplification/S5`

Where: `src/daemon/transactions.rs:141-155`, `src/state.rs:619-635`, `src/state.rs:926`

**Impact.** Every new optional field (as `restore_abandoned_at` was) has to be added to both literals and defaulted the same way.

**Fix.** Add `Generation::staging(id, git_ref, commit, dir) -> Generation` in protocol.rs with the `None`/0 defaults. The stage path and the test helper (`Generation { state, ..Generation::staging(...) }`) then share it.

#### P7. The saturating u64-to-usize conversion is repeated inline

*Info* · confirmed · found by `reuse/R15`

Where: `src/daemon/server.rs:125`, `src/daemon/server.rs:136`, `src/daemon/transactions.rs:850`

**Impact.** This is minor noise. The intent (a request limit beyond the platform's range means everything) is restated each time.

**Fix.** Add a named helper next to `tenths` in daemon/mod.rs (`fn saturating_usize(n: u64) -> usize`), or parse `limit` straight into `usize` at the protocol boundary, so the daemon receives `Option<usize>`.

#### P8. Request::parse has a helper for text arguments but not for numbers and flags

*Info* · confirmed · found by `reuse/R17`

Where: `src/protocol.rs:174`, `src/protocol.rs:179-184`, `src/protocol.rs:265-267`

**Impact.** The lenient rule (absent or null means default, wrong type is ignored) is written five times. A future decision to refuse a wrongly typed number as a usage error, as `deadline_arg` already does, would mean editing each line.

**Fix.** Add `fn optional_u64(args, key) -> Option<u64>` and `fn flag(args, key) -> bool` next to `optional_text`, and use them in `parse`.

Same as a finding listed elsewhere in this review:

- `reuse/R1` A test builds the CLI command by hand instead of using Harness::cli, and so does not drop FAWKES_SOCKET: see **M10**.
- `reuse/R3` How an exit is described is written three times: LastExit::describe, the CLI's render_process and the TUI's last_exit: see **L55**.
- `reuse/R4` The CLI and the TUI each carry their own socket client: connect, write, quick-request timeout, timeout message, no-answer message: see **I25**.
- `reuse/R5` The TUI re-implements the daemon's abort-on-poison `lock` helper inline: see **L35**.
- `reuse/R6` `build` shortens the commit by byte slicing instead of using Generation::short_commit: see **I12**.
- `reuse/R9` Integration tests shell out to `kill -9` although rustix's kill_process is imported and used in the same file: see **L48**.

### Simplification

#### P9. Every manifest field is declared twice and copied by hand; reload's field lists are not checked by the compiler

*Low* · confirmed · found by `simplification/S3`

Where: `src/manifest.rs:25-47`, `src/manifest.rs:59-88`, `src/manifest.rs:255-279`, `src/manifest.rs:289-339`

**Impact.** A new manifest field means four edits: Written, Manifest, the copy in load, and one of the reload lists. The compiler enforces the first three. It does not enforce the fourth, so a field left out of `reload` changes silently on SIGHUP and is never named in `manifest reloaded, changed: …`.

**Fix.** In `reload`, destructure both sides exhaustively (`let Manifest { name, repo, state_dir, home, storage, port, ... } = new;`, without `..`), so a new field is a compile error until it is put in a list. To remove the duplication itself, deserialize straight into a `Written`-shaped struct and keep `Manifest` as `{ written: Written, state_dir: PathBuf, check: Check }` with paths resolved in place, or keep `Written` and build `Manifest` with `..` from a resolved `Written` (only `repo`, `home`, `state_dir` and `health` differ).

#### P10. `Pending` and `SwitchResult` are the same record

*Low* · confirmed · found by `simplification/S4`

Where: `src/protocol.rs:366-371`, `src/protocol.rs:523-529`, `src/daemon/transactions.rs:567-571`, `src/daemon/transactions.rs:601-605`

**Impact.** Two types with the same fields and the same wire shape. The values are cloned once more, and a field added to one (say `previous_commit`) is easily forgotten in the other, so `status.pending` and the switch result drift apart.

**Fix.** `pub type SwitchResult = Pending;` (or return `Pending` and drop `Reply::Switch`'s separate type). In `switch`, clone the `Pending` put into `Txn::Pending` and return that.

#### P11. The CLI handles a `--json` event on two paths with the same output

*Low* · confirmed · found by `simplification/S9`

Where: `src/main.rs:276-298`

**Impact.** For `--json` the result is the same whether or not the event parses: the raw `event` goes to stdout when following, else to stderr. The logic is written twice, the second time with `raw["event"]` indexing, which CLAUDE.md asks to avoid on the wire.

**Fix.** Test `if cli.json && let Some(event) = raw.get("event") { … ; continue; }` before typed parsing, whether or not it parses. The typed `Line::Event` arm then only handles text output: `if follow { println!(render_event) } else { eprintln!(render_progress) }`.

#### P12. The port-reload rule is split between `Manifest::reload` and the daemon, which then merges error strings

*Low* · confirmed, severity low → info by the verifier · found by `simplification/S10`, also `altitude/A2`

Where: `src/daemon/mod.rs:525-541`, `src/manifest.rs:289-313`

**Impact.** One rule ("port is fixed while something owns the live port") lives in two functions. The second has to re-combine the first's `Result` by string concatenation. The unit tests in manifest.rs cover only half of the rule.

**Fix.** Pass the reason the port is held to `reload`, for example `port_held: Option<&str>` (`Some("the app is running")` or `Some("switch g2 is in progress")`), and build the one message there. `reload_manifest` then shrinks to computing that reason and matching `Ok`/`Err`.

**Verifier.** The split and the string merge (`format!("{problems}; {moving}")`) are as described. The claim of half coverage is partly wrong: the manifest.rs unit tests cover only the app-running half, but the integration test at tests/core.rs:1219 covers the step-in-progress message. Passing a `port_held` reason into reload would be a little cleaner, but the current code is about 10 lines and both halves are tested, so info.

#### P13. `state.json`'s `daemon_pid` is written and never read

*Info* · confirmed · found by `simplification/S12`

Where: `src/state.rs:146`, `src/state.rs:160`, `src/daemon/mod.rs:414-416`, `src/daemon/mod.rs:591`

**Impact.** Two durable saves per daemon life (each `save` writes state.json and the `.bak`, with an fsync and a directory fsync each) and a field to keep in `State`, `StateFile` and the migration, for information nothing uses.

**Fix.** Drop the field and both writes. `StateFile` does not deny unknown fields, so old files still load. If it is meant for operators, document it in DESIGN.md's layout instead.

#### P14. `rollback_pending` and `abandon_restore` share the same ending, as do `stop_command` and `restart_command`

*Info* · confirmed · found by `simplification/S15`

Where: `src/daemon/transactions.rs:771-786`, `src/daemon/transactions.rs:814-829`, `src/daemon/lifecycle.rs:45-89`

**Impact.** Moderate duplication on the paths that finish a transaction. A change to how a finished rollback restarts the permanent generation has to be made twice.

**Fix.** Add `fn finish_rollback(&self, inner: &mut Inner, id: String, reason: String, message: String) -> RollbackResult`, used by both paths, and a `fn stop_current(&self, out) -> R<(String, bool)>` shared by stop and restart. This is optional: both are short, and readable as they are.

Same as a finding listed elsewhere in this review:

- `simplification/S1` How a LastExit reads as text is written three times, and the three disagree: see **L55**.
- `simplification/S2` Dead `(requested)` guard on Exited events in the TUI colouring: see **L60**.
- `simplification/S5` A full `Generation` literal is written out field by field in daemon and test code: see **P6**.
- `simplification/S6` Whether the confirm deadline has passed is computed in several places, in slightly different ways: see **L56**.
- `simplification/S7` Three hand-rolled ways to run a child in its own group with a timeout: see **P2**.
- `simplification/S8` The "has the live app already exited?" check is spelled out in five places: see **L54**.
- `simplification/S11` `draw_apps` uses a misnamed row struct and styles cells by magic column index: see **L59**.
- `simplification/S13` `AppView::rows()` allocates a Vec for every lookup: see **I14**.
- `simplification/S14` The poison-abort lock policy is inlined outside the `lock` helper: see **L35**.
- `simplification/S16` Integration tests re-implement harness helpers inline: see **L48**.

### Efficiency

#### P15. Every app start saves state.json durably even when nothing changed

*Low* · confirmed · found by `efficiency/E2`

Where: `src/daemon/supervisor.rs:381-387`, `src/state.rs:460-466`, `src/state.rs:308-312`, `src/state.rs:264-275`, `src/daemon/mod.rs:663-673`

**Impact.** Each start (switch, rollback, crash restart, start, restart) pays for re-serializing the whole state pretty-printed, two temp files, two renames and four full fsyncs. That is tens of milliseconds on macOS. The caller holds `inner` throughout, so `status`, `confirm` and every subscriber snapshot wait for writes that change nothing. A switch already saves twice before this (Switching and Pending).

**Fix.** Save only when the value changes. Guard it in `start_live` (`if why != Why::Crash && state.find(id).is_some_and(|g| g.failed_starts != 0)`), or let `Store::update` take a closure that returns whether it changed something. To keep "a later save repeats it", add a `dirty` flag to `Store` that a failed save sets, and save when `changed || dirty`.

#### P16. prune and the startup sweep run `git worktree prune` once per removed generation

*Low* · confirmed · found by `efficiency/E4`

Where: `src/daemon/transactions.rs:319-343`, `src/daemon/transactions.rs:852-869`, `src/daemon/transactions.rs:921-934`, `src/daemon/transactions.rs:956`, `src/daemon/transactions.rs:163`

**Impact.** Pruning k generations spawns 2k git processes, each with two pipe-draining threads and a process group, plus k durable saves (four fsyncs each), all while holding the step slot. The final sweep prune repeats the per-generation ones.

**Fix.** Take `worktree prune` out of `remove_generation` and run it once after the loop in `prune`, and once at the end of `sweep_generations`, which already does. The `stage` failure path, the third caller, then runs its own single prune. Keep the per-generation record removal and save: one batched save would leave records whose directories are gone if the daemon died mid-loop.

#### P17. Each change makes every subscriber clone and serialize status and history on its own

*Low* · plausible · found by `efficiency/E5`

Where: `src/daemon/server.rs:162-179`, `src/daemon/supervisor.rs:532-549`, `src/daemon/mod.rs:203-214`, `src/daemon/mod.rs:708-717`, `src/daemon/mod.rs:743-751`, `src/daemon/mod.rs:762-783`

**Impact.** With N subscribers (a TUI per terminal, `log --follow` does not count) every step produces N × (2 `inner` acquisitions + a full history deep clone + JSON serialization) several times over. Without `keep` the history grows without bound. The repeated `inner` acquisitions contend with the supervisor and with transaction steps.

**Fix.** Build the snapshot once per change rather than once per subscriber. For example, keep a change counter, and let the first subscriber that sees a new value build and serialize `Status` and `History` under a single `inner` hold, then cache the bytes (`Arc<[u8]>`) for the others. At the least, take `inner` once in `send_snapshot` for both. DESIGN.md says `status` and `history` are sent together on every change, so leaving out an unchanged history line would be a contract change and needs DESIGN.md.

**Verifier.** The code matches the evidence. stream coalesces only via `std::iter::once(first).chain(rx.try_iter())` (server.rs:163). send_snapshot calls self.status() and self.history() separately, each taking `inner`, and history deep-clones `generations` (supervisor.rs:546-548). status calls self.manifest() twice (name, and port via process()), though that is only a lock and an Arc clone. begin (mod.rs:781), Busy::drop (:213) and every non-progress event (:749-751) send Changed, and the supervisor's tick uses begin (supervisor.rs:153). DESIGN.md:430-432 requires fresh status and history whenever something changed, so dropping the history line would change the contract, as noted. Kept as plausible: the claim that bursts are 'seldom merged' is untested speculation, and with the typical N of 1-2 TUIs the cost is small. Taking `inner` once in send_snapshot is the cheap part of the suggestion.

#### P18. The prune after a confirm polls once a second instead of waiting for the slot

*Low* · confirmed · found by `efficiency/E9`, also `altitude/A1`

Where: `src/daemon/transactions.rs:888-904`, `src/daemon/mod.rs:203-214`, `src/daemon/mod.rs:258-290`

**Impact.** While a long install or test holds the step slot (`install_timeout` defaults to 30 min), the thread wakes every second to try `begin`, and the prune then starts up to a second after the slot frees. Two confirms in that window leave two pollers.

**Fix.** Wait on the daemon's `wake` (`let seen = wake.seen(); ... wake.wait(seen, None)`): `Busy::drop` already notifies it when the slot frees. Keep a flag so that one prune thread at a time waits.

#### P19. Daemon startup parses up to 20 MiB of events to keep the newest 1000

*Info* · confirmed, severity low → info by the verifier · found by `efficiency/E3`

Where: `src/state.rs:512-535`, `src/daemon/mod.rs:355-360`

**Impact.** A long-lived daemon restarts (after a crash or a launchd restart) only after allocating up to about 20 MiB of text and parsing on the order of 100k JSON lines, which takes noticeably longer in the debug builds the project uses. The startup recovery of a pending switch waits behind it.

**Fix.** Read the current file from its end: seek back in blocks until RECENT_EVENTS newlines are found, and parse only those lines. Open the rotated file only when the current one holds fewer than RECENT_EVENTS, again from its end. `seq` increases in file order, so the newest parsed line already gives `last_seq` (legacy lines are 0 and come first).

**Verifier.** The code is as quoted: both events.1.jsonl and events.jsonl are read whole with read_to_string, every line is parsed into an Event, and only the last RECENT_EVENTS are kept (state.rs:517-526). This happens before bind (mod.rs:356 vs 360). Downgraded to info. It is a one-time startup cost bounded by about 20 MiB. In a release build (how a daemon is deployed; debug is only the dev-workflow rule) parsing about 100k short lines takes tens of milliseconds, well below the leftover-stop waits that the same startup can incur (E12). The reverse block reader it recommends adds more code than it saves. The impact figures are estimates, not measurements.

#### P20. Each `wait_until` spawns a new `waitid` thread, even for a child that already has one

*Info* · confirmed · found by `efficiency/E8`

Where: `src/process.rs:87-98`, `src/process.rs:77-84`, `src/process.rs:233-256`, `src/daemon/supervisor.rs:364-370`, `src/daemon/transactions.rs:398-408`

**Impact.** Stopping the app creates one or two extra threads that sit in `waitid` on the same pid, and a long-running health command adds one thread per try. Every one of them ends when the child is reaped, so nothing leaks. It is wasted thread creation on stop paths.

**Fix.** Create the exit receiver once per child and pass it along: keep it in `App` next to `child`, and let `wait_until` and `stop_group` take `&Receiver<()>`. The receiver disconnects after its one message, so a later wait that sees `Disconnected` simply goes on to `try_wait`. Alternatively, have the existing `on_exit` callback notify a per-child condvar.

#### P21. Startup stops a leftover app and a leftover step one after the other

*Info* · confirmed · found by `efficiency/E12`

Where: `src/daemon/mod.rs:400-412`, `src/process.rs:233-244`, `src/manifest.rs:115`

**Impact.** This only happens after a daemon crash, but then the recovery (and the rollback of a pending switch it leads to) can wait up to 2 × (stop_timeout + 2 s) before the second group even receives its SIGTERM. That is 24 s with the default 10 s.

**Fix.** Signal both groups first, then wait for them under one shared deadline: split `stop_group` into a send step and a wait step, or run the two stops on scoped threads. Then the startup wait is bounded by one stop_timeout instead of two.

Same as a finding listed elsewhere in this review:

- `efficiency/E1` The storage snapshot and restore sleep 10 ms per 100 pages while the app is down: see **M7**.
- `efficiency/E6` Each event is deep-cloned 2 + 2N times on its way to N subscribers: see **I13**.
- `efficiency/E7` The TUI rebuilds per-event text in every frame, and with a filter scans the whole backlog: see **I14**.
- `efficiency/E10` The supervisor parses the pending deadline two or three times per pass: see **L56**.
- `efficiency/E11` A test polls 1000 events through a CLI process every 50 ms, and a comment cites a 20 ms accept interval that is gone: see **L58**.

### Altitude

#### P22. take_running throws away what it replaced, so stop and switch each peek at `live` first to learn whether a backoff was cancelled

*Info* · plausible, severity low → info by the verifier · found by `altitude/A3`

Where: `src/daemon/mod.rs:142-153`, `src/daemon/lifecycle.rs:44-65`, `src/daemon/transactions.rs:505-512`, `src/daemon/transactions.rs:519-530`

**Impact.** Each caller that needs to know about a cancelled restart has to read `live` before the call, in the right order, under the same lock. The comment at lifecycle.rs:57 ("`take_running_alive` cancelled the scheduled crash restart") describes a side effect that the return type hides.

**Fix.** Return what was taken: `enum Taken { App(App), Restart { until: Instant, at: String }, Nothing }` (Failed stays in place as it does now). `stop_command` matches `Taken::Restart` directly, and `switch` keeps the `Taken::Restart` value to restore in `resume`. Both pre-peeks go.

**Verifier.** The cited code is accurate: `take_running` maps `Live::Backoff` to None, `stop_command` peeks `matches!(inner.live, Live::Backoff { .. })` first, and `switch` clones `until`/`at` before the call and restores them in `resume`. But the side effect is not hidden: take_running's doc comment says "A scheduled crash restart is cancelled". Only 2 of the 6 callers care (switch, stop). The other four (shutdown mod.rs:584, rollback transactions.rs:721, reap supervisor.rs:279, restart via take_running_alive) would each have to unwrap a 3-variant `Taken` enum or call a helper. Both peeks are under the same lock hold as the take, so no ordering bug exists. Whether this is simpler overall is a judgement call. Accurate, but no defect.

#### P23. Wire names are kept twice (serde rename_all and hand-written as_str) and a test with hand-listed variants keeps them in step

*Info* · plausible · found by `altitude/A10`

Where: `src/protocol.rs:294-320`, `src/protocol.rs:374-417`, `src/protocol.rs:462-480`, `src/protocol.rs:916-960`

**Impact.** Each enum's variant names are written in three places. The test does not catch a new variant left out of its list, though `as_str`'s exhaustive match forces the name itself to exist.

**Fix.** Generate both from one list with a small `macro_rules!` (the file already uses `reply_from!`): the macro emits the enum with `#[serde(rename = ...)]` per variant and the matching `as_str`. Then the agreement holds by construction and the test can go.

**Verifier.** Accurate: three enums use `rename_all = "snake_case"` plus a hand-written `as_str`, and the test at 917 lists every variant again. It is also true that the test misses a new variant left out of its list. Whether a `macro_rules!` is simpler is doubtful. The enums carry per-variant doc comments (ProcessState::Failed), and EventKind may gain `#[serde(other)]` (A6). A macro that emits derives, docs, per-variant serde renames and `as_str` is harder to read and grep than today's explicit code, in a crate that avoids clever code on purpose. The duplication exists, but the proposed change is a trade-off, not a clear win.

#### P24. Subscriber threads re-check `shutting_down()` every 500 ms, though the hub's existing disconnect path could end every stream at once

*Info* · confirmed · found by `altitude/A11`

Where: `src/daemon/server.rs:153-161`, `src/daemon/mod.rs:236-248`, `src/daemon/mod.rs:569-593`

**Impact.** A small extra flag check on a timer. Whether shutdown ends a stream depends on the 500 ms tick, not on an event. The poll is still needed for `out.closed()`; the flag check is not.

**Fix.** At the end of `shutdown` (after the final `stopped` event), call `lock(&self.hub.0).clear()`: every stream sees `Disconnected` and ends, through the same path a slow subscriber takes. The `shutting_down()` term in the loop condition can then go.

Same as a finding listed elsewhere in this review:

- `altitude/A1` prune_soon retries `busy` with a 1 s sleep loop although the daemon now has a wake condvar that every slot release moves: see **P18**.
- `altitude/A2` The port-during-a-move check is a daemon-side special case bolted onto Manifest::reload, with error strings merged by hand: see **P12**.
- `altitude/A4` The 'exited but not yet reaped' window is guarded by hand at five sites, with two different policies for a try_wait error; abe54f0 added the fifth: see **L54**.
- `altitude/A5` The confirm deadline stays a String, so it is re-parsed, and its 'unreadable counts as passed' rule re-implemented, at five sites: see **L56**.
- `altitude/A6` EventKind has no `#[serde(other)]` fallback, so the CLI carries a raw-JSON special case and the TUI silently drops unknown events and their seq: see **L25**.
- `altitude/A7` A test keeps a workaround whose reason, a 20 ms accept loop, is gone: see **L58**.
- `altitude/A8` event_color matches on message text for a case the event kind already settles; the guard never fires: see **L60**.
- `altitude/A9` How an exit is described is written three times, in the protocol type, the CLI and the TUI: see **L55**.
- `altitude/A12` Tests still crash the app through the external kill(1) and a hand-copied `crash` closure, after 2d1baf8 moved test signalling to rustix: see **L48**.

---

## 7. Strengths

What the reviewers agreed is done well, and should be kept as the code changes:

- **The transaction is in the types.** `Txn` is one enum for the step in flight, so "pending and rolling back"
  cannot be represented. `Store::modify` (an intent, adopted only once on disk) and `Store::update` (bookkeeping)
  make durability a choice at each call site. `State::transition` is the one gate for generation states, pinned by
  a test over all 64 edges. M1 is the one place where the `modify` contract leaks.
- **Decide where the facts are.** `reap`, `decide_overdue` and `judge_unhealthy` record the rollback in the
  critical section that learns its cause, and `confirm` makes every refusal in the same hold as the commit, so no
  confirm can slip between a crash and its rollback.
- **Pure policy, injected clocks.** `after_exit`, `outcome` and `sleep_for` are pure and unit-tested in
  milliseconds, as CLAUDE.md asks.
- **A correct event-driven daemon.** The accept loop waits in `poll(2)` on the listener and a signal pipe. `Wake`
  reads its count before looking at the state and uses `wait_timeout_while`, so there is no lost or spurious wakeup.
  Child exits come from a `waitid(WNOWAIT)` thread that never reaps, which leaves `Child::try_wait` the only reaper.
- **Process hygiene.** Every child runs in its own group; signals go to groups through rustix's typed `Pid`;
  `group()` refuses 0 and 1; `stop_group` is bounded even for a leader that will not die. The start wrapper takes
  the pid file and the command as `$0`/`$1`, so nothing is spliced into the script.
- **Careful git and shell handling.** Refs are resolved with `--end-of-options` to a SHA before `worktree add`,
  repository hooks are disabled, and template values are expanded in one pass and shell-quoted.
- **Bounded resources on the daemon side.** `take(64 KiB)` on requests, 2 s write timeouts with a `gone` latch,
  bounded subscriber queues that drop a stalled reader, `run_bounded` for git and cp. M6 and L7 are the two gaps.
- **A typed protocol shared by daemon, CLI and TUI**, with wire names, error-code strings and the `status` field
  order pinned by tests, and `ErrorCode::Unknown` for forward compatibility.
- **Strict lint policy, actually clean.** Pedantic clippy, `unwrap_used`/`expect_used`, the cast lints,
  `undocumented_unsafe_blocks`. Five `#[expect(..., reason)]` and no `#[allow]`.
- **An isolated test harness.** `flock`-claimed port blocks, per-test directories, pids from files, condition
  polls that print the daemon log on failure, a `Drop` that cleans up after a failed test.
- **Docs that match the code.** Almost every DESIGN.md table (manifest fields and defaults, commands, error codes,
  generation record) matches the code one for one. The contract findings are gaps at the edges, not drift.

---

## 8. Findings the skeptics refuted

One refutation cuts across areas: `supervision/S9` is refuted here, while I11 (`rust/S14`) keeps the same `failed_starts + 1` as an info-level hardening. Both skeptics agree that valid input cannot reach the overflow; they differ only on whether a hand-edited `state.json` can, and `saturating_add` costs nothing either way.

- `supervision/S9` **`after_exit` adds to a count read from state.json without a check.** `failed_starts + 1` is there, but the premise that a hand-edited or corrupt state.json can bring a huge count to it does not hold. `reap` only judges an app this daemon started, and every first start goes through `start_live` with `Why::Fresh` or `Why::Restart` (mod.rs:503, lifecycle.rs:41/87, transactions.rs:525/579/780/823). For those, `start_live` sets `g.failed_starts = 0` in memory before the app can exit (supervisor.rs:381-387). Only `Why::Crash` keeps the count, and it follows a reap in the same daemon, so the count is bounded by `max_failed_starts` crashes since a fresh start. Edits to state.json while the daemon runs are never re-read. Reaching u32::MAX would need `max_failed_starts = u32::MAX` and about 4e9 crashes at a 30 s backoff. The overflow is unreachable, and the claimed abort loop also fails because each daemon start resets the count. `saturating_add` would only be a cosmetic hardening.

- `reuse/R8` **The health URL is split into authority and path by two separate parsers.** The two sites share only `strip_prefix("http://")` and a split at the first '/'. They work on different inputs: `has_port` checks the raw template, with `{port}` not yet expanded, and `http_get` reads the expanded URL. Both split the authority at the first '/' today, so they agree, including on edge cases like `http://h:3000?x` (both see `h:3000?x` as the authority, and `has_port` rejects it). The claimed disagreement is hypothetical, and a shared `split_http` would add API surface to save about two lines.

- `reuse/R14` **"Has the confirm deadline passed" is computed in three places, two ways.** Two of the three sites already share `deadline_secs`, and each is a one-line comparison. The third (`due`, supervisor.rs:202) differs on purpose. `deadline_secs` emits an Error event whenever the deadline is unreadable, and `due` runs on every supervisor pass, so calling it there would repeat that event on every pass. The silent `is_none_or` keeps the same rule (unreadable counts as passed) without the event. An `overdue` wrapper for two lines, plus a `quiet` flag for the third, is not simpler.

- `reuse/R16` **draw.rs has two near-identical duration formatters.** `duration` gives two-unit output with a float saturating cast (and an `#[expect]` explaining that cast), while `age` gives a single unit from a u64. The formats differ on purpose. Merging them needs a mode flag, or string surgery on the output of `duration`, and neither is simpler than two four-arm matches. Not equivalent enough to share.

- `critic/X3` **An inherited FAWKES_SOCKET silently overrides an explicit --manifest, so commands an app runs for another app go to its own daemon.** The code is as quoted: `socket_path` checks `--socket`, then `FAWKES_SOCKET`, then the manifest. DESIGN.md:356-357 states that exact order as the contract ("A client finds the daemon by `--socket`, else `FAWKES_SOCKET`, else `<state_dir>/fawkes.sock` from the manifest"), so the behavior is intended and documented. DESIGN.md:473 (`fawkes tui --socket PATH` (or `--manifest`) shows that one app only) does not contradict it: with FAWKES_SOCKET set, the TUI still shows a single app. The finding describes a design preference (explicit option before ambient variable), not a defect or a contract violation. It is reasonable as an info-level suggestion to reconsider the precedence for agents that run inside a supervised app.

---

## 9. Suggested order of work

Each phase is one branch with `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` clean, and
a reproducing test written first, as CLAUDE.md asks.

1. **The promise and the daemon's survival:** M1 (adopt `next` once `state.json` is renamed; report a `.bak` failure
   as an event), M3 (best-effort stderr, written outside `inner`), M2 and M7 (one `step(-1)` for snapshot and
   restore; a bounded single-transaction copy for `test`), L6 (SIGTERM during a rollback). M11 and M13 are the
   tests that pin this area, so they go in the same phase.
2. **The mark-good gate and the contract:** M4 (default `confirm` to `$FAWKES_GENERATION`, document
   `FAWKES_PHASE=live`), M6 (a per-request deadline, a connection cap), M8 (`--json` client errors as JSON, a new
   error code for the timeout), L13 (a `step` or `rollback` field in `status`), L14 (mistyped arguments refused),
   L7 and L8 (bounds in the health check), L25 (`#[serde(other)]` on `EventKind`). DESIGN.md changes go with each.
3. **Security hardening:** M5 (refuse a symlinked or foreign-owned `state_dir`, canonicalize it, move the examples
   off `/tmp`), L30 (database and `home` modes), L31 (harness paths), L32 (tests for the socket and directory modes).
4. **Tests:** M10 (only `Harness` builds a CLI), M12, M14, then L36 to L48.
5. **TUI:** M9, then L22 to L29.
6. **Quality and /simplify:** L54 and L56 (one "exited" accessor and one parsed deadline remove five sites each),
   L55 (one `LastExit` description), P9 (an exhaustive destructure in `Manifest::reload`), P2 (one bounded-child
   runner), L49 (trim features), L50 (cargo-deny, after asking, and `--locked` in the lander), then the rest.
