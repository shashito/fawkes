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

- Run the narrowest test first (`cargo test --test core <name>`, `cargo test tui::`), then the whole suite.
- Use debug builds. Don't build with `--release` unless asked.

## Tests

- Integration tests (`tests/core.rs`) run the real binary against a throwaway world built by `tests/common/mod.rs`.
  They need `git` and `node` on `PATH`, and the sample app is `examples/sample/app.js`.
- Each test claims a block of ten ports from `Harness::new(port, ..)` and a directory `/tmp/fk<port>`, which is kept
  short because socket paths have a length limit. Tests run in parallel, so **give a new test an unused block**
  (4800 to 4890 are taken; use 4900, 4910, …).
- Unit tests live next to the code in `#[cfg(test)] mod tests` (`state.rs`, `storage.rs`, `manifest.rs`,
  `process.rs`, `protocol.rs`, `daemon/mod.rs`, `tui.rs`).
- To fix a bug, first write a test that reproduces it.
- Never point a test or a manual run at a live app or a real `~/.fawkes`. Use a temp directory and a manifest in it.

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
  process stop). The lock order is `inner` → `log` → `hub`, and no thread waits on another while holding one.
- **Processes:** the app runs in its own process group, and stopping or crash handling signals the whole group
  (`process::kill_group`, rustix's `kill_process_group`), never just the child pid.
- **Storage snapshots** use the SQLite online backup API (`storage.rs`), never a file copy, because a copy can miss
  WAL contents.
- **The socket protocol** sends one JSON object per line. Errors carry a stable `code` (the `ErrorCode` enum) and a
  readable `message`. Agents depend on the code strings, so add variants instead of renaming them. The daemon, CLI and
  TUI share the typed requests, records and results in `protocol.rs`; don't read the wire with `value["key"]`.
- **CLI exit codes** (`main.rs`): 0 ok, 1 failed, 2 usage, 3 daemon unreachable. Every command supports `--json`.

## Dependencies

The crate list is deliberate (`DESIGN.md` → Crates): serde, serde_json, toml, rusqlite, rustix, signal-hook, libc and
ratatui. Argument parsing, the HTTP health check, UTC timestamps and the thread-based concurrency are hand-written on
purpose. thiserror is agreed but not used yet; it goes into the Crates section when it is. Don't add any other crate
(anyhow, clap, tokio, chrono, …) without asking. If one is agreed, add it to the Crates section
too. Update a single dependency with `cargo update --precise`, never everything at once.

## Code style

- Follow the surrounding code. Modules open with a `//!` summary, and long files are split into sections with
  `// ------------------------------------------------ name` dividers.
- Comments explain *why*, invariants and locking, not what the next line does. Write them for a future reader:
  never mention the conversation, a review or a previous version ("now", "no longer", "fixed").
- Errors: use `protocol::err(ErrorCode::Busy, "message")` and `R<T>` in the daemon and `Result<_, String>` in the CLI. Write
  messages for an agent: say what is wrong and what to do next (e.g. point at `rollback`).
- Don't drop a fallible result with `let _ =` unless failure really doesn't matter (best-effort cleanup). If it's
  not obvious, say why in a short comment.
- `unsafe` is only for libc calls. Put a `// SAFETY:` comment on any `unsafe` block you add or touch.
- Prefer `#[expect(...)]` with a reason over `#[allow(...)]`, and delete dead code instead of silencing the lint.
- Imports go at the top of the file in the existing order: `crate::` first, then external crates, then `std`.
- Use full-word names, and keep a struct next to its `impl`.
- TUI: use the named ANSI colors already in `tui/draw.rs` (`Color::Green`, `Cyan`, `Red`, `Gray`, `Reset`), not
  RGB or indexed colors, so it reads well in any terminal theme.

## Commits

- The subject describes the behavior in plain language (`tui: show local times`, `Automatic rollback needs only
  the live slot: …`). An optional `area:` prefix names the part touched (`tui:`, `feat:`).
- Commit only when asked. Don't commit scratch files such as `review*.md`.
- Keep trailers that are already present (e.g. `Ultimate-Conversation:`), and don't invent new ones.
