---
name: fawkes-hosts
description: Upgrade every Ultimate host this Mac runs under Fawkes to one Ultimate ref, one host at a time (the canary first, your own host last, stopping at the first failure), with rollout.sh, and report each host's before → after. Use when the user asks to upgrade all the hosts, roll a ref or origin/main across them, or bring the hosts up to date.
---

# Rolling Ultimate across the hosts

`rollout.sh` sits next to this file, in the host project at `~/Code/fawkes/.ultimate/skills/fawkes-hosts/rollout.sh`.
It resolves the ref once in the Ultimate clone (`~/Code/ultimate`) and upgrades each host to that commit with
`fawkes upgrade`, then `fawkes wait`, one host at a time.

## Before the rollout

1. **Fetch, never merge.** `git -C ~/Code/ultimate fetch` updates `origin/main`. Never `git merge`, `pull`, check out
   or reset in that clone: every host's user and project extensions load from its working tree, so moving it changes
   every host at once, outside any transaction.
2. **The plan.** `sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rollout.sh --plan origin/main` prints, for every app
   in `$FAWKES_HOME`, its generation, its commit and whether it is a host to upgrade, already there, refused or not a
   host (its repo is another clone), then the order, and the precheck it would run. It changes nothing.
3. **The impact check.** Run the `ultimate-impact` skill over `<oldest commit the plan shows>..origin/main`. A break
   it finds would fail the hosts' tests (nothing goes live, but nothing upgrades either): fix it first, or tell the
   user.

## The rollout

```sh
sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rollout.sh origin/main
```

The command takes about half a minute: it runs the same pre-checks as `--plan`, then the precheck, starts the
rollout in the background and prints its status file. A host that no daemon answers, or with a switch pending or a
step running, refuses the whole rollout before anything moves; its line says what to run (`fawkes wait --manifest
<name>`), or leave it out with `--hosts`.

**The precheck** runs before any host moves: the clone lends a throwaway worktree of the commit (`git worktree add`;
nothing else in the clone changes), the command runs in it with `$ULTIMATE` set to that worktree and `$REF` to the
ref, and the worktree is removed whatever the outcome. The default, `~/Code/ultimate-extensions/fleet/precheck`,
type-checks every user extension and Xana's phone extension against the commit, so a ref that would break them on
every host stops here. Its outcomes, each before anything moves:

- `precheck passed in 0m15s`: the hosts' state is checked again (one that started a step meanwhile refuses the
  rollout as before), and the rollout goes on.
- `refused, nothing moved: the precheck failed`: its last lines (one `FAIL <extension>` line per broken extension)
  and the path of its whole log, which is kept. Tell the user which extension breaks on that ref, or fix it in its
  own repository; don't skip the precheck to get past it.
- `refused, nothing moved: ~/Code/ultimate-extensions has uncommitted changes`: every host loads that tree live, and
  the precheck would check the work in progress instead of the ref. An agent may be editing it: ask before committing
  or dropping anything there, then run the same command again.

`--precheck CMD` runs another command (through `sh -c`, in the worktree); `--no-precheck` skips both checks.

- `--canary NAME` goes first (default `xana`; `--canary ''` for none).
- `--self NAME` goes last. Without it, the host whose daemon listens on `$FAWKES_SOCKET`, the one this conversation
  runs on, is last.
- `--hosts a,b` upgrades only those apps; `--repo DIR` names another clone.

Then poll the status file, each check its own bash call with `interruptible: true`, never one long wait:

```sh
sleep 30; cat <status file>
```

Its first line is `state: running`, `done` or `failed`, then one line per host (before → after, the outcome or the
step it is in, how long it took), and its last line the next command. A host's test can take 15 minutes; keep
polling.

## Your own host

The switch of your own host (the `--self` one, `fawkes` on 4747 for this project) restarts the Ultimate this
conversation runs on, and interrupts every tool call in flight there: yours and every other busy conversation's.
So roll it separately, from another host:

1. Leave it out of the rollout: `--hosts` with every other host's name, then poll that rollout to `done`.
2. List the conversations it would interrupt, and give them to the user with the hand-over:

   ```js
   (await ultimate.conversations()).filter((c) => c.busy).map((c) => `#${c.id} ${c.title}`)
   ```

3. Hand the move to Ultimate's @ultimate (the `ultimate` host's, which does not restart with yours): ask your
   @ultimate (`message`, `reply: true`) to relay it, with the commit the plan resolved, the busy list, and the
   command, `sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rollout.sh --hosts <your host> <commit>`, to run once
   those conversations are idle and to report the status file's lines back.

If your host's switch does run under you, the rollout runs in a session of its own and goes on; after the restart,
read the same status file again (its path is in the output above).

## The report

Give the user the host lines and the `next:` line as the status file has them. On `failed`, the file also names the
host, the failed step, the error's code and message, the rollback cause and the last lines of the generation's log
(or of the install output when the stage failed). The hosts before it stay on the new commit and the hosts after it
were not touched. Don't retry blindly: find the cause (`fawkes history --manifest <name>` has the whole log tail),
fix it, then run the same command again: hosts already on the commit are left alone, so it goes on from the one that
failed.

Once reported, remove the rollout's folder (the `files in` path) by its exact path.

## Rehearsing a new fawkes

`rehearse.sh`, next to `rollout.sh`, tries `fawkes reexec --all` (the move of the supervisor and every daemon onto a
newly installed fawkes, apps still running) in a world of its own: a `FAWKES_HOME` under `/tmp/fkr.XXXXXX` with N
sample apps from `examples/sample` and a supervisor (`fawkes up --all` in the background, never launchd: the label
`dev.fawkes` is the live agent's). It drops every inherited `FAWKES_*` variable and never touches `~/.fawkes` or the
live supervisor. Rehearse before the live `reexec --all` of a new release, starting from the binary the fleet runs:

```sh
sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rehearse.sh up 2 --from ~/.local/bin/fawkes   # before the install
# install the new fawkes (fawkes-release step 3), then:
sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rehearse.sh reexec --to ~/.local/bin/fawkes
sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rehearse.sh down
```

`up` copies its binary into the rehearsal, so the install does not change it, and returns once every app runs.
`reexec` installs `--to` at the rehearsal's binary path, runs `reexec --all`, and prints one line per process,
version before → after and `pid … kept` or `CHANGED`; it exits 1 when a pid changed or a version is not the new one.
Both binaries default to this repository's `target/debug/fawkes`, else `fawkes` on `PATH`; a `--to` that is the
very file the rehearsal runs already gets a note, since it proves only the handover. `down` stops the supervisor it recorded (which
stops its daemons and apps), kills by pid any of its own that survived, and removes its exact directory. One
rehearsal at a time: `up` is refused while one is recorded in `/tmp/fawkes-rehearse`.

## Why this shape

One `fawkes upgrade` can outlast a bash call's timeout, which ends the client, and the switch of your own host stops
your own process group. So the script detaches itself (`setsid`, else perl's), polls each host's daemon every 3 s,
and writes everything to a file you read in short checks, which also leaves steering free while it runs.

Never try it against the live hosts or `~/.fawkes` to test a change to the script: use a throwaway `FAWKES_HOME` in
`/tmp` with sample apps from `examples/sample` and a debug `fawkes` first on `PATH`.
