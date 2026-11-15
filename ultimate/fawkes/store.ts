/**
 * What every host on this machine shares to do each after-upgrade task once, in `~/.ultimate/fawkes/`: claims made by
 * exclusive create, locks that a crashed host left behind (older than `STALE`) taken over, the watermark of the newest
 * reviewed Ultimate commit and the autofix records. Whichever host gets there first acts; the others see its file.
 */
import { execFile } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { open, readFile, realpath, rename, rm, stat, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { promisify } from 'node:util'
import type { Owner } from './tasks.ts'

const run = promisify(execFile)

/** A lock this old was left by a host that crashed holding it: no task holds one for minutes. */
export const STALE = 10 * 60_000

/** The folder every host shares. */
export const sharedFolder = (userHome: string) => join(userHome, '.ultimate', 'fawkes')

const missing = (error: unknown) => (error as NodeJS.ErrnoException).code === 'ENOENT'

/** Create `path` unless it exists: true for the one caller that made it. */
export async function claim(path: string) {
  try {
    const file = await open(path, 'wx')
    await file.writeFile(`${process.pid} ${new Date().toISOString()}\n`)
    await file.close()
    return true
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'EEXIST') return false
    throw error
  }
}

/** When `path` was last written, or undefined when it does not exist. */
const writtenAt = (path: string) =>
  stat(path).then(
    (info) => info.mtimeMs,
    (error: unknown) => {
      if (missing(error)) return undefined
      throw error
    },
  )

const stale = (since: number | undefined) => since !== undefined && Date.now() - since >= STALE

/**
 * Take the lock `path`: a claim, or the takeover of a stale one. A takeover holds `<path>.takeover` while it checks
 * the lock again and removes it, so of several hosts that found it stale only one removes it, never a lock another has
 * just taken over; the lock itself is still only ever taken by a claim. A takeover is a moment's work, so a stale
 * `.takeover` is a crashed host's and is removed for the next tick.
 */
export async function lock(path: string) {
  if (await claim(path)) return true
  const since = await writtenAt(path)
  // Released meanwhile.
  if (since === undefined) return claim(path)
  if (!stale(since)) return false
  const takeover = `${path}.takeover`
  if (!(await claim(takeover))) {
    if (stale(await writtenAt(takeover))) await rm(takeover, { force: true })
    return false
  }
  try {
    if (stale(await writtenAt(path))) await rm(path, { force: true })
    return await claim(path)
  } finally {
    await rm(takeover, { force: true })
  }
}

/** Run `work` under the lock `path`; false, without running it, while another host holds it. */
export async function withLock(path: string, work: () => Promise<void>) {
  if (!(await lock(path))) return false
  try {
    await work()
  } finally {
    await rm(path, { force: true })
  }
  return true
}

/** Write `text` to `path` whole or not at all: a temporary file renamed over it. */
export async function writeAtomic(path: string, text: string) {
  const temporary = `${path}.${process.pid}.${randomUUID()}.tmp`
  await writeFile(temporary, text)
  await rename(temporary, path)
}

/** The JSON in `path`, or undefined when there is no such file. */
export async function readJson<T>(path: string): Promise<T | undefined> {
  const text = await readFile(path, 'utf8').catch((error: unknown) => {
    if (missing(error)) return undefined
    throw error
  })
  return text === undefined ? undefined : (JSON.parse(text) as T)
}

// ------------------------------------------------ the review watermark

/** The newest reviewed Ultimate commit, from the watermark file; undefined before the first. */
export async function readWatermark(path: string) {
  const text = await readFile(path, 'utf8').catch((error: unknown) => {
    if (missing(error)) return undefined
    throw error
  })
  if (text === undefined) return undefined
  const commit = text.trim()
  if (!/^[0-9a-f]{7,64}$/.test(commit))
    throw new Error(`${path} holds no commit; remove it, and the next upgrade review starts over quietly`)
  return commit
}

/**
 * What the review of commit `to` is, given the watermark: `quiet` before the first (the caller sets the watermark),
 * `skip` when `to` brings nothing beyond it, else a `review` of `from..to`, which `advance`s the watermark only when it
 * is an ancestor of `to`. Every host's commits are in the clone's one object store, so `repo` is any of its worktrees.
 */
export async function reviewDecision(
  repo: string,
  watermark: string | undefined,
  to: string,
): Promise<
  { state: 'quiet' } | { state: 'skip' } | { state: 'review'; from: string; count: number; advance: boolean }
> {
  if (!watermark) return { state: 'quiet' }
  const { stdout } = await run('git', ['rev-list', '--count', '--end-of-options', `${watermark}..${to}`], {
    cwd: repo,
    timeout: 10_000,
  })
  const count = Number(stdout.trim())
  if (!count) return { state: 'skip' }
  const advance = await run('git', ['merge-base', '--is-ancestor', watermark, to], { cwd: repo, timeout: 10_000 }).then(
    () => true,
    (error: { code?: unknown }) => {
      // Exit 1 is git's "no"; anything else failed.
      if (error.code === 1) return false
      throw error
    },
  )
  return { state: 'review', from: watermark, count, advance }
}

// ------------------------------------------------ owners

/**
 * Who owns the folder at `path`: its real path, the top of its git repository, and the project whose root is that
 * repository. A folder in no repository, or in one no project has, has no project.
 */
export async function ownerOf(path: string, projects: { name: string; root?: string }[]): Promise<Owner> {
  const real = await realpath(path)
  const top = await run('git', ['rev-parse', '--show-toplevel'], { cwd: real, timeout: 10_000 }).then(
    ({ stdout }) => realpath(stdout.trim()),
    () => undefined,
  )
  if (!top) return { path: real }
  for (const project of projects) {
    const root = project.root && (await realpath(project.root).catch(() => undefined))
    if (root === top) return { path: real, repo: top, project: project.name }
  }
  return { path: real, repo: top }
}
