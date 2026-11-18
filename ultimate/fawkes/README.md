Runs Ultimate's upgrades through the Fawkes supervisor: `fawkes upgrade` stages, tests and switches a ref, and the new generation confirms itself once it works, or Fawkes rolls back code and storage.

## Setup

One path for every project, including one that never ran Ultimate. The steps, as `setupSteps` in `init.ts` lists them
for the card and for `init.ts`'s own output:

1. Set up the project, once: `node <this folder>/init.ts [--ultimate <clone>] [--port <n>] <project>`.
2. Put `fawkes` on `PATH`, when it is not: `cargo install --path <this repository> --locked` from a Fawkes checkout,
   else `install.sh`, which puts it in `~/.local/bin`.
3. Stop the Ultimate running on the project, if any.
4. Start the Fawkes supervisor, once per Mac, unless one already runs: `fawkes service install` loads the launchd agent
   `dev.fawkes`, which runs `fawkes up --all`, the daemon of every manifest in the Fawkes home (`fawkes up --all` under
   the init system elsewhere). A supervisor that runs starts a new manifest within seconds, so with one there is
   nothing to start: the project's daemon adopts the Ultimate once its port is free, retrying at most 30 s apart until
   then. `fawkes service install` is offered only when no supervisor answers, since installing again over a loaded
   agent restarts every app.

`init.ts` first looks at four pieces and writes nothing; if one is refused it exits 1 with the reasons and still writes
nothing. Otherwise it writes the missing ones, prints `wrote …` or `already …` for each, and the terminal steps left.
Run again, it changes nothing. Exit codes: 0 done, 1 failed or refused, 2 usage.

1. **The manifest** `$FAWKES_HOME/<name>.toml` (`~/.fawkes` by default; `<name>` is the project folder's name reduced to
   letters, digits, `_` and `-`), with the project, the clone, its home (Ultimate's own `projectHome`), the port
   and the first free block of ten spare ports from 4790 up. A manifest that already sets the project up (the same
   `project`, or the same `home`) is left exactly as it is; another app's manifest of the same file name gives this
   one the next free name (`<name>-2`). The port is the one `<home>/ultimate.lock` records while its Ultimate lives (the
   port you open); otherwise the lowest from 4747 that no manifest claims (its `port` and `spare_ports`), no
   `~/.ultimate/projects/*/ultimate.lock` has recorded and nothing listens on. `--port` overrides it.
2. **The link** `<project>/.ultimate/extensions/fawkes` to this folder, so the project's Ultimate loads the extension.
   A link elsewhere, or a real folder, is refused and never replaced.
3. **`update` off** in `<project>/.ultimate/settings.json` (added to `extensions.disable` and removed from
   `extensions.enable`; every other key stays): the built-in `update` would move a clone Fawkes runs generations from.
4. **Trust** of the project's Ultimate setup, which Ultimate needs to load the link and the settings. It trusts the whole
   `<project>/.ultimate/` (agents, skills, settings). A project that declined its setup (`~/.ultimate/trust.json`) is
   refused: init never overrides that decision. So is a `trust.json` that cannot be read.

Ultimate decides where a project's home is, whose trust a worktree shares and how settings are saved, so `init.ts`
loads those helpers from the clone at run time. The clone is `--ultimate`, else `$ULTIMATE`, else the clone behind
`ultimate` on `PATH` (a link to `<clone>/bin/ultimate.ts`, or a pnpm shim naming it), else `~/Code/ultimate`: the first
that names something is used, and it must have `bin/ultimate.ts` and `node_modules`. `FAWKES_HOME` (default
`$HOME/.fawkes`, where `fawkes` looks) and `ULTIMATE_USER_HOME` (default: the home directory) move the Fawkes home and
the user's `.ultimate/`.

A host that does not run under Fawkes shows a muted `fawkes · set up` item in its status line (not a preview or a test
server). Its card writes nothing: it shows the same steps with a copy button each (the `init.ts` command, with
`--ultimate` only where a terminal would not find this host's clone itself), and the manifest that already sets the
host up. Once all four pieces are there the
item reads `fawkes · ready`. `fawkes down`, from a shell outside the app, stops the host and its daemon.

The manifest has `adopt = true`: the first start adopts the version that was running. The daemon stages the clone's
`HEAD`, tests it on a spare port, then makes it permanent (an `adopted` event) and runs it. If that test fails, nothing
changes: no generation goes live, and `ultimate` can be started as before. An agent may read the steps
(`setupPreview`); nothing in the extension writes the setup but `init.ts`.

## Where it runs

A user extension, linked into the project by `init.ts` (or by hand into `~/.ultimate/extensions/fawkes`, which every host
loads). Without a Fawkes daemon around it (`FAWKES_SOCKET`, `FAWKES_GENERATION` and
`FAWKES_PHASE` unset) it has no tool and no hold, and its status-line item is the setup item.

Under a daemon, Ultimate runs from a generation (a git worktree of the clone at one commit), not from the clone
itself, so a landing or a merge in the clone no longer goes live before it is installed and tested. Turn the built-in
`update` extension off in that host (`"extensions": { "disable": ["update"] }`, which `init.ts` writes): it would move a
clone the host does not run from.

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

In both phases it first links the generation's `node_modules/ultimate` to the generation itself (`own-code.ts`).
Ultimate loads extensions through Vite, which resolves every package in the generation's `node_modules` from there
(`resolve.dedupe` in `server/vite-extensions.ts`), on the server and in the web bundle. So a user or project extension's
`ultimate/...` imports load the code of the generation that runs it (live, under test, or back after a rollback), not
the clone its own folder's `node_modules` links to, which then serves only editors and type checks. A generation
whose `node_modules/ultimate` is a folder rather than a link does not start, and fails its test.

## The report

The live host tells the daemon where it can be reached (`report.ts`): `url` (the address it serves), `remote` (its
Tailscale HTTPS URLs, joined by `, `, left out while there are none) and `log` (`supervisor.log` in the host's home).
`fawkes up` and `fawkes status` print them, since the host's own dashboard goes to that log file under Fawkes. The
report is sent when the subscribe stream connects or reconnects and whenever the info changes (the host is asked every
5 s; it has no change notification for extensions). Each report replaces the previous one. A refused or failed report
is logged once and retried on the next tick; only the live generation reports, never a test run, a preview or a host
without a daemon.

## The hold and the confirm

The server facet holds Pi Durable's scheduler in `onOpen`, before the server resumes it:

- **In a test run** for good: the copied storage's interrupted runs and queued work never start, while the page
  answers Fawkes's health check.
- **In a new live generation** until it is confirmed: a rollback then forgets nothing that ran on the new code.

The pending generation confirms itself (`confirm <its generation>` over the socket) once every enabled built-in
extension's server facet loaded (a facet broken by a changed core or a missing package is what must not become
permanent) and its page answers. A user or project extension does not count: it imports the generation's
`ultimate/...` (below), but its own files are shared by every generation and edited live, so a rollback cannot mend a
fault of its own; its failure is a notice, once per set of failed names. While the generation
waits, each new reason is a notice (the status line, and the generation's `app.log`, whose tail `fawkes history` keeps
after a rollback) and the `confirm` key of its report (`waiting: server facets failed to load: update`, or
`refused: …` when the daemon refused the confirm), which the daemon logs as a `reported` event. It lets go of the
hold only when the daemon's status says the generation is permanent, whoever confirmed it (this facet, the Confirm
button, `fawkes confirm`), and not while the user paused the host. A refused or lost confirm is retried. If the
checks never pass, the deadline runs out and Fawkes rolls back code and storage together. A restart of a generation that is already permanent releases the hold at once; every reopen of the harness
(a failed core swap) holds again until then.

In a test run the hold fails closed: a hold that throws ends the process, and `health.ts` passes only once the
facet wrote `fawkes-held` into the test home, so a generation that breaks this extension or the hold never goes live.
A test run whose built-in facets failed to load ends too, before it holds, with their names in `app.log`: live, it
could never confirm itself, and its switch would cost a missed deadline and a storage restore.

The pause extension holds the same scheduler with one flag, and its Play (pressed, or its auto-play after a rate limit,
which a copied storage carries into a test run) would end this hold too. So while this extension holds, it sets
every other `release()` of the harness aside; its own release then lets the scheduler run unless the user's pause
still holds it. A counted hold in Ultimate would make that wrapper unnecessary.

## The upgrade

An upgrade fetches the ref in the clone, then runs `"$FAWKES_BIN" upgrade --json --socket "$FAWKES_SOCKET" -- <ref>`
as a child (`upgrade.ts`): one call, so the composition of stage, test and switch, its retry-safe stage and its
refusals stay Fawkes's own. Its stdout is the result (`generation`, `previous`, `deadline`, `existing`, `test`) or the error object
(`code`, `step`, `generation`, `log`), which the popover and the tool show as they are. Its progress lines are not
read from the child: the daemon sends every event to every subscriber, and the step in progress is `status.busy`.

In the live app the switch stops this process, the `upgrade` child with it, so nobody here could run `fawkes wait`.
The generation that runs next learns the outcome instead, from the same `status` and `history` pictures `wait` decides
from, on the subscribe stream it already holds (`facts.ts`, `outcome`): `permanent`, `rolled_back` with its
`rollback_cause`, `storage_failed` while the restore failed, or still waiting. The new generation confirms itself; if
it is rolled back, the permanent generation starts again and shows why ("g8 rolled back: missed its confirm deadline").

## Status line

One `subscribe` connection to the daemon (reconnecting with `since`, so no event is lost) keeps the generations,
the status and the latest events. Everything shown is read from fields (`status.step`, `status.actions`,
`rollback_cause`, the facts of each event), never from the text of a message. The item shows:

- `✓ g7` (muted) while the running generation is current;
- `↓ 3 new · upgrade` when the upgrade ref (`origin/main`) has commits the generation lacks (Fetching, below);
- the step in progress during an upgrade, or any other client's step (`stage`, `test g8`, `switch g8`, from
  `status.busy`);
- `⏱ confirming 01:42` while the new generation awaits its confirm, with the reason in the popover when its checks
  fail;
- `rolling back g8` while a rollback runs, and `restore failed` in red when its storage restore failed;
- `g8 rolled back` or `upgrade failed`, in red, for an hour after it happened;
- `disk low · 9.2 GB free` in the warning colour while `status.disk.low` (the daemon's decision for the volume of the
  state directory), unless something above shows: those keep priority. An older daemon, or a failed measurement,
  sends no `disk` and shows nothing. The popover adds a row, "Disk low: 9.2 GB free of 245.0 GB. Upgrades are
  refused below 10.0 GB (the manifest's min_free); free disk space.", which also explains the missing
  Upgrade button (`stage` leaves `status.actions` while the disk is under `min_free`);
- `fawkes too old` when the daemon's status has no `version`: it predates the fields this extension reads. The
  generation still holds the scheduler and confirms itself, but upgrades are off until Fawkes is upgraded;
- `✓ g7 · fawkes 0.2.0` (muted) when a newer Fawkes is out or installed (Releases, below).

The popover says how the latest switch ended (its cause in words, where it went back to and what became of the
storage), lists the newest generations with the cause of each rollback and the latest events, and offers only what
`status.actions` allows: Confirm (`confirm gN`), Roll back (`rollback`, which also retries a failed restore), and
Upgrade when the ref has new commits, `stage` is allowed and no switch is in flight. Check now is there unless a
switch awaits its confirm. An older daemon sends no `actions`: its pending switch still offers Confirm and Roll back.

## Fetching

The live generation compares its commit with the upgrade ref in the clone 30 s after it starts and then every minute.
That comparison is local. It fetches the ref's remote first when the clone's last fetch is older than
`fawkes.fetchEvery` in `~/.ultimate/settings.json` (`fetch.ts`):

```json
{ "fawkes": { "fetchEvery": "15m" } }
```

The value is minutes or hours (`"5m"`, `"1h"`) or a number of minutes; without it the clone is fetched every 15
minutes. `"off"` or `0` turns the scheduled fetch off, and anything shorter than a minute counts as a minute. The file is
read at every tick, so a change applies within a minute, without a restart. A value it cannot read fails the check,
which the popover says.

The last fetch is the mtime of `FETCH_HEAD` in the clone's shared git directory (`--git-common-dir`), where every
fetch here writes it: a generation is a linked worktree, whose own git directory would keep one per generation. So
the hosts on one clone fetch about once per interval between them. A failed fetch (offline) is tried again an interval
later; it never changes the item, and the popover shows it as "Last check failed". The popover's footer says when the
clone was last fetched (when the comparison ran, before the first fetch). Check now and an upgrade always fetch.

## Releases

The live generation checks for a newer Fawkes 30 s after it starts and then every hour (`release.ts`):

- `$FAWKES_BIN --version`: the binary the daemon was started from, as it is installed now. `install.sh` renames the
  new binary into that path, so it says what `fawkes reexec` would run.
- Once a day, the tag GitHub's `releases/latest` redirect names (a `HEAD` request, not the API, which limits clients
  without a token to 60 requests an hour). No release yet redirects to the releases page instead. A check that fails
  (offline, a timeout of 10 s) keeps the last answer and runs again at the next tick.

When that release is newer than the installed binary and the daemon, the popover shows it with two commands to copy:
the upgrade (the `curl` line of `install.sh`, or `git pull` and `cargo install` in the checkout when this folder is
one), then `fawkes reexec --all` with the daemon's own binary (which may not be the first `fawkes` on `PATH`): it moves
the supervisor and every daemon onto the new binary while every app, this Ultimate included, keeps running, so this
Ultimate may run it itself. A daemon with a switch pending refuses until the switch is decided. When only the
installed binary is newer than the daemon, it shows the re-exec alone. The `fawkes` tool's `status` says the same. Versions
compare as numbers, a prerelease before its release. Check now asks GitHub at once, after a check already under way.

`FAWKES_NO_UPDATE_CHECK` (set and not empty), in the daemon's environment or the manifest's `[env]`, turns the check
off. A test run never checks.

## Actions

- `state` (no arguments): what the status line shows.
- `check` (no arguments): fetch the clone and count what an upgrade would bring, and ask GitHub for the newest
  release.
- `upgrade` `{ ref? }` (user-only): `fawkes upgrade` `ref` (default `origin/main`) in the background.
- `confirm` (user-only): confirm the pending generation now, without its own checks.
- `setupPreview` (no arguments): the terminal steps that set this host up (the `init.ts` command while a piece is
  missing), the reasons `init.ts` would refuse, and the manifest that already sets the host up.
- `rollback` (user-only): roll the pending generation back now.
- `projectProgress` (no arguments, read-only): the step of this app's transaction for Ultimate's `status` extension,
  out of `stage → test → switch → confirm`: `switch` (2 of 4) while the switch runs, then `confirm`, `rollback` or
  `restore failed` (3 of 4); `null` outside a switch and outside the live generation. It reads the typed `step` of the
  daemon's status alone, so a stage or test (which only `busy` names) shows none.

## For agents

A live generation adds a prompt section (the generation's folder is the running host and is never edited; landed
changes go live only through an upgrade; `restart` reboots the same generation), which never changes between turns
so the prompt stays cached, and the `fawkes` tool: `status` (the generations, how the latest switch ended, what
`status.actions` allows, recent events), `fleet` (`fawkes status --all --json`, without the host's `FAWKES_SOCKET` and
`FAWKES_MANIFEST`: the supervisor and every app in the Fawkes home, one line each, this host's marked), or
`upgrade {ref?}`. What needs an agent's attention (a pending confirm, a
rollback of the last hour with its cause, a daemon that is too old, and a low disk) comes as a note with its next
input, only when it changed since that conversation last heard, and once more when it no longer applies. A low disk
is a sentence of its own after the others ("the disk is low: 9.2 GB free of 245.0 GB; upgrades are refused below
10.0 GB (min_free)."), so it hides neither a pending confirm nor a rollback, and they do not hide it.

## After upgrades and autofix

After an upgrade a task goes to one @ultimate: on success a review of what the new Ultimate commits offer every
extension, on failure an investigation right away. Extensions listed under `autofix` are repaired hands-off when they
fail to load. Both are set in `~/.ultimate/settings.json`, read at every tick like `fetchEvery`:

```json
{ "fawkes": {
    "afterUpgrade": { "project": "fawkes", "success": true, "failure": true },
    "autofix": ["phone"] } }
```

- `afterUpgrade`: `false` turns both tasks off. `project` is the project whose @ultimate receives them (this host's own
  without it). `success` and `failure` are `true` or missing for the default instructions, `false` to turn that task
  off, or a string that replaces the default instructions. The facts of the upgrade are always appended.
- `autofix`: the names of the extensions to keep loading; missing or `[]` turns it off.

A value it cannot read is a notice, once per message. Only the live generation runs them, once it is permanent: at
open, on the minute tick, and at once when a facet fails to load. Every host on the machine shares
`~/.ultimate/fawkes/` (`store.ts`), so each task happens once whichever host sees it first. `installed` is made by the
first host to run them: an upgrade or failure from before it is the state the extension found, and brings nothing.

- **Success:** `reviewed` holds the reviewed Ultimate commits, one tip a line, newest first, written under
  `review.lock`. A generation confirmed after `installed` whose commit has commits in none of them sends a review of
  those (`review.md`, the procedure, beside this file, which also runs the receiving project's `ultimate-impact` skill
  when it has one), with every extension the host loads beyond the built-in ones and the project owning its folder,
  then records its commit as a tip and drops the tips it contains. A commit they already hold sends nothing, so a
  restart of the same generation or a host on an older commit is quiet. The first run only writes `reviewed`. A set of
  tips rather than one commit: a reviewed branch the lander rebases never becomes an ancestor of main, and with one mark
  every later review would repeat the main line since.
- **Failure:** a failed test, a failed stage, or the latest switch rolled back for a cause other than `requested` or
  `daemon_restart`, of the last 24 hours and after `installed`. The claim `failure-<commit>` makes it one investigation per failed commit
  (`failure-<host>-<generation>` for a stage, whose generation is removed). Its facts: what happened, the cause and
  reason, the storage outcome, the generation's last `confirm` report, the last 20 lines of its `app.log` (or the failed
  install's log) and the `fawkes history` command for the rest.
- **Autofix:** `autofix-<name>.json` keeps the error, the tries, when the last one was and the host it was for, under
  `autofix-<name>.lock`, written before each delivery and put back when it fails. Records go by name: two projects'
  extensions of one name share one. A try goes to the @ultimate of the project whose root is the git repository of
  the extension's real folder, or to the coordinating project with a request to route it when no project owns it. It
  is a hands-off goal with guardrails: edit only that folder and nothing else of the clone, a generation or
  `~/.ultimate`, land through the project's lander (commit in its repository when no project has it, or fix it in
  place after a copy when it is in none) and check the facet loads. A second try follows 30 minutes later, with the first one's error; 30 minutes
  after that it gives up once: a notice, and a message asking the coordinating project's @ultimate to tell the user.
  Once the extension loads again on that host, the record is removed with a notice; removing it by hand starts the
  tries over. A host that does not load the extension leaves it to the hosts that do.

- **Disk low:** one message per low-disk episode, for all hosts on the machine, whatever `afterUpgrade` says (it is no
  upgrade task, and has no setting). While `status.disk.low`, the first host to create `disk-low` (an exclusive
  create naming its manifest, written before the delivery since a full disk may take no write after it) tells the coordinator's @ultimate
  to find what takes the space and report it, deleting nothing without the user's go; a delivery that fails removes
  the file again, so the next tick retries. Once the claimer's own daemon says the disk is not low, it removes
  `disk-low`, which ends the episode: the next time the disk runs low, it is told again. Only the claimer ends it, since
  daemons can disagree (`min_free = 0`, another volume, a daemon restarted inside the recovery margin). Without `disk`
  nothing changes; a claimer that is gone for good leaves the file, which is removed by hand.

Success, failure, the give-up and the low disk go to `afterUpgrade.project`, else to the host's own @ultimate. A delivery that fails
(the project's host is down) changes nothing, and the next tick tries again. Each delivery is a notice: "Fawkes: asked
fawkes's @ultimate to investigate g8's rollback".

## Checks

`ultimate/check` runs `storage.test.ts` (the hooks against real WAL databases), `test-env.test.ts` (the test run's
environment against the clone's own preview helpers, so a change in Ultimate that breaks the isolation fails it),
`upgrade.test.ts` (the `fawkes upgrade` call and its result and error fields), `facts.test.ts` (outcomes and words
from fields), `release.test.ts` (versions, the redirect's tag and what the popover says), `fetch.test.ts` (the
`fetchEvery` setting and the shared `FETCH_HEAD` gate, on a clone with a local bare `origin`), `tasks.test.ts` (the
`afterUpgrade` and `autofix` settings, claims and locks, the install time, the reviewed tips on a real repository, the autofix
decision, the texts and who owns an extension's folder), `fleet.test.ts` (the `fleet` action's call, its environment
and its view), `step.test.ts` (`projectProgress` from `step` alone) and the rest, and
type-checks both facets against an Ultimate checkout: `$ULTIMATE`, else `ultimateCheck` in the repository's
`.ultimate/settings.json` (a path, or `live` for the generation the reachable Fawkes daemon runs, falling back to
`~/Code/ultimate` with a note), else `~/Code/ultimate`; `ultimate/check --which` prints it. The lander and
`scripts/check` run it when `ultimate/` differs from `main`.

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
