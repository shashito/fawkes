# PLAN-04 phase 10 (quality and /simplify), reassessed at `62c49d0`

**Verdict:** phase 10 still has work in it, but about a third of its items are gone: the TUI and `storage.rs` went,
and phases 6 to 8 and PLAN-05 did others. What is left is smaller than the phase was. The non-TUI code also grew by
70% since review 4, so the best value now is a short phase of guards and dedup, followed by one fresh `/simplify`
pass on the new code.

Sources: [04-review.md](04-review.md) (written at `abe54f0`), [PLAN-04.md](PLAN-04.md) phase 10,
[PLAN-05.md](PLAN-05.md), `DESIGN.md`, `CLAUDE.md`, and the code at `62c49d0`. Every `file:line` below is from
`62c49d0`. `ultimate/` is out of scope.

## 1. Measurements

### Lines in `src/`

"Code" means the lines before a file's `mod tests`, doc comments included.

| File | Total | Code | Code at `abe54f0` |
|---|---:|---:|---:|
| `daemon/transactions.rs` | 1329 | 1329 (no tests of its own) | 985 |
| `main.rs` | 1830 | 1189 | 499 |
| `daemon/mod.rs` | 1662 | 1066 | 813 |
| `protocol.rs` | 1605 | 1065 | 694 |
| `manifest.rs` | 1265 | 821 | 439 |
| `daemon/supervisor.rs` | 1151 | 737 | 553 |
| `state.rs` | 1212 | 710 | 615 |
| `watch.rs` | 983 | 558 | (new) |
| `process.rs` | 910 | 532 | 381 |
| `service.rs` | 440 | 423 | (new) |
| `daemon/server.rs` | 409 | 306 | 180 |
| `discovery.rs` | 431 | 238 | (new) |
| `daemon/lifecycle.rs` | 120 | 120 | 90 |
| **Total** | **13347** | **9094** | 7066, of which ~1720 TUI and 98 `storage.rs` |

- Without the TUI and `storage.rs`, code went from about 5250 lines to 9094 lines (+73%).
- `tests/` adds 4089 lines (`core.rs` 3497, `common/mod.rs` 592).
- There are 162 `#[test]` functions: 103 unit and 59 core.

### Longest functions

Measured by clippy `too_many_lines` with the threshold lowered to 50. The project threshold is 150 (`clippy.toml`).

| Lines | Function |
|---:|---|
| 149 | `Daemon::test`, `daemon/transactions.rs:552` (one line under the threshold) |
| 131 | `main`, `main.rs:327` |
| 126 | `Daemon::run`, `daemon/mod.rs:441` |
| 119 | `Daemon::stage`, `daemon/transactions.rs:156` |
| 114 | `Daemon::switch`, `daemon/transactions.rs:710` |
| 105 | `Daemon::rollback_pending`, `daemon/transactions.rs:980` |
| 99 | `Service::install`, `service.rs:107` |
| 98 | `render_result`, `main.rs:1087` |
| 97 | `Daemon::start_live`, `daemon/supervisor.rs:362` |
| 82 | `exchange`, `main.rs:850`; `Daemon::build`, `daemon/transactions.rs:286` |
| 77 / 75 | `Written::read`, `manifest.rs:222`; `Written::problems`, `manifest.rs:675` |

The comment in `clippy.toml` still says "the longest is 138 lines". The longest is now 149.

### Clippy

- **pedantic:** 0. It is on in `Cargo.toml` and kept clean by the lander.
- **nursery** (`cargo clippy --all-targets -- -W clippy::nursery`): 280 hits.

  | Lint | Hits |
  |---|---:|
  | `use_self` | 213 |
  | `missing_const_for_fn` | 33 |
  | `significant_drop_tightening` | 13 |
  | `option_if_let_else` | 7 |
  | `or_fun_call` | 5 |
  | `derive_partial_eq_without_eq` | 2 |
  | `suboptimal_flops` | 2 |
  | `literal_string_with_formatting_args` | 1 |
  | `iter_with_drain` | 1 |
  | `iter_on_single_items` | 1 |
  | `needless_pass_by_ref_mut` | 1 |
  | `collection_is_never_read` | 1 |

  I15's advice still holds: adopt `or_fun_call`, `derive_partial_eq_without_eq` and
  `literal_string_with_formatting_args` (8 fixes). Never enable `significant_drop_tightening`: it nudges lock
  scopes. `use_self` contradicts the code's style (`-> Manifest`). `missing_const_for_fn` buys nothing in a binary.
  The two `suboptimal_flops` go away with item 2 below.
- **`unreachable_pub`** (rustc): about 300 hits. This is the visibility half of L35. It is churn without value in a
  binary, so don't adopt it.

## 2. Status of every phase 10 item

Phase 10 names L54, L56, L55, P9, L49, L33, L34, L57–L61, I23–I26, I15 (through P1–P24) and "the rest of P1–P24".
It points to L1–L3, L9–L12 and I1–I3. L35 and L53 come from the same section of the review.

| Item | Status | Evidence at `62c49d0` |
|---|---|---|
| **L54** one "has the app exited" accessor | **open** | Five production sites, each hand-written: `daemon/mod.rs:165`, `daemon/supervisor.rs:207`, `:291`, `:512`, `daemon/transactions.rs:905`. An `Err` from `try_wait` counts as "still running" at four of them, but as "exited" at `supervisor.rs:512` (`!matches!(…, Ok(None))`). |
| **L56** one parsed deadline, one seconds format | **changed, smaller** | The CLI and TUI parse sites are gone. The daemon parses at `supervisor.rs:211`, `:231` and `daemon/mod.rs:928`; R14 refuted merging these, since `due` must stay silent. `watch.rs:469` and `:535` are client-side. The seconds format is still open: `{:.1}s` (`supervisor.rs:325`), `{}s` (`supervisor.rs:342`, `transactions.rs:690`, `process.rs:104`, `:424`, `main.rs:1116`) and `{} s` (`server.rs:125`, `watch.rs:539`). |
| **L55** one exit description | **changed, still open** | The TUI is gone, but three spellings remain: `exit status 3` (`protocol.rs:737`, in events and the confirm refusal at `transactions.rs:909`), `exit 3` (`main.rs:1000-1004`), and std's `exit status: 3` (`transactions.rs:426`, `service.rs` `launchctl_failed`). `watch.rs:303` imports `crate::render_event` from the binary root. |
| **P9** manifest fields declared twice | **open, grown** | `Written` (`manifest.rs:200-218`), `Manifest` (`:66-89`), `Written::read` (`:222`), the copy in `load` (`:560-585`), and `reload`'s two lists (`:599-650`), which use no exhaustive destructure. A new field now means five edits and DESIGN.md. |
| **L49** toml features | **open, one line** | `toml = "1.1.8"` brings in `display` and so `toml_writer` (`Cargo.lock:438`). Fawkes only parses (`manifest.rs:515`, `discovery.rs:198`). |
| **L33** f64 durations | **open** | Eight production `Duration::from_secs_f64` sites: `process.rs:363`, `:411`, `:428`; `daemon/mod.rs:735`; `transactions.rs:316`, `:373` (the same value twice in `build`), `:465`; `service.rs:258`. They are safe only because of `MAX_SECONDS` (`manifest.rs:346-348`). `Manifest::reload` needs `#[expect(clippy::float_cmp)]` (`:591`) because of them. |
| **L34** `let _ =` that matters | **mostly done** | The `storage.rs` case is gone, and almost every `let _ =` carries a why. One is left: `point_current_link` (`supervisor.rs:547-549`) drops a failed symlink and rename silently. |
| **L57** named literals | **mostly done** | Phase 6 named most of them (`REQUEST_TIMEOUT`, `MAX_CONNECTIONS`, `HTTP_TIMEOUT`, `KILL_WAIT`, …). Still inline: the socket path limit 100 (`server.rs:43`), the 500 ms close check (`server.rs:277`), the 100 ms accept retry (`daemon/mod.rs:635`), the 1 s prune retry (`transactions.rs:1222`), 20 ms (`process.rs:396`), and the log default 20 (`server.rs:246`, repeated in `main.rs:198` and `protocol.rs:201`). New: two constants named `REQUEST_TIMEOUT` mean different things: the client's 30 s (`protocol.rs:23`) and the daemon's 5 s read deadline (`server.rs:74`). |
| **L58** stale comments | **mostly done/moot** | The TUI comments are gone, and the `core.rs` pre-connect comment was fixed in phase 8.3. One is left: `phase_two_state` and `a_phase_two_state_json_loads_…` (`state.rs:892-932`) name a development phase. |
| **L59** doc and CLAUDE.md drift | **mostly moot** | The `draw.rs` and `tui.rs` items are gone. Left: the `clippy.toml` comment (138 vs 149 lines), and CLAUDE.md's lock order, which omits `wake` (`daemon/mod.rs:5-6` has it). |
| **L60** dead `(requested)` guard | **moot** | `event_color` went with the TUI. |
| **L61** abbreviated names | **open, smaller** | The TUI names are gone. Left: `Why` (`daemon/mod.rs:80`), two `Msg` enums (`watch.rs:45`, `daemon/mod.rs:241`), `Txn`, and `m` for the manifest at about 48 sites (`transactions.rs` 34, `supervisor.rs` 13). |
| **I23** transactions.rs layout | **open, grown** | 1329 lines with no tests of their own. Git (`:131-153`), the step and hook runner (`:386-500`), pruning and the sweep (`:1151-1293`) and `port_free`/`clone_directory` (`:1305-1329`) all sit beside switch, confirm and rollback. The verifier called this an opinion. |
| **I24** long functions | **open, grown** | See the table above: `test` 149 (it was about 118), `stage` 119, `switch` 114. `clippy.toml` documents top-to-bottom steps as deliberate. |
| **I25** CLI and TUI client loops | **moot as written, changed** | The TUI is gone, but there are now three stream readers: `exchange` (`main.rs:850-948`), `Reader::follow` (`watch.rs:201-258`) and `Waiter::follow` (`watch.rs:411-449`). All three parse a line twice, first to `Value` and then to `Line<Value>` (`main.rs:889-893`, `watch.rs:220-225`, `:432-434`). |
| **I26** adding a socket command | **open** | `parse` still matches strings with a catch-all (`protocol.rs:230-262`). The round-trip test covers 3 of 16 requests (`protocol.rs:1385-1394`). |
| **I15** three nursery lints | **open** | 8 hits, listed in §1. |
| **P1** can_become | **changed, small** | Phase 11's `check_test` and `check_switch` (`transactions.rs:49-74`) repeat find + `allowed` + `refusal` beside `State::transition` (`state.rs:426-430`). Two copies. |
| **P2** one bounded-child runner | **done** | PLAN-05 phase 10.5 (`1e89746`): `process::shell` (`process.rs:40`) and `run_bounded` (`:82`). |
| **P3** pid file reader | **moot** | One production reader remains (`process.rs:294-298`). |
| **P4** test polling loops | **open, test-only** | `daemon/mod.rs:1202-1205`, `:1540-1543`. |
| **P5** `Event::new` | **open** | Event literals at `daemon/mod.rs:857` and `:970` (and the `state.rs:734` test). |
| **P6** `Generation::staging` | **open, grown** | `transactions.rs:231-246`, plus five test literals: `state.rs:714`, `:1050`, `daemon/mod.rs:1475`, `:1569`, `supervisor.rs:968`. |
| **P7** limit as usize | **open** | `server.rs:247`, `:259`, `transactions.rs:1162`. |
| **P8** argument helpers | **done** | `05943c6`: `optional_arg`, `optional_count` and `optional_bool` (`protocol.rs:360-385`). |
| **P9** | see above | |
| **P10** `Pending` = `SwitchResult` | **open** | Identical structs at `protocol.rs:517` and `:877`. |
| **P11** one `--json` event path | **done** | `7d63e9a`/`3f417ae`: one `Line<Value>` path (`main.rs:888-911`). |
| **P12** port-reload rule split | **open (info)** | `daemon/mod.rs:690-704` merges error strings. |
| **P13** `state.json` `daemon_pid` | **open** | Written at `daemon/mod.rs:548` and `:751` and never read. `status.daemon_pid` comes from `std::process::id()` (`supervisor.rs:614`). It costs two durable saves per daemon life. |
| **P14** shared rollback ending | **mostly done** | `left_to_next_start` is shared. A six-line tail is still repeated (`transactions.rs:1090-1100`, `:1137-1146`). `lifecycle.rs` is small now. |
| **P15** a save on every start | **open** | `supervisor.rs:419-425`. |
| **P16** `worktree prune` per generation | **open** | `transactions.rs:544`, and again at `:1286`. |
| **P17** snapshot per subscriber | **open** | `server.rs:296-301` takes `inner` twice. |
| **P18** prune polls 1 s | **open** | `transactions.rs:1214-1229`. |
| **P19** startup reads 20 MiB | **open (info)** | `state.rs:607-620`. |
| **P20** waitid thread per wait | **open** | `process.rs:168`. |
| **P21** leftovers stopped in sequence | **open, grown** | Three groups now (app, step, hook) are stopped one after another (`daemon/mod.rs:522-542`): up to 3 × (`stop_timeout` + 2 s) after a crash. |
| **P22** `take_running` returns what it took | **open (trade-off)** | `switch` peeks at `live` first (`transactions.rs:740-746`). |
| **P23** wire names twice | **changed** | The test half is done: the phase 8.8 macro makes the variant lists exhaustive (`protocol.rs:1078`). Five enums still keep `rename_all` plus `as_str` (`protocol.rs:96`, `:437`, `:544`, `:583`, `:761`). The verifier doubted that a macro would be simpler. |
| **P24** stream's `shutting_down` poll | **open** | `server.rs:276`. |
| **L1** test copy races restore | **moot** | Storage is the app's hooks now. DESIGN.md:478-479 states that `test_copy` runs while the live app writes. |
| **L2** `exists()` as absent | **moot** | `storage.rs` is gone. |
| **L3** non-UTF-8 event log | **open, a bug** | `state.rs:609`: `read_to_string(…).unwrap_or_default()` drops a whole file for one bad byte. `seq` then starts over, which breaks `watch --since` and `wait`'s resume. |
| **L9** leaderless group, stale `app.pid` | **partly open** | Startup removes `step.pid` and `hook.pid` but never `app.pid` (`daemon/mod.rs:543-546`). The leaderless branch (`process.rs:299-300`) is unchanged. |
| **L10** git/cp untracked | **partly done** | The install and the hooks join `step_pgids` (`transactions.rs:410-420`). `git` and `cp` (`:147`, `:1324`) do not. |
| **L11** spawn after the drain | **open** | `spawn_step` (`transactions.rs:503-514`) pushes without checking `shutting_down`. The drain is at `daemon/mod.rs:748`. |
| **L12** run-link check-then-act | **open, narrower** | `server.rs:27-37`. Names in `FAWKES_HOME` now match file names (`50c437e`). |
| **I1** restore fsync | **moot** | The restore is the app's hook. |
| **I2** `Out::closed` | **open** | It peeks one byte (`protocol.rs:1036-1048`). |
| **I3** small style items | **not re-traced** | Its lines all moved. The fresh look below covers the same ground. |
| **L35** lock helper | **moot** | Its TUI copy is gone. Its visibility half is `unreachable_pub`, which §1 advises against. |
| **L53** state.rs five modules, cycle | **open** | `protocol.rs:6` imports `state::now`, `state.rs:3` imports `protocol`, and `watch.rs:12` takes the clock from `state`. |
| **I12** byte-sliced commit | **open** | `transactions.rs:312` instead of `Generation::short_commit`. |

Tally:

- 7 done: P2, P8, P11, and mostly L34, L57, L58 and P14.
- 10 moot: L1, L2, I1, L60, L35, P3, and largely I25 and L59.
- 6 changed: L55, L56, I25, P1, P23 and L61.
- The rest are still open, most of them small.

## 3. A fresh look at today's code

These problems were not in review 4, mostly in code added since:

- **F1. `main()` is a 131-line chain of early returns** (`main.rs:327-461`).
  - It keeps two hand-written lists of "the commands returned above" (`main.rs:240-241`, `:459`).
  - It has an unreachable "this command sends no request" error (`:450-454`).
  - It carries both test-run refusals inline (`:385-395`, `:418-447`).
  - One `match cli.command` over `run_*` functions would replace this. The confirm refusal would become a pure
    `fn test_run_refusal(…) -> Option<&str>` that can be unit-tested.
- **F2. The same "result as JSON or text on stdout, exit 0; otherwise report the failure" tail is written five
  times:** `client`, `run_up` (`main.rs:756-767`), `run_wait` (`:830-848`), `run_service` (`:606-617`) and
  `run_watch`. One helper would do.
- **F3. The rendering code lives in the binary root, and three readers decode each line twice.** `render_*` sits in
  `main.rs:950-1189`, which `watch.rs` imports. A `render.rs` (with L55's one exit description) shrinks `main.rs`
  by about 240 lines. One `protocol::read_line(&str) -> Option<(Value, Line<Value>)>` serves the three readers.
- **F4. The daemon's shutdown bound is written twice.** `2.0 * stop_timeout + 5.0` (`daemon/mod.rs:735`) is
  mirrored by `3.0 * stop_timeout + 5.0` in launchd's `ExitTimeOut` (`service.rs:258`), linked only by a comment.
  If one changes, launchd SIGKILLs a daemon mid-shutdown. One `Manifest` method used by both removes the risk.
- **F5. Compatibility shims in a single-user, pre-1.0 tool:**
  - the hidden `fawkes tui` tombstone (`main.rs:236`, `:352-360`, DESIGN.md:538);
  - the pre-`txn` `state.json` migration (`state.rs:169-217`, with its test at `:892-990`);
  - the `storage` key's own message (`manifest.rs:217`, `:276`, `:756`). An unknown key is refused anyway, so only
    the wording changes.

  Deleting the migration is the one change on this list that touches the promise. Without it, an old file whose
  `pending` switch is ignored would load as "no switch". So it could only become a refusal.
- **F6. Duration and seconds handling repeated across sites:** `Duration::from_secs_f64(m.install_timeout)` twice
  in one function (`transactions.rs:316`, `:373`), and `stop_group`, `run_bounded` and `wait_healthy` take `f64`
  seconds. L33 covers all of it.
- **Clean:** `discovery.rs` is pure and well tested. `watch.rs`'s `decide` is a pure function with table tests.
  The hook runner (`run_step`/`run_hook`) is one path. The refusals give an actionable next step everywhere I read.

## 4. Recommended: a cut-down quality phase

The items are ranked by value over risk to the transactional core. Messages may get clearer, but error codes, exit
codes, event kinds and fields, and the wire format stay unchanged. None of the items changes DESIGN.md's contract.

| # | Item | What it removes or simplifies | Risk to the core |
|---|---|---|---|
| 1 | **P9 guard:** exhaustive destructure of both manifests in `reload` | A field left out of `reload` becomes a compile error, so it can no longer change silently on SIGHUP. About +15 lines. | None |
| 2 | **L33 + F4:** `Manifest` keeps `Duration`s (validated once in `load`); `stop_group`, `run_bounded` and `wait_healthy` take `Duration`; one `shutdown_bound()` for the daemon and launchd | 8 conversion sites, the `float_cmp` expect, 2 nursery hits, and the duplicated formula. About −20 net. | Low-medium: the stop and health paths change signature, not behaviour. The core tests cover them. |
| 3 | **L54:** `App::exit_status()` / `has_exited()`, with the `Err` case decided once | Five hand-written checks become one, with a comment. | Low-medium: these are the crash, confirm and rollback decision sites. Only `judge_unhealthy`'s impossible `Err` case changes. |
| 4 | **L55 + F3:** one exit description; `render.rs` out of `main.rs`; `protocol::read_line` | Three exit spellings become one, `watch` no longer imports from the binary root, about −20 net, and `main.rs` drops to about 950 lines. | None: client code and event text only (facts unchanged). |
| 5 | **F1 + F2:** `main` as one `match`, a pure `test_run_refusal`, one result/failure tail | About −50 lines, two hand-kept comment lists, and one unreachable error. The refusal becomes unit-testable. | Low: client only, and the test-run refusals have core tests. |
| 6 | **Small batch:** P10, P13, P5, P6, P7, I12, L49, I15's three lints, the L57 leftovers (renaming the daemon's `REQUEST_TIMEOUT` to `REQUEST_DEADLINE`), L34 (`point_current_link` logs an `error` event), L58/L59 (`phase_two` names, `clippy.toml`, CLAUDE.md lock order) | About −80 lines, mostly test literals and duplicate types, plus two durable saves fewer per daemon life (P13). | None to low. P13 drops an unread field from `state.json`, and old files still load. |
| 7 | **L3:** read the event log as bytes, skipping bad lines (with a reproducing test first), plus L9's one line: remove a stale `app.pid` at startup | A real bug: one bad byte resets `seq` and breaks `watch --since` and `wait`. | Low: startup only. |

**Size:** medium. It is about +350/−500 lines, roughly −150 net, across `main.rs`, `manifest.rs`, `process.rs`,
`protocol.rs`, `daemon/*` and `state.rs`, with a new `render.rs`. It is one branch, with items 1–3 committed first,
since they touch the core.

**Left out on purpose:**

- **I23 and I24 (file split, function splits):** the verifier called them opinion, and `clippy.toml` documents
  the choice. They mean churn in promise-critical code.
- **P22 and P23:** trade-offs, not wins.
- **The efficiency items P15–P21 and P24:** no cost was measured. P21 is the only one users would notice, as a
  slower recovery after a crash.
- **L53:** the `time.rs` split is cheap, but the module cycle is harmless.
- **L10, L11, L12 and I2:** hardening, not quality. They stay on the record for a later phase.

## 5. Smaller alternative: only the guards

Items 1, 3 and 6, without the `render.rs` move and without P6's test literals. That is about a day and about −40
lines. Every future manifest field and every exited check becomes compiler- or helper-checked, the lints and the
stale text are fixed, and nothing in the CLI is restructured. It gives up L33's `Duration` cleanup (and F4's
launchd coupling), the `main.rs` shrink, and the L3 bug fix.

## 6. `/simplify` again?

**Yes, once, after this phase lands and before the next feature (PLAN-05 phase 13 or Linux service units).**

- The last pass (#9933: `be108cf`, `2d5a75a`, `801605f`, 2026-11-04, net −95 lines over 15 files) ran *before*
  review 4.
- About 3,800 lines of non-TUI code came after it: `discovery.rs`, `watch.rs`, `service.rs`, the hooks, `up`,
  `wait`, the event fields and the refusals. `tests/core.rs` grew to 3,497 lines.
- Running it after the phase means it doesn't rediscover the list above, and the diff it reviews is new code only.
- **Shape:** Opus, apply mode as before, scoped to `abe54f0..main` in `src/` and `tests/`.
  - **Rule:** messages may get clearer, but error codes, exit codes, events and the wire stay unchanged.
  - **Off limits:** the switch, confirm and rollback bodies.
  - **Order:** tests first (`core.rs` has the most new duplication), then `main.rs`, `watch.rs` and `manifest.rs`.
  - **Review:** one reviewer on the result.
- Running it *instead of* the phase, with this file as its brief, is cheaper. But it would leave the core-touching
  items 2–3 to a pass that is told to keep away from the core.

## 7. Decisions

1. **Scope:**
   - (a) the cut-down phase, items 1–7;
   - (b) the guards-only alternative;
   - (c) close phase 10 without it and run only `/simplify`.
2. **`/simplify`:**
   - (a) once on Opus, after the phase lands, scoped to `abe54f0..main`;
   - (b) now, instead of the phase, with this file as its brief;
   - (c) not before the next feature.
3. **Compatibility shims:**
   - (a) drop the hidden `fawkes tui` tombstone, and keep the pre-`txn` `state.json` migration and the `storage`
     message;
   - (b) drop all three, with a pre-`txn` `state.json` refused rather than ignored;
   - (c) keep all three.
4. **Open correctness items found here:**
   - (a) L3 and the stale `app.pid` in this phase, each with a reproducing test; L10, L11, L12 and I2 stay on the
     record;
   - (b) all of them in a separate hardening phase;
   - (c) none.
5. **Long functions (I24) and the transactions.rs split (I23):**
   - (a) leave them as documented, and correct `clippy.toml`'s comment (149 lines);
   - (b) split `test` and `switch` along their seams, and lower the threshold.

My picks: 1a 2a 3a 4a 5a. @ultimate's picks: 1a 2a 3c 4a 5a.

## 8. The user's picks

**1a 2a 4a 5a**, and for 3 a clean cut, since nobody runs Fawkes yet:

- The phase is items 1–7 of §4, plus the clean cut below.
- `/simplify` runs once on Opus after the phase lands, scoped to `abe54f0..main`.
- L3 and the stale `app.pid` are fixed in this phase, each with a reproducing test first. L10, L11, L12 and I2
  stay on the record.
- I23 and I24 stay as they are; only `clippy.toml`'s comment is corrected.
- **Clean cut (3):** all three compatibility paths go outright.
  - The hidden `fawkes tui` goes, with DESIGN.md's tombstone paragraph.
  - The `storage` key's own message goes; it is refused as any unknown key.
  - The pre-`txn` `state.json` migration (`StateFile`) goes, with its test.

  No `state.json` in the old shape exists, so nothing needs a refusal in its place. DESIGN.md's "Upgrading to
  this contract" note (hooks) addresses the same nonexistent installs. The build branch should ask before cutting
  it as well.
