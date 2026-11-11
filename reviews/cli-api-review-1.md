# Fawkes's CLI against the CLI style guide

This review runs the checklist of the CLI style guide (the `cli-designer` skill's `style.md`) against Fawkes's
command line on `main` at `d4bacad` (PLAN-05 phase 11). It then adds what the contrast study
(the skill's `patterns.md`) suggests, and weighs the user's pending ideas.
Rules are cited as `R<n>` (the style guide's) and `P<n>` (other tools'). Nothing here changes code.

The inputs were `fawkes --help` and each command's `--help` from a debug build of `d4bacad`, the CLI section of
`DESIGN.md` (lines 461–533), and `src/main.rs`. Phase 12's `fawkes service install|uninstall|restart` (#26095) is
being built in parallel and is not assessed; it is a noun group of three verbs, which is exactly what R1 allows.

**Verdict:** the grammar, errors, exit codes and agent safety already match the style, in places more strictly
than its own tools do. Three things fall short: the first use (a hand-written manifest and `--manifest` on every
command), the help text (a three-line `--json` description repeated under every command, positionals without help,
no examples), and `--version`, which is missing. The user's ideas fix the first, and they fit the style, except for
grouped command help and the "only one daemon is running" fallback.

## The checklist

| # | Check | Fawkes today | Evidence |
|---|---|---|---|
| 1 | Verbs, at most two levels (R1) | **Yes** | 15 commands, one level: `daemon start stop restart stage test switch up confirm rollback prune status history log watch` |
| 2 | Positionals name the object and have help (R3) | **Partly** | `<REF>` and `<GEN>` print an empty help line under `stage`, `test`, `switch` and `up` (`src/main.rs:67-94`); `confirm [GEN]` has one |
| 3 | The common call needs no flags (R4, R7) | **No** | Without `./fawkes.toml` every command fails: `error: cannot read manifest fawkes.toml: No such file or directory (os error 2); pass --manifest or --socket to say which daemon to talk to`, exit 2 |
| 4 | Every flag earns its place (R5) | **Yes** | Three global options and five command options (`--deadline`, `--keep-storage`, `-n`, `--follow`, `--since`); `--keep-storage` is a deliberate opt-in |
| 5 | Project found from the current directory (R8) | **No** | Only `./fawkes.toml` (`DESIGN.md:481`) |
| 6 | Precedence stated (R8, R11) | **Yes for the socket** | `--socket`, else `FAWKES_SOCKET`, else the manifest's (`DESIGN.md:482-483`). The manifest itself has no environment variable |
| 7 | Ambiguity refuses (R9) | **Does not apply yet** | No discovery. `watch` without a target follows every daemon, which is not a guess |
| 8 | No abbreviations; aliases explicit (R10) | **Yes** | clap's inference is off; `statsu` fails with `error: unrecognized subcommand 'statsu'` |
| 9 | One prefix, variables shown with their options (R11) | **Partly** | `FAWKES_SOCKET` and `FAWKES_GENERATION` are named in help text; `FAWKES_HOME` only in `DESIGN.md:303,335` |
| 10 | One-sentence abouts, details in long help (R13) | **No** | `--json`'s help is a 240-character line, repeated under all 15 commands, `daemon` included, where it ends in "Not for `daemon`". The `up` and `watch` abouts are clauses joined by semicolons, and `test`'s carries a parenthesis about hooks (`src/main.rs:39-42,73,86,124`) |
| 11 | Options under headings once there are many (R13) | **Does not apply** | Eight options in all |
| 12 | Runnable examples in help (R14) | **No** | None; they live in `README.md` and `skills/fawkes/SKILL.md` |
| 13 | `--help` works with a broken project (R15) | **Yes** | clap answers `--help` before any manifest is read (`DESIGN.md:483-484`) |
| 14 | `--version` (R16) | **No** | `fawkes -V`: `error: unexpected argument '-V' found`, exit 2; `--version` the same |
| 15 | `error:` on stderr, causes on their own lines (R17) | **Yes** | One `error: <sentence>` line on stderr; under `--json`, one `{"code","message"}` object on stdout (`DESIGN.md:511-518`) |
| 16 | Errors name the fix (R18) | **Yes** | "…; stage the ref again for a new generation", "…; start one with `fawkes daemon --manifest fawkes.toml`" (`DESIGN.md:528-533`). A mistyped command gets no suggestion |
| 17 | Exit codes 0/1/2, documented (R19) | **Yes, better than the guide's tools** | 0, 1, 2 and 3 (`daemon_unreachable`), in `--help`'s footer and `DESIGN.md:519-522` |
| 18 | Starting twice fails clearly (R20) | **Yes** | A second daemon is refused by the `flock` on `<state_dir>/lock` and by a socket that answers (`DESIGN.md:232-246`) |
| 19 | stdout is only the result (R21) | **Yes** | Progress lines go to stderr (`DESIGN.md:511`) |
| 20 | One machine format, additive promise written down (R22) | **Partly** | Every command but `daemon` has `--json`, and readers tolerate unknown enum values (`DESIGN.md:567-575`). No sentence promises that result fields are only ever added |
| 21 | Human output ends with the next command (R23) | **Partly** | `status` prints `actions: …`, the commands the daemon accepts now, though before the process line rather than at the end (`DESIGN.md:666-668`); other commands' text output does not name a next step |
| 22 | Colour and progress only on a terminal (R25) | **Does not apply** | No colour, no progress bars; progress is plain lines |
| 23 | No prompt without a flag (R26) | **Yes** | No prompts at all |
| 24 | First use sets itself up (R27) | **No** | A first run needs a hand-written manifest of about seven fields, a daemon in its own terminal, and a confirm the app must make |
| 25 | The tool installs its own skill (R28) | **Partly** | `skills/fawkes/SKILL.md` is in the repository; the binary cannot print or install it |
| 26 | Renamed names keep working or explain (R31) | **Yes** | A manifest with `storage` is refused with an error that says `[hooks]` replaced it (`DESIGN.md:343`) |
| 27 | Bounded time, readable logs (R32) | **Yes** | Quick commands give up after 30 s with `no_answer` (`DESIGN.md:524-526`); `log`, `history`'s `log_tail`, and the event log on disk |

**What already matches the style:**
- Verbs at one level (R1).
- Few flags (R5).
- Errors that say what to do next (R18).
- Exit codes beyond 0/1/2, and documented, which the guide's tools never do (R19).
- Refusing a second daemon, the shoreman lesson (R20).
- A clean stdout (R21).
- `status`'s `actions` line (R23).
- No prompts (R26).
- A tombstone error for the removed `storage` key (R31).

## Proposed changes

The source column says whether a change follows the style guide (`style`) or another tool's practice (`other`). The order is the
recommended order of work.

| # | Change | Source | Size |
|---|---|---|---|
| C1 | Find the app from the current directory (user idea 1, below) | style R8, R9; other P12 | about 80 lines + unit tests |
| C2 | `--manifest` also takes a bare name, plus `FAWKES_MANIFEST` (user idea 2) | style R6, R11; other P7 | about 20 lines |
| C3 | Looser manifest defaults (user idea 6) | style R7 | about 30 lines + `DESIGN.md` |
| C4 | Help text: one-line `--json`, help for `<REF>`/`<GEN>`, one-sentence abouts, examples in long help | style R3, R13, R14 | help strings only |
| C5 | `fawkes --version` | style R16 | one attribute |
| C6 | Say which manifest was picked, and by which rule | style R16; other P14 | a line in `status` and in discovery errors |
| C7 | The binary installs its skill | style R28 | about 30 lines |
| C8 | Write the additive-only promise for `--json` results into `DESIGN.md` | style R22 | one sentence |
| C9 | Adopt the running version on the first daemon start (user idea 3) | style R27 | a phase of its own |
| C10 | Show `[env: FAWKES_…]` next to the options | style R11; other P11 | clap's `env` feature, no new crate |

### C4 in detail: what `--help` would look like

```
A process supervisor whose upgrades are transactions: stage, test, switch, confirm, or roll back

Usage: fawkes [OPTIONS] <COMMAND>

Commands:
  daemon    Run the supervisor in the foreground
  start     Run the current generation (pending, else permanent)
  stop      Stop the app
  restart   Stop the app and start the current generation again
  stage     Check out and install a git ref as a new generation
  test      Run a generation on a spare port with a copy of the storage
  switch    Go live; roll back unless confirmed in time
  up        Stage, test and switch a git ref in one call
  confirm   Make the pending generation permanent
  rollback  Undo the pending switch now
  prune     Remove old generations beyond the manifest's `keep`
  status    Show the app, its process and the pending switch
  history   Show the generations
  log       Show the event log
  watch     Print the events of every running daemon, one line each

Options:
      --json             Print the result as one JSON document (see --help for streams)
      --manifest <APP>   The app's manifest: a path, or a name in ~/.fawkes [env: FAWKES_MANIFEST=]
      --socket <PATH>    The daemon's socket [env: FAWKES_SOCKET=]
  -h, --help             Print help (see more with '--help')
  -V, --version          Print version

Exit codes: 0 ok, 1 the command failed, 2 usage error, 3 no daemon answers.
```

The full `--json` paragraph (streams, `log --follow`, `watch`), the "confirming stays the new generation's own step"
note on `up`, and these examples go into the long help (`--help`, not `-h`), as minijinja-cli's `--long-help` does
(R13, R14):

```
Examples:
  fawkes up main                 stage, test and switch main; the new generation confirms itself
  fawkes status --json           what runs, what is pending, and the commands accepted now
  fawkes rollback                undo the pending switch
  fawkes --manifest xana log -n 20
```

`daemon` should not list `--json` at all. The positionals get help lines: `<REF>  A git branch, tag or commit` and
`<GEN>  A generation id, as shown by history (g4)`.

## The user's pending ideas

### 1. Find the app's daemon from the current directory

**Fits the style, with one change.** Walking up from the current directory to a project marker is what rye
(`pyproject.toml`), sentry-cli (`.sentryclirc`), platter (`setup.py`) and gh-issue-sync (`.issues/`, stopping at
`.git`) do (R8). rye, sentry-cli and platter walk all the way to `/`; only gh-issue-sync stops at the repository
root. The side chat's order was `--socket`, `--manifest`, `FAWKES_SOCKET`, the current directory, `./fawkes.toml`,
then "the only daemon running". In the guide's terms:

1. `--socket`, then `--manifest` (a name or a path): the flag wins.
2. `FAWKES_SOCKET`, then `FAWKES_MANIFEST` (C2): the environment comes next, and an app Fawkes runs keeps reaching
   its own daemon.
3. **Walk up** from the current directory to the first `fawkes.toml`, stopping at the repository root (the `.git`).
   The walk is the guide's pattern, and the project carries its manifest. The stop at `.git` is gh-issue-sync's (and
   just's `--ceiling`, P12, other), chosen so that a project nested in another never picks up the outer one's
   manifest. It replaces today's `./fawkes.toml`, which is the first step of that walk.

**Steps 1 and 2 change today's contract.** Today `FAWKES_SOCKET` beats `--manifest` (`DESIGN.md:482-483`,
`socket_path` in `src/main.rs:293-304`), so an inherited `FAWKES_SOCKET` silently overrides an explicit
`--manifest`. Flag over environment is the guide's rule (R8), and it fits Fawkes better: an app running under Fawkes that
names another app's manifest means that app. It needs a `DESIGN.md` change. A bare `confirm` must then use
`FAWKES_GENERATION` only when the daemon it reaches is the caller's own, as `src/main.rs:228-232` already does for
`--socket`.
4. **Then the registry:** match the current directory against the `project`, else the `repo`, of each
   `<fawkes_home>/*.toml`. The guide's tools never do this reverse lookup, because their configuration always lives in the
   tree. Fawkes needs it for Ultimate hosts, whose manifests live in `~/.fawkes` and whose `repo` is the shared
   Ultimate clone. It stays in the style's spirit as long as it is deterministic and refuses when it is ambiguous.
5. **Ambiguous: refuse, list the names, and say how to be explicit:** `error: ~/Code/ultimate matches the apps
   fawkes, xana; pass --manifest <name>` (R9, Flask's "Use '{module}:name' to specify the correct one").

**Drop "if exactly one daemon is running, that one."** Flask's "the only Flask instance" heuristic (R9) works
inside a module the user named, a closed set. "The only running daemon" is an open set: it changes meaning when
someone starts a second app. That is the hazard Click's "No Auto Correction" warns about: a script that works
today and silently targets something else tomorrow (R10). For `rollback` or `up` that is a real risk. `watch`
already covers "every daemon" for reading.

The new `project` field is one optional manifest key (a `DESIGN.md` change). An agent gains plain
`fawkes status` and `fawkes up main` inside a project folder, and loses nothing, because the explicit flags still
win.

### 2. `--manifest` taking a bare name instead of an `--app` flag

**Fits.** One flag whose value is read by its shape is the guide's habit (R6): Flask's `FLASK_APP` takes `hello`,
`hello:app` or a path in one value, and systemfd's `-s` takes `TYPE::SPEC`. tmux splits the same idea into `-L name`
and `-S path` (P7, other). Rule: a value with a `/` or ending in `.toml` is a path, anything else is
`<fawkes_home>/<name>.toml`. Add `FAWKES_MANIFEST` with the same meaning (R11). It ranks below `FAWKES_SOCKET`,
which Fawkes sets for its own children, so an inherited `FAWKES_MANIFEST` cannot pull an app's `fawkes confirm` to
another daemon.

On the name: the guide's tools name a flag after a file when it takes a path (`--pyproject`, `--manifest-path`,
`--config-file`), and after the object when it takes a name (`--org`, `--project`, Flask's `--app`). A flag that
takes both could be either. Keeping `--manifest` costs nothing, and renaming it would need a hidden alias forever
(R31). Keep it.

### 3. `fawkes daemon` adopting the running version on first start

**Fits.** This is the guide's "bootstrap on first use" (R27): bare `rye` installs itself (`rye/rye/src/cli/mod.rs:105`), and
idasen's client starts its daemon when none answers. Three details keep it in the style and inside Fawkes's
promise:

- **Log it as its own event.** The guide's tools say what they did automatically.
- **Only with an empty `state.json`.** It is first-use behaviour, never a fallback for a broken state.
- **No flag to start with.** If an opt-out is ever needed, make it an environment variable like
  `RYE_NO_AUTO_INSTALL`, not a new flag (R5).

The transactional point stands: adopting re-trusts what is already running, so skipping the test re-trusts
nothing new.

### 4. Setup through the Ultimate extension rather than `fawkes init`

**Fits for Ultimate, and leaves room for `init` later.** Agent-era integrations configure themselves from the
host they run in. pi-llamacpp and pi-ds4 start and configure their servers ("Most users do not need any
configuration.", `pi-ds4/README.md:80`), so an extension that writes the manifest from what the host already knows
is in that line. Tools that own files in a project also have an `init` (`rye init`, `gh-issue-sync init`,
`absurdctl init`; R27). For a generic app, a later `fawkes init` should write `./fawkes.toml` into the repository,
because that is the file the walk-up in idea 1 finds. Together with looser defaults (idea 6), that file could be
four lines. The extension first; `init` once a non-Ultimate app needs it.

### 5. Grouped help

**Does not fit, and is not needed.** The guide's tools group options under headings (minijinja-cli's "Template Behavior",
"Security", "Advanced"; R13) but never commands: rye lists 23 in one block and sentry-cli 24. When commands need
structure they nest them (R1), and phase 12's `service` group does exactly that. clap 4.6.7 offers only one heading
for all subcommands (`Command::subcommand_help_heading`), so per-group headings would mean a hand-written help
template kept in sync by hand. The real problem the side chat named, a page that does not fit on one screen, is the
repeated `--json` paragraph and the long abouts. C4 fixes both, and the 15 commands, already in workflow order,
then fit on one screen.

### 6. Looser manifest defaults

**Fits.** "it picks winners" (R7). `spare_ports` defaulting to the ten ports after `port`, and `[health]` to `url =
"http://127.0.0.1:{port}/"`, are the kind of defaults `flask run` has (`127.0.0.1:5000`). A minimal manifest is
then `name`, `repo`, `port` and `start`. Validation keeps refusing unknown keys, so looser defaults never turn into
silent typos. Write each default into `DESIGN.md`'s manifest example (CLAUDE.md: `DESIGN.md` is the contract).

## Suggestions from the other tools

Each item is marked with its source.

- **Already matched:**
  - lazygit's always-visible legal actions (P13, other): `status.actions`.
  - jj's operation log with honest limits (P4, other): the event log with `seq`, `history`, `rollback`, and
    `DESIGN.md`'s statement that a rollback restores storage but not what the app did outside it.
  - docker's separate exit code for the tool itself (P8, other): exit 3 for `daemon_unreachable`.
  - gh's "no prompt without a flag" (P1, other): Fawkes has no prompts at all.
- **Worth taking:**
  - just's ceiling for the walk-up (P12, other), as the `.git` stop in C1.
  - starship's and curl's introspection (P14, other), as C6.
  - fish's translated idioms (P15, other): when a guessed command fails (`fawkes deploy`, `fawkes logs`), the error
    could name the right one. That needs clap's `suggestions` feature, which adds the `strsim` crate, so it needs the
    user's go under the crate rule.
- **Not recommended:**
  - Output that changes when piped (P6, other): an agent would read something different from what a person sees.
    `--json` stays the explicit switch.
  - gh's field-selected `--json` (P2, other): the documents are small, and the additive promise (C8) is simpler.
  - cargo's `--format-version` (P5, other): versioning only pays off once a field must change meaning. The additive
    promise avoids that.
  - A client that starts the daemon itself, as tmux and idasen do (P7): a supervisor must outlive its clients, and
    phase 12's service covers starting it.
  - Prefix matching for commands (P7, other): R10 forbids it.
- **Later, not proposed now:** a blocking wait for a generation's outcome (tmux's `wait-for`, P7, other). Today an
  agent watches `fawkes watch --json` or polls `status`.

## Open decisions

1. **Discovery order (C1):** (a) `--socket`, `--manifest`, `FAWKES_SOCKET`, `FAWKES_MANIFEST`, walk up to
   `fawkes.toml` (stopping at `.git`), then the registry match; ambiguity refused, no single-daemon fallback; an
   explicit `--manifest` then beats an inherited `FAWKES_SOCKET`, a `DESIGN.md` change; (b) the same, but keep
   today's `FAWKES_SOCKET` over `--manifest`; (c) the side chat's order including the single-daemon fallback.
   Recommended: (a).
2. **Skill command (C7):** (a) `fawkes install-skill [DIR] [--force]`, absurdctl's shape; (b) `fawkes skill` printing
   it to stdout for the extension or the agent to place; (c) neither. Recommended: (b), the smaller change, since the
   Ultimate extension does the setup.
3. **"Did you mean" for commands:** (a) enable clap's `suggestions` (adds `strsim`); (b) leave it. Recommended: (b)
   for now; the command list is short.
