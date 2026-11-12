# Fawkes

A process supervisor whose upgrades are transactions: `stage → test → switch → confirm`, or automatic rollback.
One daemon per app; every other command is a thin client of its Unix socket. Read `README.md` for the intent and
`DESIGN.md` for the contract.

## Before you reply

Run these yourself before saying a change is done, not at commit time:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

All three are clean on `main`. Treat any warning or failure as yours: don't assume it was already there.
`scripts/check` runs these and `cargo deny check` in the lander's order, keeps cargo's exit code and prints only a
failing step's output; `scripts/check --only <test>` runs fmt, clippy and one test.

- Run the narrowest test first (`cargo test --test core <name>`, `cargo test watch::`), then the whole suite.
- Use debug builds. Don't build with `--release` unless asked.

## Tests

- Integration tests (`tests/core.rs`) run the real binary against a throwaway world built by `tests/common/mod.rs`.
  They need `git` and `node` on `PATH`, and the sample app is `examples/sample/app.js`, with its storage hooks in
  `examples/sample/hooks.js`.
- `Harness::new(extra)` claims a free block of ten ports between 20000 and 29999 through a `flock` on
  `/tmp/fawkes-test-ports/<port>`, and the directory `/tmp/fk<port>`, kept short because socket paths have a length
  limit. Tests, and suites in other worktrees, run in parallel without colliding, so never hard-code a port or a
  path: use `h.port` and `h.dir`, and never look for processes machine-wide (`pgrep`, `pkill`), since another
  suite's look the same. Have the app record its pid in a file instead.
- Tests never build the binary's command themselves (`Command::new` of it): use `common::fawkes()`, `Harness::cli`
  or `Harness::fk`. They drop the `FAWKES_*` variables a suite inherits when it runs inside an app Fawkes supervises,
  which would otherwise send the test's requests to that app's live daemon.
- Unit tests live next to the code in `#[cfg(test)] mod tests` (`state.rs`, `manifest.rs`,
  `process.rs`, `protocol.rs`, `main.rs`, `render.rs`, `discovery.rs`, `daemon/mod.rs`, `daemon/supervisor.rs`,
  `daemon/transactions.rs`, `watch.rs`); a scratch directory comes from `tempfile::tempdir()`. The restart policy is
  the pure `after_exit` in `daemon/supervisor.rs`: test a change to it there, in milliseconds, and keep the
  integration tests as end-to-end smoke tests.
- To fix a bug, first write a test that reproduces it.
- Never point a test or a manual run at a live app or a real `~/.fawkes`. Use a temp directory and a manifest in it.

## Working on this machine

Several branches, workers and test suites share one Mac with little free disk.

- **Kill only what you started.** Stop processes by the pid or process group your own run recorded. Never kill
  machine-wide (`pkill -f`, `killall`): another suite's processes look the same as yours.
- **Clean up before you finish.** Run `cargo clean` in your worktree and remove every `/tmp` folder you created
  (sandboxes, VM images, scratch clones). A finished worktree's `target/` is about 2 GB.
- **Short socket paths.** A unit test that binds a Unix socket makes its directory with
  `tempfile::Builder::new().tempdir_in("/tmp")`, not under `$TMPDIR`: the lander's `$TMPDIR` is long enough to
  exceed the socket path limit.
- **Probes that stream** (`fawkes up`, `fawkes watch`, `log --follow`) run in the background with a bounded wait
  and are stopped by pid, never in the foreground of a tool call.
- **Keep cargo's exit code.** When you filter cargo's output (`| tail`, `| grep`), use `set -o pipefail`, or the
  filter's exit code hides a failure.
- **Never touch the live Ultimate** (`~/Code/ultimate`, `~/.ultimate`) or link into `~/.ultimate/extensions`. Check
  the Ultimate extension against a sandboxed fresh clone under `/tmp`, started under `env -i`.
- **Nobody runs Fawkes yet.** Remove old names, migrations and upgrade notes outright; don't keep aliases or
  tombstones (this waives R31 of the `cli-designer` skill until Fawkes has users).

## DESIGN.md is the contract

`DESIGN.md` defines the manifest, CLI, socket protocol, on-disk layout and state machine. If a change alters any of
them (a field, an error code, a state, an exit code, an event), update `DESIGN.md` in the same change. If the code
and `DESIGN.md` disagree, ask which one is right instead of choosing one silently. Features under **Later** are
not built yet. Don't start one unless asked.

## Invariants that are easy to break

- **The transactional promise comes first:** every upgrade either becomes permanent or rolls back. A panic, an early
  `return` or a half-written `state.json` partway through a switch, confirm or rollback breaks that promise. In
  daemon code paths, return `protocol::Error` with `?` and don't add `unwrap`/`expect`. They are fine in tests.
- **A generation rolls back at most once.** Never add a path that rolls back a rolled-back generation.
- **Locking** (see the header of `daemon/mod.rs`): never hold `inner` across a slow step (install, health check,
  process stop). The lock order is `inner` → `manifest` → `log` → `hub` and `echo` (`step_pgids` and `downs` are
  only ever taken alone, and `wake` last, with nothing taken while it is held), and no thread waits on another while
  holding one. Nothing that may hold a lock writes stderr: event lines go to it through `Echo`'s thread.
- **Processes:** the app runs in its own process group, and stopping or crash handling signals the whole group
  (`process::kill_group`, rustix's `kill_process_group`), never just the child pid. Every child that must end by
  itself (install, hooks, git, cp, health commands) goes through `process::run_bounded`.
- **Storage hooks** run from the permanent generation's worktree (`repo` before the first), never from the
  generation being tested, switched to or rolled back. A rollback runs `restore` every time; only `rollback
  --keep-storage` gives it up.
- **The socket protocol** sends one JSON object per line. Errors carry a stable `code` (the `ErrorCode` enum) and a
  readable `message`. Agents depend on the code strings, so add variants instead of renaming them. The daemon and the
  CLI share the typed requests, records and results in `protocol.rs`; don't read the wire with `value["key"]`.
- **CLI exit codes** (`main.rs`): 0 ok, 1 failed, 2 usage, 3 daemon unreachable. Every command but `up`
  supports `--json`, and under it every failure, the client's own included, prints one error object.

## Releases

`DESIGN.md` → Releases is the contract for the archives and `install.sh`. Never push a tag or create a release: the
user does. Check `install.sh` against `scripts/package` output with `FAWKES_DOWNLOAD_URL=file://$PWD/dist` and a
`HOME` under `/tmp`, and the workflows with `actionlint`.

## Dependencies

The crate list is deliberate (`DESIGN.md` → Crates): serde, serde_json, toml, clap, rustix, signal-hook,
jiff and libc, strsim through clap's `suggestions` (the "similar command" tip, instead of a hand-written edit
distance), and tempfile for the tests. The HTTP health check and the thread-based concurrency are
hand-written on purpose, and errors stay `protocol::Error` and `String`, without anyhow or thiserror.

A well-known crate is preferred over hand-written code when it removes a meaningful amount of code or `unsafe`, but
ask before adding one (and never tokio: the threads are deliberate). Once one is agreed, add it to the Crates section
with what it replaces. Update a single dependency with `cargo update --precise`, never everything at once. After
any change to `Cargo.toml` or `Cargo.lock`, run `cargo deny check` (the lander runs it too): a crate under a
licence `deny.toml` does not allow fails it, so ask before adding that licence.

## Code style

- Follow the surrounding code. Modules open with a `//!` summary, and long files are split into sections with
  `// ------------------------------------------------ name` dividers.
- Comments explain *why*, invariants and locking, not what the next line does. Write them for a future reader:
  never mention the conversation, a review or a previous version ("now", "no longer", "fixed").
- Errors: use `protocol::err(ErrorCode::Busy, "message")` and `R<T>` in the daemon and `Result<_, String>` in the CLI.
  Write messages for an agent: say what is wrong and what to do next (e.g. point at `rollback`).
- Don't drop a fallible result with `let _ =` unless failure really doesn't matter (best-effort cleanup). If it's
  not obvious, say why in a short comment.
- `unsafe` is only for libc calls. Put a `// SAFETY:` comment on any `unsafe` block you add or touch.
- Prefer `#[expect(...)]` with a reason over `#[allow(...)]`, and delete dead code instead of silencing the lint.
- Imports go at the top of the file in the existing order: `crate::` first, then external crates, then `std`.
- Use full-word names, and keep a struct next to its `impl`.

## Commits

- The subject describes the behavior in plain language (`watch: name the app first`, `Automatic rollback needs
  only the live slot: …`). An optional `area:` prefix names the part touched (`watch:`, `feat:`).
- Commit only when asked. Don't commit scratch files such as `review*.md`.
- Keep trailers that are already present (e.g. `Ultimate-Conversation:`), and don't invent new ones.
