---
name: fawkes
description: Run under Fawkes, the transactional process supervisor, as an app or as an agent that drives it. Use when FAWKES_GENERATION or FAWKES_PHASE is set in your environment, when you upgrade an app Fawkes supervises (`fawkes upgrade`, stage, test, switch), confirm or roll back a generation, or read `fawkes status`, `history` or an error code from it.
---

# Fawkes for agents

Fawkes runs one app and moves it between *generations*: git worktrees of the app's repository at a commit, with
their dependencies installed. Every upgrade is a transaction:

```
stage <ref> → test <gen> → switch <gen> → confirm      (or automatic rollback)
```

A switched generation is *current* until it confirms itself; then it is *permanent*. If it crashes first, or misses
its confirm deadline, Fawkes rolls back to the permanent generation, code and storage together. A generation rolls
back at most once and is never started again. `DESIGN.md` in the Fawkes repository is the full contract; this file
is what you need to act.

Every command but `up` (the supervisor itself, in the foreground) takes `--json`. Then stdout is one JSON document,
the result or an error object `{"code": "...", "message": "..."}`, and progress lines go to stderr. The two streams are the exception:
`log --follow --json` prints one event per line, and `watch --json` one `{"name", "event"}` per line, for as long as
they run. Exit codes: `0` ok, `1` the command failed, `2` usage error, `3` no daemon answers.

**Read fields, never parse the message.** The message is for people; every fact in it is also a field:

- An error object has `code`, and when they apply `generation` (the generation it is about) and `log` (the absolute
  path of the log that explains it: read that file).
- An event has `kind`, `generation` and `message`, plus the facts of its kind: `started` has `pid` and `port`;
  `switched` has `previous` and `deadline`; `exited` has `code`, `signal` and `restart_at`; `tested` and
  `test_failed` have `port` and `seconds`; `confirmed` has `named`; `adopted` (a first daemon start made its tested
  HEAD permanent, with no switch and no confirm) has none; `rolled_back` has `to`, `reason`, `cause` and
  `storage` (`restored`, `kept`, `restore_failed`, `none`); `pruned` has `removed`; `reported` has `info` (see
  below); `error` has `error_code`; `daemon` has `action` (`started`, `stopped`, `reloaded`, `cleaned_up`,
  `switch_cut_short`, `deferred`, `adopting`).

A result only ever gains fields, and an enumeration may gain values: ignore the fields you do not know, and treat a
value you do not know as "something else happened". `fawkes skill` prints this file as built into the binary you run.

## Which app does a command reach?

The first of: `--socket PATH`; `--manifest APP`; `$FAWKES_SOCKET`; `$FAWKES_MANIFEST`; the nearest `fawkes.toml`
from the current directory up to the repository root (the first directory with a `.git`), never inside
`~/.fawkes`; the manifest in `~/.fawkes` (`$FAWKES_HOME`) whose `project` holds the current directory, or, for one
without a `project`, whose `repo` does. `APP` is a path
when it has a `/` or ends in `.toml`, else an app name: `--manifest xana` means `~/.fawkes/xana.toml`, whose `name`
must be `xana`. When several apps match, or none does, the command fails with `usage` and names the apps; pass
`--manifest <name>`. Nothing is guessed: a daemon is never picked because it is the only one running.

## Do I run under Fawkes?

Fawkes starts your process with these variables:

| Variable | Meaning |
|---|---|
| `FAWKES_PHASE` | `live`: you are the app on its live port. `test`: you are a test run on a spare port, with a test home of your own. |
| `FAWKES_GENERATION` | Your generation, `g7`. |
| `FAWKES_BIN` | The `fawkes` binary that runs you. Call it by this path. |
| `FAWKES_SOCKET` | Your daemon's socket. The CLI finds it from this variable, so you need no `--manifest`; a `--manifest` or `--socket` you pass reaches that other daemon instead. |

None of them set: Fawkes does not run you. Run from the app's repository, or pass `--manifest <name>`.

In the `test` phase your home is a copy of the live storage (or empty), and the live app may be running beside you.
Serve your health check and nothing else: no confirm, no upgrade, no work with effects outside your home (messages
sent, jobs run, remote writes), since your code is not trusted yet. Fawkes enforces part of it: with `FAWKES_PHASE=test`
every command that changes something (`up`, `down`, `stage`, `test`, `switch`, `upgrade`, `rollback`, `prune`,
`start`, `stop`, `restart`, `reload`) fails with `test_phase`, whichever app it names, and so does a `confirm` of any generation but your
own `$FAWKES_GENERATION` towards your own daemon. `status`, `history`, `log`, `wait` and `watch` still work.

## Confirm yourself

Only in the `live` phase, and only once you know you work (your own checks pass, not just "the process started"):

```sh
"$FAWKES_BIN" confirm "$FAWKES_GENERATION"
```

Always name yourself. A bare `confirm` sends your `FAWKES_GENERATION` only to the daemon `FAWKES_SOCKET` names; to
another app's, or from anywhere outside an app, it confirms whatever is pending, which may not be you. Confirm before the
deadline the switch set (`status` → `pending.deadline`); after it you are rolled back. A confirm is safe to retry: once
you are permanent, confirming yourself again answers ok with your record. Being restarted (a crash restart, `fawkes
restart`) as the permanent generation needs no confirm.

Hold back work with effects outside your storage (scheduled tasks, outgoing messages, migrations of anything the
hooks do not copy) until the confirm has answered ok: until then a rollback can still undo your storage, but not those
effects.

## Say where you are: `report`

The live app may tell the operator where it is, which Fawkes cannot know: a few named lines, such as `url` (where it
listens), `remote` or `log`. It is a socket request, not a CLI command: write one JSON line to `$FAWKES_SOCKET` and read
the answer.

```json
{"command": "report", "args": {"generation": "<$FAWKES_GENERATION>", "info": {"url": "http://127.0.0.1:4749"}}}
```

Only the live app that runs now is heard (`invalid_state` otherwise, so never from a test run). At most 8 keys, each
`[a-z][a-z0-9_]{0,15}`, each value 1 to 256 characters without control characters, else `usage`. Every report replaces
the whole info, so report everything each time and retry freely. The info ends with your process; report again after
each start. Read it from `status` → `process.info` (`{}` when none); a change is a `reported` event, and `fawkes up`
prints it in the block it shows after every healthy start.

## Upgrade

```sh
fawkes upgrade <ref> [--deadline SECS] --json  # → {"generation": "g8", "previous": "g7", "deadline": …}
fawkes wait g8 --json                          # → exit 0 once g8 is permanent, 1 if it rolled back
```

`upgrade` stages the ref, tests it and switches to it, streaming each step's progress on stderr; it ends with the result
of the switch (`generation`, `previous`, `deadline`), plus `existing` (the stage found the commit already built) and
`test` (the test's record). Any step's failure is that step's error, with `step` (`stage`, `test` or `switch`) saying
which, and nothing is switched. `upgrade` never confirms: the new generation confirms itself.

`upgrade` returns as soon as the new generation is live. `fawkes wait [<gen>]` (default: the pending generation) then
waits for the outcome: exit `0` with the record once it is permanent, or exit `1` with `rolled_back` (its `cause`
says why) or, when the rollback could not restore the storage, `storage_failed`. It is bounded by the confirm
deadline, answers at once for a generation already decided, so it is safe to retry. Name the generation
`upgrade` answered with: an app that confirms quickly is permanent before a bare `wait` starts, and a bare `wait`
with nothing pending fails `invalid_state`. Upgrading an app other than yourself, always follow `upgrade` with `wait`.

If you run `upgrade` on yourself (you are the live app upgrading your own code), the switch stops you, and your
`upgrade` client with you, so there is no `wait` for you. That is expected: the daemon finishes the switch without you, and the
new generation reads the outcome. A failed stage or test still reaches you, since you are only stopped once the switch
begins.

Before retrying an `upgrade` whose answer you never got, read `status`: if a switch is pending (`step` is `pending`),
the last `upgrade` got as far as the switch, and another one would only be refused at its switch. Otherwise `upgrade`
again is safe: it picks up the generation the last one staged.

The steps one by one, when you need to look in between:

```sh
fawkes stage <ref> --json      # → the generation record, with "existing": true when it was already built
fawkes test g8 --json          # boots g8 on a spare port with a copy of the storage; must pass the health check
fawkes switch g8 --json        # stops the live app, snapshots the storage, starts g8 live; the deadline starts
```

Retrying is safe: `stage` of a commit that already has a `staged`, `tested`, `test_failed` or `retired` generation
returns that one with `"existing": true` instead of building again, and `test` of a tested generation tests it again.
A `rolled_back` generation is never reused; staging its ref builds a new one. **The catch:** re-staging a commit to
pick up a changed install environment (a new toolchain, a changed `install` command) returns the old build. Stage
another commit, or `fawkes prune` the old generation first.

## What may I call now? `status.actions`

```sh
fawkes status --json
```

`actions` lists the commands the daemon accepts right now, as command lines without `fawkes`, for example
`["stop", "restart", "stage", "test g7", "test g8", "switch g7"]` (g7 tested, g8 staged) or, with a switch
pending, `["stop", "restart", "stage", "confirm g9", "rollback"]`. Pick from it instead of working out the state
machine yourself. While `busy` names a step in progress, the commands it would refuse with `busy` are left out; read
status again when `busy` is null. An empty list means a step is under way that leaves nothing else to do: wait for it. `step` says where a transaction is: `switching`, `pending` (awaits confirm), `rolling_back`,
`restore_failed`, or null.

`status` also says which daemon answered: `version`, `manifest` (the file it loaded), `state_dir` and `socket`. After
editing the manifest, `fawkes reload --json` applies it and answers `changed` (the fields that changed), or refuses
(`usage` for an invalid manifest, `invalid_state` for a field that needs a daemon restart or a stopped app).

## When something rolled back

```sh
fawkes history --json
```

Each generation carries its `state`, `rollback_cause`, its last `test`, and `log_tail`: the last lines of its app
log. `rollback_cause` decides what you do next:

| `rollback_cause` | Means | What to do |
|---|---|---|
| `deadline` | It did not confirm before its deadline. | Confirm sooner, or switch with a longer `--deadline`. |
| `exited` | Its process exited before it confirmed. | Read `log_tail`; fix the code. |
| `unhealthy` | Its health check gave up (`rollback_on_unhealthy`). | Read `log_tail`; fix the code or the check. |
| `start_failed` | The switch could not start it. | Check the manifest's `start`. |
| `requested` | `fawkes rollback` asked for it. | Nothing went wrong by itself. |
| `daemon_restart` | The daemon ended while it was pending. | Upgrade again. |

`rollback_reason` is the same as text for people. `failed_install_tail` holds the end of the latest failed install's
log (that stage's generation is removed). `fawkes log -n 50 --json` gives the event log; `fawkes log --since N --json`
gives the events after the `seq` N you saw last.

## Error codes

| Code | Means | What to do |
|---|---|---|
| `usage` | The request or arguments are wrong (unknown command, `--deadline` out of range, unreadable manifest). | Fix the call; retrying the same one fails the same way. |
| `not_found` | No such generation. | `fawkes history` lists them. |
| `invalid_state` | Not allowed from the current state (switch to an untested generation, confirm with nothing pending, rollback with nothing pending, start while running). | Read `status` → `actions` and the message, which names the next step. |
| `busy` | Another step holds the slot this command needs (a stage, test, switch, rollback, prune, start, stop or restart), or the daemon is shutting down. | Wait until `status` → `busy` is null, then retry. |
| `git_failed` | The ref is not a commit in the repository, or git failed creating the worktree. | Check the ref (push or fetch it into the repository Fawkes stages from). |
| `install_failed` | The install command failed or timed out; the generation is removed. | Read `failed_install_tail` in `history`, fix, stage again. |
| `health_failed` | The test run never passed its health check; the generation is `test_failed`. | Read its `log_tail` and `test.message`; fix and stage a new commit, or `test` again. |
| `storage_failed` | A storage hook (`snapshot`, `restore`, `test_copy`) failed or timed out. A failed snapshot means no switch happened; a failed restore leaves the app stopped. | Read the file the error's `log` names. After a failed restore: `fawkes rollback` retries it; `fawkes rollback --keep-storage` gives it up and starts the permanent generation on the storage as it is (it may already be migrated). |
| `start_failed` | The process could not be started, or no spare port was free for a test. A switch whose target cannot start is rolled back at once. | Check the `start` command and the spare ports. |
| `port_in_use` | Another process took the test's spare port. The generation is unchanged. | `test` again. |
| `internal` | Fawkes could not write its state (disk full, permissions). The message says what was kept. | Fix the disk; retry. |
| `rolled_back` | `fawkes wait`: the generation rolled back. Produced by the client. | Read its `cause` and `fawkes history`. |
| `test_phase` | A command that changes something, run with `FAWKES_PHASE=test`. Produced by the client. | Nothing: a test run serves its health check only. |
| `daemon_unreachable` | No daemon answers on the socket (exit 3). Produced by the client. | Do what the message says: `fawkes up --manifest APP`, or, from a shell outside the app, `fawkes service restart` when it runs as a launchd agent. |
| `no_answer` | The daemon took a quick request and did not answer within 30 s. Produced by the client. | Check `fawkes status`; the daemon may be stuck. |
| `service_failed` | `launchctl` refused a `fawkes service` command; the message carries what it said. Produced by the client. | Read the message; from a shell outside the app, `fawkes service install` again replaces the plist and reloads it. |

A code not in this table comes from a newer Fawkes: treat it as a failed command and read its message.

## On a server: `fawkes service`

On macOS the daemon may run as a launchd agent (`fawkes service install`, plist
`~/Library/LaunchAgents/dev.fawkes.<name>.plist`, its stderr in `<state_dir>/daemon.log`). Nothing changes for you:
the same commands reach it over the same socket. `fawkes service` (`install`, `restart` or `uninstall`) run from inside
the app it supervises is refused (`invalid_state`): each stops the daemon and the app, you with it, and a pending
switch rolls back; a client that dies before its `bootstrap` leaves the agent unloaded until someone logs in. Ask a
person to run it from a shell outside the app.

## Up and down

`fawkes up` runs the supervisor of one app in the foreground; `fawkes down` stops the app and its supervisor, and a
launchd agent's daemon then stays down until the next login or `fawkes service restart`. People run them, from a
shell outside the app: `down` from inside the app it supervises is refused (`invalid_state`), since it would stop you
and leave nothing to start the app again. To restart only the app, `fawkes restart`.

## Storage rolls back only through the app's hooks

Fawkes does not understand storage. The manifest's `[hooks]` do it: `snapshot` at a switch, `restore` at every
rollback, `test_copy` before each test. What they copy rolls back; nothing else does. An app without hooks gets a
code-only rollback: the old generation starts on whatever storage the failed one left, migrations included, and
tests run on an empty home. Files outside the home, remote services and messages already sent are never rolled back.
