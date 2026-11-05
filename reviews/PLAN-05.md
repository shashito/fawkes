# Plan 5: a generic, agent-driven Fawkes at `be7b996`

Source: a design review of Fawkes against `HazAT/ultimate` as it is actually run (several instances on one machine,
one shared clone, started by hand from a folder; later also on a Mac mini used for remote development), and against
the next two apps it must supervise: Pi and the user's own agent. The transactional core (`stage → test → switch →
confirm`, recovery from `state.json` after a kill) is sound and tested; nothing here touches it. This plan changes
what sits around it.

Decisions, in the user's words and in order of weight:

1. **Fawkes is generic.** Ultimate, Pi and a personal agent run under it. Anything only one app needs leaves Fawkes.
2. **Agents run and observe it.** The CLI's `--json`, the error codes and the event stream are the product. The TUI
   is stripped.
3. **Storage handling goes to the apps.** Fawkes keeps the two moments (snapshot at switch, restore at rollback) and
   runs hooks; SQLite, and the decision whether to restore at all, belong to Ultimate's extension.
4. **Desktop first, server second.** `fawkes daemon` from a terminal stays the way it is used. A server is the same
   daemon under launchd, installed by one command, never a different mode.

This plan explicitly authorises the **Later** items it names (`CLAUDE.md`: don't start one unless asked). Each
phase is one branch, landed in order. `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` are
clean at every commit; every bug gets a reproducing test first; `DESIGN.md` changes with each contract change, in
the same commit; `README.md` and `CLAUDE.md` lose what no longer exists. Error codes are added, never renamed.

## Phase 7: storage leaves Fawkes (hooks)

Why first: it removes the largest dependency and the only app-specific code, and the hook contract is what the
Ultimate extension (#2227) must be written against.

| # | Change |
|---|---|
| 7.1 | Manifest: `storage = [...]` goes. New optional `[hooks]` table: `snapshot`, `restore`, `test_copy`, each a shell command run via `/bin/sh -c` in the generation's `cwd` (7.3) with the app's `[env]` plus `FAWKES_*`, and a new template `{snapshot}`: a directory Fawkes creates per generation (`generations/<id>/snapshot/`, kept). Each hook has the step's timeout (`install_timeout` for `test_copy` and `snapshot`, `stop_timeout` for `restore`) and runs in its own process group like `install`. |
| 7.2 | Transactions: `switch` runs `snapshot` after the old generation has stopped and before the target is marked `current` (same place the SQLite backup ran). `rollback` runs `restore` after the pending generation has stopped and before the permanent one starts. `test` runs `test_copy` with `{home}` = the test home before the app starts; **with no `test_copy` hook the test home is empty**, never the live one. A hook that exits non-zero or times out fails the step with `storage_failed` (code kept, meaning widened to "a hook failed"). `restore_failed`, `fawkes rollback` retrying it, and `rollback --keep-storage` keep their semantics; `--keep-storage` just means "stop retrying the hook". |
| 7.3 | Manifest: optional `cwd` (default `{code}`), the directory `start`, `health.command` and the hooks run in. Needed so Ultimate's `start` can be `node bin/ultimate.ts /path/to/project --no-open` from the real clone rather than from the detached worktree, which otherwise becomes `projectRoot` and gets edited in place. |
| 7.4 | Delete `src/storage.rs`, the `rusqlite` dependency and its `bundled` build; `Cargo.lock` follows. Remove the "Storage snapshots use the SQLite online backup API" invariant from `CLAUDE.md` and the crate from `DESIGN.md` → Crates; add the hook contract to `DESIGN.md` → Manifest, Switch, Rollback and the on-disk layout. |
| 7.5 | Tests: `examples/sample` gets `snapshot`/`restore`/`test_copy` hooks that copy its sqlite file with `sqlite3 .backup` (or plain `cp`, the sample is tiny and stopped), so `tests/core.rs` keeps every storage scenario it has (`upgrade_confirms_and_a_missed_deadline_rolls_back_code_and_storage`, `failed_writes_and_restores_keep_the_switch_pending_until_a_rollback_gets_through`, `rollback_keep_storage_gives_up_a_restore_that_failed`, the test-home copy tests). Add: a manifest with no hooks does a code-only rollback and an empty test home; a hook that hangs is killed by its process group and fails the step. |
| 7.6 | Hand-off note for the Ultimate extension (not built here): the SQLite backup-API copy of `state.sqlite` and `trace.sqlite` moves into the extension's `snapshot` hook; its `restore` hook decides whether to restore at all (a confirmed-then-crashed generation may keep the database, since Pi Durable committed that work). Put this in `examples/ultimate.toml` as the hooks it will provide. |

## Phase 8: generic apps (Pi, a personal agent)

| # | Change |
|---|---|
| 8.1 | `confirm = "health"` (Later → built). Manifest `confirm = "app"` (default today) or `"health"`: with `"health"`, the live generation's first passing health check after a switch confirms it, logged as `confirmed` with `by: "health"`. With `rollback_on_unhealthy` and `"health"` both set, a switch is decided by the first health outcome either way. `fawkes confirm` from an app under `"health"` is still accepted. Apps that cannot call Fawkes (Pi, most CLIs) use this. |
| 8.2 | `port` optional. Without `port`: `spare_ports` must be absent, `health` must be a `command`, `{port}` is not substituted (left as written, by the one-pass rule), `test` runs the health command against the staged generation with no port, and `status.process.port` is null. Validation names the combinations it refuses. A terminal agent then needs `repo`, `start`, `health.command` and nothing else. |
| 8.3 | `port = "auto"`: the daemon picks the first free port from `auto_base` (default 4747) that no other daemon has claimed in `<fawkes_home>/run/ports.json`, written under a `flock`, released at daemon exit, and reports it in `status`. `spare_ports` may be `"auto"` too (a block of ten past the live port). Removes the cross-manifest collision that per-manifest validation cannot see. |
| 8.4 | Tests: a port-less manifest end to end (stage, test with a command health check, switch, health-confirm, crash before health → rollback); two daemons with `"auto"` on one `fawkes_home` get different ports; `confirm = "health"` with `rollback_on_unhealthy = false` confirms on a late first pass. `examples/pi.toml`: a wrapper repository whose `install` pins the Pi version (git-only refs stay; an `install_only` mode is **Later**). |

## Phase 9: the agent surface; the TUI goes

| # | Change |
|---|---|
| 9.1 | Strip `src/tui.rs`, `src/tui/draw.rs`, the `tui` command, its tests and `ratatui`. `rustix` and `signal-hook` become direct dependencies (they were "already in the tree through crossterm"); fix that sentence in `DESIGN.md` → Crates and the "Every command but `daemon` and `tui`" lines in `CLAUDE.md` and `main.rs`. |
| 9.2 | Keep `src/tui/net.rs` as `src/watch.rs`: `fawkes watch [--json] [--since N]` discovers every daemon in `<fawkes_home>/run/`, holds one `subscribe` per daemon, and prints one line per event with the app's `name` first; `--manifest`/`--socket` narrow it to one app. Text mode is the readable form of the same lines. Reconnect on a dropped daemon as the TUI did; the reconnect-gap test moves with it. |
| 9.3 | `status` result gains `actions`: the commands legal from this state (`["stage"]` idle; `["confirm","rollback"]` pending; `["test g4","switch g3"]` with their generations; `["rollback --keep-storage"]` on `restore_failed`; `["start"]` when stopped). Computed from the same checks the daemon applies, so an agent never encodes the state machine. Protocol addition, `DESIGN.md` → `status`. |
| 9.4 | `fawkes up <ref> [--deadline SECS] [--json]`: stage, test, switch in one client call, streaming each step's progress lines and ending with the switch result. Any step's failure is that step's error. Fails `busy` like its parts. |
| 9.5 | Idempotence for retry loops: `stage <ref>` whose commit already has a `staged`/`tested`/`test_failed`/`retired` generation returns that record with `"existing": true` instead of building another (a `rolled_back` one is never reused: stage builds anew, as now); `confirm <gen>` of the permanent generation is `ok` with the record (was `invalid_state`); `test <gen>` of a `tested` generation re-tests, as now. Document in `DESIGN.md` → CLI. |
| 9.6 | `history --json` carries, per generation, `log_tail`: the last 50 lines of `logs/app.log` (and `logs/install.log` for a removed-on-failure stage, from `install-failed.log`), so a successor process can explain its predecessor's rollback in one call. |
| 9.7 | `skills/fawkes/SKILL.md` in this repository: how an app knows it runs under Fawkes (`FAWKES_PHASE`, `FAWKES_GENERATION`), when and how to confirm, how to upgrade itself (`fawkes up`), what each error code means and what to do next, and that `status.actions` is the list of what it may call. Ultimate, Pi and the personal agent read it; it replaces an SDK. No MCP server. |
| 9.8 | Tests: `watch` across two daemons in one `fawkes_home` sees both apps' events in `seq` order per app; `status.actions` for every state the state machine has (table-driven, `daemon/mod.rs` unit test); `up` end to end and with a failing `test`; the two idempotent cases. |

## Phase 10: server, as an option

| # | Change |
|---|---|
| 10.1 | `fawkes service install [--manifest FILE]`, `uninstall`, `restart` (macOS only; Linux **Later** with systemd user units). Writes `~/Library/LaunchAgents/dev.fawkes.<name>.plist`: `ProgramArguments` = the installed `fawkes` binary (`std::env::current_exe`), `daemon --manifest <absolute path>`; `KeepAlive = { SuccessfulExit = false }`; `RunAtLoad`; `EnvironmentVariables.PATH` captured from the installing shell; stderr to `<state_dir>/daemon.log`. `bootstrap gui/$UID` to load. A desktop user never runs it; the daemon does not know it was started this way. |
| 10.2 | `daemon.log` rotates like `app.log` (10 MiB, one `.1`). |
| 10.3 | Document in `README.md` → "On a server": the plist, that the app's own door (Ultimate's Tailscale extension) is inside the core and so an independent SSH is needed to reach `pnpm cli` when the core is down. |

## Not in this plan

- Ordering upgrades across several Ultimate instances on one clone (a hub's `fawkes up` loop): the extension's job.
- A proxy in front of the app for zero-downtime switches: Fawkes stays out of the request path; Ultimate has its own.
- Falling back past the permanent generation; storage that Fawkes understands itself; Linux `service`.

## Order and check

7 → 8 → 9 → 10. After phase 9, `wc -l src/**/*.rs` should be well under 10k, and `cargo build` well under the
current half minute. The end-to-end check that none of the suites covers today, to run by hand before calling the
plan done: two Ultimate instances on one clone under two daemons, `fawkes up HEAD` on one, kill its core 30 s after
`ready`, and watch the rollback from the other instance with `fawkes watch`.
