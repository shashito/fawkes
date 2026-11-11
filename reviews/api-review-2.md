# Fawkes's API, second review

This review runs the `cli-designer` skill's style guide (rules `R<n>`) and
its patterns from other tools (patterns `P<n>`) against everything an agent or app
talks to: the CLI, `--json`, the socket protocol, events, the manifest, the `FAWKES_*` environment and
`skills/fawkes/SKILL.md`. It follows [`cli-api-review-1.md`](cli-api-review-1.md) and leaves out what branch
#28950 is building from it (discovery, `--manifest <name>`, `FAWKES_MANIFEST`, `fawkes skill`, suggestions,
`--version`, help text, the additive promise), which it treats as done, and the host-setup items in memory
`host-setup-direction`. Nothing here changes code.

**Inputs:** `main` at `4473fd2`, a debug build of it run against a throwaway world (`FAWKES_HOME=/tmp/fkapi/home`, the
sample app, ports 31790–31799; the daemon was the only process stopped afterwards), and #28950's worktree read as it
stood (uncommitted diff of `src/main.rs`, `src/discovery.rs`, `DESIGN.md`, `SKILL.md`).

**Verdict:** the command surface is in good shape; what is left is mostly *data*. Results, errors and events carry
their facts in prose, so an agent told by `SKILL.md` to "read the `code`, never parse the message" has to parse the
message anyway to learn a deadline, a rollback target, why something rolled back, or which log to read. After an
upgrade, the outcome can only be polled. And the manifest's validation has one real bug and two misleading messages.

## Proposed improvements, ranked

Every item is an **addition** unless it says otherwise. None renames a field, a code or a command.

### 1. Facts as fields: events and errors stop hiding data in `message`

**Problem.** An event is `{seq, at, kind, generation, message}` and nothing else (`src/protocol.rs:498-506`). What a
watcher needs is only in the prose, from a real run:

```
{"kind":"switched","generation":"g3","message":"switched from g1; confirm before 2026-11-10T14:34:11Z",…}
{"kind":"rolled_back","generation":"g3","message":"missed confirm deadline; back to g1",…}
{"kind":"started","generation":"g1","message":"pid 61580 on port 31790",…}
```

The `daemon` kind covers start, stop, reload, leftover cleanup and a cut-short switch (`DESIGN.md` Events table), and
`error` covers a failed stage, a lost spare port and a refused reload, with no code to tell them apart. Error objects
are `{code, message}` (`src/protocol.rs:64`), and the log to read is only in the message: `"…; see <path>"`
(`src/daemon/transactions.rs:365`, `:456`, `:664`). `SKILL.md:125` tells the agent to read
`generations/<id>/logs/hooks.log`, a path relative to a `state_dir` that no result names. This contradicts
`SKILL.md:21` ("never parse the message").

**Change.** Optional typed fields, absent when they do not apply, documented in `DESIGN.md`'s Events table:

| Where | New fields |
|---|---|
| `started` | `pid`, `port` |
| `switched` | `previous`, `deadline` |
| `exited` | `code`, `signal`, `restart_at` |
| `tested`, `test_failed` | `port`, `seconds` |
| `confirmed` | `named` (the confirm named the generation) |
| `rolled_back` | `to`, `reason`, `cause` (item 2), `storage` (`restored`, `kept`, `restore_failed`) |
| `pruned` | `removed` |
| `error` | `code` (the `ErrorCode` of the failure) |
| `daemon` | `action` (`started`, `stopped`, `reloaded`, `cleaned_up`, `deferred`), `changed` on a reload |
| error objects | `generation`, `log` (the absolute path of the log that explains it) |

Under the additive promise #28950 is writing down, this is exactly the kind of change that is allowed. Readers that
do not know a field ignore it; `watch` and `log` text output are unchanged. **Source:** R22, R32; P5's `help:` lines
hold data for the same reason. **Size:** medium, about 200 lines plus `DESIGN.md`.

### 2. A rollback cause as a code

**Problem.** `rollback_reason` is free text, and its values mix styles: `"missed confirm deadline"`
(`src/daemon/supervisor.rs:275`), `"exited before confirm (exit status 3)"` (`:313`), `"health check failed"` (`:490`),
`"start failed: …"` (`src/daemon/transactions.rs:780`), `"rollback requested"` (`src/daemon/server.rs:231`) and the
snake-case `"daemon_restart"` (`src/daemon/mod.rs:552`). `SKILL.md:110` lists some of them as if they were values. An
agent deciding "fix the code" (crash, unhealthy) against "confirm faster" (deadline) has to match prose.

**Change.** Add `rollback_cause` to the generation record, `cause` to `RollbackResult` (`src/protocol.rs:659`) and to
the `rolled_back` event: `deadline`, `exited`, `unhealthy`, `start_failed`, `requested`, `daemon_restart`. Keep
`rollback_reason` as the readable text. A record written before has no cause and reads as null. **Size:** small.

### 3. `fawkes wait [<gen>]`: the outcome of a switch without polling

**Problem.** `up` and `switch` return as soon as the target is live (`switched to g3 …; confirm before … or it rolls
back`). Whether it then confirmed or rolled back is only found by polling `status` and reading `history`: in the run
above, `g2` and `g3` rolled back four seconds after `up` exited 0. The first review deferred a blocking wait (P7,
tmux's `wait-for`). `subscribe` already streams every state change, so the client can wait without a daemon change.

**Change.** `fawkes wait [<gen>]` (default: the pending generation) follows `subscribe` until the generation is
`permanent` (exit 0, the record) or `rolled_back` (exit 1, a new code `rolled_back`, with the reason and `cause`). A
failed restore ends it at once with `storage_failed`. A generation already permanent or rolled back answers at once,
so `wait` is safe to retry. It is bounded by the deadline the switch set (at most a week), so it never hangs on
nothing. The SKILL's upgrade recipe becomes `fawkes up main --json && fawkes wait --json`, for an agent upgrading an
app other than itself. **Source:** R19 (a query that answers through its exit code), R32, P7, P9. **Size:** about 120
lines client-side, one error code, `DESIGN.md` and `SKILL.md`.

### 4. Manifest validation: one bug, two wrong hints

- **Bug:** `spare_ports = [4791, 4792, 4793]` loads without an error (ran: `status` went past the manifest to
  `daemon_unreachable`), while `[4791]` is refused. `spare_ports` is `[u16; 2]` (`src/manifest.rs:115`) and the
  three-element array is accepted. `DESIGN.md:351` says a `spare_ports` that is not `[low, high]` is refused. An agent
  writing a list of ports silently gets a range. Fix: refuse it with "spare_ports is a range [low, high], not a list".
- **The tombstone is advertised:** a misspelt key gets ``unknown field `stop_timout`, expected one of `name`, `repo`,
  `state_dir`, `home`, `storage`, …`` because `storage` is a field of `Written` (`src/manifest.rs:111`). The list
  invites the key `[hooks]` replaced.
- **"Names every problem at once" holds only after parsing** (`DESIGN.md:361`). A misspelt key or a missing field
  stops at the first one, with a TOML caret block (`missing field ``home``` at "line 1, column 1"), so a hand-written
  manifest takes one round per error. Fix: collect unknown, missing and mistyped keys into the same list as the
  semantic problems, or say in `DESIGN.md` that they come first.
- **Wrong next step:** in every client command, every manifest error, found or not, ends with "; pass --manifest or --socket to say which
  daemon to talk to" (`src/main.rs:340`). For a manifest that was found but is invalid, or one others can write
  (`run `chmod o-w bad/fawkes.toml`; pass --manifest or --socket…`), the next step is to fix the file. #28950 keeps
  this suffix in `Endpoint::find`. Fix: append it only when no manifest was found.

**Size:** small.

### 5. `fawkes reload` instead of `kill -HUP`

**Problem.** The manifest is re-read only on `SIGHUP` (`DESIGN.md:46`, `src/daemon/mod.rs:580`). An agent has to read
`daemon_pid` from `status`, signal it, and then find a `daemon` or `error` event to learn whether the reload was
taken or refused (a port change while the app runs, a changed `repo`). It is the only daemon action without a
command.

**Change.** A `reload` request and command that run the same `reload_manifest` (`src/daemon/mod.rs:639`) and answer
with `{"changed": [...]}`, or the refusal as `usage` (an invalid manifest) or `invalid_state` (`port` while the app
runs, `name`/`repo`/`state_dir`/`home` changed: "restart the daemon"). `SIGHUP` stays. **Source:** R18, R32 ("one
blessed way"). **Size:** small, about 60 lines.

### 6. `status` says which daemon it is

**Problem.** `status --json` names the app and the daemon's pid, nothing else (real output:
`{"actions":["stage"],"busy":null,"current":null,"daemon_pid":50778,"name":"sample",…}`). Once #28950's discovery
picks the app from the current directory, "which manifest did it pick?" (P14) has no answer short of reading the
`daemon` start event. Nor does the daemon say its version, while `fawkes skill` prints the *client's* skill: a newer
CLI against an older daemon is invisible.

**Change.** Add `version`, `manifest` (the path it loaded), `state_dir` and `socket` to `status`, and the version to
the `daemon` started event. With `state_dir` known, the paths in item 1 and in `SKILL.md` resolve. **Source:** R16,
P14. **Size:** small.

### 7. Refuse the two calls the skill only warns about

**Problem.** `SKILL.md:139` says never to run `fawkes service` from inside the app it supervises (it stops the daemon
and the app, and a client killed before `bootstrap` leaves the agent unloaded), and `SKILL.md:38` says a test run does
no upgrade. Nothing enforces either: `src/service.rs` reads no `FAWKES_*` variable, and a process with
`FAWKES_PHASE=test` can `up` or `switch` its own daemon.

**Change.** Client-side refusals (`invalid_state`), since these guard against mistakes, not attacks:
`fawkes service …` when `FAWKES_SOCKET` is the socket of the manifest it would act on ("run it from a shell outside
the app"); `stage`, `test`, `switch`, `up`, `rollback`, `prune`, `start`, `stop`, `restart` and `reload` when
`FAWKES_PHASE=test` and the target is the caller's own daemon ("a test run only serves its health check"). **Source:**
R32's "chaos monkey", R20. **Size:** small. **Break:** only for callers doing what the skill already forbids.

### 8. `up --json` keeps what its steps learned

**Problem.** `up`'s result is the switch's alone (`{"deadline","generation","previous"}`). In text mode the client
adds "staged g4" or "g4 was already staged" (`src/main.rs:459-465`); under `--json` nothing says whether the stage
built anything or how the test went, and its stderr JSON lines are the steps' `progress` events with no step marker.
A failure says nothing about which step failed beyond what the code implies (`busy` and `invalid_state` can come
from any of the three).

**Change.** The result gains `existing` and `test` (the stage's flag and the test's record); a failure's error
object gains `step` (`stage`, `test`, `switch`). The text form adds the missing "g4 passed its test" line. **Size:**
small.

### 9. `log --since N`, and `-n` gets a long name

**Problem.** `watch --since N` and `subscribe`'s `since` resume after a `seq`; `log` can only take the last `-n`
(`src/main.rs:119`). An agent that saw events up to 40 cannot ask for "what came after 40" without a stream. `-n` is
the only option without a long name.

**Change.** `log --since N` (the `log` request gains `since`, as `subscribe` has it), and `-n, --limit N`. **Source:**
R5 (consistent flags), P11. **Size:** small.

### 10. Looser manifest defaults (carried over from C3)

**Problem.** Seven keys are required (`name`, `repo`, `home`, `port`, `spare_ports`, `start`, `[health]`;
`src/manifest.rs:104-135`). Trying it: `missing field ``home```, then `missing field ``health```, one at a time.

**Change.** `spare_ports` defaults to the ten ports after `port`, `[health]` to `url = "http://127.0.0.1:{port}/"`,
and optionally `home` to `<state_dir>/home`. A minimal manifest is then `name`, `repo`, `port`, `start`. Validation
still refuses unknown keys. **Source:** R7. **Size:** about 30 lines plus `DESIGN.md`. Not part of #28950's picks,
and independent of `host-setup-direction`.

### 11. Small text fixes

Each is a sentence or a line; together they finish R18 and R23.

| Where | Today (real output) | Change |
|---|---|---|
| `rollback --keep-storage`, nothing pending | "…and none has; `fawkes rollback` first" (`src/daemon/transactions.rs:93`) | Name `rollback` only when a rollback is pending; otherwise say nothing is pending |
| `start` while the daemon shuts down | `start_failed` "the daemon is shutting down" (`src/daemon/supervisor.rs:349`) | `busy`, as `switch` and `rollback` answer, and as `SKILL.md`'s `busy` row says |
| `stage nope` | "`nope` is not a commit, branch or tag in …/repo" (`src/daemon/transactions.rs:167`) | add "; push or fetch it into that repository" |
| `stage` text | "staged g1 (main @ bcd743ad)" | add "; `fawkes test g1` tests it" (R23, as `test` already does) |
| `start` when running | "g1 is already running (pid 51123)" | add "; `fawkes restart` restarts it" |
| `watch` with no daemon | prints nothing at all, on either stream | one stderr note: "no daemon runs yet; waiting for one in …/run" |
| `log -n 0`, empty `history` | a blank line | no line |
| `fawkes tui` | "unrecognized subcommand 'tui'" | a hidden tombstone: "the TUI was removed; `fawkes watch` prints every app's events" (R31) |
| `SKILL.md:20` | "stdout is exactly one JSON document" | name the two streams (`log --follow`, `watch`) |

## About #28950

The way it is built is sound: `discovery.rs` is pure over what it is given and tested that way, the walk-up stops at
`.git` (a file too, so worktrees count), ambiguity is refused with the names, and the contract change is written into
`DESIGN.md`. Three things to look at:

1. **Two names for one app.** The registry names an app by its file (`<fawkes_home>/<APP>.toml`,
   `discovery.rs` `manifests`), but everything else uses the manifest's `name`: `status.name`, `watch`'s first column,
   `run/<name>.sock`. A `xana.toml` with `name = "ultimate-xana"` answers to `--manifest xana` and shows up as
   `ultimate-xana`. Either refuse a registry manifest whose file name is not its `name` (the error says to rename the
   file), or match `--manifest` against `name`.
2. **The wrong hint survives** (item 4): `Endpoint::find` appends "pass --manifest or --socket" to a found manifest
   that does not load.
3. **`daemon_unreachable` for a service.** The hint is always `fawkes daemon --manifest …`. When
   `~/Library/LaunchAgents/dev.fawkes.<name>.plist` exists, `fawkes service restart` is the right next step.

## What's already good

- One level of verbs, few flags, no prompts, `stdout` only the result (R1, R5, R21, R26).
- Exit codes 0/1/2/3, documented, and kept for the client's own failures under `--json` (R19).
- Errors that name the next command, in most places: "g1 is not running; `fawkes start` starts it", "pruning needs
  `keep = N` in the manifest", "g3 was rolled back … ; stage the ref again for a new generation" (R18).
- `status.actions`, computed from the same checks the commands make (P13), and `step` beside `pending`.
- Retry-safe `stage`, `confirm` and `test`, and a client that tolerates codes, kinds and states it does not know.
- `history`'s `log_tail` and `failed_install_tail`: one call explains a rollback.
- `watch`: resumable by `seq`, notes on stderr, never affects an app.
- Strict manifest: unknown keys refused, a tombstone for `storage`, and a refusal for a manifest others can write.

## Considered and not recommended

- **Renaming the overloaded words.** `status.current` (the pending generation, else the permanent one) is not the
  generation state `current`; the manifest's `home` (the app's data), `FAWKES_HOME` (Fawkes's registry) and
  `{state}` are easy to mix up; `switch` answers `generation`/`previous` while `rollback` answers `from`/`to`. Each
  rename would need an alias forever (R31). Explain them in `SKILL.md` instead.
- **Idempotent `stop` and `start`.** Refusing a second start loudly is R20; keep it, and the hints in item 11 name the
  other command.
- **A `command` tag on every result, or versioned JSON (P5).** The caller knows what it ran; the additive promise
  covers change.
- **Reasons for the actions that are absent (P13's "say why").** Useful, but `status` would grow per generation.
  Later, if agents ask for it.
- **Capping `history`.** Without `keep`, `history --json` grows by up to 8 KiB of `log_tail` per generation. A
  `history <gen>` would bound it; not needed while `keep = 5` is the norm.

## Recommended set: one phase after #28950 lands

**"Outcomes as data":** items 1 to 9 and 11. Agents then read facts from fields, wait for an upgrade's outcome with
one command, reload a manifest with an answer, and get correct next steps from the manifest; the two calls the skill
forbids are refused. About 700 lines including tests, all additive but item 7, with `DESIGN.md` and `SKILL.md`
updated in the same change. Item 10 is a separate decision.

**Smaller alternative:** items 2, 4, 6 and 11. Fixes and fields only, no new command or event fields: about 150
lines. It leaves agents parsing event prose and polling for outcomes.

## Decisions

1. **Scope of the follow-up phase:** (a) the recommended set (1 to 9, 11); (b) the same without 5, 7 and 9 (no
   `reload`, no refusals, no `log --since`); (c) the smaller alternative (2, 4, 6, 11).
2. **Event facts:** (a) flat optional fields on the event (`event.deadline`); (b) one nested `data` object per event.
3. **Waiting for the outcome:** (a) a `fawkes wait [<gen>]` command and a `rolled_back` error code; (b) an
   `up --wait` flag instead; (c) neither, keep polling `status`.
4. **Manifest defaults (item 10):** (a) default `spare_ports` and `[health]`; (b) also `home`; (c) not now.
5. **#28950's two names for one app:** (a) refuse a registry manifest whose file name differs from its `name`;
   (b) match `--manifest` against `name`; (c) leave it and document it.
6. **When:** (a) pass the #28950 notes (its two names, the wrong hint) to #28950 now, since they touch its lines;
   (b) leave them for the follow-up phase.

My picks: 1a 2a 3a 4a 5a 6a.

**The user's answers:** 1a 2a 3a 4a 5a 6a. The follow-up phase builds items 1 to 9 and 11, with events carrying
their facts as flat optional fields, a `fawkes wait [<gen>]` command and a `rolled_back` code. Manifests default
`spare_ports` and `[health]` (item 10, without a default for `home`). The `spare_ports` bug is fixed in any case. The
#28950 notes go to #28950 now: refuse a manifest in `<fawkes_home>` whose file name differs from its `name`, drop
the "pass --manifest or --socket" hint for a manifest that was found, and name `fawkes service restart` when a plist
exists.
