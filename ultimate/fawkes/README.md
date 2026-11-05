Runs Ultimate's upgrades through the Fawkes supervisor: stage, test and switch a ref, and the new generation confirms itself once it works, or Fawkes rolls back code and storage.

## Where it runs

A user extension: link this folder into `~/.ultimate/extensions/fawkes`. Without a Fawkes daemon around it
(`FAWKES_SOCKET`, `FAWKES_GENERATION` and `FAWKES_PHASE` unset) it does nothing: no status-line item, no tool, no hold.

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
- **Test** (`FAWKES_PHASE=test`): Fawkes gives the test run a copy of the storage, but `~`, `~/.ultimate` and the
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
lost confirm is retried. If the checks never pass, the deadline runs out and Fawkes rolls back code and storage together.
A restart of a generation that is already permanent releases the hold at once; every reopen of the harness (a failed
core swap) holds again until then.

In a test run the hold fails closed: a hold that throws ends the process, and `health.ts` passes only once the
facet wrote `fawkes-held` into the test home, so a generation that breaks this extension or the hold never goes live.

The pause extension holds the same scheduler with one flag, and its Play (pressed, or its auto-play after a rate limit,
which a copied storage carries into a test run) would end this hold too. So while this extension holds, it sets
every other `release()` of the harness aside; its own release then lets the scheduler run unless the user's pause
still holds it. A counted hold in Ultimate would make that wrapper unnecessary.

## Status line

One `subscribe` connection to the daemon (reconnecting with `since`, so no event is lost) keeps the generations,
the status and the latest events. The item shows:

- `✓ g7` (muted) while the running generation is current;
- `↓ 3 new · upgrade` when the upgrade ref (`origin/main`) has commits the generation lacks (checked every 5
  minutes from the clone, fetched by Check now or an upgrade);
- `installing g8`, `testing g8`, `switching` during an upgrade;
- `⏱ confirming 01:42` while the new generation awaits its confirm, with the reason in the popover when its checks
  fail;
- `rolling back g8` while a rollback runs, and `restore failed` in red when its storage restore failed (the popover
  offers Roll back, which retries it);
- `g8 rolled back` or `upgrade failed`, in red, for an hour after it happened.

The popover lists the newest generations with their state and the reason of a rollback, and offers Check now and
Upgrade, or Confirm and Roll back while a switch awaits its confirm (the daemon's `status.step` says so: `pending`
stays set while a switch is rolled back).

## Actions

- `state` (no arguments): what the status line shows.
- `check` (no arguments): fetch the clone and count what an upgrade would bring.
- `upgrade` `{ ref? }` (user-only): stage, test and switch `ref` (default `origin/main`) in the background.
- `confirm` (user-only): confirm the pending generation now, without its own checks.
- `rollback` (user-only): roll the pending generation back now.

## For agents

A live generation adds a prompt section (the generation's folder is the running host and is never edited; landed
changes go live only through an upgrade; `restart` reboots the same generation) and the `fawkes` tool: `status`, or
`upgrade {ref?}`.

## Checks

`ultimate/check` runs `test-env.test.ts` (the test run's environment against the clone's own preview helpers, so a
change in Ultimate that breaks the isolation fails it) and type-checks both facets against the clone in `$ULTIMATE`
(default `~/Code/ultimate`).
