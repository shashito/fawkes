Runs Ultimate's upgrades through the Fawkes supervisor: `fawkes upgrade` stages, tests and switches a ref, and the new generation confirms itself once it works, or Fawkes rolls back code and storage.

## Setup

To move an Ultimate host under Fawkes for the first time:

1. **Link this folder** into `~/.ultimate/extensions/fawkes` (every host loads it, and it stays inert outside Fawkes
   except for the setup item below), or into `<project>/.ultimate/extensions/fawkes` (that project only; it needs the
   project's Ultimate setup trusted).
2. **Open the host.** A host that does not run under Fawkes shows a muted `fawkes · set up` item in its status line (not
   a preview or a test server). Its card previews the manifest and writes it to `$FAWKES_HOME/<name>.toml`
   (`~/.fawkes` by default; `<name>` is the project folder's name reduced to letters, digits, `_` and `-`), with the
   project, the clone the host runs from, its home, the port it serves on now (the one recorded in `<home>/ultimate.lock`,
   which is the port you open) and the first free block of ten spare ports from 4790 up. A manifest that already sets
   the host up (the same `project`, or the same `home`) is shown as it is and never overwritten; another app's manifest of
   the same file name gives this one the next free name (`<name>-2`).
3. **Update turns off.** The card also turns the built-in `update` extension off for this project, through Ultimate's
   onboarding actions; if the project's Ultimate setup is still undecided, clicking Set up trusts it. A project that
   declined its setup keeps `update` on, and the card says so.
4. **In a terminal:** stop this Ultimate, then run `fawkes up` in the project's folder (`cargo install --path
   <this repository> --locked` first when no `fawkes` is on `PATH`). From then on the item reads `fawkes · ready`, and
   its card lists these steps, each with a copy button, until the host runs under Fawkes.
   `fawkes down`, from a shell outside the app, stops the host and its daemon.

The manifest has `adopt = true`: the first start adopts the version that was running. The daemon stages the clone's
`HEAD`, tests it on a spare port, then makes it permanent (an `adopted` event) and runs it. If that test fails, nothing
changes: no generation goes live, and `ultimate` can be started as before. Setup is the user's action (`setup`); an
agent may read the preview (`setupPreview`).

## Where it runs

A user extension, linked as above. Without a Fawkes daemon around it (`FAWKES_SOCKET`, `FAWKES_GENERATION` and
`FAWKES_PHASE` unset) it has no tool and no hold, and its status-line item is the setup item.

Under a daemon, Ultimate runs from a generation (a git worktree of the clone at one commit), not from the clone
itself, so a landing or a merge in the clone no longer goes live before it is installed and tested. Turn the built-in
`update` extension off in that host (`"extensions": { "disable": ["update"] }`): it would move a clone the host does
not run from.

## The manifest

`start.ts` is the manifest's `start` and `health.ts` its `health.command` (see `examples/ultimate.toml` in the Fawkes
repository; `stop_timeout = 30` leaves a test run time to clean up after Ultimate shut down):

```toml
start = "node /path/to/fawkes/ultimate/fawkes/start.ts /path/to/project"

[health]
command = "node /path/to/fawkes/ultimate/fawkes/health.ts"
```

`start.ts` names the home it prepared in `FAWKES_ULTIMATE_HOME`. The extension acts only in the Ultimate running with
that home: a preview the live host starts, the lander's test servers or an agent's shell inherit Fawkes's variables
but run with a home of their own, so they never confirm, upgrade or roll back the live app (`role.ts`).

- **Live** (`FAWKES_PHASE=live`): it runs `bin/ultimate.ts <project> --no-open` from the generation, Ultimate's own
  supervisor, unchanged.
- **Test** (`FAWKES_PHASE=test`): the test run gets a copy of the storage (the `test_copy` hook, below), but `~`, `~/.ultimate` and the
  project are the real ones. So the run is set up like an Ultimate preview, with the helpers of the generation under
  test (`extensions/instance/server.ts`): a private user home inside the copy (`~/.ultimate` copied, the rest linked
  back), a git guard that never pushes and moves only the preview's own branches, and `ULTIMATE_INSTANCE_OWNER`,
  which turns off what previews skip. The preview's branch and checkouts are removed after the run.

## The hold and the confirm

The server facet holds Pi Durable's scheduler in `onOpen`, before the server resumes it:

- **In a test run** for good: the copied storage's interrupted runs and queued work never start, while the page
  answers Fawkes's health check.
- **In a new live generation** until it is confirmed: a rollback then forgets nothing that ran on the new code.

The pending generation confirms itself (`confirm <its generation>` over the socket) once every enabled extension's
server facet loaded (a facet broken by a changed core or a missing package is what must not become permanent) and its
page answers. It lets go of the hold only when the daemon's status says the generation is permanent, whoever
confirmed it (this facet, the Confirm button, `fawkes confirm`), and not while the user paused the host. A refused or
lost confirm is retried. If the checks never pass, the deadline runs out and Fawkes rolls back code and storage
together. A restart of a generation that is already permanent releases the hold at once; every reopen of the harness
(a failed core swap) holds again until then.

In a test run the hold fails closed: a hold that throws ends the process, and `health.ts` passes only once the
facet wrote `fawkes-held` into the test home, so a generation that breaks this extension or the hold never goes live.

The pause extension holds the same scheduler with one flag, and its Play (pressed, or its auto-play after a rate limit,
which a copied storage carries into a test run) would end this hold too. So while this extension holds, it sets
every other `release()` of the harness aside; its own release then lets the scheduler run unless the user's pause
still holds it. A counted hold in Ultimate would make that wrapper unnecessary.

## The upgrade

An upgrade fetches the ref in the clone, then runs `"$FAWKES_BIN" upgrade --json --socket "$FAWKES_SOCKET" -- <ref>` as a
child (`upgrade.ts`): one call, so the composition of stage, test and switch, its retry-safe stage and its refusals stay
Fawkes's own. Its stdout is the result (`generation`, `previous`, `deadline`, `existing`, `test`) or the error object
(`code`, `step`, `generation`, `log`), which the popover and the tool show as they are. Its progress lines are not
read from the child: the daemon sends every event to every subscriber, and the step in progress is `status.busy`.

In the live app the switch stops this process, the `upgrade` child with it, so nobody here could run `fawkes wait`. The
generation that runs next learns the outcome instead, from the same `status` and `history` pictures `wait` decides
from, on the subscribe stream it already holds (`facts.ts`, `outcome`): `permanent`, `rolled_back` with its
`rollback_cause`, `storage_failed` while the restore failed, or still waiting. The new generation confirms itself; if
it is rolled back, the permanent generation starts again and shows why ("g8 rolled back: missed its confirm deadline").

## Status line

One `subscribe` connection to the daemon (reconnecting with `since`, so no event is lost) keeps the generations,
the status and the latest events. Everything shown is read from fields (`status.step`, `status.actions`,
`rollback_cause`, the facts of each event), never from the text of a message. The item shows:

- `✓ g7` (muted) while the running generation is current;
- `↓ 3 new · upgrade` when the upgrade ref (`origin/main`) has commits the generation lacks (checked every 5
  minutes from the clone, fetched by Check now or an upgrade);
- the step in progress during an upgrade, or any other client's step (`stage`, `test g8`, `switch g8`, from
  `status.busy`);
- `⏱ confirming 01:42` while the new generation awaits its confirm, with the reason in the popover when its checks
  fail;
- `rolling back g8` while a rollback runs, and `restore failed` in red when its storage restore failed;
- `g8 rolled back` or `upgrade failed`, in red, for an hour after it happened;
- `fawkes too old` when the daemon's status has no `version`: it predates the fields this extension reads. The
  generation still holds the scheduler and confirms itself, but upgrades are off until Fawkes is upgraded.

The popover says how the latest switch ended (its cause in words, where it went back to and what became of the
storage), lists the newest generations with the cause of each rollback and the latest events, and offers only what
`status.actions` allows: Confirm (`confirm gN`), Roll back (`rollback`, which also retries a failed restore), and
Upgrade when the ref has new commits, `stage` is allowed and no switch is in flight. Check now is there unless a
switch awaits its confirm. An older daemon sends no `actions`: its pending switch still offers Confirm and Roll back.

## Actions

- `state` (no arguments): what the status line shows.
- `check` (no arguments): fetch the clone and count what an upgrade would bring.
- `upgrade` `{ ref? }` (user-only): `fawkes upgrade` `ref` (default `origin/main`) in the background.
- `confirm` (user-only): confirm the pending generation now, without its own checks.
- `setupPreview` (no arguments): the manifest first-time setup would write, or the one that already sets this host up.
- `setup` (user-only): write the previewed manifest and return the terminal steps that remain (`setupPreview` returns
  them too once a manifest sets the host up).
- `rollback` (user-only): roll the pending generation back now.

## For agents

A live generation adds a prompt section (the generation's folder is the running host and is never edited; landed
changes go live only through an upgrade; `restart` reboots the same generation), which never changes between turns
so the prompt stays cached, and the `fawkes` tool: `status` (the generations, how the latest switch ended, what
`status.actions` allows, recent events), or `upgrade {ref?}`. What needs an agent's attention (a pending confirm, a
rollback of the last hour with its cause, a daemon that is too old) comes as a note with its next input, only when it
changed since that conversation last heard, and once more when it no longer applies.

## Checks

`ultimate/check` runs `storage.test.ts` (the hooks against real WAL databases), `test-env.test.ts` (the test run's
environment against the clone's own preview helpers, so a change in Ultimate that breaks the isolation fails it),
`upgrade.test.ts` (the `fawkes upgrade` call and its result and error fields), `facts.test.ts` (outcomes and words from fields)
and the rest, and type-checks both facets against the clone in `$ULTIMATE` (default `~/Code/ultimate`).

## Storage hooks

Fawkes keeps no storage code of its own: `storage.ts` snapshots, restores and copies Ultimate's two databases,
`state.sqlite` and `trace.sqlite` in the project's home, as three hooks of the manifest:

```toml
[hooks]
snapshot  = "node /path/to/fawkes/ultimate/fawkes/storage.ts snapshot {home} {snapshot}"
restore   = "node /path/to/fawkes/ultimate/fawkes/storage.ts restore {snapshot} {home}"
test_copy = "node /path/to/fawkes/ultimate/fawkes/storage.ts test-copy {home} {test_home}"
```

- **snapshot** (after the live app stopped) copies each database present into the snapshot directory and writes
  `index.json` last, so a snapshot counts once it is whole.
- **restore** (after the failed generation stopped, before the old one starts) checks the whole snapshot first: no
  index, a corrupt one, a name that is not one of the two files or a missing copy fails with nothing touched. Then
  each database is copied beside the live file and renamed over it, after its `-wal`, `-shm` and `-journal` were
  removed; a file that did not exist at the snapshot is deleted with them. Every file is as it was or wholly
  restored, and a restore that failed is safe to run again.
- **test-copy** (while the app runs and writes) copies each database present into the test home, rows that are still
  only in the `-wal` included.

A database is copied with `VACUUM INTO` (one read transaction, waiting up to 10 s for a lock), never with a file copy
of a live database, which loses the `-wal`; only a restore copies the snapshot's finished file as it is. Each copy keeps the mode of its source, a read-only file included. Exit 0 is success, 1 a failed
step with the file and the reason on stderr, 2 a usage error. It needs `node:sqlite`, and the hooks are TypeScript run by the `node` Ultimate runs on, which needs Node 26
(Ultimate's `engines`); Node 22.13 would need `--experimental-strip-types`.
