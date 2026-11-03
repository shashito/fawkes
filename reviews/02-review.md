# Fawkes code review (Fable 5.1)

Reviewed at commit `bfcac2d` on 2026-11-03. Every source file was read in full (`src/*.rs`, `src/tui/draw.rs`,
`tests/`, `examples/`, `DESIGN.md`), then `cargo build`, `cargo fmt --check`, `cargo clippy --all-targets`
(default and `-W clippy::pedantic -W clippy::nursery`) and `cargo test` were run on macOS with Node 26.

## Verdict

This is an unusually careful prototype. The design document is the contract and the code follows it; locking
rules are written down and mostly obeyed; state is written atomically; the app runs in its own process group;
storage is snapshotted with SQLite's backup API rather than a file copy; the socket is created `0600` with no
window; the two integration tests exercise the real binary end to end, and two of the test comments record bugs
found in earlier reviews. Default clippy is clean, rustfmt is clean, all 7 tests pass.

What remains are the things a prototype can defer but a supervisor cannot: a supervision thread that can die
silently, a state file that can be truncated by a power cut, a reap path that stalls every client for seconds, a
few manifest values that panic the daemon, a restart policy that discards a healthy upgrade, and a test suite that
covers the happy path and one crash but none of the recovery paths that justify the project.

The list below is ordered by what I would fix first.

## 1. Correctness and robustness findings

### 1.1 The supervision thread has no panic guard: a panic there turns Fawkes into a bystander (high)

`src/daemon.rs:288-294` spawns the watcher with a bare `thread::spawn`. If `tick()` ever panics (an `expect` in
`reap`, a poisoned-and-recovered mutex holding an inconsistent `Inner`, an arithmetic overflow in debug), the
thread ends, the panic message goes to stderr, and the daemon keeps answering `status` as if nothing happened.
From then on no deadline is enforced, no crash is restarted, no pending generation is rolled back. The whole safety
story of the project depends on this one thread.

Fix: make a watcher panic fatal so launchd/systemd restarts the daemon and the restart path (step 3 of "Daemon
start") rolls back whatever was pending.

```rust
// in Daemon::run, before spawning threads
std::panic::set_hook(Box::new(|info| {
    eprintln!("fatal: {info}");
    std::process::abort();
}));
```

or `panic = "abort"` in `[profile.release]`. Either is acceptable; the hook keeps the message. The handler threads
would then also abort the daemon on a panic, which is the right call for a supervisor: a half-finished `switch` is
exactly what the daemon-restart rollback exists for.

### 1.2 `state.json` is renamed into place without `fsync` (high for a "source of truth")

`src/state.rs:185-189` writes the temp file and renames it. On a power loss or kernel panic between the write and
the rename, APFS and ext4 can leave the renamed file empty or partial. `State::load` then returns `corrupt ...`
and `Daemon::run` refuses to start, which means the next boot runs nothing at all and a human has to repair JSON by
hand. The design document promises "atomic writes (temp + rename)"; atomic against crashes of Fawkes, not of the
machine.

```rust
pub fn save(&self, path: &Path) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(self)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}
```

Also: keep a `state.json.bak` of the last good file, and on `corrupt` fall back to it with a loud event rather
than refusing to start. The same applies to `snapshot/index.json` (`src/storage.rs:64-66`).

### 1.3 `reap` sleeps up to 3 seconds while holding the `inner` mutex (medium)

`src/daemon.rs:819-839`: `tick` takes `inner` and then `reap` calls `stop_group(pgid, 1.0, None)` (sleeps up to
1 s) followed by a loop waiting up to 2 s for the group to vanish. The module comment at the top of the file says
the lock "is never held across a slow step"; this is one. During those seconds every `status`, every `subscribe`
snapshot, the TUI, `Busy::drop`, the health threads (`is_running`) and `confirm` all block. A `confirm` sent by the
new generation right after it crashed a sibling process would wait on this lock and could then land in the window
described in 1.5.

Fix: `reap` should take the `Child` and exit status out of `inner` under the lock and do the group cleanup after
releasing it, then re-take the lock to record `last_exit` and the counters. This is the same pattern `stop_command`
already uses.

### 1.4 Manifest durations are not validated and a bad one panics the daemon (medium)

`src/manifest.rs:101-128` validates the health check and the spare port range, nothing else. `stop_timeout`,
`min_uptime`, `health.interval` and `health.timeout` are `f64` and reach `Duration::from_secs_f64`
(`src/process.rs:54,84,98`), which panics on a negative, NaN or infinite value. `stop_timeout = -1` makes the daemon
die on the first `stop`. A `SIGHUP` reload (`reload_manifest`) accepts the same values, so a typo in a running
system is enough. `deadline` passed as `u64` is added with `+ 1 +` at `src/daemon.rs:1245`; a huge `--deadline`
overflows (panic in debug, wraps to the past in release, which rolls the switch back immediately).

Add to `Manifest::load`:

```rust
for (name, value) in [("stop_timeout", m.stop_timeout), ("min_uptime", m.min_uptime),
                      ("health.interval", m.health.interval), ("health.timeout", m.health.timeout)] {
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid manifest {}: {name} must be a non-negative number", path.display()));
    }
}
if m.max_failed_starts == 0 { /* refuse or document that 0 means "never restart" */ }
if m.spare_ports[0] <= m.port && m.port <= m.spare_ports[1] { /* allowed, but say so in the docs */ }
if m.name.is_empty() || m.name.contains('/') { /* it becomes a file name under run/ */ }
```

and use `saturating_add` for the deadline.

### 1.5 `confirm` can make a generation permanent after its process has already crashed (medium)

Rollback for a crashed pending generation is decided in `tick` (`src/daemon.rs:782-786`) by looking at the
daemon-wide `last_exit`, 100 ms after the fact at the earliest, and only after `begin("rollback", Slot::Live)`
succeeds. `confirm` (`src/daemon.rs:1281`) checks `rolling_back`, the restore flag and the deadline, but not whether
the pending generation is actually alive. So a `confirm` that arrives after the crash but before the tick is
accepted: the crashed generation becomes permanent, the rollback then finds nothing pending and is ignored, and the
next tick restarts the crashed code as the permanent generation with the crash-restart policy. The sample app's
own confirm is sent from inside the process, so in practice this needs a crash right after the request leaves,
but an agent calling `fawkes confirm` from outside hits it whenever its timing is unlucky.

Two changes close it:

- `confirm` refuses when the pending generation is not running and its last exit was not requested (or, simpler,
  refuses whenever `inner.app` is `None` and `last_exit` is unrequested for that generation).
- Decide the rollback reason in `reap`, where the knowledge is, by setting a field such as
  `inner.pending_crashed: Option<String>`, instead of reconstructing it in `tick` from a global `last_exit` that
  may belong to another generation. Today the reconstruction is correct only because of a chain of invariants
  (`app` is only set to `None` by `reap` or by a path that calls `stop_app`); a future change to `restart_command`
  or `switch` would silently break it.

### 1.6 Two daemons can both bind the same socket (medium)

`bind` (`src/daemon.rs:393-415`) does exists → connect → remove → bind. Two daemons started at the same moment
(launchd retry plus a human) both see "stale", both remove, both bind; the second removes the first's socket file.
`register` (`src/daemon.rs:374-390`) has the same window on the `run/` link. The documented pid-reuse risk in
"Daemon start" step 2 is the same class of problem.

Fix with one `flock(2)` on `<state_dir>/lock`, taken exclusively and non-blocking before anything else and held
for the daemon's life. It is six lines with `libc`, it replaces the stale-pid heuristic (a locked file means a
live daemon, pid reuse or not), and it makes "another daemon already runs" exact.

### 1.7 A daemon restart always discards a healthy pending upgrade (design, medium)

"Daemon start" step 3 rolls back any pending switch with reason `daemon_restart`. A `brew upgrade` of Fawkes, a
launchd reload, or an `OOM` of the daemon during the 120 s confirm window therefore undoes an upgrade that was
about to confirm, and the agent sees `rolled_back` with no fault of its own. The deadline is already absolute in
`state.json` for exactly this reason ("so it survives a daemon restart", "On-disk layout").

Recommended change: on start, if a switch is pending and its deadline is in the future and the storage snapshot
exists, resume it: start the pending generation on the live port and let the ordinary deadline, confirm and crash
rules apply. Roll back only when the deadline has passed, the snapshot is missing, or the pending generation is
already `rolled_back` (restore retry). This is strictly safer for the agent and no less safe for storage, since
the snapshot was taken before the pending generation ever ran.

### 1.8 `subscribe` can replay an event twice; events have no sequence number (low)

`stream` (`src/daemon.rs:730-738`) joins the hub, then reads the file tail. An event logged between the two is in
both the replay and the channel. The commit message chose duplicates over gaps, which is right, but clients cannot
tell the difference because `Event` has only a one-second `at`. Add `seq: u64` (a daemon-wide counter, persisted
or restarted from the file's last line) to `Event`; the TUI can then dedupe, `log --follow` can resume, and
`subscribe` can take `since` instead of `limit`.

### 1.9 Smaller correctness notes

- `stop_group` (`src/process.rs:52-73`) returns right after `SIGKILL` without waiting for the group to disappear.
  `reap` works around it with its own 2 s loop; `stop_app` (used before `snapshot` and `restore`) does not, so a
  straggler child can still hold a storage file open for a few milliseconds while the backup runs. Give
  `stop_group` a short post-kill wait (`while group_alive(pgid) && !timed_out`), and remove the loop from `reap`.
- `EventLog::tail` (`src/state.rs:246-253`) reads and parses the whole `events.jsonl` on every `log`, `subscribe`
  and TUI reconnect. The file is append-only and never rotated; after months it is a visible stall per connect. Read
  the last N lines from the end, and rotate (or cap) the file.
- `logs/app.log` per generation is also unbounded; a chatty long-lived permanent generation fills the disk with no
  rotation. Either rotate in `process::spawn` or document that the app should log elsewhere.
- `stage` increments and saves `next_id` before the worktree exists (`src/daemon.rs:978-989`); if the daemon stops
  mid-install the worktree is left registered in the repo and on disk with no generation record, and
  `git worktree prune` is never run. Also `create_dir_all(generation(id))` happens before `git worktree add`, so a
  failed `add` leaves an empty generation directory. Record the generation as a new `staging` state before the
  slow work (so it shows up in history and can be cleaned up), or clean up on both failure paths.
- `stage` passes `{home}`, `{code}` and `{state}` into a `sh -c` string unquoted (`src/manifest.rs:91-96`). A path
  with a space or `$` breaks `install`, `start` and `health.command`. Shell-quote the substituted values (a short
  `'...'` with `'\''` escaping) or document the limitation; the `FAWKES_*` environment variables are the safe route
  and could be extended with `FAWKES_HOME`, `FAWKES_PORT`, `FAWKES_CODE`.
- `register` and `fawkes_home` use the daemon's `HOME`; a daemon started by launchd with a different environment
  from the user's shell links into a different `run/` than the TUI scans. Worth one line in the docs.
- `local_clock` (`src/tui/draw.rs:119-126`) calls `localtime_r` without ever calling `tzset`; glibc and macOS
  usually get it right, but POSIX does not require `localtime_r` to read `TZ`. Call `libc::tzset()` once in
  `tui::run`.
- `width_of` (`src/tui/draw.rs:180-182`) counts chars, not columns. `⏱` and `⟳` are double-width in many
  terminals, so the apps table can misalign by one cell per such symbol. ratatui already depends on `unicode-width`;
  use `Span::width()` or `UnicodeWidthStr::width`.
- `tui::request` (`src/tui.rs:577-590`) has no read timeout. A daemon that accepts and hangs leaves the action
  thread and the `label…` notice forever. `set_read_timeout(Some(30 s))` after `open`.
- `parse_time(...).unwrap_or(0)` at `src/daemon.rs:779,1294` turns a corrupt deadline into an instant rollback.
  That is the conservative outcome, but log an `error` event when it happens so the operator learns why.

## 2. Security

The threat model is a local, single-user supervisor whose client is the supervised app itself, so most of this is
hygiene rather than exposure.

- The socket is `0600` and created without a race; good. The state directory, `run/`, `logs/` and `snapshot/` are
  created with the umask, so with a `022` umask the app's stdout, the install log and the storage snapshots
  (copies of the agent's SQLite databases) are world-readable. Create `state_dir` with `0700`
  (`DirBuilderExt::mode`) and let the rest inherit.
- The manifest is trusted input and is executed through `sh -c`; this is by design. The reload on `SIGHUP` means
  anyone who can write the manifest can run commands as the daemon's user at the next start; the manifest file
  should therefore not be writable by the app it supervises. One sentence in `DESIGN.md`.
- `storage` entries are joined onto `home` without normalisation (`../x` escapes it). Manifest-controlled, so not
  an exploit, but `restore` deletes files at those paths; reject components of `..` and absolute paths in `load`.
- Requests are parsed from one unbounded `read_line` (`src/daemon.rs:674`); a local client can make the daemon
  allocate without limit. `take(64 * 1024)` on the reader is enough.
- The `unsafe` blocks (`signal`, `killpg`, `umask`, `recv`, `localtime_r`, and `kill`/`killpg` in the tests) are
  small and sound. `umask` is process-global and is changed on the main thread before any other thread exists,
  which is the only safe moment; add a comment saying it must stay there.

## 3. Standard Rust recommendations

**Cargo.toml.** Add `rust-version = "1.88"` (let-chains and edition 2024 need it), `license`, `description`,
`repository`, and a `[lints]` table so the clippy baseline is in the repo rather than in people's heads:

```toml
[lints.rust]
unsafe_op_in_unsafe_fn = "warn"
missing_debug_implementations = "warn"

[lints.clippy]
pedantic = { level = "warn", priority = -1 }
cast_possible_truncation = "allow"   # pid/width casts are reviewed
module_name_repetitions = "allow"
too_many_lines = "allow"

[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

Pedantic currently reports 128 warnings; the ones worth acting on are the 12 `significant_drop_tightening`
(mutex guards that live longer than their last use, e.g. `src/daemon.rs:708,721,888`) and the `cast_possible_wrap`
on `pgid as i32` in `src/process.rs:42,46,53` (a `pid_t` helper that does the one checked conversion). The rest
(`use_self`, `redundant_closure`, `option_if_let_else`) are taste; allow them in the table rather than churn the
code.

**Error types.** Four different error shapes coexist: `Result<_, String>` in `manifest`, `storage`, `git`,
`clone_directory` and `run_install`; `protocol::Error { code: &'static str, message }` in the daemon; raw
`eprintln!` strings in `main`; `std::io::Error` in `process`. The code `&'static str` is compared with `==` at
`src/daemon.rs:810`. Introduce `protocol::Code` as an enum with `serde(rename_all = "snake_case")` and an
`Error { code: Code, message: String }`, and give `storage` and `manifest` their own small enums (or `thiserror`
if a dependency is acceptable; it is the one crate that pays for itself immediately). Matching on `Code::Busy`
instead of `"busy"` also lets the TUI and tests stop comparing strings.

**Typed protocol.** The daemon serialises `Generation`, `Pending`, `Event` and `LastExit` from typed structs but
the clients (`main.rs`, `tui.rs`, `draw.rs`, the tests) index `serde_json::Value` with string keys in about 150
places (`s["pending"]["deadline"]`, `p["state"] == "running"`). A typo is a runtime `-`. Move the four structs
plus a new `Status` and `Process` into `protocol.rs`, derive `Deserialize` on all of them, and have the TUI hold
`Status` and `Vec<Generation>` instead of `Value`. `draw.rs` becomes shorter and the compiler checks the field
names. The integration tests can keep using `Value` as a black-box check of the wire format.

**Module size.** `daemon.rs` is 1,461 lines across five `impl Daemon` blocks that are already labelled startup,
shared helpers, request handling, supervisor, lifecycle and transactions. Split it along those labels into
`daemon/{mod,server,supervisor,lifecycle,transactions}.rs`; nothing else changes.

**Polling.** Three sleeping loops: the accept loop at 20 ms (`src/daemon.rs:297-313`), the watcher at 100 ms, and
`stop_group`/`run_check` at 20 ms. On a laptop this is a measurable idle wake-up rate for a process that should be
asleep. The accept loop can block on `poll(2)` with a 100 ms timeout (one libc call) and check the flags after it;
the watcher can be driven by a `Condvar` notified by `reap`-relevant changes with a timeout equal to the next
deadline or `restart_at`. Not urgent, but it is the kind of thing that gets noticed in `powermetrics`.

**Signals.** `libc::signal` is fine on both targets, but `sigaction` with an explicit `sa_flags` (`SA_RESTART`)
documents the intent and costs five lines. The handler is correctly async-signal-safe (an atomic store).

**Docs.** The module docs are good. Add `#![deny(missing_docs)]`-level coverage for the `pub` items in
`protocol.rs` and `state.rs` since they are the on-disk and on-wire contract, and a `CONTRIBUTING`-style note on
how to run the integration tests (they need `node` ≥ 22.5 for `node:sqlite`, `git`, free ports 4800–4819 and
`/tmp/fk48xx`; none of that is written down).

**CI.** There is none. A workflow with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test` on `macos-latest` and `ubuntu-latest` (the `cp --reflink` and `killpg` paths differ), plus
`cargo deny check` (advisories, licenses, duplicate crates) catches regressions in the paths the tests already
cover. `cargo audit`, `cargo deny`, `cargo outdated` and `cargo machete` were not installed here, so the dependency
tree was not checked for advisories; `cargo-deny` is the one to add.

**Dependencies.** Six crates, all justified in `DESIGN.md`. `rusqlite` with `bundled` compiles SQLite in; fine
for a prototype, but note that it pins the SQLite version the backup API talks to while the app (Node's
`node:sqlite`) uses another. The backup API is format-stable across versions so this is not a bug, just something
to know when a WAL-format change appears in release notes.

## 4. Tests

What exists is good: two end-to-end tests against the real binary, a WAL snapshot/restore test that checks the
uncheckpointed rows, a time round-trip, and three TUI tests that render into a `TestBackend` and check keys.
Coverage of the recovery paths, which are the reason the project exists, is thin:

| Path | Tested |
|---|---|
| stage → test → switch → confirm | yes |
| missed deadline rolls back code and storage | yes |
| crash before confirm rolls back and kills the group | yes |
| stop / start / restart, restart counter | yes |
| crash restart with backoff; `max_failed_starts` → `failed` | no |
| `stop` cancels a scheduled crash restart | no |
| `fawkes rollback` by hand; `rollback` with nothing pending | no |
| storage restore failure → `failed`, retry via `rollback` | no |
| daemon restart with a pending switch (`daemon_restart`) | no |
| daemon restart kills the app left by a crashed daemon | no |
| `busy` from a concurrent step; test runs while a switch is pending | no |
| `stage` with a failing install; `test` that fails health; retest | no |
| `switch` to a retired generation after a new test | no |
| `log --follow`, `subscribe` replay, `SIGHUP` reload | no |
| manifest validation errors | no |
| state machine transitions (`refusal` messages for every state × action) | no |

Recommendations:

1. **A pure state-machine test.** Pull the transition rules out of `test`, `switch`, `confirm` and
   `rollback_pending` into one function `fn allowed(state: GenState, action: Action) -> Result<(), Refusal>` on
   `Generation` and table-test it in `state.rs`. This is the cheapest test in the project and it is the one that
   guards the invariants ("rolled_back is final", "only tested can be switched to", "exactly one permanent").
2. **A supervisor test without processes.** `tick`, `reap` and the backoff arithmetic depend on `Child` only
   through `try_wait`. Put the exit-status handling behind a tiny trait (or pass `Option<ExitStatus>` and
   `ran: f64` into a pure `fn after_exit(...) -> Outcome`) and unit-test the backoff sequence, the counter reset
   after `min_uptime`, and the `failed` threshold in milliseconds instead of seconds.
3. **Four more integration tests** using the existing harness and the sample app's `MODE` file: `crash` for the
   backoff and `failed` path, a manifest edit for the restore-failure path (make a storage path a directory before
   rollback), a `kill -9` of the daemon with a switch pending for the restart path, and a `pgrep`-free check of the
   leftover-app path. The `pgrep -f "sleep 621"` assertion in the current crash test matches any process on the
   machine with that string; use the pid from `status` instead.
4. **Dynamic ports.** `Harness::new(4800, ...)` hard-codes ports and `/tmp/fk4800`, so two checkouts (for example
   this review and the one running in parallel) cannot run the suite at once. Bind port 0 to find a free base, or
   derive it from the test name.
5. **Run the TUI against a daemon once** (`fawkes tui --socket`) in CI with a `TestBackend` is not possible, but a
   test of `reader`/`follow` against a fake socket server (a `UnixListener` in the test that writes two lines and
   closes) would cover the reconnect and `Gone` logic that currently only runs by hand.

## 5. Architecture changes to consider

These go beyond fixes; each is a deliberate design change with a trade-off.

1. **Resume instead of roll back on daemon restart** (1.7). The single largest improvement for the agent's
   experience, and consistent with the stated model.

2. **A `Supervisor` that owns the process and a `Store` that owns `state.json`**, instead of one `Inner` behind one
   mutex. Today `Inner` mixes persistent state (`State`), the live `Child`, scheduling (`restart_at`), and
   bookkeeping (`busy`, `moving`, `rolling_back`). The lock is coarse because the struct is. Splitting the
   persistent part behind its own small type with `load`/`save`/`modify(|s| ...)` makes "every change is saved"
   enforceable in one place (today `self.save(&inner)` is called by hand in 14 places and it is easy to miss one:
   `reap` saves, `tick`'s crash restart saves through `start_live`, `confirm` saves; a future field will be
   forgotten). The live-process part can then be locked only around `try_wait`/`take`.

3. **Decide outcomes where the facts are** (1.5). `reap` knows which generation died and whether it was pending;
   let it produce `Outcome::RollBack(reason) | Outcome::RestartAt(Instant) | Outcome::Failed`, and let `tick`
   only execute. The code gets shorter and the rollback reason stops depending on a daemon-wide `last_exit`.

4. **Sequence-numbered events and `since` on `subscribe`** (1.8). It turns the event log into a proper journal:
   clients resume exactly, duplicates disappear, and `history` could eventually be derived from events rather than
   stored twice (`state.json` keeps the current picture; `events.jsonl` keeps how it got there; today both carry
   `rolled_back_at`/`rollback_reason` and can disagree after a crash between the two writes in `rollback_pending`).

5. **One lock file instead of pid heuristics** (1.6). `flock` on `<state_dir>/lock` is the standard answer for a
   daemon with a state directory and it removes both the stale-socket dance and the "pid reuse is accepted"
   caveat in `DESIGN.md`.

6. **A typed wire protocol shared by daemon and clients** (section 3). The TUI, the CLI renderers and the tests
   stop re-deriving the schema from string keys.

7. **Generation pruning with a retention rule** (`keep = 5` in the manifest, never the permanent, the pending or
   the pending's previous). It is listed under "Later", but disk growth is linear in upgrades and the agent will
   upgrade itself often; `git worktree remove` plus `rm -rf snapshot/` is a small `prune` command and a one-line
   call after every `confirm`.

8. **Make the health check matter a little.** Health never confirms, by design, and that should stay. But a
   pending generation whose health check fails for the full `health.timeout` is going to be rolled back at the
   deadline anyway; rolling it back at `unhealthy` (reason `health check failed`) returns the agent to a working
   system `deadline - timeout` seconds sooner. Make it a manifest switch (`rollback_on_unhealthy = true`) so
   apps that are slow to warm up keep the current behaviour.

## 6. Things that are right and should stay

- Stop before snapshot, snapshot before start, restore before restart: the ordering in `switch` and
  `rollback_pending` is correct and the comments say why.
- The `Busy` guard with two slots is a clean way to express "stage can overlap a rollback but a switch cannot";
  its `Drop` releases on every early `return` and on unwinding.
- `unwrap_or_else(|e| e.into_inner())` on every lock: the daemon survives a poisoned mutex instead of cascading.
- Process groups with `process_group(0)`, `killpg`, and the explicit "the leader is gone; end the rest of its
  group" in `reap` (with the test that found the bug).
- `umask(0o177)` around `bind`.
- `EventLog::append` ignoring a full disk, with the comment that says so.
- Hand-written argument parsing, HTTP GET and civil-time conversion with a round-trip test: each is small enough
  that a crate would cost more than it saves, and the design document says so.
- The TUI's "selection never moves on its own" rule and the test that pins it.

## 7. Suggested order of work

1. Panic hook / abort (1.1), `fsync` in `save` (1.2), manifest validation (1.4). Half a day, no design change.
2. `reap` outside the lock (1.3), `stop_group` post-kill wait, `confirm` liveness check (1.5). One day.
3. `flock` (1.6), `Code` enum and typed `Status`/`Process` in `protocol.rs` (section 3). One to two days.
4. State-machine table + pure supervisor tests, four more integration tests, dynamic ports, CI (section 4).
5. Resume on daemon restart (1.7), sequence-numbered events (1.8), pruning (5.7).
