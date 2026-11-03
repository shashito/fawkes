# Plan: fixing the findings of the review at `bfcac2d`

Sources: [01-review.md](01-review.md) (review 1) and [02-review.md](02-review.md) (review 2, Fable 5.1). Both
reviewed the same commit. Where they found the same issue it is listed once and cites both (`1:H3` means review 1,
finding H3; `2:1.2` means review 2, section 1.2).

The order follows the project's promise, *every upgrade either becomes permanent or rolls back*: first what can break
it, then races and stalls, then the typed protocol and state model that keep it from breaking again, then tests and
tooling. Each phase is one branch that lands on its own, with `cargo test`, `cargo clippy --all-targets -- -D
warnings` and `cargo fmt --check` clean.

## Phase 1: close the paths that break the promise (small, no design change)

| # | Fix | From |
|---|---|---|
| 1.1 | Panic policy: `panic = "abort"` for dev and release, plus a panic hook that logs an `error` event and the message first. A dead watcher thread becomes a daemon restart, which the startup recovery already handles. Drop the `unwrap_or_else(into_inner)` poison recovery, which only hides broken invariants. | 1:H5, 1:A5, 2:1.1 |
| 1.2 | Range-check `--deadline` (`1..=7*86400`, enforced in the protocol type) and use `checked_add`; refuse before anything is stopped. | 1:H1, 2:1.4 |
| 1.3 | Manifest validation in `Manifest::load` (also applied on reload): durations must be finite and > 0, `max_failed_starts >= 1`, `name` must match `[A-Za-z0-9_-]+`, `storage` must be relative with no `..`, `port` must be outside `spare_ports`, and `health.url` must be `http://` with a port. | 1:M8, 2:1.4, 2:§2 |
| 1.4 | Durable writes: fsync the file and the directory in `State::save` and for `snapshot/index.json`, and keep a `.bak` of the last good file that `load` falls back to with a loud event. A failed save of a transaction's intent fails that step instead of being only logged. | 1:H3, 2:1.2 |
| 1.5 | Write the intent before the side effect in `switch`: persist `pending` and `Current` before spawning the new generation, and clear it (via the normal rollback) if the spawn fails. | 1:H2 |
| 1.6 | Safe SIGHUP reload: refuse changes to `name`, `repo`, `state_dir`, `home`, `storage`, and `port` while an app runs; hot-apply only `start`, `env`, `health`, timeouts and the restart policy, and log what changed. | 1:H4 |
| 1.7 | One `flock` on `<state_dir>/lock` held for the daemon's life. It replaces the stale-socket and pid heuristics and makes "another daemon runs" exact. | 2:1.6, 2:5.5 |

Tests in this phase: daemon `kill -9` while a switch is pending → storage restored (1:§5.1); a crash between spawn and
the `pending` save (1:§5.2); manifest validation unit tests; a huge `--deadline` is refused with the old app still up.

## Phase 2: races, stalls and leaks

| # | Fix | From |
|---|---|---|
| 2.1 | `confirm` racing a crash rollback: `reap` decides the outcome where the facts are (`RollBack(reason)`, `RestartAt`, `Failed`) and records the rollback intent in the same critical section. `confirm` refuses when the pending generation is not running. | 1:M1, 2:1.5, 2:5.3 |
| 2.2 | `reap` outside the lock: take the child out, drop the lock, stop the group, then record the outcome. `stop_group` waits after SIGKILL until the group is gone, so `stop_app` before a snapshot or restore never races a straggler, and `reap`'s own 2 s loop goes. | 1:M2, 2:1.3, 2:1.9 |
| 2.3 | Back-pressure: a write timeout on every client socket, a bounded `sync_channel` per subscriber that drops a subscriber when it falls behind, `take(64 KiB)` on request reads, and a read timeout in the TUI's `request`. | 1:M3, 2:§2, 2:1.9 |
| 2.4 | `install` gets a process group, a timeout, tracking like `test_pgids`, and a kill on shutdown. | 1:M4 |
| 2.5 | A startup guard (RAII) that removes the socket and run link on any failed startup. | 1:M5 |
| 2.6 | Stale app kill: store the process start time (and boot id) with `app_pid`; kill only on a match and never after a reboot. Also close the gap between spawning the app and saving its pid: a daemon that dies in between leaves an app the next daemon cannot find (e.g. record the pgid through a pidfile the start wrapper writes, or find the group by its environment). | 1:M6, phase 1 |
| 2.7 | Event log: an in-memory ring for `tail`, rotation of `events.jsonl` by size, and a cap or rotation for each generation's `logs/app.log`. | 1:M7, 2:1.9 |
| 2.8 | Stage cleanup: record the generation as `staging` before the slow work, remove its directory on failure, and run `git worktree prune` at startup. | 1:L2, 2:1.9 |
| 2.9 | Create `state_dir` with mode 0700, so logs and storage snapshots are not world-readable. | 2:§2 |
| 2.10 | Spare-port probe also binds `0.0.0.0`, and a test reports "port in use" apart from a health failure. | 1:L3 |
| 2.11 | Single-pass template expansion; shell-quote values in commands or document the `FAWKES_*`/`[env]` route (a behaviour change for manifests that quote templates themselves: decide first). | 1:M9, 2:1.9 |
| 2.12 | `parse_time` rejects out-of-range fields (`2026-13-45T99:99:99Z` parses today). | 1:§5.8 |
| 2.13 | TUI: `tzset` once in `tui::run`; widths by terminal columns (`unicode-width`), not chars. | 2:1.9 |

Tests: the `confirm`/crash race; a subscriber that never reads does not grow memory; a failing install leaves nothing
behind; busy semantics (`stage` during `test`, `stop` during `switch`, rollback during a running `test`).

## Phase 3: typed protocol and an explicit state model

These touch the same code, so they go together after phases 1–2 have their tests.

| # | Change | From |
|---|---|---|
| 3.1 | Typed protocol in `protocol.rs`, shared by daemon, CLI and TUI: `Request` and `EventKind` enums, an `ErrorCode` enum, and typed `Status`, `Process`, `Generation`, `Pending`, `Event` and `RollbackResult` structs. The wire format is unchanged; the roughly 150 `Value["key"]` lookups in clients become fields. | 1:A1, 2:§3, 2:5.6 |
| 3.2 | Write-ahead transaction record `state.txn` (`Switching{phase}`, `Pending`, `RollingBack`), saved before each side effect. Startup recovery becomes one `match`, replacing `rolling_back`, the derived `restore_failed()` and part of `app_failed`. | 1:A2 |
| 3.3 | The live process as an enum (`Stopped`, `Running`, `Backoff`, `Failed`) instead of five fields with illegal combinations. | 1:A3 |
| 3.4 | One `transition(id, to)` that encodes the generation diagram in `DESIGN.md`, plus a table test of every legal and illegal edge. | 1:A4, 2:§4.1 |
| 3.5 | A `Store` that owns `state.json` with `modify(\|s\| …)`, so every change is saved in one place (today there are 14 hand-written saves). | 2:5.2 |
| 3.6 | Split `daemon.rs` into `daemon/{mod,server,supervisor,lifecycle,transactions}.rs` and move the TUI's connection threads into `tui/net.rs`. | 1:A6, 2:§3 |
| 3.7 | Sequence-numbered events: `seq` on `Event`, `subscribe {since}`, no duplicate replay. | 2:1.8, 2:5.4 |

## Phase 4: tests and tooling (can run in parallel with phase 3)

- **Test seams:** an injectable `Clock` and a pure `after_exit(...) -> Outcome`, so backoff, the reset after
  `min_uptime` and the `failed` threshold run in milliseconds (1:A7, 2:§4.2).
- **Integration tests still missing:** crash loop → backoff → `failed`, then `start` clears it; restore failure →
  `rollback` retries; manual `rollback`; reload; `log --follow` and `subscribe` replay; the TUI reader against a fake
  socket server. Replace the `pgrep -f "sleep 621"` check with the pid from `status` (1:§5, 2:§4.3, 2:§4.5).
- **Dynamic test ports and temp dirs** instead of fixed 4800+ and `/tmp/fk48xx`, so worktrees can test concurrently.
  This also covers to-do 1 (2:§4.4).
- **Cargo.toml:** `rust-version`, a `[lints]` table (pedantic with the agreed allows, `unwrap_used`, the cast lints,
  `undocumented_unsafe_blocks`), and `// SAFETY:` comments on the 9 `unsafe` blocks. Fix the `pgid as i32` casts with
  a checked `pid_t` helper that refuses 0 and 1 (1:§6, 1:L7, 2:§3).
- **Tooling:** `rust-toolchain.toml`, `cargo-deny`, and a lander check list in `.ultimate/settings.json`. CI on
  macOS and Linux only once there is a remote.
- **Docs:** how to run the tests (node ≥ 22.5, git), the manifest-trust note, and wall-clock deadlines after sleep
  (1:L4, 2:§2, 2:§3).

## Low-severity items (picked up in whichever phase touches the file)

Done in phase 1: `--end-of-options` (1:L1), the doc comment in `state.rs` (1:L5), daemon usage errors exit 2 (1:L6),
checked pgids (1:L7), signal-hook handlers installed before setup (1:L8), the `umask` comment (1:L9), an `error`
event for an unreadable deadline (2:1.9). The rest moved to phase 2 rows 2.10–2.13.

## Decisions before building

1. **Resume a pending switch after a daemon restart (2:1.7, 2:5.1).** Review 2 says a daemon restart inside the
   confirm window should resume the pending generation when the deadline is in the future and the snapshot exists,
   instead of always rolling back. This contradicts the settled rule "a cold start always runs the last permanent
   generation". It is safe for storage, since the snapshot was taken before the new generation ran, but it changes
   the OTP semantics.
2. **Roll back early on a failed health check (2:5.8):** add `rollback_on_unhealthy`, off by default?
3. **Generation pruning (2:5.7):** `keep = N` with a `prune` command after each confirm, now or still later? It
   matters once the Ultimate extension upgrades often.
4. **New crates:** `thiserror` for error types, `signal-hook` instead of hand-written signals, and `rustix` to
   replace most `unsafe` libc calls. Each needs its justification in `DESIGN.md`; the reviews also say the
   hand-written alternatives are fine.
5. **Event-driven waits (1:A8, 2:§3):** `poll(2)` in the accept loop and a `Condvar` watcher instead of the 20 ms and
   100 ms polling loops. Proposed for after phase 3, not now.
