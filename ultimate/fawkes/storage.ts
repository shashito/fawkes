/**
 * Fawkes's storage hooks for Ultimate: `snapshot`, `restore` and `test-copy` of the two SQLite databases in the
 * project's home, `state.sqlite` and `trace.sqlite`. Run as `node storage.ts <command> <from> <to>`; exit 0 is
 * success, 1 a failed step (the message on stderr says which file and why), 2 a usage error.
 *
 * A database is copied with `VACUUM INTO`: one read transaction, so the copy is consistent and holds the rows that
 * are still only in the `-wal`, whether or not the app is running. A plain file copy of a live database loses them.
 */
import {
  chmodSync,
  closeSync,
  copyFileSync,
  fsyncSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeSync,
} from 'node:fs'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'

/** The storage of Ultimate's home; the index of a snapshot names nothing else. */
const FILES = ['state.sqlite', 'trace.sqlite']

/** A copy gives up on a locked database after this long (SQLite's busy timeout, in milliseconds). */
const BUSY_MS = 10_000

const USAGE = `usage: node storage.ts snapshot <home> <snapshot-dir>
       node storage.ts restore <snapshot-dir> <home>
       node storage.ts test-copy <home> <test-home>`

/** One file of a snapshot's `index.json`: whether it existed in the home, and the mode it had. */
type Entry = { name: string; present: boolean; mode: number }

/** What the index says, checked: a snapshot counts only through it. */
type Plan = Entry[]

// ------------------------------------------------ files

/** The file with its `-wal`, `-shm` and `-journal`: SQLite's companions, which belong to one database file only. */
const withCompanions = (path: string) => ['', '-wal', '-shm', '-journal'].map((suffix) => path + suffix)

/** Delete a file; one that is not there is fine. */
const remove = (path: string) => rmSync(path, { force: true })

const modeOf = (path: string) => statSync(path).mode & 0o777

/** Write a file durably: a temp file beside it, flushed, then renamed, so it is whole or not there. */
function writeAtomic(path: string, text: string) {
  const temp = `${path}.tmp`
  remove(temp)
  const fd = openSync(temp, 'w', 0o600)
  try {
    writeSync(fd, text)
    fsyncSync(fd)
  } finally {
    closeSync(fd)
  }
  renameSync(temp, path)
}

/**
 * Copy the database `source` to `target`: `VACUUM INTO` a temp file beside the target, give it the mode of the
 * source (a read-only database stays read-only, and a restore gives the mode back), then rename it into place. An
 * earlier failed run's temp is removed first, since `VACUUM INTO` refuses a file that exists.
 */
function copyDatabase(source: string, target: string) {
  const temp = `${target}.tmp`
  const mode = modeOf(source)
  for (const leftover of withCompanions(temp)) remove(leftover)
  try {
    const db = new DatabaseSync(source)
    try {
      db.exec(`PRAGMA busy_timeout = ${BUSY_MS}`)
      db.exec(`VACUUM INTO '${temp.replaceAll("'", "''")}'`)
    } finally {
      db.close()
    }
    chmodSync(temp, mode)
    renameSync(temp, target)
  } catch (error) {
    for (const leftover of withCompanions(temp)) remove(leftover)
    throw new Error(`cannot copy ${source} to ${target}: ${message(error)}`)
  }
}

const message = (error: unknown) => (error instanceof Error ? error.message : String(error))

// ------------------------------------------------ commands

/** Copy each database present in `home` into `dir`, then write the index: last, so the snapshot counts once whole. */
function snapshot(home: string, dir: string) {
  const index = join(dir, 'index.json')
  remove(index)
  const entries: Entry[] = FILES.map((name) => {
    const source = join(home, name)
    const present = pathExists(source)
    if (present) copyDatabase(source, join(dir, name))
    else for (const stale of withCompanions(join(dir, name))) remove(stale)
    return { name, present, mode: present ? modeOf(join(dir, name)) : 0 }
  })
  writeAtomic(index, JSON.stringify({ files: entries }, null, 2))
}

/** Read and check the whole snapshot, so that a restore touches nothing unless it can finish. */
function plan(dir: string): Plan {
  const index = join(dir, 'index.json')
  let parsed: unknown
  try {
    parsed = JSON.parse(readFileSync(index, 'utf8'))
  } catch (error) {
    throw new Error(`no usable snapshot in ${dir}: ${index}: ${message(error)}; the storage was left as it is`)
  }
  const corrupt = (why: string) =>
    new Error(`corrupt snapshot index ${index}: ${why}; the storage was left as it is`)
  const files = (parsed as { files?: unknown } | null)?.files
  if (!Array.isArray(files)) throw corrupt('no "files" list')
  const entries: Entry[] = []
  for (const item of files) {
    const { name, present, mode } = (item ?? {}) as Record<string, unknown>
    if (typeof name !== 'string' || !FILES.includes(name)) {
      throw corrupt(`${JSON.stringify(name)} is not one of ${FILES.join(', ')}`)
    }
    if (typeof present !== 'boolean') throw corrupt(`"present" of ${name} is not true or false`)
    if (present && (!Number.isInteger(mode) || (mode as number) < 0 || (mode as number) > 0o777)) {
      throw corrupt(`"mode" of ${name} is not a file mode`)
    }
    if (present && !isFile(join(dir, name))) {
      throw new Error(`snapshot ${dir} is incomplete: ${name} is missing; the storage was left as it is`)
    }
    entries.push({ name, present, mode: present ? (mode as number) : 0 })
  }
  return entries
}

/**
 * Put the databases of `home` back as the snapshot found them. Each file is whole before the live one is touched
 * (a temp copy beside it, then a rename), and the whole restore can run again after a failure.
 */
function restore(dir: string, home: string) {
  const entries = plan(dir)
  for (const { name, present, mode } of entries) {
    const live = join(home, name)
    if (!present) {
      for (const path of withCompanions(live)) remove(path)
      continue
    }
    const temp = `${live}.tmp`
    for (const leftover of withCompanions(temp)) remove(leftover)
    try {
      // The snapshot file is a finished VACUUM INTO copy with no companions: a file copy of it is consistent.
      copyFileSync(join(dir, name), temp)
      chmodSync(temp, mode)
      const fd = openSync(temp, 'r')
      try {
        fsyncSync(fd)
      } finally {
        closeSync(fd)
      }
      // The failed generation's log would replay onto the restored pages.
      for (const stale of withCompanions(live).slice(1)) remove(stale)
      renameSync(temp, live)
    } catch (error) {
      remove(temp)
      throw new Error(`cannot restore ${live} from ${join(dir, name)}: ${message(error)}`)
    }
  }
}

/** Copy each database present in `home` to `dest`, while the app may be writing. */
function testCopy(home: string, dest: string) {
  for (const name of FILES) {
    if (pathExists(join(home, name))) copyDatabase(join(home, name), join(dest, name))
  }
}

// ------------------------------------------------ main

function pathExists(path: string) {
  try {
    statSync(path)
    return true
  } catch {
    return false
  }
}

const isFile = (path: string) => {
  try {
    return statSync(path).isFile()
  } catch {
    return false
  }
}

const [command, from, to, ...rest] = process.argv.slice(2)
const commands: Record<string, (from: string, to: string) => void> = {
  snapshot,
  restore,
  'test-copy': testCopy,
}
const run = command !== undefined && Object.hasOwn(commands, command) ? commands[command] : undefined
if (!run || !from || !to || rest.length > 0) {
  console.error(USAGE)
  process.exit(2)
}
try {
  run(from, to)
} catch (error) {
  console.error(`storage ${command}: ${message(error)}`)
  process.exit(1)
}
