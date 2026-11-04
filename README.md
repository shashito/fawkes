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
- **Switch:** the storage is snapshotted with SQLite's backup API, the new generation goes live, and a confirm deadline
  starts.
- **Confirm:** the new generation vouches for itself once it is healthy. If it crashes, or the deadline passes, Fawkes
  rolls back code and storage together.

The semantics are borrowed, not invented:

- **Generations**, after Erlang/OTP's release handler: a generation is *staged*, then *current*, then *permanent*. A
  cold start always runs the last permanent generation.
- **Mark good**, after RAUC and Mender A/B updates: the new generation must confirm itself healthy, and failed starts
  are counted.
- **Confirm deadline**, after Junos `commit confirmed`: a switch that is not confirmed in time is reverted.
- **No loops:** a generation rolls back at most once.

## Using it

Each app gets a small TOML manifest: its repository, start and install commands, a health check (URL or command), the
storage to snapshot, its live port and a range of spare ports for tests. See
[`examples/sample/fawkes.toml`](examples/sample/fawkes.toml) and [`examples/ultimate.toml`](examples/ultimate.toml).

```sh
fawkes daemon --manifest fawkes.toml      # one daemon per app, in the foreground
fawkes stage main                         # → g1
fawkes test g1
fawkes switch g1                          # the new generation then runs `fawkes confirm`
fawkes status
fawkes tui                                # every supervised app on one screen
```

Every command takes `--json` and answers with readable errors and stable error codes, made for agents. The same
commands are available over the daemon's Unix socket, with a live event stream for in-app UIs.
[DESIGN.md](DESIGN.md) is the full contract: manifest, CLI, socket protocol, on-disk layout and state machine.

## Status

A working prototype. Everything in [DESIGN.md](DESIGN.md) is built except its **Later** section: the transaction
(`stage`, `test`, `switch`, `confirm`, automatic and manual `rollback`), `start`, `stop` and `restart` with crash
restarts and backoff, `prune`, manifest reload on SIGHUP, and the `tui`. It has been tested against a copy of an
Ultimate fork, never a live install. It runs on macOS; the Linux code paths have not been compiled or tested yet.

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
