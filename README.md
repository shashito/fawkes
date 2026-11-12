<p align="center">
  <img src="assets/logo.svg" width="96" height="96" alt="Fawkes">
</p>

<h1 align="center">Fawkes</h1>

<p align="center">A process supervisor for agents that upgrade themselves.</p>

An agent that changes its own code should never move its own live checkout. It asks Fawkes instead: Fawkes builds the
new version beside the running one, tests it on a spare port with a copy of the app's storage, and switches to it.
Unless the new version confirms itself in time, Fawkes rolls code and storage back together.

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

> **Disclaimer.** Fawkes was built for [Ultimate](https://github.com/HazAT/ultimate), by Ultimate: its design, code,
> tests and reviews were written by Ultimate's agents. It is shared as is. No support for the project is planned.

## Install

Fawkes builds with stable Rust ([rustup](https://rustup.rs)), needs `git` at run time, and runs on macOS and Linux.
From a checkout of this repository:

```sh
cargo install --path . --locked
```

This puts a release build of `fawkes` in `~/.cargo/bin`. Run that copy, not one in `target/`: a supervisor should not
run from a directory the next `cargo build` replaces. To update, pull and run the same command again.

## Quickstart

The sample app in [`examples/sample/`](examples/sample/) serves its `VERSION` file, keeps a SQLite database, and
confirms itself once it listens. It needs Node.js 22.13 or later. From this repository, make it a git repository of
its own beside a manifest:

```sh
mkdir -p /tmp/fawkes-demo/app
cp examples/sample/app.js examples/sample/hooks.js /tmp/fawkes-demo/app/
cd /tmp/fawkes-demo
echo v1 > app/VERSION
git -C app init -q && git -C app add . && git -C app commit -qm v1

cat > fawkes.toml <<'EOF'
name = "demo"
repo = "app"            # the git repository generations are cut from
home = "home"           # the app's storage
port = 4790
start = "node app.js"

[hooks]                 # how to copy, snapshot and restore that storage
snapshot = "node hooks.js snapshot {home} {snapshot}"
restore = "node hooks.js restore {snapshot} {home}"
test_copy = "node hooks.js copy {home} {test_home}"

[env]
APP_PORT = "{port}"
APP_HOME = "{home}"
EOF
```

Start the supervisor in a second terminal, in `/tmp/fawkes-demo`. It runs in the foreground and prints every event:

```sh
fawkes up
```

Back in the first terminal, every command finds the app through `./fawkes.toml`:

```sh
fawkes upgrade HEAD     # stage g1, test it on port 4791, switch it live on 4790
fawkes wait g1          # g1 is permanent: the app confirmed itself
curl localhost:4790     # v1

echo v2 > app/VERSION && git -C app commit -qam v2
fawkes upgrade HEAD     # g2 replaces g1
fawkes wait g2
```

Now a release that passes its test but crashes once it is live (`crash-live` is one of the sample's modes):

```sh
echo crash-live > app/MODE && git -C app add MODE && git -C app commit -qm "a broken release"
fawkes upgrade HEAD     # g3 passes its test and goes live
fawkes wait g3          # exit 1: g3 was rolled back (exited before confirm); g2 runs
fawkes history          # g1 retired, g2 permanent, g3 rolled_back
curl localhost:4790     # v2
fawkes status
fawkes down             # stop the app and its supervisor
```

To clean up, `rm -rf /tmp/fawkes-demo ~/.fawkes/demo`.

Your own app needs a manifest like this one and one call once it works:
`"$FAWKES_BIN" confirm "$FAWKES_GENERATION"` when `FAWKES_PHASE` is `live` (as [`app.js`](examples/sample/app.js)
does). [`examples/sample/fawkes.toml`](examples/sample/fawkes.toml) shows more of the manifest's keys, and DESIGN.md →
Manifest has all of them.

## Everyday commands

```sh
fawkes up                         # run the app under supervision, in the foreground
fawkes upgrade <ref>              # stage, test and switch in one call
fawkes wait [<gen>]               # until a generation is confirmed or rolled back
fawkes status                     # the app, its process and the pending switch
fawkes history                    # the generations
fawkes log --follow               # this app's events as they happen
fawkes watch                      # every running daemon's events, one line each
fawkes rollback                   # undo the pending switch now
fawkes start | stop | restart     # the app's process; the supervisor stays up
fawkes down                       # stop the app and its supervisor
```

`upgrade` is `fawkes stage <ref>`, `fawkes test <gen>` and `fawkes switch <gen>` in one call; each runs on its own
too, and `fawkes confirm <gen>` is what the app calls. `fawkes prune` removes old generations beyond the manifest's
`keep`, and `fawkes reload` reads the manifest again.

A command finds its app through the nearest `fawkes.toml` (up to the repository root), or a manifest in `~/.fawkes`
whose `project` (or `repo`) holds the current directory; elsewhere, `--manifest <name>` names one (DESIGN.md → Finding the app).
Every command but `up` takes `--json` and answers with one JSON document (`log --follow` and `watch` stream one per
line) and stable error codes, made for agents, and `fawkes skill` prints the skill that teaches an
agent all of this. Exit codes: 0 ok, 1 failed, 2 usage, 3 daemon unreachable.

## Running an Ultimate host

The extension in [`ultimate/fawkes/`](ultimate/fawkes/) runs an [Ultimate](https://github.com/HazAT/ultimate) host
under Fawkes: upgrades go through `fawkes upgrade`, a new generation confirms itself once its extensions load, and
storage is Ultimate's two SQLite databases. Set a project up once, stop the Ultimate you run for it, and start Fawkes
there:

```sh
node ultimate/fawkes/init.ts <project>
cd <project> && fawkes up
```

`init.ts` writes the manifest (`~/.fawkes/<name>.toml`), links the extension into the project, turns Ultimate's own
`update` off and trusts the project's setup; run again, it changes nothing. The manifest has `adopt = true`, so the
first `fawkes up` tests the version that was running and makes it permanent, the generation every later upgrade can
roll back to. [`ultimate/fawkes/README.md`](ultimate/fawkes/README.md) has the details, and
[`examples/ultimate.toml`](examples/ultimate.toml) is an example of the manifest it writes.

## On a server

On a desktop, `fawkes up` in a terminal is all you need. On a Mac that must keep the app up without a terminal (a Mac
mini for remote development), run the same daemon under launchd, from the app's directory:

```sh
fawkes service install      # write ~/Library/LaunchAgents/dev.fawkes.<name>.plist and load it
fawkes service restart      # stop the daemon and start it again
fawkes service uninstall    # unload it and remove the plist
```

The agent starts the daemon at login and again whenever it fails; `fawkes down` keeps it stopped until the next login
or `fawkes service restart`, which, like every daemon start, rolls a pending switch back. Its stderr goes to
`<state_dir>/daemon.log`. Run `install` from the installed copy (it refuses a binary in cargo's `target/`), from a
shell whose `PATH` finds `git` and your app's tools (the plist keeps it), and again after an update. It is a LaunchAgent, so it runs while you are
logged in: give a headless Mac automatic login. macOS only; DESIGN.md → Service has the details.

When the app is what broke, its own way in (Ultimate's Tailscale extension, say) is gone with it. Turn on SSH on the
server (Remote Login) before you depend on it, so `fawkes status` and `fawkes rollback` can still be reached.

## How it works

A self-modifying agent such as Ultimate updates the code it runs from. When new code goes live before its
dependencies are installed, or a new core cannot boot, nothing outside the agent can bring the last good version back.
Process managers (launchd, supervisord, PM2) restart a crash, but they know nothing about versions. Fawkes makes every
upgrade a transaction:

- **Stage:** a git worktree at a ref, with its dependencies installed (copy-on-write clones on APFS keep it cheap).
- **Test:** the staged generation boots on a spare port with a copy of the app's storage and must pass its health
  check. A failed test changes nothing live.
- **Switch:** the storage is snapshotted, the new generation goes live, and a confirm deadline starts.
- **Confirm:** the new generation vouches for itself once it is healthy. If it crashes first, or the deadline passes,
  Fawkes rolls back code and storage together.

Fawkes does not read the storage itself: the app's own `[hooks]` copy, snapshot and restore it, run from the last
confirmed generation so that code under test never decides how. An app without hooks gets a code-only rollback and is
tested on an empty home.

The semantics are borrowed, not invented:

- **Generations**, after Erlang/OTP's release handler: a generation is *staged*, then *current*, then *permanent*. A
  cold start always runs the last permanent generation.
- **Mark good**, after RAUC and Mender A/B updates: the new generation must confirm itself healthy, and failed starts
  are counted.
- **Confirm deadline**, after Junos `commit confirmed`: a switch that is not confirmed in time is reverted.
- **No loops:** a generation rolls back at most once.

One daemon supervises one app. It is the parent of the app's process, and every other command is a thin client of
its Unix socket, which also streams events to in-app UIs. [DESIGN.md](DESIGN.md) is the contract: manifest, CLI,
socket protocol, on-disk layout and state machine.

## Status

A working prototype. Everything in [DESIGN.md](DESIGN.md) is built except its **Later** section. It has been tested
against a copy of an Ultimate fork, never a live install. The test suite passes on macOS and on Linux (an Alpine
micro-VM).

## Development

```sh
cargo fmt --check
cargo deny check
cargo clippy --all-targets -- -D warnings
cargo test
```

Every landing runs all four. `scripts/check` runs them in the same order, one line per step and only the output of a
step that fails; `scripts/check --only <test>` runs fmt, clippy and that one test. `cargo deny check`
([cargo-deny](https://github.com/EmbarkStudios/cargo-deny), `brew install cargo-deny`) checks the dependencies for
advisories, licences outside `deny.toml` and unknown sources.

The integration tests (`tests/core.rs`) run the real binary against the sample app, so they need `git` and Node.js
22.13 or later on `PATH` (the app uses `node:sqlite`, which 22.5 to 22.12 only have behind
`--experimental-sqlite`). Each test builds its own world under `/tmp/fk<port>` with a block of ports it claims for
itself, so the suite runs in parallel, and several checkouts can run it at once. `rust-toolchain.toml` pins stable
Rust with clippy and rustfmt.
