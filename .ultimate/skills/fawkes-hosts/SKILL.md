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
   host (its repo is another clone), then the order. It changes nothing.
3. **The impact check.** Run the `ultimate-impact` skill over `<oldest commit the plan shows>..origin/main`. A break
   it finds would fail the hosts' tests (nothing goes live, but nothing upgrades either): fix it first, or tell the
   user.

## The rollout

```sh
sh ~/Code/fawkes/.ultimate/skills/fawkes-hosts/rollout.sh origin/main
```

The command takes a few seconds: it runs the same pre-checks as `--plan`, starts the rollout in the background and
prints its status file. A host that no daemon answers, or with a switch pending or a step running, refuses the whole
rollout before anything moves; its line says what to run (`fawkes wait --manifest <name>`), or leave it out with
`--hosts`.

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
polling. When your own host's turn comes, its switch restarts this Ultimate and this conversation with it. The
rollout runs in a session of its own and goes on; after the restart, read the same status file again (its path is in
the output above).

## The report

Give the user the host lines and the `next:` line as the status file has them. On `failed`, the file also names the
host, the failed step, the error's code and message, the rollback cause and the last lines of the generation's log
(or of the install output when the stage failed). The hosts before it stay on the new commit and the hosts after it
were not touched. Don't retry blindly: find the cause (`fawkes history --manifest <name>` has the whole log tail),
fix it, then run the same command again: hosts already on the commit are left alone, so it goes on from the one that
failed.

Once reported, remove the rollout's folder (the `files in` path) by its exact path.

## Why this shape

One `fawkes upgrade` can outlast a bash call's timeout, which ends the client, and the switch of your own host stops
your own process group. So the script detaches itself (`setsid`, else perl's), polls each host's daemon every 3 s,
and writes everything to a file you read in short checks, which also leaves steering free while it runs.

Never try it against the live hosts or `~/.fawkes` to test a change to the script: use a throwaway `FAWKES_HOME` in
`/tmp` with sample apps from `examples/sample` and a debug `fawkes` first on `PATH`.
