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

/** Create `path`, holding `text`, unless it exists: true for the one caller that made it. */
export async function claim(path: string, text = `${process.pid} ${new Date().toISOString()}\n`) {
  try {
    const file = await open(path, 'wx')
    await file.writeFile(text)
    await file.close()
    return true
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'EEXIST') return false
    throw error
  }
}

/**
 * When the tasks first ran on this machine: the time of `installed` in `folder`, made by whichever host got there
 * first. Upgrades and failures before it are the state the extension found, so none of them is a task.
 */
export async function installedAt(folder: string) {
  const path = join(folder, 'installed')
  await claim(path)
  return (await stat(path)).mtimeMs
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
 * `.takeover` is a crashed host's and is removed for the next tick. Returns the token the lock holds, which `unlock`
 * needs, or undefined while another host holds it.
 */
export async function lock(path: string) {
  const token = `${process.pid} ${randomUUID()}\n`
  const take = async () => ((await claim(path, token)) ? token : undefined)
  const taken = await take()
  if (taken) return taken
  const since = await writtenAt(path)
  // Released meanwhile.
  if (since === undefined) return take()
  if (!stale(since)) return undefined
  const takeover = `${path}.takeover`
  if (!(await claim(takeover))) {
    if (stale(await writtenAt(takeover))) await rm(takeover, { force: true })
    return undefined
  }
  try {
    if (stale(await writtenAt(path))) await rm(path, { force: true })
    return await take()
  } finally {
    await rm(takeover, { force: true })
  }
}

/**
 * Release the lock `path` if it still holds `token`. A holder slower than `STALE` (a Mac that slept mid-delivery) may
 * have lost it to a takeover, and must not remove the lock the other host now holds.
 */
export async function unlock(path: string, token: string) {
  const held = await readFile(path, 'utf8').catch((error: unknown) => {
    if (missing(error)) return undefined
    throw error
  })
  if (held === token) await rm(path, { force: true })
}

/** Run `work` under the lock `path`; false, without running it, while another host holds it. */
export async function withLock(path: string, work: () => Promise<void>) {
  const token = await lock(path)
  if (!token) return false
  try {
    await work()
  } finally {
    await unlock(path, token)
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

// ------------------------------------------------ the reviewed commits

/** How many reviewed tips are kept: a branch that never lands stays one until newer ones push it out. */
export const TIPS = 20

/** The reviewed tips, newest first, one commit a line; undefined before the first review. */
export async function readReviewed(path: string) {
  const text = await readFile(path, 'utf8').catch((error: unknown) => {
    if (missing(error)) return undefined
    throw error
  })
  if (text === undefined) return undefined
  const tips = text.split('\n').filter((line) => line.trim())
  if (!tips.length || !tips.every((tip) => /^[0-9a-f]{7,64}$/.test(tip)))
    throw new Error(`${path} holds no commits; remove it, and the next upgrade review starts over quietly`)
  return tips
}

const git = (repo: string, args: string[]) => run('git', args, { cwd: repo, timeout: 10_000 })

/** Whether `ancestor` is an ancestor of (or is) `commit`. */
const isAncestor = (repo: string, ancestor: string, commit: string) =>
  git(repo, ['merge-base', '--is-ancestor', ancestor, commit]).then(
    () => true,
    (error: { code?: unknown }) => {
      // Exit 1 is git's "no"; anything else failed.
      if (error.code === 1) return false
      throw error
    },
  )

/**
 * What the review of commit `to` is, given the reviewed tips: `skip` when every commit `to` has is reviewed, else a
 * `review` of the `count` commits in `to` and in none of the
 * tips (`git log <to> ^<tip>…`). A set of tips rather than one commit: a reviewed branch that is never merged as it is
 * (the lander rebases) would otherwise hold the mark, and every later review repeat the main line since. `from` is
 * where the newest tip left the history of `to`, the base of the review's diff; `tips` the list to record once the
 * review is delivered: `to` first, then the tips that are not in it, at most `TIPS`. Every host's commits are in the
 * clone's one object store, so `repo` is any of its worktrees.
 */
export async function reviewDecision(
  repo: string,
  reviewed: string[],
  to: string,
): Promise<{ state: 'skip' } | { state: 'review'; from: string; count: number; tips: string[] }> {
  const excluded = reviewed.map((tip) => `^${tip}`)
  const { stdout } = await git(repo, ['rev-list', '--count', '--end-of-options', to, ...excluded])
  const count = Number(stdout.trim())
  if (!count) return { state: 'skip' }
  const from = (await git(repo, ['merge-base', '--end-of-options', to, reviewed[0]])).stdout.trim()
  const kept: string[] = []
  for (const tip of reviewed) if (!(await isAncestor(repo, tip, to))) kept.push(tip)
  return { state: 'review', from, count, tips: [to, ...kept].slice(0, TIPS) }
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
