# Plan 5: a generic, agent-driven Fawkes at `be7b996`

Source: a design review of Fawkes against `HazAT/ultimate` as it is actually run (several instances on one machine,
one shared clone, started by hand from a folder; later also on a Mac mini used for remote development), and against
the next two apps it must supervise: Pi and the user's own agent. The transactional core (`stage → test → switch →
confirm`, recovery from `state.json` after a kill) is sound and tested; nothing here touches it. This plan changes
what sits around it.

It continues [PLAN-04.md](PLAN-04.md), whose phases 5 to 7 have landed and whose phase 8 (tests) runs first. Its
phases are numbered from 9 on. It **supersedes PLAN-04's phase 9** (the TUI, which this plan removes), and PLAN-04's
phase 10 (quality and /simplify) moves to the end, after phase 12, less P2 (moved into phase 10 here) and L49 (cut
down to toml).

Decisions, in the user's words and in order of weight:

1. **Fawkes is generic.** Ultimate, Pi and a personal agent run under it. Anything only one app needs leaves Fawkes.
2. **Agents run and observe it.** The CLI's `--json`, the error codes and the event stream are the product. The TUI
   is stripped.
3. **Storage handling goes to the apps.** Fawkes keeps the two moments (snapshot at switch, restore at rollback) and
   runs hooks; SQLite, and the decision whether to restore at all, belong to Ultimate's extension.
4. **Desktop first, server second.** `fawkes daemon` from a terminal stays the way it is used. A server is the same
   daemon under launchd, installed by one command, never a different mode.

Decided on this plan after its assessment (#20414):

- A rollback always runs the `restore` hook. A hook never decides to keep the storage; only `fawkes rollback
  --keep-storage` gives a restore up, as now. This replaces decision 3's "the decision whether to restore at all":
  a confirmed generation never rolls back, so a skipped restore could only keep what a failed one wrote.
- Hooks run from the permanent generation or from an absolute path, never from the generation being tested, switched
  to or rolled back, and they have a timeout of their own.
- A manifest with a `snapshot` hook must also have `test_copy`. An app without hooks gets a code-only rollback and an
  empty test home, and `DESIGN.md` says so plainly.
- The Ultimate extension's hooks are written in phase 10, with `VACUUM INTO`.
- No `cwd` field: `start.ts` passes the project to Ultimate itself, so the generation's worktree never becomes the
  project, and running from the real clone would run code that was never staged or tested.
- The generic-apps phase (13) is deferred.

This plan explicitly authorises the **Later** items it names (`CLAUDE.md`: don't start one unless asked), except
those of the deferred phase 13. Each phase is one branch, landed in order. `cargo fmt`, `cargo clippy --all-targets
-- -D warnings` and `cargo test` are clean at every commit; every bug gets a reproducing test first; `DESIGN.md`
changes with each contract change, in the same commit; `README.md` and `CLAUDE.md` lose what no longer exists. Error
codes are added, never renamed.

## Phase 9: the TUI goes, `fawkes watch` comes

Why first: it is the largest deletion (`src/tui.rs` and `src/tui/draw.rs`, 2,001 lines; 93 crates down to 40),
mechanical, and independent of the storage work. The extension keeps working as it is until phase 10.

**Done:** 9.1 to 9.3. Measured against `bd2ccd1`: 93 crates down to 40, `src/` from 11,673 lines to 9,569, a clean
debug build from 10.4 s to 7.0 s. Where the build differs from the rows: without `--since`, each daemon starts at the
time `watch` started (by the events' `at`), so a daemon found later still shows its own start; a reconnect asks for
every event after the last one printed (no `limit`), and a note on stderr names a gap the daemon could not fill; the
queue to stdout is bounded, so a paused reader makes the daemon drop the stream and the reconnect resumes it. The
TUI's one-shot `request` went with the actions that used it, and `Generation::label` with the screen. The unit and
integration suites went from 104 and 41 tests to 81 and 42.

| # | Change |
|---|---|
| 9.1 | Strip `src/tui.rs`, `src/tui/draw.rs`, the `tui` command, its tests (L43's included) and `ratatui`. `rustix` and `signal-hook` become direct dependencies (they were "already in the tree through crossterm"); fix that sentence in `DESIGN.md` → Crates, remove `DESIGN.md` → TUI, and fix the "Every command but `daemon` and `tui`" lines in `CLAUDE.md` and `main.rs` and "the same in the CLI, the socket and the TUI" in Lifecycle. Drop jiff's `tz-system` and `tzdb-zoneinfo` features if nothing but the TUI's local clock uses them. |
| 9.2 | Keep `src/tui/net.rs` as `src/watch.rs`: `fawkes watch [--json] [--since N]` discovers every daemon in `<fawkes_home>/run/`, holds one `subscribe` per daemon, and prints one line per event with the app's `name` first; `--manifest`/`--socket` narrow it to one app. Text mode is the readable form of the same lines. Reconnect on a dropped daemon as the TUI did; the reconnect-gap test moves with it. |
| 9.3 | Tests: `watch` across two daemons in one `fawkes_home` sees both apps' events in `seq` order per app, and keeps going when one daemon is killed and restarted. |

## Phase 10: storage leaves Fawkes (hooks)

Why now: it removes the only app-specific code and the `rusqlite` dependency, and the extension (#2227) moves onto
the hook contract in the same branch, so no build of Fawkes ever runs Ultimate without its storage covered.

**Done:** 10.1 to 10.9. Measured against `f8adf51`: 40 crates down to 32, `src/` from 9,589 lines to 9,581, a clean
debug build from 8.2 s to 6.9 s. Where the build differs from the rows: in every hook `{home}` is the live home, and
`test_copy` writes into a template of its own, `{test_home}` (the test's empty home, which the test run then gets as
`{home}`), so the copy's source and target are both named; `{snapshot}` and `{test_home}` are refused outside their
hooks. `restore` needs `snapshot` as well as the other way round. `snapshot` and `restore` are tracked in a new
`hook.pid`, because an automatic rollback restores beside a stage or test that holds `step.pid`; `test_copy` uses
`step.pid`. `hooks` reloads with SIGHUP (default `timeout` 600 s), and a switch without a `snapshot` hook removes an
earlier snapshot of its generation, so a `restore` added later fails instead of putting old storage back. Hook output
goes to `generations/<id>/logs/hooks.log`. The extension's hold logic moved into `ultimate/fawkes/hold.ts` with a test
of its own (the review of `1007cf5..63f249b`). The unit and integration suites went from 81 and 42 tests to 77 and
46; the extension's from 4 to 13.

| # | Change |
|---|---|
| 10.1 | Manifest: `storage = [...]` goes (a manifest that still has it fails to load, naming `[hooks]`). New optional `[hooks]` table: `snapshot`, `restore`, `test_copy`, each a shell command run via `/bin/sh -c` with the app's `[env]` plus `FAWKES_*`, and `timeout` (seconds, its own default, bounded like the other durations). A new template `{snapshot}`: a directory Fawkes creates per generation (`generations/<id>/snapshot/`, kept). `snapshot` and `restore` come together, and `snapshot` requires `test_copy`; validation names the hook that is missing. |
| 10.2 | Where hooks run: in the permanent generation's worktree (`{code}` of the permanent generation), never in the generation being tested, switched to or rolled back, so code under suspicion never decides how storage is copied or restored. Before any generation is permanent (the first switch), in `repo`. A hook that names an absolute path (as Ultimate's does) runs the same code from any of them. Each hook runs in its own process group, and a timeout ends the whole group. |
| 10.3 | Transactions: `switch` runs `snapshot` after the old generation has stopped and before the target is marked `current` (where the SQLite backup ran). `rollback` runs `restore` after the pending generation has stopped and before the permanent one starts, **every time**: a hook has no way to skip it. `test` runs `test_copy` with `{home}` = the test home before the app starts; without hooks the test home is empty, never the live one. A hook that exits non-zero or times out fails the step with `storage_failed` (code kept, meaning widened to "a storage hook failed"). `restore_failed`, `fawkes rollback` retrying it, and `rollback --keep-storage` keep their semantics; `--keep-storage` means "stop retrying the hook". |
| 10.4 | The hook contract in `DESIGN.md` → Manifest, Switch, Rollback, Test and the on-disk layout: `restore` must be safe to run again after it failed, and leave every file either as it was or wholly restored (write beside, then rename), since Fawkes trusts its exit code alone. **A manifest without hooks gets a code-only rollback**: the old generation then starts on whatever storage the failed one left, migrations included, and its tests run on an empty home. |
| 10.5 | P2 from PLAN-04: one bounded-child runner for `install`, the hooks and the health command (process group, timeout, the group ended on every way out). |
| 10.6 | Delete `src/storage.rs`, the `rusqlite` dependency and its `bundled` build; `Cargo.lock` follows. Remove the "Storage snapshots use the SQLite online backup API" invariant from `CLAUDE.md` and the crate from `DESIGN.md` → Crates. The harness reads the sample's rows without `rusqlite` (through the sample app or `node:sqlite`). |
| 10.7 | The Ultimate extension: `ultimate/fawkes` gets the three hooks for `state.sqlite` and `trace.sqlite`, with `VACUUM INTO` a file beside the target, then a rename (one read transaction, so a live WAL database is copied consistently, uncheckpointed writes included; it works on Node 22.13, where `node:sqlite`'s `backup()` needs 22.16). Restore removes the target's `-wal`, `-shm` and `-journal` before the rename, deletes a file that was absent at snapshot time, and keeps each file's mode, as `storage.rs` did. Tests beside them, like `test-env.test.ts`. `examples/ultimate.toml` names them in `[hooks]`, with absolute paths. |
| 10.8 | Tests: `examples/sample` gets `snapshot`/`restore`/`test_copy` hooks that copy its WAL-mode sqlite file consistently (a plain `cp` of `app.sqlite` loses rows still in `-wal`), so `tests/core.rs` keeps every storage scenario it has (`upgrade_confirms_and_a_missed_deadline_rolls_back_code_and_storage`, `failed_writes_and_restores_keep_the_switch_pending_until_a_rollback_gets_through`, `rollback_keep_storage_gives_up_a_restore_that_failed`, the test-home copy tests); the ones that broke the copy with "not a database" use a failing hook instead. Add: a manifest with no hooks does a code-only rollback and an empty test home; a hook that hangs is killed by its process group and fails the step; `snapshot` without `test_copy` is refused; a restore hook runs in the permanent generation's worktree. |
| 10.9 | Upgrade note: a `snapshot/` written before this phase (`index.json` and `<n>.sqlite`) is not what the new hooks read. Install this build with no switch pending; `README.md` says so. |

## Phase 11: the agent surface

**Done:** 11.1 to 11.6. Where the build differs from the rows: `actions` lists every command the daemon would accept
at the moment of the `status`, by the same check functions the commands call, so it is a superset of the row's
examples (with a switch pending it also holds `stop`, `restart`, `stage` and the `test` entries), and a command whose
slot is taken is left out while `busy` names the step; `confirm` names the pending generation (`confirm g4`), like
`test g4`, and a confirm of the permanent generation is not listed. `up` is client-side: it sends the three requests
in turn, so each step keeps its slots, refusals and progress, the daemon's transaction code is unchanged, and a client
that dies midway leaves a generation that `up` again picks up through the idempotent `stage`. Its result is the
`switch` result. `confirm` of the permanent generation answers ok before any refusal and changes nothing. `log_tail`
is one string (the last 50 lines within the last 8 KiB) or null; the failed stage's install log is `history`'s
top-level `failed_install_tail`, since that generation's record is gone. The sample app's `slow-test` mode went: the
busy-steps test holds its test run on a marker file instead of a 6 s timer, which flaked under load.

| # | Change |
|---|---|
| 11.1 | `status` result gains `actions`: the commands legal from this state (`["stage"]` idle; `["confirm","rollback"]` pending; `["test g4","switch g3"]` with their generations; `["rollback --keep-storage"]` on `restore_failed`; `["start"]` when stopped). Computed from the same checks the daemon applies, so an agent never encodes the state machine. Protocol addition, `DESIGN.md` → `status`. |
| 11.2 | `fawkes up <ref> [--deadline SECS] [--json]`: stage, test, switch in one client call, streaming each step's progress lines and ending with the switch result. Any step's failure is that step's error. Fails `busy` like its parts. |
| 11.3 | Idempotence for retry loops: `stage <ref>` whose commit already has a `staged`/`tested`/`test_failed`/`retired` generation returns that record with `"existing": true` instead of building another (a `rolled_back` one is never reused: stage builds anew, as now); `confirm <gen>` of the permanent generation is `ok` with the record (was `invalid_state`); `test <gen>` of a `tested` generation re-tests, as now. Document in `DESIGN.md` → CLI. |
| 11.4 | `history --json` carries, per generation, `log_tail`: the last 50 lines of `logs/app.log` (and `logs/install.log` for a removed-on-failure stage, from `install-failed.log`), bounded in bytes too, so a successor process can explain its predecessor's rollback in one call. |
| 11.5 | `skills/fawkes/SKILL.md` in this repository: how an app knows it runs under Fawkes (`FAWKES_PHASE`, `FAWKES_GENERATION`), when and how to confirm, how to upgrade itself (`fawkes up`), what each error code means and what to do next, that `status.actions` is the list of what it may call, and that storage is covered only through the app's hooks. Ultimate, Pi and the personal agent read it; it replaces an SDK. No MCP server. |
| 11.6 | Tests: `status.actions` for every state the state machine has (table-driven, `daemon/mod.rs` unit test); `up` end to end and with a failing `test`; the two idempotent cases. |

## Phase 12: server, as an option

**Done:** 12.1 to 12.3. Where the build differs from the rows: `install` refuses a binary with a `CACHEDIR.TAG` in an
ancestor (cargo's `target/`), and with no plist yet a daemon that already answers on the socket; an existing plist is
replaced and reloaded, so `install` again after an update is how a new binary or `PATH` is picked up. `--json` applies
to all three, and a failing `launchctl` is a new client-only code, `service_failed`. `uninstall` is idempotent;
`restart` is a bootout and a bootstrap (a graceful SIGTERM, and the plist read afresh), `not_found` without a plist.
The plist also carries `ExitTimeOut` (`stop_timeout` rounded up plus 10 s, since launchd's 20 s would SIGKILL a daemon
still stopping a slower app) and `FAWKES_HOME` when set; `install` creates `state_dir`, which launchd needs for
`daemon.log`. A bootout waits until `launchctl print` no longer finds the agent: checked by hand against launchd (a
throwaway label, `HOME` in `/tmp`), a bootstrap right after a bootout fails with `5: Input/output error`. The same
check saw `RunAtLoad`, a restart after `kill -9` and a clean SIGTERM on restart and uninstall. Tests reach a fake
`launchctl` on `PATH` with `HOME` in the harness's directory, which are the only seams. `daemon.log` is capped by the
thread that writes stderr, only when stderr is that file (same inode), so no line is lost and the daemon has no launchd
code.

| # | Change |
|---|---|
| 12.1 | `fawkes service install [--manifest FILE]`, `uninstall`, `restart` (macOS only; Linux **Later** with systemd user units). Writes `~/Library/LaunchAgents/dev.fawkes.<name>.plist`: `ProgramArguments` = the installed `fawkes` binary (`std::env::current_exe`), `daemon --manifest <absolute path>`; `KeepAlive = { SuccessfulExit = false }`; `RunAtLoad`; `EnvironmentVariables.PATH` captured from the installing shell; stderr to `<state_dir>/daemon.log`. `bootstrap gui/$UID` to load. A desktop user never runs it; the daemon does not know it was started this way. |
| 12.2 | `daemon.log` rotates like `app.log` (10 MiB, one `.1`). |
| 12.3 | Document in `README.md` → "On a server": the plist, that the app's own door (Ultimate's Tailscale extension) is inside the core and so an independent SSH is needed to reach `pnpm cli` when the core is down. |

## Phase 13: generic apps (deferred)

Not built until the user asks. Recorded so the findings of the assessment are not lost:

- `confirm = "health"`: the first passing health check of a pending generation confirms it, decided under `inner`
  with the same refusals as `confirm` (its process live, no rollback decided), like `judge_unhealthy` in
  `daemon/supervisor.rs`, and ordered against a deadline shorter than `health.timeout`.
- `port` optional: refuse `{port}` in a manifest without one rather than leave it unsubstituted;
  `status.process.port` and a test's `port` become nullable, which the extension's `types.ts` must follow.
- Port collisions across manifests: a daemon start that refuses a port another daemon in `run/` already claims is
  simpler than `port = "auto"`. If `auto` is still wanted, claim each port with a `flock` file held for the daemon's
  life (the kernel releases it after a crash), not a JSON registry, and keep the chosen port across daemon restarts.
- Pi first needs a spike: it is an interactive terminal agent, and Fawkes runs an app with stdin closed and its
  output in `app.log`. `examples/pi.toml` comes out of that spike.

## Not in this plan

- Ordering upgrades across several Ultimate instances on one clone (a hub's `fawkes up` loop): the extension's job.
- A proxy in front of the app for zero-downtime switches: Fawkes stays out of the request path; Ultimate has its own.
- Falling back past the permanent generation; storage that Fawkes understands itself; Linux `service`.
- A `cwd` field (see the decisions above).

## Order and check

PLAN-04 phase 8 lands first, then 9 → 10 → 11 → 12, then the rest of PLAN-04 phase 10.

Measured at `be7b996`: `src/` is 10,875 lines (7,839 of code, 3,036 of unit tests) and the build pulls in 93 crates
(`cargo tree -e normal --prefix none | sed 's/ (\*)//' | sort -u | wc -l`). After phase 10: `src/tui.rs`,
`src/tui/draw.rs` and `src/storage.rs` are gone, and that count is 32 or fewer. Lines are not the check: the phases
add about as much as storage removes, so `src/` ends near 9.5k lines with its unit tests.

The end-to-end check that none of the suites covers, to run by hand before calling the plan done, on copies only
(`~/Code/ultimate-bak` and a copied state home, never `~/Code/ultimate` or `~/.ultimate`): two Ultimate instances on
one clone under two daemons, `fawkes up HEAD` on one, kill its core 30 s after `ready`, and watch the rollback,
storage restored by the extension's hook, from the other instance with `fawkes watch`.
