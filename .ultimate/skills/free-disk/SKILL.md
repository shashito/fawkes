---
name: free-disk
description: Free disk space taken by Fawkes work on this Mac: cargo-clean idle worktrees and lander checkouts and list stale /tmp test folders to remove. Use after every landing, when the disk runs low (under about 10 GB free, or a build or test fails with ENOSPC), or when asked to free space.
---

# Free disk

Disk is tight on this Mac: one Fawkes `target/` is about 2 GB, and the disk once fell under 1 GB free. Run this
after every landing, not only when space runs out. `free-disk.sh` sits next to this file, in the host project at
`~/Code/fawkes/.ultimate/skills/free-disk/free-disk.sh`.

## Steps

1. **Find the busy branches.** In codemode, list the conversations and keep the busy ones' working folders:

   ```js
   const busy = (await ultimate.conversations()).filter((c) => c.busy && c.cwd).map((c) => c.cwd)
   ```

   When a landing is queued or running (`(await ultimate.doc('ultimate.workspaces', 1)).queue` is not empty), add
   the lander's checkout, `~/.ultimate/projects/fawkes-*/lander/<hash>`, too.

2. **Clean.** Pass each of those folders as a `--skip`:

   ```sh
   sh ~/Code/fawkes/.ultimate/skills/free-disk/free-disk.sh --clean --skip <cwd> --skip <cwd>
   ```

   Without `--clean` it only reports. With it, it runs `cargo clean` in every Fawkes worktree and lander checkout
   that is not skipped and that no process is inside (as working directory, executable or open file), and prints
   what each freed and `df -h` afterwards. It exits 1 when a `cargo clean` failed, with cargo's message above.
   The process check is a snapshot: a build that starts in a checkout right after it can lose its target and
   fail, so the `--skip` list is what keeps busy branches safe.

3. **Remove the stale test folders it lists.** Under "Stale test folders" it prints each `/tmp/fk*` folder that
   nothing changed within a day and no process has open, one path per line. Remove each with its own plain
   command, the path written out, so the guard can read it:

   ```sh
   rm -rf /tmp/fk12345
   ```

   The script deletes nothing itself, so it never waits on a guard hold.

## Never

- `~/Code/fawkes/target`: worktree setup copies it into each new worktree. The script reports it and leaves it.
- `/tmp/fawkes-test-ports`: the test harness's port claims. Neither are other `/tmp/fawkes-*` folders removed.
- `~/.cache/gondolin`, the cargo registry (`~/.cargo/registry`), or anything outside these paths.
- A busy branch's worktree or its `/tmp` folders: skip it, even when the script would find it idle.

## Reading the result

The sizes come from `du`. A worktree's `target` starts as an APFS clone of `~/Code/fawkes/target` and shares its
blocks, so `df` before and after shows what was really freed. If little came free, conclude finished branches:
their worktrees go with them.
