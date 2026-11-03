# Fawkes

A process supervisor for agents that upgrade themselves. The agent never moves its own live checkout: it asks
Fawkes, and every upgrade is a transaction that either becomes permanent or rolls back by itself.

```
stage → test → switch → confirm   (or automatic rollback)
```

## Why

A self-modifying agent (first user: [Ultimate](https://github.com/HazAT/ultimate)) updates the code it runs from.
When new code goes live before its dependencies, or a new core cannot boot, nothing outside the agent can bring the
last good version back. Process managers (launchd, supervisord, PM2) restart a crash but know no versions.

## Semantics (borrowed, not invented)

- **Generations**, after Erlang/OTP's release handler: a generation is *staged*, then *current*, then *permanent*. A
  cold start always runs the last *permanent* generation.
- **Mark good**, after RAUC/Mender A/B updates: the new generation must confirm itself healthy; failed starts are
  counted and fall back.
- **Confirm deadline**, after Junos `commit confirmed`: a switch that is not confirmed in time is reverted.
- A generation rolls back at most once: a broken rollback never loops.

## Shape

- **Generation:** a git worktree at a ref with its dependencies installed (copy-on-write clones on APFS keep it
  cheap), plus a storage snapshot taken at switch time (SQLite backup API, not a file copy).
- **Manifest** per app: start command, install command, health check (URL or command), storage paths to snapshot,
  spare port range for tests. App-agnostic; Ultimate is the first manifest.
- **API for agents:** a CLI with `--json` output and readable errors (`stage`, `test`, `switch`, `confirm`,
  `rollback`, `status`, `log`, `history`), and the same commands over a local Unix socket for in-app UIs and live
  progress. Signals only for stop and reload.

## Status

Prototype. Everything in [DESIGN.md](DESIGN.md) is built except its **Later** section: the transaction (`stage`,
`test`, `switch`, `confirm`, automatic and manual `rollback`), `start`/`stop`/`restart` with crash restarts and
backoff, `prune`, manifest reload on SIGHUP, and the `tui`. It is tested against a copy of an Ultimate fork, never a
live install.
