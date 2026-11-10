<p align="center">
  <img src="assets/logo.svg" width="96" height="96" alt="Fawkes">
</p>

<h1 align="center">Fawkes</h1>

<p align="center">A process supervisor for agents that upgrade themselves.</p>

An agent that changes its own code should never move its own live checkout. It asks Fawkes instead, and every
upgrade becomes a transaction that either becomes permanent or rolls itself back.

```
stage → test → switch → confirm   (or automatic rollback)
```

> **Disclaimer.** Fawkes was built for [Ultimate](https://github.com/HazAT/ultimate), by Ultimate: its design, code,
> tests and reviews were written by Ultimate's agents. It is shared as is. No support for the project is planned.

## Why

A self-modifying agent such as [Ultimate](https://github.com/HazAT/ultimate) updates the code it runs from. When new
code goes live before its dependencies are installed, or a new core cannot boot, nothing outside the agent can bring
the last good version back. Process managers (launchd, supervisord, PM2) restart a crash, but they know nothing about
versions.

## How it works

- **Stage:** a git worktree at a ref, with its dependencies installed (copy-on-write clones on APFS keep it cheap).
- **Test:** the staged generation boots on a spare port with a copy of the app's storage and must pass its health
  check.
- **Switch:** the storage is snapshotted, the new generation goes live, and a confirm deadline starts.
- **Confirm:** the new generation vouches for itself once it is healthy. If it crashes, or the deadline passes, Fawkes
  rolls back code and storage together.

Fawkes does not read the storage itself: the app's own `[hooks]` in the manifest copy, snapshot and restore it, run
from the last confirmed generation so code under test never decides how. An app without hooks gets a code-only
rollback and tests on an empty home.

The semantics are borrowed, not invented:

- **Generations**, after Erlang/OTP's release handler: a generation is *staged*, then *current*, then *permanent*. A
  cold start always runs the last permanent generation.
- **Mark good**, after RAUC and Mender A/B updates: the new generation must confirm itself healthy, and failed starts
  are counted.
- **Confirm deadline**, after Junos `commit confirmed`: a switch that is not confirmed in time is reverted.
- **No loops:** a generation rolls back at most once.

## Installation

Fawkes builds with stable Rust ([rustup](https://rustup.rs)) and needs `git` at run time. It runs on macOS and
Linux. From a checkout of this repository:

```sh
cargo install --path . --locked
```

This builds a release binary and puts `fawkes` in `~/.cargo/bin`, which rustup
adds to your `PATH`. Run that installed copy rather than `target/`: a supervisor should not run from a build directory
that the next `cargo build` replaces. To update, pull and run the same command again.

**Upgrading from a build with `storage = [...]`:** that field is gone and such a manifest no longer loads; name the
app's storage hooks in `[hooks]` instead (DESIGN.md → Hooks). A snapshot the old build took is not one the hooks can
restore, so install the new build only while no switch is pending (`fawkes status` shows no `pending`).

## Using it

Each app gets a small TOML manifest: its repository, start and install commands, a health check (URL or command), the
hooks that snapshot and restore its storage, its live port and a range of spare ports for tests. See
[`examples/sample/fawkes.toml`](examples/sample/fawkes.toml) and [`examples/ultimate.toml`](examples/ultimate.toml).

```sh
fawkes daemon                             # one daemon per app, in the foreground
fawkes stage main                         # → g1
fawkes test g1
fawkes switch g1                          # the new generation then runs `fawkes confirm`
fawkes up main                            # or all three in one call: stage, test and switch
fawkes status
fawkes watch                              # every supervised app's events, one line each
```

Run from the app's repository, a command finds its `fawkes.toml`; from anywhere else, `--manifest <name>` names a
manifest in `~/.fawkes` (DESIGN.md → Finding the app). `fawkes skill` prints the skill that teaches an agent all of
this. Every command takes `--json` and answers with readable errors and stable error codes, made for agents. The same
commands are available over the daemon's Unix socket, with a live event stream for in-app UIs.
[DESIGN.md](DESIGN.md) is the full contract: manifest, CLI, socket protocol, on-disk layout and state machine.

## On a server

On a desktop, run `fawkes daemon` in a terminal as above. That stays the normal way, and nothing else is needed. On a
Mac that must keep the app up without a terminal (a Mac mini for remote development), run the same daemon under
launchd, installed with one command:

```sh
fawkes service install --manifest fawkes.toml     # write the agent's plist and load it
fawkes service restart --manifest fawkes.toml     # stop the daemon and start it again
fawkes service uninstall --manifest fawkes.toml   # unload it and remove the plist
```

`install` writes `~/Library/LaunchAgents/dev.fawkes.<name>.plist` and loads it with `launchctl bootstrap
gui/$UID`. The plist runs the `fawkes` you ran it with as `fawkes daemon --manifest <absolute path>`, with the
installing shell's `PATH` (and `FAWKES_HOME`, when set), at login (`RunAtLoad`) and again whenever the daemon fails
(`KeepAlive` with `SuccessfulExit = false`; a daemon stopped on purpose stays stopped). Its stderr goes to
`<state_dir>/daemon.log`, which the daemon keeps under 10 MiB. Run `install` from the installed copy (it refuses a
binary in cargo's `target/`), and again after an update to load the new binary and `PATH`. A daemon restart rolls a
pending switch back, as every daemon start does. The daemon cannot tell it runs under launchd: every other command
talks to it over its socket as before. It is a LaunchAgent, so it runs while you are logged in: give a headless Mac
automatic login. macOS only; DESIGN.md → Service has the details.

**Reaching it when the app is down.** An app's own way in is gone when the app is what broke. Ultimate's door, its
Tailscale extension, lives inside Ultimate's core, so when the core is down so is the door, and `pnpm cli` (or
`fawkes status`, `fawkes rollback`) can only be reached over a path of its own: turn on SSH on the server (Remote
Login, reachable over the tailnet or your network) before you depend on it.

## Status

A working prototype. Everything in [DESIGN.md](DESIGN.md) is built except its **Later** section: the transaction
(`stage`, `test`, `switch`, `confirm`, automatic and manual `rollback`), `start`, `stop` and `restart` with crash
restarts and backoff, `prune`, manifest reload on SIGHUP, and `watch`. It has been tested against a copy of an
Ultimate fork, never a live install. The test suite passes on macOS and on Linux (an Alpine micro-VM).

## Running the tests

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`rust-toolchain.toml` uses stable Rust with clippy and rustfmt, and `rust-version` in `Cargo.toml` names the release
it is developed and tested on. The integration tests (`tests/core.rs`) run the real binary against the sample app in
[`examples/sample/`](examples/sample/), so they need `git` and Node.js 22.13 or later on `PATH` (the app uses
`node:sqlite`, which 22.5 to 22.12 only have behind `--experimental-sqlite`). Each test builds its own world under
`/tmp/fk<port>` with a block of ports it claims for itself, so the suite runs in parallel, and several checkouts can
run it at the same time.
