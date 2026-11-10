# Fawkes design

Fawkes supervises one app and moves it between *generations*. Every upgrade is a transaction:

```
stage <ref> → test <gen> → switch <gen> → confirm      (or automatic rollback)
```

This document is the contract: manifest, CLI, socket protocol, on-disk layout, and the state machine. The prototype
implements all of it except where a section says **later**.

## Process model

`fawkes daemon` is a long-running foreground process (launchd or systemd keeps it alive; on macOS `fawkes service
install` sets up launchd, see Service). It is the parent of the app's live process and the only writer of the state
directory. Every other command (`start`, `stop`, `restart`, `stage`, `test`, `switch`, `up`, `confirm`, `rollback`,
`prune`, `status`, `log`, `history`, `watch`) is a thin client that sends requests over the daemon's Unix socket and
renders the answer. One daemon supervises one app; run one daemon per manifest.

A step takes one or both of two slots. The *step* slot belongs to the transaction steps: `stage`, `test` and `prune`
take it alone, `switch` and `rollback` take it together with the live slot, and a second step gets `busy`. The *live*
slot belongs to whatever moves the live process: `start`, `stop` and `restart`, a `switch` or `rollback`, and the
supervisor's own work (reaping the app after a crash, restarting it after its backoff, an automatic rollback). So the
lifecycle commands exclude each other and a switch or rollback, and get `busy` while the supervisor works, but a
`stage`, `test` or `prune` never delays them, and an automatic rollback never waits for a stage or test (it runs
beside them; a `switch` or `rollback` asked meanwhile gets `busy` from the stage or test). When both slots are
taken, `busy` names the step in the step slot first, and the live slot's step otherwise. `status`, `log`, `history`
and `confirm` are always answered.

The daemon is a few threads and no async runtime: one accepts clients, one per client answers it, one supervises
the app, one per running child waits for its exit, and short-lived ones run health checks and read a step's output.
The accept thread sleeps in `poll(2)` on the socket and on a pipe the signal handlers write to; the supervisor
sleeps until the app exits, a request changes something, or its next timer runs out (a crash restart's backoff, the
confirm deadline, and every 10 s while an app runs the `app.log` size cap). An idle daemon without an app or a
client does not wake at all; a client that follows the log (`subscribe`, as `watch` does, and `log --follow`) has
its thread look every 500 ms whether the client has gone.

The daemon writes every event to stderr as a line, for whoever watches the terminal. That copy is best effort, from
a thread of its own: a stderr that blocks or breaks (a closed terminal, a `| tee` that died) never stalls or ends the
daemon, and lines it cannot take are dropped. `events.jsonl` is the record. When stderr is `<state_dir>/daemon.log`
(the same file, as a launchd service's is), the thread that writes it keeps it under 10 MiB: past that it copies the
file to `daemon.log.1` (replacing the one before) and empties it, between two lines, so none is lost. Whoever starts
the daemon decides where stderr goes; the daemon only looks at what it is.

Signals are only for the daemon's lifecycle: `SIGTERM`/`SIGINT` stop the live app and exit (the generation keeps its
state, so the next start resumes it), `SIGHUP` re-reads the manifest. A reload is all or nothing and is logged as a
`daemon` event naming the changed fields, or an `error` event saying why it was refused:

- `name`, `repo`, `state_dir` and `home` never change by reload; they need a daemon restart. (A changed `home` would
  restore a snapshot into another directory than the one it was taken from.)
- `port` does not change while the app runs (`stop` it first), nor while a step holds the live process (a switch,
  restart, rollback or the supervisor's own restart: between stopping the old process and starting the new one no app
  runs, yet the step announces the port it starts on). The reload is refused whole; send `SIGHUP` again once the step
  has finished.
- Everything else (`start`, `env`, `health`, `hooks`, `rollback_on_unhealthy`, `install`, `reuse`, `keep`,
  `spare_ports`, `deadline`, the timeouts and the restart policy) applies from the next stage, test, switch, rollback
  or start on. An invalid manifest is not reloaded.

Change `home` only while no switch is pending: a daemon start rolls a pending switch back into the `home` of the
manifest it reads. The same goes for a `restore` hook that cannot read what the `snapshot` hook before it wrote, since
the rollback of a pending switch runs the `restore` the daemon has then.

**Panics** abort the daemon (`panic = "abort"`) after an `error` event with the message. The init system restarts
it, and the daemon start below recovers whatever was in flight; no thread carries on past a broken invariant.

## Generations and states

A generation is a git worktree at a ref with its dependencies installed, numbered `g1`, `g2`, … in staging order.

```
staging ──▶ staged ──test──▶ tested ──switch──▶ current ──confirm──▶ permanent ──(next confirm)──▶ retired
   │           ▲  ◀─fails─┘ (test_failed,                  │
   │           │            retestable)                      └──missed deadline / exit / rollback──▶ rolled_back
   └──fails──▶ removed
```

- **staging:** recorded before the worktree is created and the install runs, and the record (with the id it spends)
  must be on disk first: a stage whose `state.json` cannot be written fails with `internal` before any worktree
  exists. A stage that fails later removes the generation again (record, directory, worktree); one that a daemon's end
  cut short is removed at the next daemon start. Ids are never reused.
- **staged:** worktree created and install succeeded. **test_failed:** the last test failed; `test` may run again.
  `stage <ref>` is safe to retry: when the commit it resolves to already has a `staged`, `tested`, `test_failed` or
  `retired` generation, `stage` builds nothing and answers with the newest such record and `"existing": true`
  (one `progress` line says so, no `staged` event is logged, since nothing changed); a new build answers
  `"existing": false`. A `staging`, `current`, `permanent` or `rolled_back` generation is never reused. The catch:
  staging the same commit again to pick up a changed install environment (a new toolchain, a changed `install`
  command or `[env]`) returns the old build; `prune` it (or stage another commit) to build anew.
- **tested:** started on a spare port with a test home of its own and passed the health check. The test home is
  created empty, and the `test_copy` hook (see Hooks) fills it with a copy of the storage before the app starts; a
  hook that fails fails the test with `storage_failed` and leaves the generation as it was. Without hooks the app
  starts on the empty home, never on the live one. The spare port is the first in `spare_ports` that is free on both
  `127.0.0.1` and `0.0.0.0`; a test that fails while another process holds its port afterwards is `port_in_use` and
  leaves the generation as it was. Only a tested generation can be switched to.
- **current:** live, not yet confirmed. A confirm deadline runs (Junos `commit confirmed`).
- **permanent:** confirmed by the app (RAUC/Mender mark-good). Exactly one generation is permanent at a time. A cold
  start of the daemon always runs the permanent generation (Erlang/OTP release handler).
- **retired:** was permanent, superseded by a newer confirm. Its worktree stays until `prune` removes it; `switch` to it
  again is allowed (after a new `test`), which is how an agent goes back to an older version on purpose.
- **rolled_back:** final. A rolled-back generation is never started again, so a broken rollback cannot loop, and
  `stage` never reuses it; stage the same ref again to get a fresh generation.

### Switch

1. Refuse unless the target is `tested` and no other switch is pending.
2. Save the step `switching` to `state.json`, then stop the running generation (SIGTERM to its process group,
   SIGKILL after `stop_timeout`).
3. Run the `snapshot` hook into the target's `snapshot/`, made afresh (see Hooks; without hooks there is no
   snapshot). Stopping first means no write of the old generation lands after the snapshot, so a rollback loses
   nothing the old generation wrote.
4. Mark the target `current` and write the step `pending` with its deadline (absolute time) to `state.json`, then
   start the target on the live port. The deadline is wall-clock time, so it survives a daemon restart, and it also
   runs on while the machine sleeps: a switch whose deadline passes during a sleep rolls back on wake (within a
   second, as after a change of the system clock), however healthy the new generation was. Confirm before closing the lid, or give a longer `--deadline`. The intent is on disk before any process of the target exists, so a daemon
   that dies from here on rolls the switch back at its next start. If the target cannot be started, the switch is
   rolled back at once (below).
5. Health-check it in the background and log `healthy` when it passes. Health never confirms: only `confirm` does.
   A check that gives up after `health.timeout` logs `unhealthy`; with `rollback_on_unhealthy = true` in the
   manifest the switch then rolls back at once (reason `health check failed`) instead of at its deadline; that
   rollback is decided before the `unhealthy` event is logged. It is off by default, for apps that are slow to warm
   up.

A switch that fails before step 4 (its `snapshot` hook fails, `storage_failed`, or its `state.json` cannot be
written) ends with no step in flight
and starts the previous generation again, or, when that one was waiting out a crash backoff, leaves its restart
scheduled as it was; a confirm that cannot be written fails and stays pending. A `SIGTERM` that arrives while the old
generation is stopping ends the switch the same way, just before step 4: the answer is `busy` (the daemon is shutting
down), the target stays `tested`, and the old generation is not started again by a daemon that is leaving; the next
daemon start runs the permanent generation.

### Confirm

`fawkes confirm [<gen>]`, normally called by the new generation itself once it knows it is healthy (Fawkes gives it
`FAWKES_BIN`, `FAWKES_SOCKET`, `FAWKES_GENERATION` and `FAWKES_PHASE`). An app confirms only when `FAWKES_PHASE` is
`live`, and names itself: `"$FAWKES_BIN" confirm "$FAWKES_GENERATION"`. Its test run gets the same variables, with
`FAWKES_PHASE=test`, while another generation may be pending. Without `<gen>` the CLI sends `$FAWKES_GENERATION`
when it is set and the daemon was found through `$FAWKES_SOCKET` (no `--socket` or `--manifest` points it at another
daemon), so a process Fawkes started (or one it left behind) can only ever confirm its own generation; outside
an app, a bare `confirm` confirms whichever generation is pending. The `confirmed` event says whether the confirm
named the generation. The pending generation becomes `permanent`, the previous permanent becomes `retired`, the
deadline is cleared. Confirming a generation that is not pending is `invalid_state`, except the permanent one:
`confirm <gen>` naming the permanent generation is ok with its record and changes nothing (no event, no write),
whatever else is going on, so an app whose confirm answer was lost can retry it (it starts no prune either). A bare
`confirm` with nothing pending stays `invalid_state`. Confirming the pending generation is also `invalid_state` while
its process is not running (stopped, or exited and about to be rolled back) or once its rollback is decided: only a generation that runs can vouch for itself. The supervisor
records a rollback (crash, missed deadline, failed health check) in the same step that learns of it, so a confirm
never slips in between.

### Rollback

Triggered by a missed deadline, by the pending generation's process exiting before confirm (a failed start), by its
health check giving up when `rollback_on_unhealthy` is set, by a
switch that cannot start the target's process, by `fawkes rollback`, or by a daemon start that finds a pending switch
in `state.json` (Fawkes itself died mid-switch). A deadline in `state.json` that cannot be parsed counts as passed
and is reported as an `error` event.
Code and storage roll back together:

1. Mark the pending generation `rolled_back` with the reason and save the step `rolling_back` with it. Deciding is
   one step under the daemon's lock, so no confirm fits in between, and the first reason stays. The save is best
   effort: rolling back is the safe direction, and a `state.json` that still says `pending` rolls back at the next
   start anyway.
2. Stop the pending generation.
3. Run the `restore` hook from the pending generation's `snapshot/` into `home` (see Hooks), every time: no hook
   decides to keep the storage, and only `rollback --keep-storage` (below) gives a restore up. Without hooks the
   rollback is code-only.
4. Clear the step, then start the permanent generation again (none on a first-ever switch).

If the restore fails (the hook exits non-zero or times out, `storage_failed`), the generation is still `rolled_back` (its code never runs again), but the step stays in
`state.json` as `rolling_back` with `restore_failed` set, and nothing is started on storage in an unknown state: `status` shows `failed`, and
`fawkes rollback` or the next daemon start retries the restore, and the error says so, with the other way out:
"`fawkes rollback` retries the restore, or `fawkes rollback --keep-storage` starts g1 on the storage as it is (it may
already be migrated)". The same holds when the restore worked but the end
of the switch could not be written to `state.json`. A confirm after the deadline is refused, even if the rollback has
not begun yet.

A `SIGTERM` that arrives during a rollback (requested, automatic, or the one a daemon start carries out) lets it
finish, but a daemon on its way out does not start the permanent generation: it logs a `daemon` event saying so (not
`failed`), the next daemon start runs it, and a requested rollback is answered `busy` instead of ok.

Rollback happens at most once per generation and only ever goes to the permanent generation. If that one then fails
too, Fawkes does not roll further back; the restart policy below applies and the failure is reported in `status`.

A restore that can never succeed (a deleted or corrupt `snapshot/`, a hook that is broken for good) would keep the
app down for good, so the operator can give it up and stop retrying the hook: `fawkes rollback --keep-storage`. It is allowed only while the step is `rolling_back` with
`restore_failed` (else `invalid_state`). It clears the step, leaves the generation `rolled_back` and records
`restore_abandoned_at` on it, logs a `rolled_back` event saying the storage was kept as it was, and starts the
permanent generation. **The risk:** the permanent generation then runs on storage that the failed generation may
already have migrated. The result is that of an ordinary rollback.

`fawkes rollback` with no pending switch is `invalid_state`; to go back past a confirm, `test` and `switch` to a
retired generation.

### Prune

`fawkes prune`, and in the background after every confirm (once no stage or test runs), when the manifest sets
`keep = N`: removes the oldest generations so that `N` remain. It never removes the permanent generation, the pending
one or the one it would roll back to, so more than `N` remain when those alone are more. A removed generation loses
its worktree (`git worktree remove`), its directory with its snapshot and logs, and its record in `state.json`;
`history` no longer lists it and its id is not reused. `prune` takes the transaction step slot, so it waits for no
stage or test and gets `busy` while one runs. Without `keep` it is `invalid_state` and nothing is ever pruned.

### Lifecycle: start, stop, restart

The usual supervisor actions, the same in the CLI and the socket. They act on the *current* generation:
the pending one while a switch awaits confirm, else the permanent one. They never change generations, the confirm
deadline or storage; only `switch`, `confirm` and rollback do.

- **start:** start the current generation if it is not running (also clears `failed`). Already running is
  `invalid_state`.
- **stop:** SIGTERM to the app's process group, SIGKILL after `stop_timeout` seconds of grace. The app stays stopped:
  no automatic restart until `start` or `restart`. Stopping a pending generation does not stop its deadline, and it
  cannot be confirmed while stopped; if it is
  not started and confirmed in time it is rolled back like any other. While the app waits out a crash backoff,
  `stop` cancels the scheduled restart and answers ok (`state` `stopped`, `restart_at` null); an app that is
  stopped and not scheduled is `invalid_state`.
- **restart:** stop (with the same grace), then start the same generation. It resets the backoff and does not count
  as a failed start.

`stop` and `restart` of an app that has already exited, before the supervisor has reaped it, are `busy`: the exit is a
crash for the supervisor to decide (a pending generation gets no retries), never a requested stop.

A requested stop is never a crash. Every other exit of the app's main process is, and Fawkes then ends the rest of
its process group (SIGTERM, SIGKILL after 1 s), so nothing the app started lives on unsupervised:

### Crashes, failed-start counter and backoff

Each generation counts `failed_starts`: unrequested exits within `min_uptime` seconds of a start. The pending
generation gets no retries: its first crash rolls it back. The permanent generation is restarted after a crash with a
backoff of 1, 2, 4, … s for the first, second, third, … failed start in a row (at most 30 s; a crash after a run
longer than `min_uptime` waits 1 s); a run longer than `min_uptime` resets its counter and the backoff; after
`max_failed_starts` consecutive failed starts it is no longer restarted and `status` reports `"state": "failed"` until
`start`, `restart`, the next `switch` or a daemon start. While a restart is scheduled, `status` shows
`"state": "stopped"` with `restart_at` set to its time; `stop` cancels it.

A cold start of the daemon always runs the permanent generation; `start` and `restart` run the current one.

### Daemon start

1. Create `state_dir` with mode `0700` (an existing one is set to it): it holds the app's logs and copies of its
   storage. It must be a directory the daemon's user owns, and it is never followed through a symlink: one that is a
   symlink, or another user's, is refused before its mode is touched, since whoever controls it could run code as
   the daemon's user from it. From then on the daemon uses its resolved path (symlinks in the components above it
   resolved), so swapping one of them later redirects nothing; events, generation `dir`s, `{state}` and `FAWKES_SOCKET` show
   that path.
   `home` is checked the same way; a missing one is created `0700`, an existing one keeps its mode. `<fawkes_home>`
   and its `run/` are created `0700` when missing.
2. Take an exclusive `flock` on `<state_dir>/lock`, held for the daemon's life; refuse to start when another daemon
   holds it. The kernel releases it when the process ends, however it ends, so it is never stale.
3. Bind the socket: refuse when a daemon of another state directory answers on it (a `--socket` shared by two
   manifests); a socket file nobody answers on belongs to a dead daemon and is replaced.
   Then link it into `<fawkes_home>/run/`. A startup that fails after this removes the socket and the link again.
4. Read `state.json`, or `state.json.bak` when `state.json` is unreadable (reported as an `error` event). Refuse to
   start when neither can be read, and when a generation is in a state this daemon does not know (a `state.json`
   written by a newer fawkes): the error names the generation and says to run the newer fawkes again or upgrade this
   binary, and the copy is not tried, since `state.json` was readable.
5. If `app.pid` names an app an earlier daemon left running, stop its process group (a crashed daemon leaves its
   app running). The live app's start wrapper writes its pid there before it runs the `start` command, so even an
   app spawned an instant before its daemon died is found. Only a process that started at most a minute before the
   file was written counts: a pid reused since, after a reboot too, belongs to a later process and is left alone.
   When that leader is gone but its group still has members (`start = "npm start"`), the group counts if the file
   was written after the machine booted.
   The install, test or `test_copy` hook that daemon was running is found and stopped the same way, through
   `step.pid`, before the next step removes the directory it runs in, and a `snapshot` or `restore` hook through
   `hook.pid`, before the rollback below runs the restore again.
6. Remove what a daemon that died mid-stage left behind: `staging` generations, generation directories no record
   owns, and the worktrees git still lists for them (`git worktree prune`).
7. Act on the step `state.json` says was in flight (`txn`):
   - none: start the permanent generation, if there is one;
   - `switching`: the target never ran (the last daemon was killed, or stopped by `SIGTERM` while the switch stopped
     the old generation). Log that the switch to it was cut short (it stays `tested`), clear the step and start the
     permanent generation;
   - `pending`: roll the switch back (reason `daemon_restart`); a pending switch is never resumed;
   - `rolling_back`: retry the rollback with its recorded reason (a `restore` hook is safe to run again, see
     Hooks), then start the permanent generation.

## Manifest

The manifest is trusted input, like a crontab or a unit file: its commands run with the daemon's user and
environment, and the repository's code runs the same way once staged. Whoever can write the manifest or push to the
repository can run anything as that user. Validation (below) catches mistakes, not attacks, so keep the manifest
writable by its owner only. Fawkes refuses to read (to start a daemon, reload, or find the socket) a manifest owned by
another user than the one running it, root excepted, or one that other users can write; group-writable passes, as
under a umask of `002` with a group of one's own. The error says which, and to `chmod o-w` it.

TOML, one file per app. Relative paths resolve against the manifest's directory. Strings in `install`, `start`,
`health.command`, `health.url`, the hooks and `env` values expand these templates:

| Template | Value |
|---|---|
| `{port}` | the live `port`, or the spare port during `test` |
| `{home}` | the app's state directory, or the test home during `test`; in a hook always the live one |
| `{code}` | the generation's worktree; in a hook the directory it runs in (see Hooks) |
| `{generation}` | the generation id, `g3` |
| `{state}` | Fawkes's state directory |
| `{snapshot}` | `snapshot` and `restore` hooks only: the generation's `snapshot/` |
| `{test_home}` | `test_copy` hook only: the test's empty home |

Expansion is one pass: a substituted value is never scanned again, and an unknown `{...}` stays as written. In
`install`, `start`, `health.command` and the hooks each value is shell-quoted (bare when it only has
`A-Z a-z 0-9 _ - . / : , + = @ %`, otherwise in single quotes with `'\''` for a single quote), so a path with a space or a `'` stays one word. In `[env]`
values and `health.url` values are substituted as is. A manifest that quotes a template itself (`'{home}'`) only keeps
working for paths without special characters, so commands use the bare template, or the `[env]` variable
(`"$APP_HOME"`).

```toml
name = "sample"                # used in messages and the default state_dir
repo = "../sample-repo"        # git repository generations are cut from
# state_dir = "state"         # default <fawkes_home>/<name>, fawkes_home = $FAWKES_HOME or ~/.fawkes (absolute)
home = "sample-home"           # the app's own state directory
port = 4790                    # live port
spare_ports = [4791, 4799]     # inclusive range for `test`
install = "npm ci"             # run in the worktree after checkout; optional
install_timeout = 1800         # seconds before a hanging install is stopped and the stage fails
reuse = ["node_modules"]       # cloned (copy-on-write) from the permanent generation before install; optional
keep = 5                       # generations `prune` keeps (it runs after every confirm); optional, default all
start = "node app.js"          # run in the worktree via /bin/sh -c
deadline = 120                 # seconds to confirm after switch; `switch --deadline` overrides
stop_timeout = 10              # seconds between SIGTERM and SIGKILL
min_uptime = 10                # seconds a start must survive to not count as failed
max_failed_starts = 3
rollback_on_unhealthy = false  # roll a pending switch back as soon as its health check gives up

[health]                       # exactly one of url or command
url = "http://127.0.0.1:{port}/"   # healthy on a 2xx or 3xx answer
# command = "curl -fsS http://127.0.0.1:{port}/health"   # healthy on exit 0
interval = 0.5                 # seconds between attempts
timeout = 60                   # seconds until `test` gives up

[hooks]                        # optional; see Hooks
snapshot = "node hooks.js snapshot {home} {snapshot}"   # at a switch: copy the storage into {snapshot}
restore = "node hooks.js restore {snapshot} {home}"     # at every rollback: put it back
test_copy = "node hooks.js copy {home} {test_home}"     # before a test: copy it into the test home
timeout = 600                  # seconds each hook may take

[env]                          # environment of start, health.command and the hooks, not of install
APP_PORT = "{port}"
APP_HOME = "{home}"
```

`$FAWKES_HOME` (or, without it, `$HOME`) must be an absolute path: a relative one would resolve against the current
directory of whoever runs the command, so worktrees would appear inside the repository and the CLI would look for the
socket somewhere else than the daemon. Fawkes refuses it with a message saying to use an absolute path.

**Validation:** the daemon refuses to start with (and does not reload) a manifest where:

- `name` is not made of letters, digits, `_` and `-` (it becomes a file name);
- a `reuse` entry is empty, absolute or contains `..` (it is joined onto the worktree);
- it has `storage`, which `[hooks]` replaced: the error says so;
- `hooks.snapshot` comes without `hooks.restore` or `hooks.test_copy`, or `hooks.restore` without `hooks.snapshot`
  (the error names the hook that is missing);
- `{snapshot}` appears anywhere but in `hooks.snapshot` and `hooks.restore`, or `{test_home}` anywhere but in
  `hooks.test_copy` (elsewhere they would stay as written);
- `port` lies inside `spare_ports`, or `spare_ports` is not `[low, high]`;
- `deadline` is not between 1 and 604800 seconds (a week);
- `install_timeout`, `stop_timeout`, `min_uptime`, `health.interval`, `health.timeout` or `hooks.timeout` is not a
  number of seconds above 0 and at most 86400;
- `max_failed_starts` or `keep` is 0;
- `[health]` does not have exactly one of `url` and `command`, or `url` is not `http://` with an explicit port
  after the host (`:{port}`, or a number from 1 to 65535);
- `port` or the first of `spare_ports` is 0;
- the file has a key the manifest does not know (a misspelt `stop_timeout` is an error, never silently ignored).

The error names every problem at once.

Every child that must end by itself (`install`, the hooks, `git`, `cp` and a health command) goes through one
runner: its own process group, stdin closed, a bound, and the whole group ended on every way out (SIGTERM, then
SIGKILL after a grace of `stop_timeout` for the install and the hooks, at once for the rest), so neither a child
that hangs nor what a finished one left running outlives it. `git worktree add` and the `reuse` copies get
`install_timeout` seconds, the other git calls 60. Past it the step fails with "timed out".
`git worktree add` skips the repository's hooks (`core.hooksPath=/dev/null`); the install is the place for setup.

A health attempt is bounded too. A `url` attempt must connect, send and read the status line (at most 1 KiB of it)
within 2 s in all, however slowly the app answers. A `command` attempt gets 10 s, and its process group is ended when
it finishes, passed or not, so nothing it backgrounded outlives it; the wait for the group after SIGKILL is bounded
(2 s). The pause between attempts is `interval` or the time left until `timeout`, whichever is shorter, so `unhealthy`
comes at `timeout` and not up to an `interval` after it (one attempt may still run past it).

`install` runs with Fawkes's own environment so package managers find their real caches, in its own process group.
After `install_timeout` seconds it is stopped like the app (SIGTERM, SIGKILL after `stop_timeout`) and the stage
fails; whatever it left running in its group is ended when it finishes; a stopping daemon kills it. A failed
install's output is kept in `<state_dir>/install-failed.log` (the latest one only). `start`, `health.command` and
the hooks get the inherited environment plus `[env]` plus `FAWKES_BIN`, `FAWKES_SOCKET`, `FAWKES_GENERATION` and
`FAWKES_PHASE` (`test` or `live`; `live` for `snapshot` and `restore`, `test` for `test_copy`).

**Copy-on-write reuse:** before `install`, every `reuse` directory is cloned from the permanent generation's worktree
(or the repository's working tree when there is none): `cp -cR` on macOS (APFS `clonefile`, near-instant and no extra
space), `cp -R --reflink=auto` elsewhere. The install then only reconciles the difference. A copy that fails or
times out (`install_timeout`) is skipped: its partial result is removed, a progress line says so, and the install
starts without it. The stage does not fail for it.

The Ultimate manifest (first user; see `examples/ultimate.toml`). Its `start` is the wrapper of the Ultimate extension
in `ultimate/fawkes/`, which runs Ultimate for `<project>` from the generation and isolates a test run like an
Ultimate preview (README there), and its `storage.ts` is the three hooks for Ultimate's `state.sqlite` and
`trace.sqlite`. Fawkes restores storage on a rollback, but not what the app did outside it, so the
extension holds Ultimate's task scheduler in a test run and in a new live generation until that generation has
confirmed itself: no agent work runs on code that may still roll back. Its `health.command` passes a test run only
once that hold is in place.

```toml
name = "ultimate"
repo = "/path/to/ultimate"
home = "/path/to/.ultimate/projects/my-app-<hash>"
port = 4747
spare_ports = [4790, 4799]
install = "pnpm install --frozen-lockfile"
reuse = ["node_modules"]
keep = 5
start = "node /path/to/fawkes/ultimate/fawkes/start.ts /path/to/my-app"
deadline = 120
stop_timeout = 30

[health]
command = "node /path/to/fawkes/ultimate/fawkes/health.ts"
timeout = 120

[hooks]
snapshot = "node /path/to/fawkes/ultimate/fawkes/storage.ts snapshot {home} {snapshot}"
restore = "node /path/to/fawkes/ultimate/fawkes/storage.ts restore {snapshot} {home}"
test_copy = "node /path/to/fawkes/ultimate/fawkes/storage.ts test-copy {home} {test_home}"

[env]
ULTIMATE_HOME = "{home}"
ULTIMATE_PORT = "{port}"
```

### Hooks

Fawkes does not understand storage. It keeps the two moments a transaction needs, a snapshot at the switch and a
restore at the rollback, plus a copy for each test, and runs the app's own commands for them, `[hooks]`. Each is a
shell command run via `/bin/sh -c`, with `{home}` the live home:

- `snapshot`: at a switch, after the old generation has stopped and before the target is marked `current`. It
  copies the storage into `{snapshot}`, `generations/<target>/snapshot/`, which Fawkes makes afresh just before
  and keeps until the generation is pruned. A switch without the hook removes the one an earlier switch to the same
  generation took, so a `restore` added later fails instead of putting old storage back.
- `restore`: at every rollback, after the pending generation has stopped and before the permanent one starts. It
  puts back the storage from `{snapshot}`. It must be **safe to run again after it failed** (`fawkes rollback` and
  the next daemon start retry it), and leave every file either as it was or wholly restored (write beside it, then
  rename), since Fawkes trusts its exit code alone. A hook never decides to keep the storage: if there is nothing to
  restore it fails, and only `rollback --keep-storage` gives a restore up.
- `test_copy`: at a test, before the app starts, into `{test_home}`, which is empty and is the `{home}` the test run
  gets. The live app may be running and writing meanwhile.

A hook runs in the **permanent generation's worktree** (`{code}`), or in `repo` before the first confirm; never in
the generation being tested, switched to or rolled back, so code under suspicion never decides how storage is
copied or restored. A hook that names an absolute path (as Ultimate's do) runs the same code from any of them. Each
runs in its own process group through the bounded runner above, for at most `hooks.timeout` seconds (default 600),
and its output goes to `generations/<id>/logs/hooks.log`. A hook that exits non-zero or times out fails its step
with `storage_failed`, which means "a storage hook failed": the test fails and the generation stays as it was, the
switch does not happen (Switch), or the restore fails and the app stays stopped (Rollback). `snapshot` and `restore`
are tracked in `hook.pid`, `test_copy` in `step.pid`, so a stopping daemon kills them and the next one stops a hook
the last one left.

**An app without hooks gets a code-only rollback.** The old generation then starts on whatever storage the failed one
left, migrations included, and every test starts on an empty home, never the live one. This is a change from the
first design, where Fawkes copied the SQLite files listed in `storage` itself: that is now the app's hook (the
sample's `examples/sample/hooks.js` and the Ultimate extension's `ultimate/fawkes/storage.ts` copy with `VACUUM
INTO` a file beside the target and a rename, which a plain `cp` of a live WAL database is not). `snapshot` and
`restore` come together, and `snapshot` requires `test_copy`, so an app whose storage is worth a snapshot is never
tested on an empty home.

**Upgrading to this contract:** a manifest with `storage` no longer loads, and a `snapshot/` written before
(`index.json` and `<n>.sqlite`) is not what the new hooks read. Install this build only with no switch pending, and
add `[hooks]` to the manifest.

## CLI

```
fawkes daemon
fawkes start
fawkes stop
fawkes restart
fawkes stage    <ref>
fawkes test     <gen>
fawkes switch   <gen> [--deadline SECS]
fawkes up       <ref> [--deadline SECS]
fawkes confirm  [<gen>]
fawkes rollback [--keep-storage]
fawkes prune
fawkes status
fawkes log      [-n N] [--follow]
fawkes history
fawkes watch    [--since N]
fawkes service  install|uninstall|restart
fawkes skill
fawkes --version
```

Global options, before or after the command: `--json`, `--manifest APP`, `--socket PATH`. A command's own options come
after it. `-h` prints a command's usage and `--help` its long help (details, examples), after `fawkes` or after a
command, and exits 0; `help` is not a command, so `fawkes stage help` stages the ref `help`. A mistyped command is a
usage error that names the similar commands (`tip: a similar subcommand exists: 'log'`), never one run in its place.
`fawkes --version` (or `-V`) prints `fawkes <version>` and exits 0.

`fawkes skill` prints `skills/fawkes/SKILL.md` as it was when the binary was built, so the skill an agent installs
(`fawkes skill > .agents/skills/fawkes/SKILL.md`) always matches the binary. Under `--json`: `{"skill": "<text>"}`.

### Finding the app

A client command reaches its daemon by the first of:

1. `--socket PATH`;
2. `--manifest APP`: that manifest's `<state_dir>/fawkes.sock`;
3. `$FAWKES_SOCKET`, which Fawkes sets for every process it starts, so an app reaches its own daemon without a word;
4. `$FAWKES_MANIFEST`, read like `--manifest`;
5. the nearest `fawkes.toml`, from the current directory up, stopping at the first directory that holds a `.git` (the
   repository root, so a project inside another never takes the outer one's manifest);
6. the manifest in `<fawkes_home>` (`*.toml`, the app named by the file name) whose `repo` is the current directory
   or one of its parents. When more than one does, the command is refused (`usage`) with their names and asks for
   `--manifest <name>`; when none does, the error names the apps there are.

`APP`, for `--manifest` and `$FAWKES_MANIFEST`, is a path when it has a `/` or ends in `.toml`, and otherwise an app
name: `<fawkes_home>/<APP>.toml`, which must exist. An empty variable counts as unset. There is no guess beyond this:
not finding the app is a `usage` error (exit 2), and a running daemon is never picked because it is the only one.
`daemon` and `service`, which need a manifest, take steps 2, 4, 5 and 6; `watch` takes steps 1 to 4 and otherwise
follows every daemon (see Watch).

This order puts the flags before the environment, which **changes the earlier contract**, where `$FAWKES_SOCKET`
beat `--manifest` and only `./fawkes.toml` was read: an app Fawkes runs that names another app's manifest now
reaches that app's daemon, and its bare `confirm` then sends no `$FAWKES_GENERATION` (Confirm). An app that names
nothing still reaches its own daemon, since `$FAWKES_SOCKET` comes before every discovered manifest.

`--deadline` is whole seconds from 1 to 604800 (a week); anything else is a usage error and nothing is stopped (for
`up`, nothing is staged either).
`--keep-storage` belongs to `rollback` (a usage error elsewhere): it gives up a restore that failed, and the permanent
generation then runs on storage that the failed generation may already have migrated.

Commands that agents retry are idempotent: `stage <ref>` of a commit that already has a `staged`, `tested`,
`test_failed` or `retired` generation answers with it (`existing: true`; the text form says "already staged") instead
of building another, so to rebuild the same commit after the install environment changed, `prune` it first. `confirm
<gen>` of the permanent generation is ok. `test <gen>` of a `tested` generation tests it again.

`up <ref>` is `stage`, `test` and `switch` in one call, so an agent upgrades with one command. It is client-side, with
no request of its own: it sends the three requests in turn (the generation `stage` answers with is the one it tests
and switches to), so each step keeps its own slots, refusals and progress lines and the daemon's transaction code is
unchanged. Their progress goes to stderr as for the single commands (in text mode, plus a line saying `g4` was staged or
was already staged), and the output is the `switch` result alone, in text or `--json`. A step that fails ends the
command with that step's error and exit code, printed as that command prints it, and nothing after it runs (`busy`
too: a stage, test or switch in progress is the same refusal as for the single command). `up` never confirms: the new
generation does, as after `switch`, and the deadline still rolls it back. A client that dies midway leaves a `staged`
or `tested` generation, which running `up` again picks up, because `stage` of the same commit answers with the
existing generation. A client that dies once the switch has begun leaves a pending generation instead: `up` again
then stages a new generation and its switch is refused (`invalid_state`) until that one is confirmed or rolled back,
so a retry reads `status` first. Run from inside the app it upgrades, the switch stops the old generation's process group, the
client's included, so it gets no answer; that is expected, the daemon finishes the switch anyway and the new
generation reads the outcome from `status` or `history`.

Output: readable text by default, progress lines on stderr while a step runs. With `--json`, stdout carries exactly one
JSON document, the `result` or the `error` object of the response below, and progress goes to stderr as JSON lines.
That holds for every failure of the client itself too: it prints the same `{"code", "message"}` object (arguments it cannot
parse and a manifest that cannot be read are `usage`, no daemon on the socket, a connection that breaks or closes without an answer is
`daemon_unreachable`, a daemon that does not answer a quick command in time is `no_answer`). The exceptions are the
streams. `log --follow`: stdout carries one event document per line, as they happen, and nothing else when the
daemon ends the stream (in text mode too). `watch` (see Watch): one `{"name", "event"}` document per event, for as
long as it runs. `--json` does not apply to `daemon`, which prints text only.
**A `--json` result only ever gains fields:** a field is never removed, renamed or given another meaning, so a
reader ignores the fields it does not know. Enumerations may gain values (see Socket protocol).
Exit codes: `0` ok, `1` the command failed (`no_answer` included), `2` usage error (a manifest that cannot be loaded,
for `daemon` too), `3` the daemon is unreachable (`daemon_unreachable`). A stdout or stderr that
cannot be written (a reader that has gone, as in `fawkes status | head -0`) changes none of them: the daemon's answer
decides, and `log --follow` ends with `0`.

`start`, `confirm`, `status`, `history` and `log` without `--follow` wait 30 seconds for the daemon's next line, then
fail with exit 1 and `no_answer`, "the daemon … did not answer in time". The other commands wait as long as their step runs, which the
manifest's timeouts bound, and `log --follow` waits for ever.

Errors read as a sentence that says what to do next, for example:

```
error: g4 was rolled back at 2026-11-03T14:02:11Z (missed confirm deadline); stage the ref again for a new generation
error: no daemon answers on /Users/me/.fawkes/sample/fawkes.sock; start one with `fawkes daemon --manifest fawkes.toml`
```

### Service

`fawkes service install|uninstall|restart` runs the daemon under launchd as a user LaunchAgent, for a Mac that is a
server. A desktop user never needs it. It is a client-side convenience: it writes a plist and calls `launchctl`, and the
daemon cannot tell it was started this way (it has no code for it). macOS only; elsewhere each action is a `usage` error
(run `fawkes daemon --manifest …` under the init system; systemd user units are **Later**). The manifest is found as
for `daemon` (Finding the app); `--socket` is a usage error, since the service runs the daemon on the manifest's own
socket.
`--json` applies to all three: one result object, or the usual error object, with the usual exit codes.

The label is `dev.fawkes.<name>` and the plist `$HOME/Library/LaunchAgents/<label>.plist` (`HOME` must be set and
absolute; the directory is created), loaded in the domain `gui/<uid>`. `HOME` and `PATH` decide where the plist goes and
which `launchctl` runs. The plist holds:

- `ProgramArguments`: the canonical path of the running binary, `daemon --manifest <absolute manifest path>`.
- `EnvironmentVariables`: the installing process's `PATH` (launchd's own is minimal, and the app's commands need
  `node`, `git` and the rest), and `FAWKES_HOME` when it is set, because it decides the default `state_dir` and the
  `run/` links. The paths and variables must be UTF-8.
- `RunAtLoad` and `KeepAlive { SuccessfulExit = false }`: the daemon starts at login and again after a failed exit, and
  stays down after a clean one (a SIGTERM).
- `ExitTimeOut`: `3 × stop_timeout + 5`, rounded up, plus 10 s: the longest a daemon's shutdown takes (it waits up
  to `2 × stop_timeout + 5` s for a switch or rollback in flight, then stops the app with `stop_timeout` of grace).
  launchd's default of 20 s would SIGKILL it partway, and the next daemon start would have to recover. A changed
  `stop_timeout` reaches the plist with the next `install`.
- `StandardErrorPath`: `<state_dir>/daemon.log`, where the daemon's stderr goes. `install` creates `state_dir` (mode
  `0700`) when it is missing, because launchd opens that file before the daemon exists to create the directory. The
  daemon caps it at 10 MiB (Process model), but only the lines its own writer writes: a daemon that fails before it
  gets that far (an invalid manifest) adds its error at every start launchd retries, about every 10 s, uncapped.
- The binary is named as it was run, not resolved, so a symlinked install (Homebrew's `bin/fawkes`) keeps working
  after an upgrade removes the version it pointed at; a binary that moved needs `install` again.

**install** refuses a binary with a `CACHEDIR.TAG` in any ancestor directory (`usage`): cargo writes one at the top of
every target directory, and the next `cargo build` replaces the binary under the running daemon. Install a copy
(`cargo install --path . --locked`) and run `fawkes service install` from it. With a plist, `install` replaces it: it
boots the old agent out and waits until `launchctl print` no longer finds it (launchd answers a bootout before it has
let go of the service, and a bootstrap until then fails), writes the new plist beside it and renames it into place,
then bootstraps. Unless a loaded agent was just booted out, it refuses (`invalid_state`) when a daemon already answers
on the manifest's socket (one started from a terminal): it holds the state lock, so the agent's daemon would fail at
every start. Stop it first. A failing `launchctl` is `service_failed` with its message; after a failed
bootstrap the plist stays, and running `install` again or `uninstall` cleans up. Result:
`{"label", "plist", "log", "replaced"}` (`replaced`: the plist existed before).

**uninstall** boots the agent out and removes the plist; the state directory stays. It is idempotent: an agent that is
not loaded (`launchctl` exits 3, "No such process") is fine. Result: `{"label", "plist", "removed"}`, where `removed`
is false when there was neither a plist nor a loaded agent.

**restart** boots the agent out (a SIGTERM and the wait for a graceful stop) and bootstraps the existing plist, which
launchd reads afresh. Without a plist it fails with `not_found`: `install` first. The daemon starts as at any start, so
a pending switch is rolled back (see Daemon start). Result: `{"label", "plist"}`.

`launchctl` runs through `run_bounded` with `ExitTimeOut` plus 30 s.

## Socket protocol

`<state_dir>/fawkes.sock`, mode `0600`, newline-delimited JSON, one request per connection. Keep the state directory
path short: Unix socket paths are limited to about 100 bytes, counted on the resolved path (on macOS `/tmp` and `/var`
resolve to `/private/…`).

Request:

```json
{"command": "switch", "args": {"generation": "g3", "deadline": 60}}
```

A request line longer than 64 KiB, or one that has not arrived whole within 5 s of the connection (one deadline for the
whole line, however slowly it trickles in), is refused with `usage`. So is an optional argument of the wrong JSON type
(`{"generation": 7}`, `{"keep_storage": "true"}`, `{"limit": -1}`): it is never read as absent. `null` means absent,
and keys a command does not use are ignored. At most 64 connections are open at once, streams (`subscribe`,
`log --follow`) not counted; a 65th is answered `busy` at once without reading its request, and the client retries.
Response: zero or more progress lines, then exactly one final line, then the daemon closes the connection.

The daemon never waits on a client that stops reading: a write that blocks for 2 s ends that connection (a step in
progress carries on without it), and a `subscribe` or `log --follow` client that falls 1024 messages behind is
dropped, so its stream simply ends. Live clients reconnect.

```json
{"event": {"seq": 42, "at": "2026-11-03T14:00:01Z", "kind": "progress", "generation": "g3", "message": "stopping g2"}}
{"ok": true, "result": {"generation": "g3", "previous": "g2", "deadline": "2026-11-03T14:02:01Z"}}
{"ok": false, "error": {"code": "invalid_state", "message": "g3 is staged, not tested; run `fawkes test g3` first"}}
```

Error codes: `usage`, `not_found`, `invalid_state`, `busy`, `git_failed`, `install_failed`, `health_failed`,
`storage_failed`, `start_failed`, `port_in_use`, `internal`. (`daemon_unreachable`, `no_answer` and `service_failed` are
produced by the client only: no daemon answers, or it closed the connection without an answer; the daemon accepted a
quick request and did not answer in time; `fawkes service` could not write its plist or `launchctl` refused.)
A client treats a code it does not know as a failed command.

The same goes for the other enumerations a newer daemon may extend: a client reads an event `kind`, a generation
`state`, a `process.state` or a `step` it does not know as `unknown` and keeps the line, instead of dropping it and
leaving a stale picture. `unknown` is never written: a daemon does not send it, and refuses a `state.json` with a
generation in a state it does not know (see Daemon start). An event of a kind it does not know, in an `events.jsonl` a
newer fawkes wrote, is not replayed; its `seq` still counts. `--json` prints the event line as the daemon sent it, so an
agent sees the real kind. In text, `log` and `watch` print it as `unknown`.

Results per command:

| Command | Args | Result |
|---|---|---|
| `start` | — | the `process` object of `status` |
| `stop` | — | the `process` object, with `last_exit` of the stop; during a crash backoff, with `restart_at` null |
| `restart` | — | the `process` object |
| `stage` | `ref` | the generation record plus `existing`: `true` when the commit already had a `staged`, `tested`, `test_failed` or `retired` generation and nothing was built (see Generations), else `false` |
| `test` | `generation` | the generation record (its `test` field holds port, outcome, duration) |
| `switch` | `generation`, `deadline?` (seconds, `1..=604800`, else `usage`) | `generation`, `previous`, `deadline` |
| `confirm` | `generation?` | the generation record, now permanent (naming the generation that already is permanent answers ok with its record and changes nothing) |
| `rollback` | `keep_storage?` (`true` gives up a restore that failed, see Rollback: the permanent generation then runs on storage the failed generation may already have migrated; `invalid_state` unless one failed) | `from`, `to`, `reason` |
| `prune` | — | `removed` (ids, oldest first), `kept` (ids); `invalid_state` without `keep` in the manifest |
| `status` | — | see below |
| `log` | `limit?` (default 20, at most the 1000 newest), `follow?` | `events`; with `follow` the daemon instead sends the newest `limit` events and then every new one as `event` lines until the client closes or the daemon ends the stream with a `result` of null |
| `history` | — | `generations`, oldest first, each the generation record plus `log_tail`; `failed_install_tail` |
| `subscribe` | `limit?`, `since?` | never ends; see below |

`subscribe` is the stream for live clients such as `watch`. With `limit` it first replays that many recent events as
`{"event": …}` lines. With `since` (a `seq`) it instead replays the events after that number that are still in memory
(the newest 1000), at most `limit` of them when both are given; a client that lost its connection resumes with the last
`seq` it saw. A `since` at or above the next `seq` (the daemon's log started over) is ignored, so a client never waits
for a number that will not come. Then one `{"status": …}` line (the
`status` result) and one `{"history": …}` line (the `history` result), then, until the client closes, every event as
an `{"event": …}` line, followed by fresh `status` and `history` lines whenever a generation or the live app changed
state. Clients never poll; a countdown to the confirm deadline is computed client-side from `pending.deadline`.
Every event, including progress lines of a step another client started, reaches every subscriber exactly once and in
`seq` order: the replay and the live stream are cut at one point, so none repeats and none falls in a gap. `seq` is the
first field of an event, a number the daemon counts across all its events: it increases by one, continues after a
daemon restart, and is `0` for lines written before events were numbered.

**Events.** Every event has a `kind`, a `generation` (or `null`) and a `message`. The kinds, with what `generation`
holds:

| Kind | Logged when | `generation` |
|---|---|---|
| `daemon` | the daemon starts (`started, pid …`) or ends (`stopped`); a manifest reload succeeds; startup stops an app or step an earlier daemon left, removes a stage it cut short or a directory no generation owns, or finds a switch it cut short; a start is left to the next daemon because this one is shutting down | the generation concerned, else `null` |
| `progress` | a step narrates itself to the client waiting for it (stage, test, switch, rollback, prune, stop, restart); it does not mark a change of state | the generation the line is about |
| `started` | the live app's process has been spawned, with its pid and port (a `test` run does not log it) | the generation started |
| `healthy` | the live app's health check passed | the generation checked |
| `unhealthy` | the live app's health check gave up and its process is still the live one | the generation checked |
| `exited` | the supervisor found the live app's process gone without a request from Fawkes; the message says when it restarts, if it does. A stop on request logs `stopped` instead | the generation that ran |
| `failed` | Fawkes gives up the app: `max_failed_starts` starts in a row ended early, or a start of the permanent generation could not spawn the process; nothing restarts it until `start` or a switch | the generation given up |
| `stopped` | `stop` ended the app, or cancelled the restart a crash had scheduled | the generation stopped |
| `staged` | `stage` finished: worktree, reuse and install are done | the new generation |
| `tested` | `test` passed | the generation tested |
| `test_failed` | `test` ran and the health check failed (a spare port taken by another process is an `error`, not this) | the generation tested |
| `switched` | `switch` started the target on the live port; the confirm deadline runs | the target |
| `confirmed` | `confirm` made a generation permanent | the generation confirmed |
| `rolled_back` | a rollback ended: normally, or with its restore failed, with `state.json` unwritable, or with the restore given up by `--keep-storage`; the message says which | the generation rolled back (not the one that runs now) |
| `pruned` | `prune` removed at least one generation; the message lists them | `null` |
| `error` | something failed that no answer to a client reports: a stage that failed (its generation is already removed), a test that lost its spare port, a failed write of `state.json`, a refused reload, a failed `prune`, an unreadable deadline, a panic | the generation concerned, else `null` |

A generation record:

```json
{"id": "g3", "ref": "main", "commit": "238c447f…", "state": "current", "dir": "/Users/me/.fawkes/sample/generations/g3/code",
 "staged_at": "…", "test": {"port": 4791, "passed": true, "at": "…", "seconds": 2.1, "message": "healthy"},
 "switched_at": "…", "confirmed_at": null, "rolled_back_at": null, "rollback_reason": null, "failed_starts": 0,
 "restore_abandoned_at": null}
```

`restore_abandoned_at` is set on a rolled-back generation whose failed restore `rollback --keep-storage` gave up.

`history` adds, to each generation's record, `log_tail`: the last 50 lines of its `logs/app.log`, read from the last
8 KiB of the file (a line cut by that bound is dropped; bytes that are not UTF-8 are replaced), as one string, or null
when the file does not exist or is empty. The result also has `failed_install_tail`, the same tail of
`install-failed.log`, the install output of the latest stage that failed (its generation is removed, so it cannot carry
it), null when none. Both are read when `history` is asked and are not part of `state.json`; a successor process can
explain its predecessor's rollback with one call. A client reads a missing field as null (an older daemon's history).

`status`:

```json
{"name": "sample", "daemon_pid": 812, "permanent": "g2", "current": "g3", "pending": {"generation": "g3",
 "previous": "g2", "deadline": "…"}, "step": "pending", "busy": null,
 "actions": ["stop", "restart", "stage", "confirm g3", "rollback"],
 "process": {"state": "running", "generation": "g3", "pid": 9001, "port": 4790, "started_at": "…",
             "uptime": 42.5, "restarts": 1,
             "last_exit": {"at": "…", "code": null, "signal": 15, "requested": true}, "restart_at": null}}
```

`step` is where the transaction in flight is, null when none: `switching`, `pending` (the switch is live and awaits
`confirm`), `rolling_back` (the rollback is decided and being carried out) or `restore_failed` (the restore failed and
the app stays stopped until `rollback` retries it or `rollback --keep-storage` gives it up). `pending` stays set during
`rolling_back` and `restore_failed`, because the generation is still live as far as `state.json` goes, so a client
reads `step`, not the presence of `pending`, to know whether a confirm can still happen. A status without `step` is an
older daemon's; its `pending` awaits a confirm. The text form of `fawkes status` says `waiting for confirm until …`
only for `pending`, `rolling back gN to gM` for `rolling_back`, and for `restore_failed` that the storage could not be
restored and which command retries or gives it up. A line `actions: …` (or `actions: none`) comes before the process.

`actions` lists the commands the daemon would accept right now, each as the command line without `fawkes`, so an agent
picks from it instead of encoding the state machine. The order is fixed: `start`, `stop`, `restart`, `stage`,
`test <gen>` (one per generation, oldest first), `switch <gen>` (one per generation), `confirm <gen>` (the pending
one), `rollback`, `rollback --keep-storage`, `prune`. An entry is there exactly when both checks the command itself
makes pass at the moment of the `status`: its slot is free, and the state allows it. The daemon computes the list from
the very functions the commands call, so it cannot say what a command would refuse. A command whose slot is taken is
left out, which means that while `busy` names a step, the commands it would refuse with `busy` are not listed. The
state checks, in short: `start` needs a current generation and no app running; `stop` an app running that has not
exited, or a scheduled crash restart to cancel; `restart` a current generation whose app, if any, has not exited;
`stage` nothing; `test <gen>` a generation that may be tested (`staged`, `test_failed`, `tested`, `retired`);
`switch <gen>` a `tested` one, no switch in flight and no failed restore; `confirm <gen>` a pending switch within its
deadline whose process runs and has not exited (the permanent generation is never listed: confirming it is answered ok
and changes nothing); `rollback` a pending switch or a rollback that did not finish; `rollback --keep-storage` a
restore that failed; `prune` a `keep` in the manifest. A status without `actions` is an older daemon's.

| Situation | `actions` |
|---|---|
| idle, no generation yet | `stage` |
| permanent runs | `stop`, `restart`, `stage`, then `test` and `switch` entries for the generations that allow them |
| permanent stopped | `start`, `restart`, `stage`, … |
| crash restart scheduled (`restart_at` set) | `start`, `stop`, `restart`, `stage`, … |
| given up (`failed`) | `start`, `restart`, `stage`, … |
| switching (holds both slots) | none |
| pending, its app runs, within the deadline | `stop`, `restart`, `stage`, `test` entries, `confirm gN`, `rollback` (no `switch`) |
| pending, deadline passed | as above without `confirm` (the supervisor rolls it back next) |
| pending, its app exited and is not reaped yet | `stage`, `test` entries, `rollback` |
| pending, its app stopped | `start`, `restart`, `stage`, …, `rollback` |
| `rolling_back` (the supervisor holds the live slot) | `stage`, `test` entries |
| `restore_failed` | `stage`, `test` entries, `rollback`, `rollback --keep-storage` |
| a `stage` or `test` in progress (`busy` names it) | only what does not take the step slot: the lifecycle commands the state allows, and `confirm gN` |
| `keep` set, step slot free | the list above and `prune` last |

`process.state` is `running`, `stopped` or `failed`; `uptime` is seconds, null when not running; `restarts` counts
starts of the current generation after its first (crash restarts and `restart`) since it became current or the daemon
started; `last_exit` is the most recent exit of the app (exit `code` or terminating `signal`, and whether Fawkes asked
for it), null before the first. `restart_at` is the UTC time of the scheduled crash restart while the app waits out
its backoff (`state` is `stopped` then), null otherwise. `busy` names the step in progress: a transaction step (`stage`, `test g3`,
`switch g3`, `rollback`, `prune`), else one that moves the live process (`start`, `stop`, `restart`, or the
supervisor's `reap`, `restart` and `rollback`).

## Watch

`fawkes watch [--since N]` prints the events of every supervised app as they happen, one line per event with the
app's name first: for a person at a terminal, or an agent reading `--json`. It holds one `subscribe` stream per daemon
and no state of its own, so it works against daemons started elsewhere, and quitting it never affects an app.
`--manifest`, `--socket`, `FAWKES_SOCKET` or `FAWKES_MANIFEST` names one daemon instead (the current directory never
does), which must answer the first connection: if it
does not, `watch` fails with `daemon_unreachable` (exit 3).

**Discovery:** each daemon links its socket as `<fawkes_home>/run/<name>.sock` when it starts and removes the link
when it stops (a daemon with a custom `state_dir` is found the same way). `fawkes_home` is `$FAWKES_HOME`, default
`~/.fawkes`. `watch` lists that folder every 3 s, so daemons started later are followed too, and retries a link nobody
answers on (a daemon that stopped or crashed) quietly every second. Two daemons with the same `name` are refused at
start (the link already answers).

**Lines:** in text, the app's name and the event as `log` prints it. Under `--json`, the `subscribe` event line with
the name in front, `{"name": "ultimate", "event": {…}}`, the event as the daemon sent it. stdout carries nothing else.

```
ultimate  2026-11-03T14:02:01Z  switched    g7   switched from g6; confirm before 2026-11-03T14:04:01Z
blog  2026-11-03T14:02:02Z  exited      g3   exited with code 1 (unrequested)
ultimate  2026-11-03T14:02:03Z  healthy     g7   health check passed
```

**Where it starts:** `--since N` starts after event N of each daemon, as far back as the newest 1000 it keeps
(`--since 0` is all of them). Without it, each daemon starts at the time `watch` started: the events logged before
are skipped, so a daemon found later still shows its own start.

**Reconnect:** a stream that ends (a daemon killed or restarted, a `busy` refusal, or a stream the daemon dropped
while a paused stdout held `watch` up: it buffers 1024 lines, then stops reading) is opened again every second with
`since` set to the last `seq` printed, so the events logged meanwhile are printed once, in order. Events are missed
only when the daemon no longer keeps them (more than 1000 since), which a note says, or when its log started over
(its files were removed, and it numbers from 1 again): the new log's events up to the reconnect are not replayed,
and a note says so only if the next event is numbered at or below the last one printed. Notes go to stderr, as text
in both modes, once per outage: `ultimate: the connection was lost; reconnecting` (or the refusal's message and `; retrying`), `ultimate:
connected again`, `ultimate: events 6 to 8 were missed: the daemon no longer keeps them`, `ultimate: the daemon's
event log started over`. Until the first `status` line names the app, a note names the socket.

**End:** `watch` runs until it is stopped, or until a line cannot be written to stdout (a reader that has gone, as in
`| head`), which it notices at the next event and ends with `0`.

## On-disk layout

```
<state_dir>/
  lock                   flock'ed by the running daemon; empty
  fawkes.sock            the daemon's socket
  state.json             generations, permanent, `txn` (the transaction step in flight: `switching`, `pending`
                         with its deadline, or `rolling_back` with its reason; null between switches); durable
                         writes (temp file, fsync, rename, fsync of the directory). A file with the older
                         `pending` field instead of `txn` still loads
  app.pid                pid (= process group) of the live app, written by its start wrapper
  step.pid               the same for the install, test or `test_copy` hook in progress
  hook.pid               the same for the `snapshot` or `restore` hook in progress (an automatic rollback restores
                         beside a stage or test)
  state.json.bak         the same content, written after state.json; read when state.json is unreadable
  events.jsonl           append-only event log, one JSON object per line, each with its `seq`; at 10 MiB it
                         becomes events.1.jsonl
                         (replacing the one before). `log` and `subscribe` answer from the newest 1000 kept in
                         memory
  current -> generations/g3/code    symlink to the live worktree, for people (replaced atomically)
  generations/<id>/
    code/                git worktree at the ref (`git worktree add --detach`)
    snapshot/            what the `snapshot` hook wrote of the previous generation's storage when switching to <id>,
                         read by its `restore`; its contents are the hooks' own
    test-home/           the test run's home, filled by `test_copy` (removed after the test, whether it passed,
                         failed or never got as far as starting the app)
    logs/install.log     install output
    logs/hooks.log       output of every hook run for <id>, with a header line per run
    logs/app.log         stdout and stderr of every run, live and test, with a header line per start; past
                         10 MiB the live one is copied to app.log.1 and emptied (checked every 10 s; lines
                         written during the copy are lost)
  install-failed.log     install output of the latest stage that failed (its generation is removed)
  daemon.log             the daemon's stderr under `fawkes service` (launchd writes it); past 10 MiB copied to
                         daemon.log.1 and emptied by the daemon
```

`state.json` is the source of truth; the deadline is an absolute time so it survives a daemon restart.

## Crates

For the daemon and CLI, each replacing something not worth hand-writing:

- **serde** (derive): typed manifest, state file and protocol messages.
- **clap** (no default features; `std`, `derive`, `help`, `usage`, `error-context`, `suggestions`): the command line,
  declared as one struct and one enum whose doc comments are the help text. It keeps the global options valid on
  either side of the command, exits 2 on a usage error and gives every command its own `--help`, which a
  hand-written parser had to spell out in a usage string kept in step by hand.
- **strsim** (through clap's `suggestions`): the similar names in the error for a mistyped command or option
  (`fawkes logs` → `log`), which would otherwise be a hand-written edit distance.
- **serde_json:** the socket protocol, `--json` output, `state.json` and `events.jsonl`.
- **toml:** the manifest format; people and agents write manifests by hand and want comments.
- **rustix** (features `event`, `fs`, `net`, `param`, `process`): safe wrappers for the system calls std lacks:
  `flock` for the state lock, signalling a process group (std only sends SIGKILL to one pid), `umask` around binding
  the socket, a peeking `recv` to notice a closed client, the clock tick on Linux, `poll` for the accept loop, and
  `waitid` with `WNOWAIT`, which waits for a child's exit without reaping it, so the `Child` that owns it still
  reaps it. One waiting thread per child is the same code on macOS and Linux, where kqueue's `NOTE_EXIT` and a
  pidfd would need one path each. Its typed `Pid` refuses 0, so a corrupt
  pid never signals our own group. The tests signal processes through it too, without `unsafe`.
- **signal-hook** (no default features): SIGTERM, SIGINT and SIGHUP set atomic flags and then write to the accept
  loop's self-pipe, installed with `sigaction` and `SA_RESTART`; hand-written handlers would need `unsafe` for the
  same thing.
- **jiff** (no default features; `std`): UTC timestamps in `state.json`, the event log and the protocol, and the
  times `watch` compares to its start. It replaces hand-written calendar arithmetic.
- **libc:** what neither std nor rustix has: a process's start time for the leftover-app check (`proc_pidinfo` on
  macOS) and the boot time (`sysctlbyname("kern.boottime")` on macOS).

For the tests only, **tempfile**: the unit tests' scratch directories, removed even when a test fails. The
integration harness keeps `/tmp/fk<port>`, which belongs to its port claim and stays short for socket paths. It and
`/tmp/fawkes-test-ports` must be the test user's own (a test fails on one another user created or can write), and the
harness creates `/tmp/fk<port>` afresh, `0700`, so none of its files is ever one someone else put there.

Hand-written instead of a crate: the HTTP health check (one GET over `std::net::TcpStream`, which takes any host
`to_socket_addrs` resolves, IPv6 literals such as `[::1]` included), threads instead of an async runtime, and the
errors: `protocol::Error` is a code and a message, and the steps below it return `String` messages that become that
message. anyhow's context chains would have to be flattened into every one of them, and thiserror would only add a
`Display` nothing calls.

## Later

- `confirm = "health"` for apps that cannot call `confirm` themselves.
- Falling back past the permanent generation when it crash-loops.
