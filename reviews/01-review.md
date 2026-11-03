# Fawkes code review

Reviewed at `bfcac2d` (about 5,000 lines: daemon, CLI, TUI and tests). Every finding comes from reading the code. The
build and test results below come from running them on this commit.

## Summary

The codebase is in good shape for a prototype. The design document is precise, the code follows it closely, and the
comments explain *why* rather than *what*. Some of the harder problems are already handled well: SQLite snapshots
use the online backup API, whole process groups are killed, a rolled-back generation is final, and a failed restore
keeps `pending` so the restore is retried. `cargo fmt` is clean, default `cargo clippy` is clean, and all 7 tests pass.

The main risks come from the promise this tool makes: **"every upgrade either becomes permanent or rolls back"**.
Several paths can break that promise: a crash between two `state.json` writes, a panic in the middle of a switch,
writes that are never fsynced, and a reload that changes `home`. These matter more than any style issue.

| Severity | Count |
|---|---|
| High: can break the transactional guarantee or take the live app down | 5 |
| Medium: robustness, resource leaks, races | 9 |
| Low: correctness nits, consistency | 9 |

---

## 1. High severity

### H1. A huge `--deadline` takes the live app down for good
`src/daemon.rs:1245`

```rust
let deadline = state::format_time(state::now_secs() + 1 + deadline.unwrap_or(m.deadline));
```

`deadline` is a `u64` that comes straight from the client. In a debug build, `fawkes switch g3 --deadline
18446744073709551615` panics on overflow. By then the old app has been **stopped** (line 1224) and the snapshot is
taken, but `resume` never runs. The handler thread unwinds and `Busy` frees the slots, which leaves `permanent`
unchanged, `pending = None`, `app = None` and `restart_at = None`. Nothing restarts the live app, and `status` reports
`stopped`. In a release build the addition wraps around, the deadline lands in the past, and the switch is rolled back
right away.

**Fix:** validate the deadline at the boundary, for example `1..=7 * 86_400`, and use `checked_add`. More generally,
see A5: a panic inside a transaction must not leave the system half-done.

### H2. A crash between two `state.json` writes in `switch` skips the rollback
`src/daemon.rs:1254-1268`

`start_live` saves `state.json` with the new `app_pid` (line 575). `pending` and `GenState::Current` are written in a
**second** save (line 1268). If the daemon dies between the two saves (kill -9, OOM, a power cut), the next daemon
start finds `app_pid = <new gen>` and `pending = None`. It kills the new app and starts the permanent generation **on
storage the new generation may already have migrated**, without restoring the snapshot. This is the exact failure
Fawkes exists to prevent.

**Fix:** write the intent before the side effect. Record `pending` (and the target's `Current` state) and save
*before* spawning the new process. If the spawn fails, clear it again (or roll back through the normal path). Section
A2 generalizes this.

### H3. `state.json` and the snapshot index are not durable
`src/state.rs:185-189`, `src/storage.rs:65`

`State::save` does write-temp-then-rename, but it never calls `fsync` on the file or on the directory. After a power
loss on APFS or ext4, the rename can be durable while the data is not. That leaves a zero-length or truncated
`state.json`, and `State::load` then refuses to start (`corrupt …`). The supervisor stays down until someone repairs
the file by hand. `snapshot/index.json` has the same problem, and the rollback depends on it.

**Fix:**
```rust
pub fn save(&self, path: &Path) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(self)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}
```
Also consider keeping `state.json.prev` as a fallback for `load`.

Save errors are also only logged (`Daemon::save`, `src/daemon.rs:431`). A `switch` whose `pending` could not be
persisted carries on as if it had been. A transaction step should fail when its intent cannot be written.

### H4. SIGHUP reload can move `home`, `storage`, `port` or `state_dir` in the middle of a transaction
`src/daemon.rs:328-338`

`reload_manifest` swaps the whole manifest. Suppose a switch is pending and `home` or `storage` changed. The snapshot
was taken from the old home, but `rollback_pending` restores it into the **new** home (line 1363). That overwrites
the wrong databases, or restores nothing. A changed `name` or `state_dir` leaves the socket, the run link and the
paths inconsistent, because `paths` and `socket` are fixed at startup.

**Fix:** on reload, compare the new manifest with the current one and reject changes to identity fields (`name`,
`repo`, `state_dir`, `home`, `storage`, and `port` while an app runs). Only hot-apply `start`, `env`, `health`,
timeouts and the restart policy. Log what changed.

### H5. A panic in the watcher thread silently ends supervision
`src/daemon.rs:289-294`

The `tick` loop runs in a detached thread. If it panics, nothing restarts it. The daemon keeps answering on the
socket, so `status` looks normal, but crashes are no longer restarted and **missed deadlines are no longer rolled
back**. Panics can be reached from user input:

- `Duration::from_secs_f64` panics on negative, NaN or overflowing values. `stop_timeout`, `min_uptime`,
  `health.interval` and `health.timeout` are never validated, so `stop_timeout = -1` in the manifest panics the first
  time an app is stopped or reaped.
- The four `expect`s in `daemon.rs` are each guarded today, but they become panics if a future change breaks their
  invariants.

**Fix (all three):**
1. Validate the manifest in `Manifest::load` (see M8).
2. Set `[profile.release] panic = "abort"` (and the same for dev). launchd or systemd then restarts the daemon, and
   its startup path already recovers a pending switch. A crash you can recover from beats a zombie supervisor.
3. Alternatively, wrap `tick` in `catch_unwind` and log every panic as an `error` event.

---

## 2. Medium severity

### M1. Race: `confirm` can win against a rollback caused by a crash
`src/daemon.rs:770-815`

`tick` decides on a rollback ("exited before confirm") while holding `inner`, releases the lock, then calls `begin()`
and `rollback_pending`, and only that sets `rolling_back = true`. A `confirm` that arrives in that window succeeds,
because the deadline has not passed. That makes a **crashed** generation permanent. `rollback_pending` then sees
`pending = None` and returns `invalid_state`, which `tick` ignores. With `pending = None` and `restart_at = None`,
nothing restarts the app either. The deadline path does not have this race, because `confirm` checks the deadline;
the crash path does.

**Fix:** set `rolling_back = true` (or better, record the rollback intent) in the same critical section that decides
on the rollback.

### M2. `reap` holds `inner` for up to about 3 s
`src/daemon.rs:833-839`

`stop_group(pgid, 1.0, None)` plus a 2 s wait loop both run while `inner` is held. That blocks `status`, `confirm`,
every subscriber snapshot and the TUI. It also breaks the module's own rule ("never held across a slow step", line
3). A `confirm` sent near the deadline can be pushed past it and then refused. The extra loop is also mostly
redundant, because `stop_group` already waits until the group is gone or sends SIGKILL.

**Fix:** take the `App` out under the lock, mark it as "reaping", drop the lock, stop the group, then lock again to
record the outcome.

### M3. Subscribers have no back-pressure or write timeout
`src/daemon.rs:730-757`, `src/protocol.rs:50-55`

Each subscriber gets an unbounded `mpsc::channel`. If a TUI is suspended with Ctrl-Z, or a `log --follow` is piped
into a paused pager, its socket buffer fills, `write_all` blocks for ever, and its queue grows without limit. The
daemon leaks memory for as long as that client lives.

The same blocking write affects **progress lines inside a transaction**. `say()` in `switch` runs after the old app
was stopped, so a client that stops reading stalls the switch while the live app is down.

**Fix:** set `stream.set_write_timeout(Some(1s))` in `handle`. Use `mpsc::sync_channel(N)` for subscribers and drop
those that fall behind (`try_send` failure). For the transaction's own client, ignore write failures, which already
happens, and rely on the timeout.

### M4. `install` has no timeout, no process group, and is not stopped on shutdown
`src/daemon.rs:1437-1461`

A hung `pnpm install` holds the `Step` slot for ever, so every later `stage` and `test` gets `busy`. On SIGTERM the
daemon exits while the install keeps running, orphaned, in a worktree no generation record points to. Health checks
and apps already get process groups and timeouts; install should get the same treatment through `process::spawn`,
with its pgid tracked like `test_pgids`.

### M5. A failed startup leaves the socket and the run link behind
`src/daemon.rs:219-228`

The socket is cleaned up when `register` fails. It is not cleaned up when `State::load`, `EventLog::open` or
`current_exe` fail, so a corrupt `state.json` (see H3) leaves a socket file and a `run/<name>.sock` link behind. Use
a small RAII guard that removes both on drop, and disarm it once startup succeeds.

### M6. The stale app is killed by pid alone
`src/daemon.rs:265-274`

The design accepts pid reuse as a prototype risk. The consequence is worth spelling out: `killpg(pid, SIGTERM)`, then
SIGKILL, hits **whatever process group now has that id**, owned by the same user. That could be the user's shell or
editor after a reboot, for example, since `state.json` survives reboots.

**Fix (cheap):** store the process start time next to `app_pid` and compare it before killing. Use `proc_pidinfo`
(`PROC_PIDTBSDINFO`, `pbi_start_tvsec`) on macOS and `/proc/<pid>/stat` field 22 on Linux. Also store the boot time
or boot session UUID, and skip the kill entirely after a reboot.

### M7. The event log grows without limit and `tail` re-reads all of it
`src/state.rs:246-253`

Every `log`, `log --follow` and `subscribe` (that is, every TUI connection and every reconnect) reads and parses the
**whole** `events.jsonl` while holding the `log` mutex. `event()` needs that mutex, and it is often called with
`inner` held. A long-lived daemon gets slower over time, and lock contention ends up in `confirm`.

**Fix:** keep an in-memory `VecDeque<Event>` ring of the last N events for `tail`, and rotate the file by size
(`events.jsonl` → `events.1.jsonl`).

### M8. Manifest validation is thin
`src/manifest.rs:101-127`

Missing checks:
- `stop_timeout`, `min_uptime`, `health.interval` and `health.timeout` must be finite and `> 0` (otherwise H5 panics).
- `max_failed_starts >= 1`. With 0, the first crash marks the app `failed`.
- `name` must match `[A-Za-z0-9_-]+`. It becomes a path component in `run/<name>.sock` and in the default
  `state_dir`. A name such as `../x` escapes the run directory.
- `storage` entries must be relative and must not contain `..`. `home.join("/abs")` silently becomes `/abs`, and the
  restore then deletes or overwrites a file outside `home`.
- `port` must not fall inside `spare_ports`. Today that is skipped silently during `test`.
- `health.url` must start with `http://` and include a port. Today this only fails at check time, as
  `cannot resolve`.

### M9. Template expansion is not shell-safe
`src/manifest.rs:90-96`

`{home}`, `{code}` and `{state}` are pasted into `/bin/sh -c` strings without quoting, so a home path containing a
space or a `'` breaks `start`, `install` and `health.command`. The replacements also run one after another, so a
value that contains `{port}` (for example a path) gets expanded again.

**Fix:** do a single-pass expansion. Either shell-quote the values when the target is a command, or tell users in
`DESIGN.md` to use the env vars (`"$APP_HOME"`) inside commands and keep templates for `[env]` and URLs.

---

## 3. Low severity

| # | Where | Issue |
|---|---|---|
| L1 | `daemon.rs:955-963` | A `ref` that starts with `-` reaches `git rev-parse` as an option. Add `--end-of-options` before it. Low risk (only the owner can reach the socket), but cheap to fix. |
| L2 | `daemon.rs:978-1051` | A failed stage leaves `generations/gN/` behind (and orphaned worktrees if the daemon dies mid-stage). Remove the directory on error and run `git worktree prune` at startup. |
| L3 | `daemon.rs:1089-1090` | The free-port probe binds `127.0.0.1` only. An app that binds `0.0.0.0` or `::` can still collide, and the probe has an inherent TOCTOU gap. Also probe `0.0.0.0`, and tell a "port in use" failure apart from a health failure. |
| L4 | `daemon.rs:779`, `:1294` | Deadlines use wall-clock time. After a laptop wakes from sleep, or the clock jumps under NTP, a pending switch rolls back at once. That is probably the right call, but document it, and have the rollback reason say so when the deadline was overshot by a lot. |
| L5 | `state.rs:199-200` | A doc comment was pasted in the wrong place: the comment for `current()` ("The generation `start`, `stop`…") sits above `restore_failed`. |
| L6 | `main.rs:240` | The daemon's `usage` errors exit with `1`, but the documented contract says usage errors exit with `2`. Map `error.code == "usage"` to `EXIT_USAGE`. |
| L7 | `process.rs:42,46,53` | `pgid as i32` is never checked. A `0` (killpg(0) signals **our own** group) or a value above `i32::MAX` from a hand-edited `state.json` would misfire. Guard with `pgid > 1` and `i32::try_from`. |
| L8 | `daemon.rs:38-43` | `libc::signal` works on macOS and glibc, but `sigaction` with `SA_RESTART` is the portable choice. The handlers are also installed only after setup, so a SIGTERM during startup skips cleanup. Consider the `signal-hook` crate (`flag::register`), which matches what you do now. |
| L9 | `daemon.rs:404-415` | `umask` is process-wide. It is changed at startup before any other thread exists, which is safe today. Add a comment so nobody moves `bind` later. |

---

## 4. Architecture recommendations

### A1. One typed protocol shared by daemon, CLI and TUI
Today the protocol is held together by strings:
- Command names appear in `parse_cli`, `request_for`, `dispatch`, `render_result` and the TUI's actions.
- Error codes are `&'static str` (`protocol::Error.code`), compared with `e.code == "invalid_state"`.
- Event kinds are `String`, and the TUI colors them by string match.
- The client reads results with `r["test"]["port"]`, so a field renamed in the daemon breaks the CLI without any
  compile error.

**Proposal:** a `protocol` module with serde-tagged enums:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", content = "args", rename_all = "snake_case")]
pub enum Request {
    Start, Stop, Restart,
    Stage { r#ref: String },
    Test { generation: GenId },
    Switch { generation: GenId, deadline: Option<Deadline> },  // Deadline validates its range on deserialize (H1)
    Confirm { generation: Option<GenId> },
    Rollback, Status, History,
    Log { limit: Option<usize>, #[serde(default)] follow: bool },
    Subscribe { limit: Option<usize> },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode { Usage, NotFound, InvalidState, Busy, GitFailed, InstallFailed,
                     HealthFailed, StorageFailed, StartFailed, Internal, DaemonUnreachable }

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum EventKind { Daemon, Progress, Started, Healthy, Unhealthy, Exited, Failed, Stopped,
                     Staged, Tested, TestFailed, Switched, Confirmed, RolledBack, Error }
```

Add typed `Status`, `ProcessInfo` and `RollbackResult` response structs that the daemon serializes and the CLI and
TUI deserialize. The wire format stays exactly the same, but drift becomes a compile error, and the shape of
`DESIGN.md`'s protocol table is checked by the compiler. `GenState::as_str` can then go too, since serde already
knows the names.

### A2. Write-ahead transaction intents (generalizes H2)
`pending` is already a write-ahead record for one phase of the switch. Make it explicit and cover the whole step:

```rust
enum Txn {
    Switching { target: GenId, previous: Option<GenId>, phase: SwitchPhase }, // Stopping → Snapshotted → Started
    Pending   { target: GenId, previous: Option<GenId>, deadline: u64 },
    RollingBack { target: GenId, reason: String, restore_failed: bool },
}
```

Persist `state.txn` (and fsync, H3) **before** each side effect. Daemon startup then becomes one `match` on `txn`
with a defined recovery for every phase. This also replaces three flags that encode the same thing implicitly:
`rolling_back`, `restore_failed()` (derived from `GenState::RolledBack` + `pending`) and part of `app_failed`.

### A3. Model the live process as an enum, not flags
`Inner` has `app: Option<App>`, `app_failed`, `restart_at`, `last_exit` and `rolling_back`. Several combinations are
illegal: `app.is_some() && app_failed`, or `restart_at.is_some() && app.is_some()`. A sum type removes them:

```rust
enum Live {
    Stopped { requested: bool },
    Running(App),
    Backoff { until: Instant, failed_starts: u32 },
    Failed { reason: String },
}
```

`process_value`, `tick` and `reap` then become exhaustive `match`es, and the compiler finds the transitions nobody
handled.

### A4. Centralize generation state transitions
`g.state = …` is assigned in five places, each with its own precondition checks (`refusal` mixes those checks with
the error text). Use one `fn transition(&mut self, id, to: GenState) -> R<()>` that encodes the diagram in
`DESIGN.md`, plus a unit test that walks every legal and illegal edge. This makes the "rolled_back is final" invariant
something the code enforces, not a convention.

### A5. Panic and failure policy
Choose and document one policy (H5): `panic = "abort"` plus the init system restarting the daemon, plus the startup
recovery from A2. That is simpler and safer than auditing every thread for unwind safety. Poisoned mutexes are
already tolerated through `unwrap_or_else(|e| e.into_inner())`, which hides corrupted invariants. With an abort
policy, that pattern can go.

### A6. Split `daemon.rs` (1,461 lines)
It is easy to read today, but four concerns share one file and one `impl`:
- `daemon/server.rs`: bind, register, accept, `handle`, `dispatch`, `stream`
- `daemon/supervisor.rs`: `tick`, `reap`, `start_live`, `stop_app`, backoff
- `daemon/txn.rs`: stage, test, switch, confirm, rollback
- `daemon/mod.rs`: `Daemon`, `Inner`, `Busy`, lock-order docs

`tui.rs` (966 lines) is already split from `tui/draw.rs`. The connection threads (`reader`, `follow`, `scan`) could
move to `tui/net.rs`.

### A7. Injectable clock and process layer, for fast and deterministic tests
The integration tests are realistic, which is good, but they need `node`, `git` and `pgrep`, they bind fixed ports,
and they wait on wall-clock deadlines (about 8 s in total, and this grows with every scenario). Two seams would let
most of the state machine be tested in milliseconds:
- `trait Clock { fn now(&self) -> u64; fn instant(&self) -> Instant; }`, with a fake clock in tests that you advance
  by hand.
- `trait Spawner`, with a fake whose processes can be told to "exit with status N after T".

Keep the two end-to-end tests as smoke tests, and skip them with a clear message when `node` is not on `PATH`.

### A8. Event-driven instead of polling (later)
The accept loop polls every 20 ms, `tick` every 100 ms and subscribers every 500 ms. That is fine for one app.
Possible improvements, roughly in order of value:
- Child exit through `kqueue` `EVFILT_PROC`/`NOTE_EXIT` on macOS, or `pidfd` on Linux (`rustix` covers both). This
  also fixes M6 properly.
- A blocking `accept` that shutdown wakes by connecting to its own socket.

This does not need an async runtime. The thread-per-connection model suits this workload.

---

## 5. Missing tests (highest value first)

1. **Daemon restart with a pending switch** (kill -9 the daemon mid-pending → the restart rolls back and restores
   storage). This is the core promise and has no test today.
2. **Crash in the middle of `switch`** (H2), once fixed: crash after the spawn and before the `pending` save.
3. **Restore failure** (`chmod 000` the home, roll back → `failed`, `status` shows it, `rollback` retries and
   recovers).
4. **Crash loop of the permanent generation** → backoff 1, 2, 4 s → `failed` after `max_failed_starts`; then
   `start` clears it.
5. **`confirm` racing a crash rollback** (M1).
6. **Busy semantics**: `stage` during `test` → `busy`; `stop` during `switch` → `busy`; a rollback during a running
   `test` still happens (commit `d1b4b9e`).
7. **Manifest validation** unit tests (M8), and **`parse_cli`** unit tests (unknown flags, `--deadline` on the wrong
   command, argument counts).
8. **`parse_time` rejects garbage** (`"2026-13-45T99:99:99Z"` parses today and gives a wrong but valid time; a
   `pending.deadline` corrupted that way would never expire).
9. **Reload**: SIGHUP with a changed `start` applies at the next start; a changed `home` is refused (H4).
10. **A subscriber that never reads** does not grow the daemon's memory (M3).

---

## 6. Standard Rust tooling recommendations

### Cargo.toml
```toml
[package]
rust-version = "1.88"     # edition 2024 + let-chains (used throughout)

[lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_debug_implementations = "warn"

[lints.clippy]
pedantic = { level = "warn", priority = -1 }
# Pedantic lints that are noise for this codebase:
module_name_repetitions = "allow"
must_use_candidate = "allow"
too_many_lines = "allow"            # revisit after A6
# Lints that encode this project's rules:
unwrap_used = "warn"
expect_used = "warn"                # each of the 4 remaining should carry an #[expect] with its reason
cast_possible_truncation = "warn"
cast_possible_wrap = "warn"
undocumented_unsafe_blocks = "warn" # 9 unsafe blocks, none has a // SAFETY: comment

[profile.release]
panic = "abort"   # see H5 / A5
lto = "thin"
```

Running `clippy::pedantic` on this commit gives 53 warnings. Most are mechanical (16 redundant closures, 6 long
literals, `format!` appended to a `String`). The ones worth reading are the **casts**: 8 `usize → u16` in the TUI, and
`u32 → i32` / `u64 → i64` for pids and times, which is exactly L7. Run `cargo clippy --fix -- -W clippy::pedantic`
for the mechanical part and fix the casts by hand with `try_from`.

### `unsafe` audit
There are 9 `unsafe` blocks: `killpg` ×3, `signal`, `umask` ×2, `recv`, `localtime_r` ×2. All are sound as written.
Add `// SAFETY:` comments, and consider `rustix` (`rustix::process::kill_process_group`, `rustix::net::recv`) to
remove most of them while keeping zero C glue. `nix` works too, but it is heavier.

### Supporting tools
- **rust-toolchain.toml** pinning the stable version, so contributors and CI see the same lints.
- **cargo-nextest:** the two integration tests run in separate processes and report better on timeouts.
- **cargo-deny** (licenses + advisories) or **cargo-audit**. `rusqlite` with `bundled` ships C code, so advisories
  matter.
- **cargo-machete** to keep the dependency list as minimal as `DESIGN.md` intends.
- **cargo-mutants** on `state.rs` and `storage.rs`. It would show at once that `parse_time` has no negative tests.
- **CI** (GitHub Actions) on macOS and Linux: `fmt --check`, `clippy --all-targets -D warnings`, `nextest`,
  `deny check`. Linux matters because `clone_directory` has a separate Linux branch and nothing tests it.
- **Claude Code hooks** for this repository: a `PostToolUse` hook running `cargo clippy --all-targets` after edits to
  `*.rs` files, and a `Stop` hook running `cargo nextest run`.

---

## 7. Suggested order of work

1. H1 (range-check the deadline), M8 (manifest validation), `panic = "abort"`: small changes that close the input-driven
   panics.
2. H3 (fsync) and H2 (persist intent before spawn), plus tests 1–2.
3. M1, M2 and M3 (races, lock hold, back-pressure).
4. H4 (safe reload) and M4 (install timeout and process group).
5. A1 (typed protocol). After that, A2 to A4 together, since they touch the same code.
6. Tooling and CI, which can run in parallel with everything above.
