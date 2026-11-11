# Fawkes's API, third review: the first run

This review looks at how an app first comes up under Fawkes, and proposes a command line built around the command
being supervised instead of a hand-written manifest and a `daemon` noun. It follows
[`api-review-2.md`](api-review-2.md) and [`cli-api-review-1.md`](cli-api-review-1.md), whose check 24 ("First
use sets itself up") is still **No**. Nothing here changes code; everything proposed is a change to the CLI contract
in `DESIGN.md`.

**Inputs:** `main` at `fd98e27`: `README.md`, `DESIGN.md` (Process model, Daemon start, Manifest, CLI, Finding the
app, Service), `src/main.rs`, `src/discovery.rs`, `src/service.rs`, `src/daemon/lifecycle.rs`,
`examples/ultimate.toml`, `ultimate/fawkes/README.md`, `ultimate/fawkes/start.ts` and `ultimate/fawkes/up.ts`.
No binary was run.

**Verdict:** the transaction verbs are right and stay. The way in is wrong in three places: the command that starts
supervision is a noun (`daemon`), a first run needs a manifest of about seven hand-filled paths, and the daemon
starts nothing until someone runs `up <ref>` from another terminal. The proposal is `fawkes up <command> [args…]`:
the command to supervise is the target, Fawkes derives the repository and start command from it, the app's own facts
(port, health, hooks) come from a defaults file, and today's `up <ref>` becomes `upgrade <ref>`.

## What a first run takes today

For one Ultimate project:

1. Copy `examples/ultimate.toml` to `~/.fawkes/<name>.toml` and fill in seven `/path/to/…` placeholders: `repo`,
   `home` (`~/.ultimate/projects/<project>-<hash>`, a hash the user has to look up), and the extension's paths in
   `start`, `health.command` and the three hooks. Each further instance needs its own `name`, `port` and
   `spare_ports`.
2. Link `ultimate/fawkes` into `~/.ultimate/extensions/fawkes`, and disable the built-in `update` extension in that
   host (`ultimate/fawkes/README.md`, "Where it runs").
3. `fawkes daemon --manifest <name>`. With no generation it starts nothing (Daemon start, step 7: "none: start the
   permanent generation, if there is one").
4. `fawkes up main` from a second terminal. Only now does Ultimate run.

Every later command also needs `--manifest <name>`: discovery (Finding the app, step 6) matches the current
directory against `repo`, which for Ultimate is the shared clone, not the project. From the project folder nothing
matches; from the clone every instance matches and the command is refused.

## 1. Verbs: `up` and `down`, not `daemon`

Every command that changes something is a verb (`start`, `stop`, `stage`, `test`, `switch`, `confirm`, `rollback`,
`prune`, `reload`), and the reads are nouns (`status`, `history`, `log`), which is conventional. `daemon` is the one
noun that acts, and it names the implementation rather than what the user wants. `cli-api-review-1.md` check 1
("Verbs, at most two levels") passed the list with `daemon` in it, which this review corrects.

Docker Compose's vocabulary maps onto Fawkes's two levels exactly:

| Command | Meaning | Today |
|---|---|---|
| `fawkes up <command> [args…]` | Bring the app up under supervision, in the foreground, events on stderr | `fawkes daemon` |
| `fawkes down` | Stop the app and its supervisor | Ctrl-C or `SIGTERM` only |
| `fawkes start` / `stop` / `restart` | The app's process; the supervisor stays | unchanged |
| `fawkes upgrade <ref>` | Stage, test and switch | `fawkes up <ref>` |

The rename of `up <ref>` to `upgrade <ref>` is the price: one verb cannot mean both. `upgrade` is also the word the
Ultimate extension already uses for the same thing (its `upgrade` action and the `↓ 3 new · upgrade` status item).

## 2. The supervised command is the target

`fawkes up ultimate .` and `fawkes up pi`: the command and its arguments, as `docker run <image> [args…]`,
`pm2 start <script>` and `systemd-run <command>` take them. Everything after the command is passed through
untouched (`fawkes up ultimate . --no-open` would need a `--` only if Fawkes ever grows an option of the same name).

Most of the manifest follows from the command:

| Field | Derived from |
|---|---|
| `repo` | the command resolved on `PATH`, symlinks followed, up to the first directory with a `.git` |
| `start` | the same relative path inside the generation's worktree, plus the arguments, relative paths resolved against the directory `up` ran in |
| `name` | the command's name plus the project (the first argument that is a directory, else the current one), so two Ultimate projects are two apps |
| `state_dir` | `<fawkes_home>/<name>`, as today |
| `port`, `spare_ports` | the defaults file (below), else a free port Fawkes picks and records, given to the app as `{port}` |

What cannot be derived is the app's own knowledge: its health check, install command, `reuse`, storage hooks, `env`
and port convention. Those go in a **defaults file** found by command name: a `fawkes.toml` at the root of the
command's repository, else `<fawkes_home>/apps/<command>.toml`. It is today's manifest without `name`, `repo` and
`start`, and with `{project}` added to the templates for the folder argument (Ultimate's `home` is derived from it).
An app with neither file gets Fawkes's existing defaults: the default health URL and a code-only rollback.

The first run of a new app stages the repository's `HEAD` and switches to it, logged as its own `daemon` event with
`action: "adopted"`, only when `state.json` is empty (never as a fallback for a broken state). Whether that first
generation is tested and must confirm, or is made permanent at once because it is what the user just ran, is open
question 1.

`fawkes up` writes the manifest it derived to `<fawkes_home>/<name>.toml`, so discovery, `service install` and
`reload` keep working on a manifest as before, and editing it is how a user overrides a derived field. A second
`fawkes up` of the same command and project reads that file instead of deriving again.

### Discovery

The derived manifest records the project folder as `project`, and discovery matches the current directory against
`project` before `repo` (the reverse lookup `cli-api-review-1.md` idea 1 already proposed). From a project folder,
`fawkes status`, `fawkes upgrade main` and `fawkes down` then need no `--manifest`, and several instances of one clone
stop being ambiguous.

### Word order: `fawkes up ultimate .`, not `fawkes ultimate up .`

The app-first order reads well (`fawkes pi status`), but this review recommends verb-first:

- **The command is only needed once.** After `up`, the app reaches its daemon through `FAWKES_SOCKET` and a user
  through discovery from the project folder, so no other command names it.
- **App-first reserves every verb.** No app could be called `status` or `log`, and each verb added later could break
  an app of that name.
- **It is the shape of every comparable tool:** `docker run`, `pm2 start`, `systemd-run`, `nohup`, `time`.

### Alternative: `fawkes init ultimate .`, then `fawkes up`

Setup and running can be two verbs: `init <command> [args…]` derives the manifest above and writes
`<fawkes_home>/<name>.toml`, and `up` (no command) runs the manifest that discovery finds.

- **The manifest is seen before it runs.** It is trusted input that runs commands as the user (Manifest), so writing
  it first and starting nothing lets the user read what Fawkes derived.
- **`up` never derives or guesses.** It takes no command, so `up main` cannot be mistaken for a command or a ref,
  which makes the `up <ref>` compatibility form (section 5) unambiguous.
- **It is R27's own shape:** "offer `init` only for files the tool owns", and this file is Fawkes's. A second `init`
  of the same command and project refuses (`invalid_state`) and names the file, or shows what would change.
- **Against:** two commands where one would do.

**Recommended:** both, with `init` as the real step. `fawkes up ultimate .` is shorthand for `init` then `up` when no
manifest exists yet for that command and project, and plain `up` otherwise. The extension's `install` (section 3)
then only writes the defaults file that `init` reads.

## 3. The Ultimate extension

Without the extension, `fawkes up ultimate .` could run Ultimate's live phase (`start.ts` only runs
`bin/ultimate.ts <project> --no-open` from the worktree), but no upgrade would be safe: nothing confirms, so every
switch rolls back at its deadline; no hooks, so a rollback restores code but not the databases; and no isolated test
run. So the extension stays a prerequisite, installed once per machine rather than per project:

```sh
cargo install --path . --locked
~/Code/fawkes/ultimate/fawkes/install     # proposed, once per machine
cd ~/Code/my-app && fawkes up ultimate .  # per project, every time
```

`install` would link the folder into `~/.ultimate/extensions/fawkes` and write
`<fawkes_home>/apps/ultimate.toml` (port, health, install, `reuse` and hooks, with paths pointing back at the
extension). The one per-project step left is disabling the built-in `update` extension; the extension could do that
itself when `FAWKES_PHASE` is set, if Ultimate lets one extension disable another (not checked).

The cleaner end state is for the extension and the defaults file to live in Ultimate's own repository, as
`fawkes.toml` at its root. The extension already does nothing without a daemon around it, so nothing would need
installing at all.

## 4. Other apps

- **A command outside a git checkout** (an npm global install of Pi) has nothing to stage from. `fawkes up` should
  still supervise it (crash restarts, backoff, `stop`, `restart`), and `stage` and `upgrade` refuse with
  `invalid_state` and a message that says upgrades need the command to run from a git checkout.
- **An app that never confirms** is rolled back at every deadline. That is `confirm = "health"`, under **Later** in
  `DESIGN.md`; a generic `fawkes up pi` needs it.

## 5. Compatibility

- **`up <ref>` from older generations.** `ultimate/fawkes/up.ts:33` runs `up --json --socket … -- <ref>`, and a
  generation staged before this change keeps running that code. A `fawkes up` whose argument after `--` is not a
  command on `PATH` and is a git ref of the daemon's repository is run as `upgrade` for a deprecation period, with a
  warning on stderr; afterwards it fails `usage` naming `upgrade`. A `--socket` together with a command is the old
  form too, since bringing an app up never names a socket.
- **`daemon` stays as a hidden name** for `up` with a manifest: `src/service.rs:162` writes
  `fawkes daemon --manifest …` into every installed plist, and those keep working until reinstalled. `service install`
  then writes the new form.
- **`--json` and error codes** are unchanged; `up` (foreground, events on stderr) stays text-only as `daemon` is, and
  `upgrade --json` is today's `up --json` result field for field, `step` included.
- **`SKILL.md`** and `fawkes skill` change with it: agents are taught `upgrade`, and the refusals list for
  `FAWKES_PHASE=test` gains `upgrade`, `up` and `down`.

## Open questions

1. **The first generation:** made permanent at once (adopting what the user just ran re-trusts nothing new), or
   tested and confirmed like any other? `cli-api-review-1.md` idea 3 argued for adopting.
2. **Where defaults live:** the command's repository (`fawkes.toml` at its root) first, `<fawkes_home>/apps/` second,
   or only one of them?
3. **`down` and launchd:** for a daemon run as a service, `down` either refuses and names `service uninstall`, or
   stops it with `launchctl bootout` and leaves the plist. The second matches `docker compose down`.
4. **Word order:** verb-first (recommended) or app-first.
5. **`init`:** a separate setup verb with `up <command>` as its shorthand (recommended), `init` only, or `up <command>` only.

## What `DESIGN.md` would change

CLI (the command list, `init`, `up`, `down`, `upgrade`, the hidden `daemon`), Finding the app (`project` first), Manifest (a
derived manifest, the defaults file, `{project}`), Daemon start (adopting on an empty `state.json`, the `adopted`
action), Service (the plist's arguments), the Process model's command list, and the event table.
