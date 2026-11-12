/**
 * Fetching the clone the generations come from, so the status line hears of new commits without Check now. The interval
 * is `fawkes.fetchEvery` in the user's Ultimate settings, read at every tick so a change applies without a restart.
 * Every host on one clone shares its `FETCH_HEAD`: a fetch is due only when nobody fetched within the interval, so the
 * hosts together fetch about once per interval.
 */
import { execFile } from 'node:child_process'
import { readFile, stat } from 'node:fs/promises'
import { join } from 'node:path'
import { promisify } from 'node:util'

const run = promisify(execFile)

/** Without a setting the clone is fetched every 15 minutes. */
export const FETCH_DEFAULT = 15 * 60_000
/** A shorter interval is raised to this: the scheduled check runs once a minute. */
export const FETCH_FLOOR = 60_000

/**
 * `"15m"`, `"2h"` or a number of minutes as milliseconds, at least `FETCH_FLOOR`; `0`, `"0m"` or `"off"` is 0 (never);
 * no setting is the default. Anything else throws, naming the setting.
 */
export function parseFetchEvery(value: unknown): number {
  if (value === undefined) return FETCH_DEFAULT
  if (value === 'off') return 0
  const match = typeof value === 'string' ? /^(\d+(?:\.\d+)?)(m|h)$/.exec(value.trim()) : undefined
  const minutes = typeof value === 'number' ? value : match ? Number(match[1]) * (match[2] === 'h' ? 60 : 1) : NaN
  if (!Number.isFinite(minutes) || minutes < 0)
    throw new Error(
      `fawkes.fetchEvery in ~/.ultimate/settings.json must be like "15m" or "1h", a number of minutes, or "off"; it is ${JSON.stringify(value)}`,
    )
  return minutes === 0 ? 0 : Math.max(FETCH_FLOOR, minutes * 60_000)
}

/** The interval the user's settings file (`<userHome>/.ultimate/settings.json`) sets; a missing file sets none. */
export async function fetchEvery(userHome: string) {
  const path = join(userHome, '.ultimate', 'settings.json')
  const text = await readFile(path, 'utf8').catch((error: unknown) => {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return '{}'
    throw error
  })
  let settings: { fawkes?: { fetchEvery?: unknown } } | null
  try {
    settings = JSON.parse(text)
  } catch (error) {
    throw new Error(`${path} is not valid JSON: ${(error as Error).message}`)
  }
  return parseFetchEvery(settings?.fawkes?.fetchEvery)
}

/** Whether a fetch is due: never when off, at once before the first, else once `every` has passed since `last`. */
export const fetchDue = (last: number | undefined, every: number, now: number) =>
  every > 0 && (last === undefined || now - last >= every)

/**
 * The clone's shared git directory. A generation is a linked worktree, whose own git directory would keep a
 * `FETCH_HEAD` per generation; the common one is the same for every host on the clone.
 */
export async function cloneGitDir(repo: string) {
  const { stdout } = await run('git', ['rev-parse', '--path-format=absolute', '--git-common-dir'], {
    cwd: repo,
    timeout: 10_000,
  })
  return stdout.trim()
}

/** When the clone was last fetched, by any host on it (`FETCH_HEAD`'s mtime); undefined before the first fetch. */
export const lastFetch = (gitDir: string) =>
  stat(join(gitDir, 'FETCH_HEAD')).then(
    (info) => info.mtimeMs,
    () => undefined,
  )

/** Fetch `remote` into the clone's shared git directory, never prompting: a fetch without a login fails rather than hangs. */
export async function fetchRemote(gitDir: string, remote: string) {
  // A fixed directory, so a relative remote URL never depends on where the host was started.
  await run('git', [`--git-dir=${gitDir}`, 'fetch', '--quiet', '--end-of-options', remote], {
    cwd: gitDir,
    timeout: 60_000,
    env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
  })
}
