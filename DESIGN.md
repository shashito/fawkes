# Fawkes design

Fawkes supervises one app and moves it between *generations*. Every upgrade is a transaction:

```
stage <ref> → test <gen> → switch <gen> → confirm      (or automatic rollback)
```

This document is the contract: manifest, CLI, socket protocol, on-disk layout, and the state machine. The prototype
implements all of it except where a section says **later**.

## Process model

`fawkes daemon` is a long-running foreground process (launchd or systemd keeps it alive). It is the parent of the
app's live process and the only writer of the state directory. Every other command (`stage`, `test`, `switch`,
`confirm`, `rollback`, `status`, `log`, `history`) is a thin client that sends one request over the daemon's Unix
socket and renders the answer. One daemon supervises one app; run one daemon per manifest.

The daemon runs one transaction step (stage, test, switch, rollback) at a time; a second one gets `busy`. `status`,
`log`, `history` and `confirm` are always answered.

Signals are only for the daemon's lifecycle: `SIGTERM`/`SIGINT` stop the live app and exit (the generation keeps its
state, so the next start resumes it), `SIGHUP` re-reads the manifest (applies to the next start of any process).

## Generations and states

A generation is a git worktree at a ref with its dependencies installed, numbered `g1`, `g2`, … in staging order.

```
staged ──test──▶ tested ──switch──▶ current ──confirm──▶ permanent ──(next confirm)──▶ retired
   ▲  ◀─fails─┘ (test_failed,                  │
   │            retestable)                      └──missed deadline / exit / rollback──▶ rolled_back
```

- **staged:** worktree created and install succeeded. **test_failed:** the last test failed; `test` may run again.
- **tested:** started on a spare port with a copy of the storage and passed the health check. Only a tested
  generation can be switched to.
- **current:** live, not yet confirmed. A confirm deadline runs (Junos `commit confirmed`).
- **permanent:** confirmed by the app (RAUC/Mender mark-good). Exactly one generation is permanent at a time. A cold
  start of the daemon always runs the permanent generation (Erlang/OTP release handler).
- **retired:** was permanent, superseded by a newer confirm. Its worktree stays; `switch` to it again is allowed
  (after a new `test`), which is how an agent goes back to an older version on purpose.
- **rolled_back:** final. A rolled-back generation is never started again, so a broken rollback cannot loop; stage
  the same ref again to get a fresh generation.

### Switch

1. Refuse unless the target is `tested` and no other switch is pending.
2. Stop the running generation (SIGTERM to its process group, SIGKILL after `stop_timeout`).
3. Snapshot every storage file into the target's `snapshot/` (SQLite online backup API). Stopping first means no
   write of the old generation lands after the snapshot, so a rollback loses nothing the old generation wrote.
4. Start the target on the live port, mark it `current`, write the deadline (absolute time) to `state.json`.
5. Health-check it in the background and log `healthy` when it passes. Health never confirms: only `confirm` does.

### Confirm

`fawkes confirm [<gen>]`, normally called by the new generation itself once it knows it is healthy (Fawkes gives it
`FAWKES_BIN`, `FAWKES_SOCKET` and `FAWKES_GENERATION`). The pending generation becomes `permanent`, the previous
permanent becomes `retired`, the deadline is cleared. Confirming a generation that is not pending is `invalid_state`.

### Rollback

Triggered by a missed deadline, by the pending generation's process exiting before confirm (a failed start), by
`fawkes rollback`, or by a daemon start that finds a pending switch in `state.json` (Fawkes itself died mid-switch).
Code and storage roll back together:

1. Stop the pending generation.
2. Restore every storage file from the pending generation's `snapshot/` (backup API into the live path; a file that
   did not exist at snapshot time is deleted together with its `-wal`, `-shm` and `-journal`).
3. Mark the pending generation `rolled_back` with the reason, then start the permanent generation again (none on a
   first-ever switch).

Rollback happens at most once per generation and only ever goes to the permanent generation. If that one then fails
too, Fawkes does not roll further back; the restart policy below applies and the failure is reported in `status`.

`fawkes rollback` with no pending switch is `invalid_state`; to go back past a confirm, `test` and `switch` to a
retired generation.

### Failed-start counter and restarts

Each generation counts `failed_starts`: exits within `min_uptime` seconds of a start. The pending generation gets no
retries: its first exit rolls it back. The permanent generation is restarted after an exit with a backoff of 1, 2, 4 s;
a run longer than `min_uptime` resets its counter; after `max_failed_starts` consecutive failed starts it is no longer
restarted and `status` reports `"app": "failed"` until the next `switch` or daemon start.

### Daemon start

1. Bind the socket (refuse if another daemon answers on it; remove a stale socket file).
2. If `state.json` records a live pid from an earlier daemon that is still running, kill its process group (a crashed
   daemon leaves its app running; pid reuse is accepted as a prototype risk).
3. If a switch is pending, roll it back (reason `daemon_restart`).
4. Start the permanent generation, if there is one.

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

```toml
name = "sample"                # used in messages and the default state_dir
repo = "../sample-repo"        # git repository generations are cut from
state_dir = "/tmp/fawkes-sample"   # default ~/.fawkes/<name>
home = "/tmp/sample-home"      # the app's own state directory
storage = ["app.sqlite"]       # SQLite files relative to home, snapshotted at switch
port = 4790                    # live port
spare_ports = [4791, 4799]     # inclusive range for `test`
install = "npm ci"             # run in the worktree after checkout; optional
reuse = ["node_modules"]       # cloned (copy-on-write) from the permanent generation before install; optional
start = "node app.js"          # run in the worktree via /bin/sh -c
deadline = 120                 # seconds to confirm after switch; `switch --deadline` overrides
stop_timeout = 10              # seconds between SIGTERM and SIGKILL
min_uptime = 10                # seconds a start must survive to not count as failed
max_failed_starts = 3

[health]                       # exactly one of url or command
url = "http://127.0.0.1:{port}/"   # healthy on a 2xx or 3xx answer
# command = "curl -fsS http://127.0.0.1:{port}/health"   # healthy on exit 0
interval = 0.5                 # seconds between attempts
timeout = 60                   # seconds until `test` gives up

[env]                          # environment of start and health.command, not of install
APP_PORT = "{port}"
APP_HOME = "{home}"
```

`install` runs with Fawkes's own environment so package managers find their real caches. `start` and
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
fawkes stage    <ref>
fawkes test     <gen>
fawkes switch   <gen> [--deadline SECS]
fawkes confirm  [<gen>]
fawkes rollback
fawkes status
fawkes log      [-n N] [--follow]
fawkes history
fawkes tui
```

Global options, before or after the command: `--json`, `--manifest FILE` (default `./fawkes.toml`), `--socket PATH`.
A client finds the daemon by `--socket`, else `FAWKES_SOCKET`, else `<state_dir>/fawkes.sock` from the manifest.

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

Response: zero or more progress lines, then exactly one final line, then the daemon closes the connection.

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
| `stage` | `ref` | the generation record |
| `test` | `generation` | the generation record (its `test` field holds port, outcome, duration) |
| `switch` | `generation`, `deadline?` | `generation`, `previous`, `deadline` |
| `confirm` | `generation?` | the generation record, now permanent |
| `rollback` | — | `from`, `to`, `reason` |
| `status` | — | see below |
| `log` | `limit?`, `follow?` | `events`; with `follow` the daemon instead streams every new event as an `event` line until the client closes |
| `history` | — | `generations`, oldest first |
| `subscribe` | — | never ends; see below |

`subscribe` is the stream for live clients such as the TUI. The daemon first sends one `{"status": …}` line (the
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
 "previous": "g2", "deadline": "…"}, "app": "running", "pid": 9001, "port": 4790, "busy": null}
```

`app` is `running`, `stopped` or `failed`; `busy` names the transaction step in progress.

## TUI

`fawkes tui` is a full-screen terminal client of the same socket: one `subscribe` connection for everything it
shows, and ordinary one-shot requests for the actions. It holds no state of its own and works against a daemon
started elsewhere; quitting it never affects the app.

```
┌ fawkes · ultimate ───────────────────────────────────────────────────────────┐
│ live    g7  main@238c447f  current   confirm in 01:42   pid 9001  :4747      │
│ perm.   g6  main@bf0ad442  permanent                                         │
│ staged  g8  feat@91aa2c01  tested ✓ 2.1s on :4791                            │
├ generations ─────────────────────────┬ log ──────────────────────────────────┤
│ g8  tested       feat@91aa2c01       │ 14:02:01 switch g7 (deadline 120s)    │
│ g7  current      main@238c447f       │ 14:02:03 g7 healthy                   │
│ g6  permanent    main@bf0ad442       │ 14:02:40 g8 test passed on :4791      │
│ g5  rolled_back  main@0c1d2e3f       │                                       │
├──────────────────────────────────────┴───────────────────────────────────────┤
│ s stage  t test  w switch  c confirm  r rollback  ↑↓ select  q quit          │
└──────────────────────────────────────────────────────────────────────────────┘
```

- The header shows the live, permanent and newest staged generation, the deadline countdown (red under 15 s) and the
  app's pid and port; `failed` and `busy` show there too.
- The generations list is the history; the selected row is the target of `t`, `w` and `c`. `s` asks for a ref.
- `w` and `r` ask for a `y` confirmation; a failed action shows its error message in the footer.
- The log pane tails the event stream, progress lines included.

Crate: **ratatui** (with its re-exported `crossterm` backend, so one dependency), the standard Rust TUI library;
hand-written ANSI would need its own diffing, resize and raw-mode handling.

## On-disk layout

```
<state_dir>/
  fawkes.sock            the daemon's socket
  state.json             generations, permanent, current, pending switch, live pid; atomic writes (temp + rename)
  events.jsonl           append-only event log, one JSON object per line (what `log` reads)
  current -> generations/g3/code    symlink to the live worktree, for people (replaced atomically)
  generations/<id>/
    code/                git worktree at the ref (`git worktree add --detach`)
    snapshot/            storage of the previous generation, taken when switching to <id>
      index.json         [{"path": "state.sqlite", "file": "0.sqlite", "present": true}]
      0.sqlite
    test-home/           storage copy the test run used (removed after the test)
    logs/install.log     install output
    logs/app.log         stdout and stderr of every run, live and test, with a header line per start
```

`state.json` is the source of truth; the deadline is an absolute time so it survives a daemon restart.

## Crates

Five for the daemon and CLI, plus ratatui for the TUI (see above), each replacing something not worth hand-writing:

- **serde** (derive): typed manifest, state file and protocol messages.
- **serde_json:** the socket protocol, `--json` output, `state.json` and `events.jsonl`.
- **toml:** the manifest format; people and agents write manifests by hand and want comments.
- **rusqlite** (features `bundled`, `backup`): the SQLite online backup API for consistent snapshots of live
  databases, including WAL contents. `bundled` builds a known SQLite instead of depending on the system one.
- **libc:** `kill(2)` on a process group (std only sends SIGKILL to one pid) and installing SIGTERM/SIGINT/SIGHUP
  handlers.

Hand-written instead of a crate: argument parsing (nine commands, three flags), the HTTP health check (one GET over
`std::net::TcpStream` to localhost), UTC timestamps (days-to-civil conversion), threads instead of an async runtime.

## Later

- `confirm = "health"` for apps that cannot call `confirm` themselves.
- Pruning old generations and snapshots; storage that is not SQLite.
- The Ultimate extension in `~/.ultimate/extensions/` that drives Fawkes for self-upgrades.
- Falling back past the permanent generation when it crash-loops.
