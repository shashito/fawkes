# Fawkes design

Fawkes supervises one app and moves it between *generations*. Every upgrade is a transaction:

```
stage <ref> → test <gen> → switch <gen> → confirm      (or automatic rollback)
```

This document is the contract: manifest, CLI, socket protocol, on-disk layout, and the state machine. The prototype
implements all of it except where a section says **later**.

## Process model

`fawkes daemon` is a long-running foreground process (launchd or systemd keeps it alive). It is the parent of the
app's live process and the only writer of the state directory. Every other command (`start`, `stop`, `restart`,
`stage`, `test`, `switch`, `confirm`, `rollback`, `prune`, `status`, `log`, `history`, `tui`) is a thin client that sends
requests over the daemon's Unix socket and renders the answer. One daemon supervises one app; run one daemon per
manifest.

The daemon runs one transaction step (stage, test, switch, rollback) at a time; a second one gets `busy`. `start`,
`stop` and `restart` get `busy` only while a switch or rollback moves the live process, or while the supervisor does
(`reap` after a crash, `restart` after its backoff, an automatic `rollback`). `status`, `log`, `history` and
`confirm` are always answered.

Signals are only for the daemon's lifecycle: `SIGTERM`/`SIGINT` stop the live app and exit (the generation keeps its
state, so the next start resumes it), `SIGHUP` re-reads the manifest. A reload is all or nothing and is logged as a
`daemon` event naming the changed fields, or an `error` event saying why it was refused:

- `name`, `repo`, `state_dir`, `home` and `storage` never change by reload; they need a daemon restart. (A changed
  `home` would restore a snapshot into another directory than the one it was taken from.)
- `port` does not change while the app runs; `stop` it first.
- Everything else (`start`, `env`, `health`, `rollback_on_unhealthy`, `install`, `reuse`, `keep`, `spare_ports`, `deadline`,
  the timeouts and the restart policy) applies from the next stage, test, switch or start on. An invalid manifest is not reloaded.

Change `home` or `storage` only while no switch is pending: a daemon start rolls a pending switch back into the
`home` of the manifest it reads.

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

- **staging:** recorded before the worktree is created and the install runs. A stage that fails removes the
  generation again (record, directory, worktree); one that a daemon's end cut short is removed at the next daemon
  start. Ids are never reused.
- **staged:** worktree created and install succeeded. **test_failed:** the last test failed; `test` may run again.
- **tested:** started on a spare port with a copy of the storage and passed the health check. Only a tested
  generation can be switched to.
- **current:** live, not yet confirmed. A confirm deadline runs (Junos `commit confirmed`).
- **permanent:** confirmed by the app (RAUC/Mender mark-good). Exactly one generation is permanent at a time. A cold
  start of the daemon always runs the permanent generation (Erlang/OTP release handler).
- **retired:** was permanent, superseded by a newer confirm. Its worktree stays until `prune` removes it; `switch` to it
  again is allowed (after a new `test`), which is how an agent goes back to an older version on purpose.
- **rolled_back:** final. A rolled-back generation is never started again, so a broken rollback cannot loop; stage
  the same ref again to get a fresh generation.

### Switch

1. Refuse unless the target is `tested` and no other switch is pending.
2. Stop the running generation (SIGTERM to its process group, SIGKILL after `stop_timeout`).
3. Snapshot every storage file into the target's `snapshot/` (SQLite online backup API). Stopping first means no
   write of the old generation lands after the snapshot, so a rollback loses nothing the old generation wrote.
4. Mark the target `current` and write the pending switch with its deadline (absolute time) to `state.json`, then
   start the target on the live port. The intent is on disk before any process of the target exists, so a daemon
   that dies from here on rolls the switch back at its next start. If the target cannot be started, the switch is
   rolled back at once (below).
5. Health-check it in the background and log `healthy` when it passes. Health never confirms: only `confirm` does.
   A check that gives up after `health.timeout` logs `unhealthy`; with `rollback_on_unhealthy = true` in the
   manifest the switch then rolls back at once (reason `health check failed`) instead of at its deadline. It is off
   by default, for apps that are slow to warm up.

A switch whose `state.json` cannot be written fails before step 4 and starts the previous generation again; a
confirm that cannot be written fails and stays pending.

### Confirm

`fawkes confirm [<gen>]`, normally called by the new generation itself once it knows it is healthy (Fawkes gives it
`FAWKES_BIN`, `FAWKES_SOCKET` and `FAWKES_GENERATION`). The pending generation becomes `permanent`, the previous
permanent becomes `retired`, the deadline is cleared. Confirming a generation that is not pending is `invalid_state`,
and so is confirming the pending generation while its process is not running (stopped, or exited and about to be
rolled back) or once its rollback is decided: only a generation that runs can vouch for itself. The supervisor
records a rollback (crash, missed deadline, failed health check) in the same step that learns of it, so a confirm
never slips in between.

### Rollback

Triggered by a missed deadline, by the pending generation's process exiting before confirm (a failed start), by its
health check giving up when `rollback_on_unhealthy` is set, by a
switch that cannot start the target's process, by `fawkes rollback`, or by a daemon start that finds a pending switch
in `state.json` (Fawkes itself died mid-switch). A deadline in `state.json` that cannot be parsed counts as passed
and is reported as an `error` event.
Code and storage roll back together:

1. Stop the pending generation.
2. Restore every storage file from the pending generation's `snapshot/` (backup API into the live path; a file that
   did not exist at snapshot time is deleted together with its `-wal`, `-shm` and `-journal`).
3. Mark the pending generation `rolled_back` with the reason, then start the permanent generation again (none on a
   first-ever switch).

If the restore fails, the generation is still `rolled_back` (its code never runs again), but the pending switch
stays in `state.json` and nothing is started on storage in an unknown state: `status` shows `failed`, and
`fawkes rollback` or the next daemon start retries the restore. The same holds when the restore worked but the end
of the switch could not be written to `state.json`. A confirm after the deadline is refused, even if the rollback has
not begun yet.

Rollback happens at most once per generation and only ever goes to the permanent generation. If that one then fails
too, Fawkes does not roll further back; the restart policy below applies and the failure is reported in `status`.

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

The usual supervisor actions, the same in the CLI, the socket and the TUI. They act on the *current* generation:
the pending one while a switch awaits confirm, else the permanent one. They never change generations, the confirm
deadline or storage; only `switch`, `confirm` and rollback do.

- **start:** start the current generation if it is not running (also clears `failed`). Already running is
  `invalid_state`.
- **stop:** SIGTERM to the app's process group, SIGKILL after `stop_timeout` seconds of grace. The app stays stopped:
  no automatic restart until `start` or `restart`. Stopping a pending generation does not stop its deadline, and it
  cannot be confirmed while stopped; if it is
  not started and confirmed in time it is rolled back like any other.
- **restart:** stop (with the same grace), then start the same generation. It resets the backoff and does not count
  as a failed start.

A requested stop is never a crash. Every other exit of the app's main process is, and Fawkes then ends the rest of
its process group (SIGTERM, SIGKILL after 1 s), so nothing the app started lives on unsupervised:

### Crashes, failed-start counter and backoff

Each generation counts `failed_starts`: unrequested exits within `min_uptime` seconds of a start. The pending
generation gets no retries: its first crash rolls it back. The permanent generation is restarted after a crash with a
backoff of 1, 2, 4, … s (at most 30 s); a run longer than `min_uptime` resets its counter and the backoff; after
`max_failed_starts` consecutive failed starts it is no longer restarted and `status` reports `"state": "failed"` until
`start`, `restart`, the next `switch` or a daemon start.

A cold start of the daemon always runs the permanent generation; `start` and `restart` run the current one.

### Daemon start

1. Take an exclusive `flock` on `<state_dir>/lock`, held for the daemon's life; refuse to start when another daemon
   holds it. The kernel releases it when the process ends, however it ends, so it is never stale.
2. Bind the socket: refuse when a daemon of another state directory answers on it (a `--socket` shared by two
   manifests); a socket file nobody answers on belongs to a dead daemon and is replaced.
3. Read `state.json`, or `state.json.bak` when `state.json` is unreadable (reported as an `error` event). Refuse to
   start when neither can be read.
4. If `state.json` records a live pid from an earlier daemon that is still running, kill its process group (a crashed
   daemon leaves its app running; pid reuse is accepted as a prototype risk).
5. Remove what a daemon that died mid-stage left behind: `staging` generations, generation directories no record
   owns, and the worktrees git still lists for them (`git worktree prune`).
6. If a switch is pending, roll it back (reason `daemon_restart`).
7. Start the permanent generation, if there is one.

## Manifest

TOML, one file per app. Relative paths resolve against the manifest's directory. Strings in `install`, `start`,
`health.command`, `health.url` and `env` values expand these templates:

| Template | Value |
|---|---|
| `{port}` | the live `port`, or the spare port during `test` |
| `{home}` | the app's state directory, or the test copy of it during `test` |
| `{code}` | the generation's worktree |
| `{generation}` | the generation id, `g3` |
| `{state}` | Fawkes's state directory |

Expansion is one pass: a substituted value is never scanned again, and an unknown `{...}` stays as written. In
`install`, `start` and `health.command` each value is shell-quoted (bare when it only has `A-Z a-z 0-9 _ - . / : , + = @ %`,
otherwise in single quotes with `'\''` for a single quote), so a path with a space or a `'` stays one word. In `[env]`
values and `health.url` values are substituted as is. A manifest that quotes a template itself (`'{home}'`) only keeps
working for paths without special characters, so commands use the bare template, or the `[env]` variable
(`"$APP_HOME"`).

```toml
name = "sample"                # used in messages and the default state_dir
repo = "../sample-repo"        # git repository generations are cut from
state_dir = "/tmp/fawkes-sample"   # default <fawkes_home>/<name>, fawkes_home = $FAWKES_HOME or ~/.fawkes
home = "/tmp/sample-home"      # the app's own state directory
storage = ["app.sqlite"]       # SQLite files relative to home, snapshotted at switch
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

[env]                          # environment of start and health.command, not of install
APP_PORT = "{port}"
APP_HOME = "{home}"
```

**Validation:** the daemon refuses to start with (and does not reload) a manifest where:

- `name` is not made of letters, digits, `_` and `-` (it becomes a file name);
- a `storage` or `reuse` entry is empty, absolute or contains `..` (they are joined onto `home` and the worktree,
  and a restore deletes files at those paths);
- `port` lies inside `spare_ports`, or `spare_ports` is not `[low, high]`;
- `deadline` is not between 1 and 604800 seconds (a week);
- `install_timeout`, `stop_timeout`, `min_uptime`, `health.interval` or `health.timeout` is not a number of seconds
  above 0 and at most 86400;
- `max_failed_starts` or `keep` is 0;
- `[health]` does not have exactly one of `url` and `command`, or `url` is not `http://` with an explicit port
  after the host (`:4790` or `:{port}`).

The error names every problem at once.

`install` runs with Fawkes's own environment so package managers find their real caches, in its own process group.
After `install_timeout` seconds it is stopped like the app (SIGTERM, SIGKILL after `stop_timeout`) and the stage
fails; whatever it left running in its group is ended when it finishes; a stopping daemon kills it. A failed
install's output is kept in `<state_dir>/install-failed.log` (the latest one only). `start` and
`health.command` get the inherited environment plus `[env]` plus `FAWKES_BIN`, `FAWKES_SOCKET`, `FAWKES_GENERATION`
and `FAWKES_PHASE` (`test` or `live`).

**Copy-on-write reuse:** before `install`, every `reuse` directory is cloned from the permanent generation's worktree
(or the repository's working tree when there is none): `cp -cR` on macOS (APFS `clonefile`, near-instant and no extra
space), `cp -R --reflink=auto` elsewhere. The install then only reconciles the difference.

The Ultimate manifest (first user; see `examples/ultimate.toml`):

```toml
name = "ultimate"
repo = "/path/to/ultimate"
home = "/path/to/.ultimate/projects/ultimate-<hash>"
storage = ["state.sqlite", "trace.sqlite"]
port = 4747
spare_ports = [4790, 4799]
install = "pnpm install --frozen-lockfile"
reuse = ["node_modules"]
start = "node bin/ultimate.ts --no-open"
deadline = 120

[health]
url = "http://127.0.0.1:{port}/"
timeout = 120

[env]
ULTIMATE_HOME = "{home}"
ULTIMATE_PORT = "{port}"
```

## CLI

```
fawkes daemon   [--manifest FILE]
fawkes start
fawkes stop
fawkes restart
fawkes stage    <ref>
fawkes test     <gen>
fawkes switch   <gen> [--deadline SECS]
fawkes confirm  [<gen>]
fawkes rollback
fawkes prune
fawkes status
fawkes log      [-n N] [--follow]
fawkes history
fawkes tui
```

Global options, before or after the command: `--json`, `--manifest FILE` (default `./fawkes.toml`), `--socket PATH`.
A client finds the daemon by `--socket`, else `FAWKES_SOCKET`, else `<state_dir>/fawkes.sock` from the manifest.

`--deadline` is whole seconds from 1 to 604800 (a week); anything else is a usage error and nothing is stopped.

Output: readable text by default, progress lines on stderr while a step runs. With `--json`, stdout carries exactly one
JSON document, the `result` or the `error` object of the response below, and progress goes to stderr as JSON lines.
Exit codes: `0` ok, `1` the command failed, `2` usage error, `3` the daemon is unreachable.

Errors read as a sentence that says what to do next, for example:

```
error: g4 was rolled back at 2026-11-03T14:02:11Z (missed confirm deadline); stage the ref again for a new generation
error: no daemon answers on /tmp/fawkes-sample/fawkes.sock; start one with `fawkes daemon --manifest fawkes.toml`
```

## Socket protocol

`<state_dir>/fawkes.sock`, mode `0600`, newline-delimited JSON, one request per connection. Keep the state directory
path short: Unix socket paths are limited to about 100 bytes.

Request:

```json
{"command": "switch", "args": {"generation": "g3", "deadline": 60}}
```

A request line longer than 64 KiB is refused with `usage`. Response: zero or more progress lines, then exactly one
final line, then the daemon closes the connection.

The daemon never waits on a client that stops reading: a write that blocks for 2 s ends that connection (a step in
progress carries on without it), and a `subscribe` or `log --follow` client that falls 1024 messages behind is
dropped, so its stream simply ends. Live clients reconnect.

```json
{"event": {"at": "2026-11-03T14:00:01Z", "kind": "progress", "generation": "g3", "message": "stopping g2"}}
{"ok": true, "result": {"generation": "g3", "previous": "g2", "deadline": "2026-11-03T14:02:01Z"}}
{"ok": false, "error": {"code": "invalid_state", "message": "g3 is staged, not tested; run `fawkes test g3` first"}}
```

Error codes: `usage`, `not_found`, `invalid_state`, `busy`, `git_failed`, `install_failed`, `health_failed`,
`storage_failed`, `start_failed`, `internal`. (`daemon_unreachable` is produced by the client only.)

Results per command:

| Command | Args | Result |
|---|---|---|
| `start` | — | the `process` object of `status` |
| `stop` | — | the `process` object, with `last_exit` of the stop |
| `restart` | — | the `process` object |
| `stage` | `ref` | the generation record |
| `test` | `generation` | the generation record (its `test` field holds port, outcome, duration) |
| `switch` | `generation`, `deadline?` (seconds, `1..=604800`, else `usage`) | `generation`, `previous`, `deadline` |
| `confirm` | `generation?` | the generation record, now permanent |
| `rollback` | — | `from`, `to`, `reason` |
| `prune` | — | `removed` (ids, oldest first), `kept` (ids); `invalid_state` without `keep` in the manifest |
| `status` | — | see below |
| `log` | `limit?`, `follow?` | `events`; with `follow` the daemon instead streams every new event as an `event` line until the client closes |
| `history` | — | `generations`, oldest first |
| `subscribe` | `limit?` | never ends; see below |

`subscribe` is the stream for live clients such as the TUI. With `limit` it first replays that many recent events as
`{"event": …}` lines (joined to the stream before the replay, so none falls in a gap). Then one `{"status": …}` line (the
`status` result) and one `{"history": …}` line (the `history` result), then, until the client closes, every event as
an `{"event": …}` line, followed by fresh `status` and `history` lines whenever a generation or the live app changed
state. Clients never poll; a countdown to the confirm deadline is computed client-side from `pending.deadline`.
Every event, including progress lines of a step another client started, reaches every subscriber.

A generation record:

```json
{"id": "g3", "ref": "main", "commit": "238c447f…", "state": "current", "dir": "/tmp/fawkes-sample/generations/g3/code",
 "staged_at": "…", "test": {"port": 4791, "passed": true, "at": "…", "seconds": 2.1, "message": "healthy"},
 "switched_at": "…", "confirmed_at": null, "rolled_back_at": null, "rollback_reason": null, "failed_starts": 0}
```

`status`:

```json
{"name": "sample", "daemon_pid": 812, "permanent": "g2", "current": "g3", "pending": {"generation": "g3",
 "previous": "g2", "deadline": "…"}, "busy": null,
 "process": {"state": "running", "generation": "g3", "pid": 9001, "port": 4790, "started_at": "…",
             "uptime": 42.5, "restarts": 1,
             "last_exit": {"at": "…", "code": null, "signal": 15, "requested": true}}}
```

`process.state` is `running`, `stopped` or `failed`; `uptime` is seconds, null when not running; `restarts` counts
starts of the current generation after its first (crash restarts and `restart`) since it became current or the daemon
started; `last_exit` is the most recent exit of the app (exit `code` or terminating `signal`, and whether Fawkes asked
for it), null before the first. `busy` names the transaction step in progress.

## TUI

`fawkes tui` is a full-screen terminal client of the same socket. One window shows every supervised app: a table with
one row per app, and below it the selected app's detail, generations and events, as sketched below. It holds one
`subscribe` connection per daemon, so every row stays live, and sends ordinary one-shot requests for the actions,
always to the selected app. It holds no state of its own and works against daemons started elsewhere; quitting it
never affects an app. `fawkes tui --socket PATH` (or `--manifest`) shows that one app only.

**Discovery:** each daemon links its socket as `<fawkes_home>/run/<name>.sock` when it starts and removes the link
when it stops (a daemon with a custom `state_dir` is found the same way). `fawkes_home` is `$FAWKES_HOME`, default
`~/.fawkes`. The TUI lists that folder, skips links nobody answers on, and rescans every few seconds so daemons
started later appear. Two daemons with the same `name` are refused at start (the link already answers).

The screen, at 80 columns, during an upgrade (the clock and event times are local; the daemon stores UTC):

```
 fawkes  3 apps                                                     14:02:31
 ST         APP        LIVE  PERM  STAGED  CONFIRM    PID    PORT  UP     RST
▌● running  ultimate   g7    g6    g8      ⏱ 01:42    9001   4747  3m12s  1  ▐
 ● running  blog       g3    g3    -       -          4188   3000  5d     0
 ✘ failed   worker     g5    g5    -       -          -      -     -      4
─ ultimate ────────────────────────────────────────────────────────────────────
 ● running  g7 main@238c447f  pid 9001  :4747  up 3m12s  restarts 1
   last exit SIGTERM (requested) · health ✔ 14:02:03
 ⏱ confirm g7 within 01:42, or it rolls back to g6 (code + storage)
 GEN  REF             STATE        AGE   INFO
 g8   feat@91aa2c01   tested       1m    ✔ test 2.1s on :4791
 g7   main@238c447f   current      3m    live · awaiting confirm
 g6   main@bf0ad442   permanent    2d    rollback target
 g5   main@0c1d2e3f   rolled_back  3d    ✖ missed confirm deadline
─ events ──────────────────────────────────────────────────────────────────────
 14:02:01 switched  g7  switched from g6; confirm before 14:04:01
 14:02:03 healthy   g7  health check passed
 14:02:40 tested    g8  test passed on port 4791 in 2.1s
 c confirm  ? help  b rollback  n stage  t test  w switch  s/x/r start/stop/restart
```

- **Layout:** no boxes around panes. A one-line header (app count, `N disconnected` when daemons dropped, the clock),
  the apps table, then one-line title rules (`─ name ───`) over the selected app's detail, generations and events,
  and a one-line footer. Only dialogs and the help overlay are boxed. The selection is a full-width reverse bar with
  `▌ ▐` marks at both ends (dimmed while the other list has focus).
- **State** is always symbol and word: `● running` green, `○ stopped` grey, `✘ failed` red (the whole row is red),
  `◌ disconnected` red (the row shows the last known picture, dimmed) and `⟳ <step>` yellow while the daemon is busy
  with a transaction step (`switch`, `rollback`, …).
- **Countdown:** `⏱ mm:ss` while a switch awaits confirm, in the table and in the detail line; yellow, red and bold
  under 15 s. The detail line says what a missed deadline does (`rolls back to g6 (code + storage)`). While a switch
  is pending the footer puts `c confirm` first, highlighted.
- **Generations**, newest first, with a relative AGE (since its last state change) and an Argo-style INFO: the test
  result, `live · awaiting confirm`, `rollback target` for the permanent generation while a switch is pending,
  `✖ <reason>` for `rolled_back` rows (red) and `✖ test failed: …` for `test_failed` ones.
- **Events** follow the newest line by default (kinds coloured: `switched` yellow; `rolled_back`, `failed`, `error`,
  `unhealthy`, an unrequested `exited` and `test_failed` red; `healthy`, `confirmed`, `tested` green; `progress`
  grey). `f` pauses the pane (the title says `paused`), `z` zooms it to full screen and back, `/` filters events by
  text (any case; Enter keeps the filter, Esc clears it).
- **Narrow terminals:** columns go in this order: PERM, RST, STAGED, UP, PID, PORT, LIVE (ST, APP and CONFIRM stay;
  the generations list drops AGE). The footer drops whole items from the end, never part of a label. Nothing panics
  on a tiny terminal; panes simply get no room.
- **Notices:** the last action's result (`✓ switch g3`, or the daemon's error in red) sits at the right end of the
  footer until the next key.

Keys:

| Key | Does |
|---|---|
| `↑` `↓` `j` `k` | select an app; a generation while the generations list has focus |
| `Tab` | move focus between the apps table and the generations list (the focused list's header is highlighted); `t` and `w` act on the selected generation |
| `s` `x` `r` | start, stop, restart the app |
| `n` | stage: type a ref inline, Enter stages it |
| `t` | test the selected generation |
| `w` | switch to the selected generation |
| `c` | confirm the pending generation |
| `b` | roll back the pending generation |
| `p` | prune old generations beyond the manifest's `keep` |
| `f` `z` `/` | pause/follow, zoom, filter the events |
| `?` | help overlay listing every key |
| `q`, Ctrl-C | quit |
| Esc | close the prompt, dialog, help or filter |

`w`, `b`, `p`, `x` and `r` ask first in a small centered dialog naming the target (`Switch demo to g4? y/N`); only `y`
confirms. A failed action shows the daemon's error message in the footer.

Crate: **ratatui** (with its re-exported `crossterm` backend, so one dependency), the standard Rust TUI library;
hand-written ANSI would need its own diffing, resize and raw-mode handling.

## On-disk layout

```
<state_dir>/
  lock                   flock'ed by the running daemon; empty
  fawkes.sock            the daemon's socket
  state.json             generations, permanent, current, pending switch, live pid; durable writes (temp file,
                         fsync, rename, fsync of the directory)
  state.json.bak         the same content, written after state.json; read when state.json is unreadable
  events.jsonl           append-only event log, one JSON object per line (what `log` reads)
  current -> generations/g3/code    symlink to the live worktree, for people (replaced atomically)
  generations/<id>/
    code/                git worktree at the ref (`git worktree add --detach`)
    snapshot/            storage of the previous generation, taken when switching to <id>
      index.json         [{"path": "state.sqlite", "file": "0.sqlite", "present": true}], written last and durably
      0.sqlite
    test-home/           storage copy the test run used (removed after the test)
    logs/install.log     install output
  install-failed.log     install output of the latest stage that failed (its generation is removed)
    logs/app.log         stdout and stderr of every run, live and test, with a header line per start
```

`state.json` is the source of truth; the deadline is an absolute time so it survives a daemon restart.

## Crates

Seven for the daemon and CLI, plus ratatui for the TUI (see above), each replacing something not worth hand-writing:

- **serde** (derive): typed manifest, state file and protocol messages.
- **serde_json:** the socket protocol, `--json` output, `state.json` and `events.jsonl`.
- **toml:** the manifest format; people and agents write manifests by hand and want comments.
- **rusqlite** (features `bundled`, `backup`): the SQLite online backup API for consistent snapshots of live
  databases, including WAL contents. `bundled` builds a known SQLite instead of depending on the system one.
- **rustix** (features `fs`, `net`, `process`): safe wrappers for the system calls std lacks: `flock` for the state
  lock, signalling a process group (std only sends SIGKILL to one pid), `umask` around binding the socket and a
  peeking `recv` to notice a closed client. Its typed `Pid` refuses 0, so a corrupt pid never signals our own
  group. Already in the tree through ratatui's crossterm.
- **signal-hook** (no default features): SIGTERM, SIGINT and SIGHUP set atomic flags, installed with `sigaction` and
  `SA_RESTART`; hand-written handlers would need `unsafe` for the same thing. Also already in the tree.
- **libc:** `localtime_r` for the TUI's local clock (neither std nor rustix has it).

Hand-written instead of a crate: argument parsing (fourteen commands, six options), the HTTP health check (one GET over
`std::net::TcpStream` to localhost), UTC timestamps (days-to-civil conversion), threads instead of an async runtime.

## Later

- `confirm = "health"` for apps that cannot call `confirm` themselves.
- Storage that is not SQLite.
- The Ultimate extension in `~/.ultimate/extensions/` that drives Fawkes for self-upgrades.
- Falling back past the permanent generation when it crash-loops.
