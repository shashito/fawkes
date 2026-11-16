---
name: fawkes-release
description: Answer "released?", "pushed?" or "pushed to origin?" for Fawkes in one short status (version, tag, local commits, CI and Release runs, the installed binary and each daemon's version) and give the user the exact commands that finish the job. Use when the user asks whether Fawkes is pushed, released or installed, after a landing they may want to ship, or while a Release workflow runs.
---

# Fawkes release state

`release-state.sh` sits next to this file, in the host project at
`~/Code/fawkes/.ultimate/skills/fawkes-release/release-state.sh`. Run it from there, so it reports the checkout
`just release` runs in:

```sh
sh ~/Code/fawkes/.ultimate/skills/fawkes-release/release-state.sh
```

It takes about 5 s and reads only: a `git fetch` of origin, then git, `gh` and `fawkes status` for each manifest
in `~/.fawkes`. It prints:

- **Checkout:** the branch and whether it is clean (`just release` needs main, clean).
- **Version:** main's `Cargo.toml`, and whether its tag `v<version>` exists locally and on origin, and how many
  commits main is past it.
- **main:** the commits not on origin (`origin/main..main`), listed, and whether main is behind.
- **Published, CI main, Release run:** the newest GitHub release, the last CI run on main and the last Release run,
  each with its status, conclusion and URL.
- **Binary and Daemons:** `fawkes --version`, and each daemon's version with any pending switch or step in progress.
- **Next:** the commands left, in order, for the user.

## Who acts

The skill only reads. Pushing, tagging and releasing belong to the user: give them the **Next** commands and stop.
Run one yourself only when the user handed it over in this conversation ("you can release", "push it"), and only
what they handed over.

Once the user says to release in words ("release v0.1.6", "you can release"), that is the confirmation: run
`just release yes` in `~/Code/fawkes`, which skips only the recipe's question and keeps every refusal. Never tag or
push a tag by hand (`git tag`, `git push origin v…`), and never work around a refusal: report it with the **Next**
command that clears it. When the user names a version, check that it is main's `Cargo.toml` version first; if it is
not, stop and say so instead of releasing another one. `just release yes` ends by following the Release run with
`gh run watch`: start it in the background, or stop it once the tag is pushed, and poll as below.

## The commands, in order

1. `git push origin main`, when main has local commits.
2. `just release`, in `~/Code/fawkes`: it tags `v<version>` from `Cargo.toml` and pushes the tag, which publishes
   once the checks pass. It asks first (`just release yes` does not) and refuses unless main is clean and matches
   origin/main and the tag does not exist yet. A tag that exists
   means that version is out: shipping newer commits starts with a version bump in `Cargo.toml`, which is a
   change to land like any other.
3. After the Release run succeeds, install it:

   ```sh
   curl -LsSf https://github.com/shashito/fawkes/releases/latest/download/install.sh | FAWKES_REQUIRE_ATTESTATION=1 sh
   ```

4. Move the daemons onto the new binary: `fawkes reexec --all` once the single supervisor (#55852) has landed;
   the script prints it when `fawkes reexec` exists. Until then, `fawkes service restart --manifest <name>` for
   each daemon on an older version, from a terminal: it is refused inside the app, it restarts the app with the
   daemon, and it rolls back a pending switch, so never while one is pending or a step runs (the script lists
   those as busy). The `fawkes` manifest's app is the Ultimate host on 4747 that runs this conversation.

## Waiting for a Release run

A Release run takes a few minutes. Never wait in one long call (`gh run watch`, a long `sleep`): poll its id from
the **Release run** line in short interruptible checks, each one bash call with `interruptible: true` and a sleep of
30 s or less:

```sh
sleep 30; gh run view <id> --repo shashito/fawkes --json status,conclusion --jq '"\(.status) \(.conclusion)"'
```

Stop when it is `completed`. On `failure`, read `gh run view <id> --repo shashito/fawkes --log-failed`; a macOS
runner that could not be acquired only needs the failed job rerun, which is the user's call. Then run the script
again for the next step.
