<p align="center">
  <img src="assets/logo.svg" width="96" height="96" alt="Fawkes">
</p>

<h1 align="center">Fawkes</h1>

A process supervisor for agents that upgrade themselves.

```mermaid
flowchart LR
    stage["stage<br/>check out, install"] --> test["test<br/>spare port,<br/>copy of the storage"]
    test -- fails --> untouched(["live app untouched"])
    test -- passes --> switch["switch<br/>snapshot the storage,<br/>go live"]
    switch --> confirm{"confirmed<br/>in time?"}
    confirm -- yes --> permanent(["permanent"])
    confirm -- "crash, missed<br/>deadline or rollback" --> rollback["rollback<br/>restore the storage"]
    rollback --> previous(["previous<br/>generation runs"])
    adopt(["first start,<br/>adopt = true"]) -. "test HEAD" .-> permanent
```

<p align="center">
  <i>Every upgrade is a transaction: it becomes permanent, or it rolls back.</i>
</p>

## Highlights

- Upgrades as transactions: **stage, test, switch, confirm**, or an automatic rollback.
- Tests every new version on a spare port, with a copy of the app's storage, before it goes live.
- Rolls code and storage back together when the new version crashes or does not confirm itself in time.
- Leaves storage to the app: its own hooks snapshot and restore it, always run from the last good version.
- Built for agents: `--json` output with stable error codes, and `fawkes skill` teaches an agent the rest.
- Runs [Ultimate](https://github.com/HazAT/ultimate) out of the box, with one setup command.
- One small Rust binary that needs only `git`. Runs on macOS and Linux, and under launchd on a Mac server.

> **Disclaimer.** Fawkes was built for Ultimate, by Ultimate's agents. It is shared as is, and no support is
> planned.

## Installation

Build and install Fawkes with [Rust](https://rustup.rs):

```bash
git clone https://github.com/shashito/fawkes.git
cd fawkes
cargo install --path . --locked
```

To update, pull and run `cargo install` again.

## Documentation

[DESIGN.md](DESIGN.md) is the full contract: the manifest, the CLI, the socket protocol, the files on disk and the
states. `fawkes --help` covers every command, and [`ultimate/fawkes/README.md`](ultimate/fawkes/README.md) the
Ultimate extension.

## Features

### Run Ultimate under Fawkes

Stop the Ultimate of your project, then set the project up once from the `fawkes` folder, and start it:

```console
$ node ultimate/fawkes/init.ts ~/Code/my-app
$ cd ~/Code/my-app
$ fawkes up
```

`init.ts` writes a manifest to `~/.fawkes`, links the extension into the project, turns off Ultimate's own `update`
and trusts the project's setup. The first `fawkes up` tests the Ultimate you were running and keeps it as the version
to roll back to.

### Upgrade

Upgrade from the `fawkes` item in Ultimate's status line, or from a terminal in the project:

```console
$ git -C ~/Code/ultimate fetch    # your Ultimate clone
$ fawkes upgrade origin/main
```

The new version goes live once it passes its test, and confirms itself once its extensions have loaded. Until then,
Ultimate holds its agents' work, so a rollback loses nothing.

### Roll back

A crash or a missed deadline rolls back on its own. To undo a switch that has not confirmed yet:

```console
$ fawkes rollback
```

### Day to day

```console
$ fawkes status     # the app, its process and the pending switch
$ fawkes history    # every generation and how it ended
$ fawkes watch      # the events of every app, one line each
$ fawkes down       # stop the app and Fawkes
```

### Keep it up on a server

On a Mac that has to run without a terminal, launchd starts Fawkes at login and again whenever it fails:

```console
$ fawkes service install
```

Turn on SSH (Remote Login) too: when the app breaks, its own remote access may break with it.

## How it works

An agent that edits its own code can break the code it runs from. A new version may go live before its dependencies
are installed, or fail to boot, and then nothing brings the last good version back. launchd, supervisord and PM2
restart a crash, but they know nothing about versions.

Fawkes adds versions, called generations:

- **Stage:** a git worktree at a ref, with its dependencies installed.
- **Test:** the generation starts on a spare port with a copy of the storage and must pass a health check. A failed
  test changes nothing.
- **Switch:** Fawkes snapshots the storage, puts the new generation live and starts a deadline.
- **Confirm:** the new generation confirms itself. If it crashes first or misses the deadline, Fawkes restores the
  old code and the snapshot.

The ideas are borrowed:

- **Generations** come from Erlang/OTP releases. A cold start always runs the last permanent generation.
- **Confirming itself** comes from RAUC and Mender A/B updates.
- **The deadline** comes from Junos `commit confirmed`.
- **No loops:** a generation rolls back at most once.

## Status

A working prototype. Everything in DESIGN.md is built except its **Later** section. It has been tested on a copy of
Ultimate, never on a live install. The tests pass on macOS and Linux.

## Development

```bash
scripts/check           # cargo fmt, cargo deny, cargo clippy and cargo test, as every landing runs them
scripts/check --only X  # fmt, clippy and the one test X
```

`cargo deny` comes from `brew install cargo-deny`. The integration tests run the sample app in `examples/sample`, so
they need `git` and Node.js 22.13 or later. They run in parallel, each in its own `/tmp/fk<port>` with its own ports.
