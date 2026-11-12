/**
 * The scheduled fetch: `fawkes.fetchEvery` read from the user's settings, and the gate on the clone's shared
 * `FETCH_HEAD`, against a real clone whose `origin` is a local bare repository, fetched from a linked worktree as a
 * generation is.
 */
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, realpathSync, rmSync, utimesSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import {
  cloneGitDir,
  FETCH_DEFAULT,
  FETCH_FLOOR,
  fetchDue,
  fetchEvery,
  fetchRemote,
  lastFetch,
  parseFetchEvery,
} from './fetch.ts'

test('fetchEvery takes minutes or hours, a number of minutes, or off; a short one is raised to the floor', () => {
  assert.equal(parseFetchEvery(undefined), FETCH_DEFAULT)
  assert.equal(parseFetchEvery('15m'), 15 * 60_000)
  assert.equal(parseFetchEvery('2h'), 2 * 3_600_000)
  assert.equal(parseFetchEvery('1.5h'), 90 * 60_000)
  assert.equal(parseFetchEvery(5), 5 * 60_000)
  assert.equal(parseFetchEvery(0.25), FETCH_FLOOR)
  assert.equal(parseFetchEvery('off'), 0)
  assert.equal(parseFetchEvery(0), 0)
  assert.equal(parseFetchEvery('0m'), 0)
  for (const bad of ['15', '30s', 'hourly', -1, Number.NaN, null, true, {}])
    assert.throws(() => parseFetchEvery(bad), /fawkes\.fetchEvery/)
})

test('the interval is read from the user settings file, and none there is the default', async (t) => {
  const home = mkdtempSync(join(tmpdir(), 'fkfetch-'))
  t.after(() => rmSync(home, { recursive: true, force: true }))
  assert.equal(await fetchEvery(home), FETCH_DEFAULT)
  mkdirSync(join(home, '.ultimate'))
  const settings = (value: unknown) => writeFileSync(join(home, '.ultimate/settings.json'), JSON.stringify(value))
  settings({ extensions: { disable: ['update'] } })
  assert.equal(await fetchEvery(home), FETCH_DEFAULT)
  settings({ fawkes: { fetchEvery: '5m' } })
  assert.equal(await fetchEvery(home), 5 * 60_000)
  settings({ fawkes: { fetchEvery: 'off' } })
  assert.equal(await fetchEvery(home), 0)
  settings({ fawkes: { fetchEvery: 'soon' } })
  await assert.rejects(fetchEvery(home), /fawkes\.fetchEvery/)
  writeFileSync(join(home, '.ultimate/settings.json'), '{ "fawkes": ')
  await assert.rejects(fetchEvery(home), /settings\.json is not valid JSON/)
})

test('a fetch is due before the first, then once the interval passed since any host fetched, and never when off', () => {
  const now = 10 * 3_600_000
  assert.ok(fetchDue(undefined, FETCH_DEFAULT, now))
  assert.ok(!fetchDue(now - FETCH_DEFAULT + 1, FETCH_DEFAULT, now))
  assert.ok(fetchDue(now - FETCH_DEFAULT, FETCH_DEFAULT, now))
  assert.ok(!fetchDue(undefined, 0, now))
})

test('a generation fetches into the clone, so every worktree of it sees one shared FETCH_HEAD', async (t) => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fkfetch-')))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const env = { PATH: process.env.PATH ?? '', HOME: root, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' }
  const git = (cwd: string, ...args: string[]) =>
    execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', ...args], { cwd, env, encoding: 'utf8', stdio: 'pipe' })
  const [origin, clone, work] = ['origin.git', 'clone', 'work'].map((name) => join(root, name))
  git(root, 'init', '-q', '--bare', '-b', 'main', origin)
  git(root, 'clone', '-q', origin, work)
  git(work, 'commit', '-q', '--allow-empty', '-m', 'one')
  git(work, 'push', '-q', 'origin', 'HEAD:main')
  git(root, 'clone', '-q', origin, clone)
  git(clone, 'worktree', 'add', '-q', '--detach', join(root, 'g1'), 'HEAD')
  const generation = join(root, 'g1')

  const gitDir = await cloneGitDir(generation)
  assert.equal(gitDir, join(clone, '.git'))
  const before = await lastFetch(gitDir)
  git(work, 'commit', '-q', '--allow-empty', '-m', 'two')
  git(work, 'push', '-q', 'origin', 'HEAD:main')
  const old = new Date(Date.now() - 60 * 60_000)
  if (before !== undefined) utimesSync(join(gitDir, 'FETCH_HEAD'), old, old)

  await fetchRemote(gitDir, 'origin')
  const after = await lastFetch(gitDir)
  assert.ok(after !== undefined && after > Date.now() - 60_000, 'the fetch wrote the clone’s FETCH_HEAD')
  assert.ok(!fetchDue(after, FETCH_DEFAULT, Date.now()), 'another host on the clone sees it fetched')
  assert.equal(git(generation, 'rev-list', '--count', 'HEAD..origin/main').trim(), '1')
})
