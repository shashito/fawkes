---
name: ultimate-impact
description: Check whether a range of Ultimate commits breaks or changes anything Fawkes depends on (the ultimate/fawkes extension, start.ts, health.ts, the storage hooks, init.ts, the Ultimates menu bar app) and propose the fixes. Use before or after any upgrade of an Ultimate host, when Ultimate's origin/main moves, or when the user asks whether upstream changes affect Fawkes.
---

# Ultimate impact check

Fawkes is generic, but its Ultimate extension (`ultimate/fawkes`) reaches into Ultimate's code, and every Ultimate host
on this machine runs under Fawkes. An Ultimate commit can break a host's next upgrade, `init.ts` on a new project, or
the Ultimates menu bar app. This check reads a range of Ultimate commits and says, with evidence, whether anything
here must change.

## The range

The range is `<old>..<new>` in the Ultimate clone (`~/Code/ultimate`):

- **After an upgrade:** `<old>` is the generation a host ran before and `<new>` the one it runs now (`fawkes history
  --manifest <name> --json`, the two newest generations' `commit`).
- **Before an upgrade, or when `origin/main` moves:** `<old>` is the oldest commit any host runs (`fawkes status
  --manifest <name> --json` for each manifest in `~/.fawkes`, then the current generation's commit), and `<new>`
  is `origin/main`.

Run `git fetch` in the clone first. Read only: never check out, merge or reset anything in the clone.

## What Fawkes depends on

Check each against `git diff <old>..<new> -- <path>` and `git log`:

1. **Modules the extension imports or loads at run time.** List them fresh with
   `grep -rhoE "ultimate/[A-Za-z0-9_./-]+\.tsx?|load\('[^']+'\)|join\(code, '[^']+'\)" ultimate/fawkes/*.ts | sort -u`.
   For each one, check that the file still exists at `<new>` and still exports every symbol the extension uses
   (`git show <new>:<path> | grep -n "export"`). A moved symbol breaks `init.ts` or the type check at once.
2. **The extension host API** (`server/extensions.ts`, `shared/extension-api.ts`, the `@/sdk` web exports): every
   `host.*` member the extension uses (`grep -ohE "host\.[a-zA-Z]+" ultimate/fawkes/*.ts | sort -u`), `onOpen`,
   `onClose`, `harness` hold and release, `defineExtension`, `defineTool`, `section`, `hook`, and the web SDK's
   `actionRef`, `useAction`, `defineWebExtension`.
3. **How Ultimate starts:** `bin/ultimate.ts` (its arguments, `--no-open`, `ULTIMATE_HOME`, `ULTIMATE_PORT`,
   `ULTIMATE_USER_HOME`), `server/main.ts`, and `extensions/instance/server.ts` and `cleanup.ts`, which `start.ts`
   loads for a test run (`testEnv`, `cleanPreview`, the preview env `ULTIMATE_INSTANCE_OWNER`,
   `ULTIMATE_PREVIEW_BRANCH`).
4. **Storage:** the files the hooks copy (`state.sqlite`, `trace.sqlite` and their WAL), a storage migration (a Pi
   Durable upgrade, a schema change), or a new file in the home that holds state a rollback must restore.
5. **The pause extension's API** (`extensions/pause/api.ts`), which the hold wraps.
6. **Discovery that init.ts and the menu bar app read:** `ultimate.lock` and `project.json` in the home, the home's
   name (`projectHome`), `trust.json`, `extensions.disable`, and the `update`, `status`, `tailscale` and `instance`
   extension actions the menu bar app calls (`~/Code/ultimate-menubar/Sources/UltimatesCore/UltimateAPI.swift`).
7. **The packages:** a changed `pnpm-lock.yaml` or Node `engines` floor changes the install every stage runs, and
   the Node the hooks run on.

Then run `ultimate/check` against the clone at `<new>`. Use a detached worktree in `/tmp`, removed by its exact path
afterwards, when the clone is not at `<new>`. It type-checks the extension against Ultimate's own types, which
catches most of 1, 2 and 5.

## The answer

One short report:

- the range, and how many commits it has;
- **Breaks**, each with the Ultimate commit, the Fawkes file and line it breaks, and what fails (the type check,
  `init.ts`, a stage, a test run, a rollback);
- **Changes worth adopting**: new Ultimate features Fawkes could use or should follow, each with its commit;
- **Nothing to do**, when that is the answer, saying what was checked;
- for each break, a proposed fix sized as a branch prompt, and whether it blocks the next upgrade. A break that
  would fail a host's next upgrade's test is caught by Fawkes, so nothing goes live, but the upgrade can't happen
  until it is fixed.

Never change code here: propose the fix and let @ultimate start the branch.
