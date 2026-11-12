---
name: ultimate-sandbox
description: Check the Fawkes Ultimate extension (ultimate/fawkes) end to end on a throwaway Ultimate in /tmp, never the live install. Use after changing the extension, the Ultimate manifest, adopt or anything an Ultimate host under Fawkes depends on, and before telling the user a real host can move.
---

# Ultimate sandbox

The live hosts (`~/Code/ultimate`, `~/.ultimate`, `~/.fawkes`, ports 4747–4749 and xana's spare ports 4790–4799)
are never touched. `sandbox.sh`, next to this file, builds a whole Ultimate in one folder and runs every process
there under `env -i` with its own `HOME`, `ULTIMATE_USER_HOME` and `FAWKES_HOME`, because an agent's shell inherits
the live host's `ULTIMATE_*` and `FAWKES_*` variables.

## Steps

1. **Build the binary under test** in your worktree (`cargo build`) and check ports 4870–4879 are free
   (`lsof -nP -iTCP:4870-4879 -sTCP:LISTEN`).
2. **Make the sandbox** (about 15 s, about 1 GB in `/tmp/fxs`; `SANDBOX=/tmp/other` for a second one):

   ```sh
   sh .ultimate/skills/ultimate-sandbox/sandbox.sh up target/debug/fawkes
   ```

   It holds `origin.git` (a bare clone of `~/Code/ultimate`, standing for GitHub), `ultimate/` (the clone hosts run
   from, installed from the real pnpm store), `work/` (commit and `git push origin main` here to give an upgrade
   something new), `proj/` (the project), `user/` (the home: no secrets, onboarding off, the extension linked
   user-wide), `ext/` (a copy of `ultimate/fawkes`: a landing does not change it mid-run), `bin/fawkes` and
   `fh/` (the Fawkes home, where `init.ts` writes its manifest; its text says so in the header comment).
3. **Run things in it.** `sandbox.sh run <command>` runs in the foreground, `sandbox.sh start <name> <command>` in the
   background with its pid in `pids/<name>` and its output in `logs/<name>.log`, and `sandbox.sh stop <name>` stops
   that process group. A plain host:

   ```sh
   sh .ultimate/skills/ultimate-sandbox/sandbox.sh start host node /tmp/fxs/ultimate/bin/ultimate.ts /tmp/fxs/proj --no-open --port 4870
   ```

   Look at it with the screenshot and browser tools on `http://127.0.0.1:4870/`. The daemon, from the project
   folder as a user would run it: `sandbox.sh start daemon fawkes up`; the CLI: `sandbox.sh run fawkes status`.
4. **Moving a host under Fawkes**, as a user does it: run `init.ts` once (the `fawkes · set up` card shows this
   command and writes nothing), stop the host (`sandbox.sh stop host`), start the daemon. The sandbox's Fawkes home
   cannot see `~/.fawkes`, so `init.ts` would pick xana's spare ports 4790–4799: first write a stand-in manifest
   that claims them, `/tmp/fxs/fh/xana.toml` with `name = "xana"`, `port = 4749` and `spare_ports = [4790, 4799]`
   (plus `repo`, `home` and `start`, which nothing runs), so the run takes the next free block:

   ```sh
   sh .ultimate/skills/ultimate-sandbox/sandbox.sh run node /tmp/fxs/ext/init.ts /tmp/fxs/proj --ultimate /tmp/fxs/ultimate --port 4870
   sh .ultimate/skills/ultimate-sandbox/sandbox.sh start daemon fawkes up
   ```

   With `adopt = true` it tests the clone's HEAD and makes it permanent, and the host comes back on the same port.
5. **Clean up** when done, whatever happened: `sh .ultimate/skills/ultimate-sandbox/sandbox.sh down` stops every
   process it started and deletes the folder. Then `df -h /`.

## Never

- Start anything in the sandbox without the script (`run` or `start`): it is what keeps the live variables out.
- Kill by name (`pkill`, `killall`): stop by the recorded pid (`sandbox.sh stop`).
- Save a screenshot you need afterwards under `/tmp/fxs`: `down` deletes it. Omit the screenshot's `path`.
