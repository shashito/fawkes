# Plan: fixing the findings of review 4 at `abe54f0`

Source: [04-review.md](04-review.md) (Claude Opus 5.5, whole codebase, 14 Medium, 61 Low, 26 Info, plus 24 /simplify
items). Ids below are the review's (`M1`, `L13`, `I4`, `P9`). The review found no High: the transactional promise
holds, so this plan is about the daemon surviving its environment, the mark-good gate, the contract's edges and
test gaps.

The review's own order (§9) is kept, with one change: the findings that bite once **Ultimate runs under Fawkes**
come first, because the extension (#2227) is being built now and the user chose to run the daemon from a terminal.

- **M3** aborts a daemon whose terminal is closed or whose `| tee` dies: exactly the terminal setup the user picked.
- **M10** makes this repository's own `cargo test`, run by the lander inside an Ultimate host that runs under Fawkes,
  send `stage v1` to the live daemon.
- **M4** lets Ultimate's test-phase instance confirm the live pending generation, if the extension confirms with a
  bare `fawkes confirm`.

Each phase is one branch, landed in order. `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
are clean at every commit, every bug gets a reproducing test first (CLAUDE.md), DESIGN.md changes with each contract
change, and the review team reviews each phase.

## Phase 5: safe to run Ultimate under (before the extension goes live)

**Done:** every row below, 5.1 to 5.7, and the housekeeping line, each bug with a test that reproduced it first.
Where the build differs from the row: stderr gets the event lines from a thread of its own (a bounded queue filled
under the log lock), and a CLI whose stdout has gone keeps the exit code the daemon's answer gives (the review
suggested 1), since a confirm that went through did not fail. The 10 s bound on a locked database applies to
snapshot and restore too, so a lock holder cannot hang a switch or rollback while the app is down. 5.3's harness also
drops `FAWKES_GENERATION`, `FAWKES_PHASE`, `FAWKES_BIN` and `FAWKES_HOME`, which 5.2 makes matter. 5.6 also covers
`rollback --keep-storage` and the supervisor's crash restart.

| # | Fix | From |
|---|---|---|
| 5.1 | `Daemon::event` writes stderr best-effort (`writeln!`, failure ignored with a comment) and outside `inner`. The CLI's stdout too: a closed pipe exits by the contract, not 134. | M3, L16 |
| 5.2 | `fawkes confirm` defaults to `$FAWKES_GENERATION` when set; DESIGN.md's Confirm section says an app confirms itself only when `FAWKES_PHASE=live`. Log which generation confirmed. | M4 |
| 5.3 | Only the harness builds a CLI (`Harness::cli`/`spawn`), and `daemon_command` drops `FAWKES_SOCKET` too; CLAUDE.md says tests never call `Command::new(BIN)`. | M10 |
| 5.4 | `Store::modify` adopts `next` once `state.json` is renamed into place; a `.bak` or directory-fsync failure becomes an `error` event. Memory always equals what a restart would read. | M1 |
| 5.5 | Storage: `snapshot` and `restore` copy in one `step(-1)` (the app is stopped); `copy` for `test` is one bounded read transaction that refuses with `storage_failed` after a deadline. | M2, M7 |
| 5.6 | SIGTERM during a rollback or startup recovery: no `failed` for the permanent generation, no ok answer for a rollback that did not run. | L6 |
| 5.7 | Tests that pin this area: a daemon start that finds `switching` or `rolling_back` (M11), and confirm's four untested refusals as `scratch_daemon` unit tests (M13). | M11, M13 |

## Phase 6: the contract's edges

**Done:** every row below, 6.1 to 6.8, each bug with a test that reproduced it first. Where the build differs from
the row: the connection cap is 64, and a connection counts from accept until its request turns out to be a stream;
the client reads a `busy` answer the daemon left even when its own write failed, or when the daemon closed before the
client set its read timeout (macOS refuses it then). `status.step` is `switching`,
`pending`, `rolling_back` or `restore_failed` (null without a transaction), and a client without it from an older
daemon treats `pending` as before; the Ultimate extension reads it too. A clap usage error under `--json` is the
`usage` object as well. A relative `FAWKES_HOME` (or `HOME`) is refused rather than resolved, and `fawkes daemon` exits
2 for a manifest it cannot load. With `Unknown` decoded, the daemon refuses a `state.json` holding a generation state
it does not know, and does not replay events of an unknown kind (their `seq` still counts).

| # | Fix | From |
|---|---|---|
| 6.1 | A per-request 5 s deadline instead of per `read(2)`, and a cap on concurrent non-streaming connections (`busy` past it). | M6 |
| 6.2 | Every client-side failure under `--json` prints one error object; a new `no_answer` code for the timeout (added, never renamed); `log --follow --json` documented as a stream, without the trailing `null`. | M8 |
| 6.3 | `status` says what the step is: a field beside `pending` for `rolling_back` and `restore_failed` (protocol addition), and the CLI and TUI stop showing a confirm countdown during a rollback. | L13 |
| 6.4 | A wrong-typed optional argument is a `usage` error; `From<io::Error>` for `protocol::Error` goes, so every io failure names its path and code. | L14, L15 |
| 6.5 | Health check: a total bound on a slow status line, and a bounded wait after SIGKILL that ends the whole group. | L7, L8 |
| 6.6 | `#[serde(other)]` on `EventKind` and the other wire enums, so a client keeps lines it doesn't know. | L25 |
| 6.7 | DESIGN.md: an Events table, the busy rules, `reuse` and unknown keys, refuse port 0 and URL ports above 65535, an absolute `FAWKES_HOME`, the small inconsistencies. | L17–L21, I4 |
| 6.8 | Failed-restore errors point at `rollback --keep-storage`; `test-home/` is removed when a test fails before its process starts. | L4, L5 |

## Phase 7: security hardening

**Done:** every row below, 7.1 to 7.4, and I8, each bug with a test that reproduced it first. Where the build differs
from the row: `home` gets the same check as `state_dir` (a symlink or another user's is refused at daemon start), but a
missing one is created `0700` and an existing one keeps its mode. The manifest is refused, not warned about, when
another user (root excepted) owns it or others can write it; group-writable passes, for a umask of `002`. The `.git`
check I9 suggested and ownership of the directories above `state_dir` are not checked. Restored files keep their mode
but not their owner. The harness keeps `/tmp/fawkes-test-ports` and `/tmp/fk<port>`, since the fixed lock directory
is how suites in other worktrees see each other's claims, and fails a test on one another user created or can write.
A foreign-owned `state_dir` is tested as a unit test with an injected uid; an integration test would need root.

| # | Fix | From |
|---|---|---|
| 7.1 | Refuse a symlinked or foreign-owned `state_dir`, canonicalize it as `Paths.root`, and check `home` the same way; move the examples and DESIGN.md's example off `/tmp`. | M5 |
| 7.2 | Database files and `home` keep private modes after a restore; `~/.fawkes` and `run/` are created 0700; the manifest's owner and mode are checked. | L30, I9 |
| 7.3 | The test harness stops using guessable `/tmp` paths another user can create first. | L31 |
| 7.4 | Tests for socket 0600, `state_dir` 0700, and the 64 KiB and 5 s request bounds. | L32 |

## Phase 8: tests

M12 (a failing `test`: `test_failed`, `health_failed`, `port_in_use`, no free spare port), M14 (`health.command`
checks), then L36–L48 and I16–I18. These include the weak timing tests (L36–L40), Harness cleanup of `step.pid`
(L41), harness git that ignores the user's global config (L42, see memory `git-tag-gpgsign`), the documented
features without a test (L45), the validation table (L46), CLI text output (L47), and loading the example manifests
DESIGN.md quotes (I17).

**Done:** every row below but 8.6, which [PLAN-05.md](PLAN-05.md) dropped. The new tests found no bug. The production
changes are the three seams the rows ask for: `run_check` takes its timeout, `APP_LOG_BYTES` is 64 bytes under
`cfg(test)`, and the TUI's `request_within` takes its timeout. Where the build differs from the row: the sample app's
`crash` mode is kept, and crashes only while the file `$CRASH_WHILE` names exists, so one generation fails its test
and then passes it; `port_in_use` comes from a start command whose listener, in a session of its own, takes the spare
port. Three tests the load runs showed to be flaky wait for conditions instead: the unhealthy-rollback tests get their
3 s health timeout by a reload once their tests have passed, the busy-switch test holds the old app's stop until its
refused `stop` is in, and the stop-window tests read the `progress` event from a followed log. The TUI tests written
before 8.6 was dropped stay until the TUI goes. The suite grew from 76 unit and 34 integration tests to 104 and 41.

| # | Fix | From |
|---|---|---|
| 8.1 | A failing test: `health_failed`, `test_failed`, switch refused, a retest that passes; `port_in_use` and no free spare port leave the generation unchanged. | M12 |
| 8.2 | `health.command`: `run_check` unit tests (exit 0, exit 1, env, a hang whose group is gone), one core test for test and live, the both/neither manifest rows. | M14 |
| 8.3 | Timing tests wait for conditions: the unhealthy-confirm race, the idle-wake bounds, `restart_at` around the switch in the backoff, `busy` checked, the rollback bound `< 10`. | L36–L40 |
| 8.4 | `Harness::drop` ends `step.pid`'s group; harness and daemon git ignore the user's config and `GIT_*`; one `Harness::crash_app` without `kill(1)`. | L41, L42, L48 |
| 8.5 | A namesake daemon and a shared `--socket` refused, `state.json` recovered from `.bak`, a stray generation directory removed, a daemon killed or stopped during a test frees the spare port; switching back to a retired generation, `git_failed`, prune without `keep`, `reuse`. | L44, L45, I16 |
| 8.6 | TUI tests. **Dropped** by PLAN-05 (the TUI goes). | L43 |
| 8.7 | Validation table rows (unknown keys, a reversed range, two problems in one error); `render_result` for every request; `--json` stdout and progress lines. | L46, L47 |
| 8.8 | The log cap, an oversized request through `handle`, `exited` then `try_wait`, the example manifests and DESIGN.md's TOML load, `[::1]` skipped without IPv6, exhaustive wire enum lists, children killed on a failed assertion. | I16–I18 |

## Phase 9: TUI (dropped)

**Dropped:** [PLAN-05.md](PLAN-05.md) removes the TUI in its phase 9, so M9, L22–L29 and I5–I6 are not built, and
L43's TUI tests go with it. **Done** there: the TUI and its tests are gone, and the connection tests moved to
`src/watch.rs`.

## Phase 10: quality and /simplify

**Runs last,** after PLAN-05's phase 12, since PLAN-05 rewrites the files it would polish. P2 (one bounded-child
runner) moves into PLAN-05's phase 10, which needs it for the hooks; L49 is cut down to toml's default features,
ratatui and rusqlite being gone.

L54 and L56 (one "exited" accessor and one parsed deadline remove five sites each), L55 (one `LastExit`
description), P9 (an exhaustive destructure in `Manifest::reload`), L49 (trim toml's default features), L33
(durations parsed once), L34 (`let _ =` that matters), L57–L61 (literals, stale comments, names), I23–I26, and the rest of P1–P24
where they make the code simpler. The rest of L1–L3, L9–L12 and I1–I3 go in the phase that touches their file.

## Housekeeping (with phase 5)

`.gitignore` gains editor swap files (`.*.swp`); the untracked `.README.md.swp` is removed. **Done** (the ignore
entry; the swap file in the user's checkout is left to them).

## Needs the user

- **Tools (L50):** cargo-deny (advisories, licences) is not installed. L52 is done: `cargo clippy` and `cargo test`
  pass in an Alpine Linux micro-VM, crash recovery's leftover check included (there is still no CI).
- **Overlap with the extension (#2227):** its code lives in this repository (the user's choice), and its doc fixes
  touch `examples/ultimate.toml` and DESIGN.md's "Later", which 7.1 also changes. Phase 5 touches neither, so it can
  run alongside the extension; 7.1 waits until the extension's doc fixes have landed.
