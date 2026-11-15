/**
 * The after-upgrade tasks and autofix: the settings, the claims and locks every host shares, the review watermark on a
 * real repository, the autofix decision, the facts a failure's investigation gets, and who owns an extension's folder.
 */
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { claim, lock, ownerOf, reviewDecision, STALE, withLock } from './store.ts'
import {
  AUTOFIX_WAIT,
  autofixBrief,
  autofixStep,
  failures,
  failureTask,
  giveUpText,
  parseTaskSettings,
  successTask,
} from './tasks.ts'
import type { FawkesEvent, Generation, History, Status } from './types.ts'

const scratch = (t: test.TestContext) => {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), 'fktasks-')))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  return dir
}

const gitIn = (root: string) => {
  const env = { PATH: process.env.PATH ?? '', HOME: root, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' }
  return (cwd: string, ...args: string[]) =>
    execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', ...args], {
      cwd,
      env,
      encoding: 'utf8',
      stdio: 'pipe',
    }).trim()
}

test('afterUpgrade and autofix: defaults, false, instructions as text, and a value they do not allow', () => {
  assert.deepEqual(parseTaskSettings({}), { success: true, failure: true, autofix: [] })
  assert.deepEqual(parseTaskSettings({ afterUpgrade: false, autofix: ['phone'] }), {
    success: false,
    failure: false,
    autofix: ['phone'],
  })
  assert.deepEqual(parseTaskSettings({ afterUpgrade: { project: 'fawkes', success: false, failure: 'Look at it.' } }), {
    project: 'fawkes',
    success: false,
    failure: 'Look at it.',
    autofix: [],
  })
  assert.deepEqual(parseTaskSettings({ afterUpgrade: { success: true } }), {
    success: true,
    failure: true,
    autofix: [],
  })
  for (const afterUpgrade of [true, 'off', null, [], { sucess: false }])
    assert.throws(
      () => parseTaskSettings({ afterUpgrade }),
      /^Error: fawkes\.afterUpgrade in .* must be false or an object/,
    )
  assert.throws(() => parseTaskSettings({ afterUpgrade: { project: '' } }), /fawkes\.afterUpgrade\.project .* must be/)
  assert.throws(
    () => parseTaskSettings({ afterUpgrade: { failure: 1 } }),
    /fawkes\.afterUpgrade\.failure .* must be true, false/,
  )
  assert.throws(() => parseTaskSettings({ afterUpgrade: { success: ' ' } }), /fawkes\.afterUpgrade\.success/)
  for (const autofix of ['phone', [1], ['../x'], {}])
    assert.throws(() => parseTaskSettings({ autofix }), /fawkes\.autofix .* must be a list of extension names/)
})

test('of five hosts claiming one key at once exactly one wins, and a stale lock is taken over', async (t) => {
  const dir = scratch(t)
  const key = join(dir, 'failure-abc')
  const won = await Promise.all(Array.from({ length: 5 }, () => claim(key)))
  assert.equal(won.filter(Boolean).length, 1)
  assert.equal(await claim(key), false, 'a claim stays')

  const held = join(dir, 'review.lock')
  const locks = await Promise.all(Array.from({ length: 5 }, () => lock(held)))
  assert.equal(locks.filter(Boolean).length, 1)
  assert.equal(await withLock(held, async () => assert.fail('another host holds it')), false)
  const old = new Date(Date.now() - STALE - 60_000)
  utimesSync(held, old, old)
  const takeovers = await Promise.all(Array.from({ length: 5 }, () => lock(held)))
  assert.equal(takeovers.filter(Boolean).length, 1, 'one host takes the stale lock over')
  rmSync(held)
  let ran = false
  assert.equal(await withLock(held, async () => void (ran = true)), true)
  assert.ok(ran && !existsSync(held), 'the lock is released after the work')
})

test('the watermark: none is quiet, a commit under it brings nothing, one after it is a range that moves it on', async (t) => {
  const repo = scratch(t)
  const git = gitIn(repo)
  git(repo, 'init', '-q', '-b', 'main')
  const commit = (message: string) => (
    git(repo, 'commit', '-q', '--allow-empty', '-m', message),
    git(repo, 'rev-parse', 'HEAD')
  )
  const one = commit('one')
  const two = commit('two')
  const three = commit('three')
  assert.deepEqual(await reviewDecision(repo, undefined, three), { state: 'quiet' })
  assert.deepEqual(await reviewDecision(repo, three, three), { state: 'skip' })
  assert.deepEqual(await reviewDecision(repo, three, one), { state: 'skip' }, 'a host on an older commit')
  assert.deepEqual(await reviewDecision(repo, one, three), { state: 'review', from: one, count: 2, advance: true })
  git(repo, 'checkout', '-q', '-b', 'side', two)
  const side = commit('side')
  assert.deepEqual(await reviewDecision(repo, three, side), { state: 'review', from: three, count: 1, advance: false })
})

test('autofix tries, waits, tries once more, gives up once, and forgets the record when the extension loads', () => {
  const now = 10 * AUTOFIX_WAIT
  const record = (tries: number, ago: number, gaveUp?: boolean) => ({ error: 'e', tries, lastTry: now - ago, gaveUp })
  assert.equal(autofixStep(undefined, true, now), 'try')
  assert.equal(autofixStep(record(1, AUTOFIX_WAIT - 1), true, now), 'wait')
  assert.equal(autofixStep(record(1, AUTOFIX_WAIT), true, now), 'try')
  assert.equal(autofixStep(record(2, AUTOFIX_WAIT - 1), true, now), 'wait')
  assert.equal(autofixStep(record(2, AUTOFIX_WAIT), true, now), 'give-up')
  assert.equal(autofixStep(record(2, AUTOFIX_WAIT * 5, true), true, now), 'idle')
  assert.equal(autofixStep(record(2, 0, true), false, now), 'recovered')
  assert.equal(autofixStep(record(1, 0), false, now), 'recovered')
  assert.equal(autofixStep(undefined, false, now), 'idle')
})

const generation = (id: string, more: Partial<Generation>): Generation => ({
  id,
  ref: 'origin/main',
  commit: `${id}c0ffee0000`,
  state: 'retired',
  dir: `/f/generations/${id}/code`,
  staged_at: 't',
  test: null,
  switched_at: null,
  confirmed_at: null,
  rolled_back_at: null,
  rollback_reason: null,
  rollback_cause: null,
  failed_starts: 0,
  ...more,
})

test('a rollback is investigated with its cause, reason, storage, confirm report and log tail', () => {
  const now = Date.parse('2026-11-03T12:00:00Z')
  const status = { name: 'fawkes', permanent: 'g7', pending: null, step: null, busy: null } as Status
  const rolled = {
    state: 'rolled_back' as const,
    switched_at: '2026-11-03T11:00:00Z',
    rolled_back_at: '2026-11-03T11:02:00Z',
    rollback_cause: 'deadline' as const,
    rollback_reason: 'missed confirm deadline',
  }
  const tail = Array.from({ length: 30 }, (_, n) => `line ${n + 1}`).join('\n') + '\n'
  const history: History = {
    generations: [
      generation('g7', { state: 'permanent', switched_at: '2026-11-02T10:00:00Z' }),
      { ...generation('g8', rolled), log_tail: tail },
    ],
    failed_install_tail: null,
  }
  const event = (seq: number, kind: string, more: Partial<FawkesEvent>): FawkesEvent => ({
    seq,
    at: '2026-11-03T11:01:00Z',
    kind,
    generation: 'g8',
    message: 'm',
    ...more,
  })
  const events = [
    event(1, 'reported', { info: { url: 'u', confirm: 'waiting: the page answers 502' } }),
    event(2, 'reported', { info: { url: 'u', confirm: 'waiting: server facets failed to load: update' } }),
    event(3, 'rolled_back', { to: 'g7', reason: 'missed confirm deadline', cause: 'deadline', storage: 'restored' }),
  ]
  const found = failures('fawkes', status, history.generations, events, now)
  assert.deepEqual(found, [{ kind: 'rollback', generation: 'g8', commit: 'g8c0ffee0000', key: 'failure-g8c0ffee0000' }])
  assert.deepEqual(
    failures('fawkes', status, history.generations, events, now + 25 * 3_600_000),
    [],
    'older than a day',
  )

  const text = failureTask(true, { host: 'fawkes', manifest: '/h/.fawkes/fawkes.toml' }, found[0], {
    status,
    history,
    events,
  })
  assert.match(text, /^Fawkes: on fawkes, g8 rolled back: missed its confirm deadline; back to g7; storage restored\./)
  assert.match(text, /Investigate it now/)
  assert.match(text, /^- Cause: deadline$/m)
  assert.match(text, /^- Reason: missed confirm deadline$/m)
  assert.match(text, /^- Storage: storage restored$/m)
  assert.match(text, /^- Its last confirm report: waiting: server facets failed to load: update$/m)
  assert.match(text, /^- The Ultimate commits it brought over g7: g7c0ffee0000\.\.g8c0ffee0000$/m)
  assert.match(text, /`fawkes history --manifest \/h\/\.fawkes\/fawkes\.toml --json`/)
  assert.match(text, /The last 20 lines of g8's logs\/app\.log:\n```text\nline 11\n[^]*\nline 30\n```$/)
  assert.doesNotMatch(text, /line 10\n/)
  assert.equal(
    failureTask('Only look.', { host: 'fawkes', manifest: 'm' }, found[0], { status, history, events }).split(
      '\n\n',
    )[1],
    'Only look.',
  )
})

test('a failed test and a failed stage are failures too; a requested rollback is not', () => {
  const now = Date.parse('2026-11-03T12:00:00Z')
  const status = { name: 'xana', permanent: 'g7', pending: null, step: null, busy: null } as Status
  const generations = [
    generation('g7', { state: 'permanent' }),
    generation('g8', { state: 'test_failed' }),
    generation('g9', {
      state: 'rolled_back',
      switched_at: 't',
      rolled_back_at: '2026-11-03T11:00:00Z',
      rollback_cause: 'requested',
    }),
  ]
  const at = '2026-11-03T11:30:00Z'
  const events: FawkesEvent[] = [
    { seq: 1, at, kind: 'test_failed', generation: 'g8', message: 'm', port: 4791, seconds: 3 },
    { seq: 2, at, kind: 'test_failed', generation: 'g8', message: 'm', port: 4791, seconds: 3 },
    {
      seq: 3,
      at,
      kind: 'error',
      generation: 'g10',
      message: 'stage of main failed: install',
      error_code: 'install_failed',
    },
    { seq: 4, at, kind: 'error', generation: 'g7', message: 'spare port taken', error_code: 'port_in_use' },
    { seq: 5, at, kind: 'error', generation: 'g11', message: 'prune: cannot remove', error_code: 'internal' },
  ]
  assert.deepEqual(
    failures('xana', status, generations, events, now).map(({ kind, generation, key }) => ({ kind, generation, key })),
    [
      { kind: 'test', generation: 'g8', key: 'failure-g8c0ffee0000' },
      { kind: 'stage', generation: 'g10', key: 'failure-xana-g10' },
    ],
  )
})

test('the review names its range, its procedure and every extension with its owner; a brief names its try and rules', () => {
  const host = { host: 'fawkes', manifest: '/h/.fawkes/fawkes.toml' }
  const review = successTask(true, '/f/ultimate/fawkes', {
    ...host,
    generation: 'g9',
    ref: 'origin/main',
    clone: '/h/Code/ultimate',
    from: 'aaaa1111',
    to: 'bbbb2222cccc',
    count: 12,
    inventory: [
      {
        name: 'fawkes',
        source: 'project',
        path: '/h/Code/fawkes/ultimate/fawkes',
        repo: '/h/Code/fawkes',
        project: 'fawkes',
      },
      { name: 'phone', source: 'user', path: '/h/Code/ultimate-extensions/phone', repo: '/h/Code/ultimate-extensions' },
    ],
  })
  assert.match(review, /^Fawkes: fawkes now runs Ultimate bbbb2222 as g9, 12 commits since the last review\./)
  assert.match(review, /follow `\/f\/ultimate\/fawkes\/review\.md`/)
  assert.match(review, /change nothing without the user's go/)
  assert.match(
    review,
    /^- Ultimate commits to review: aaaa1111\.\.bbbb2222cccc, 12 commits, in the clone \/h\/Code\/ultimate$/m,
  )
  assert.match(review, /^ {2}- fawkes \(project\): \/h\/Code\/fawkes\/ultimate\/fawkes, project fawkes$/m)
  assert.match(
    review,
    /^ {2}- phone \(user\): .*, the repository \/h\/Code\/ultimate-extensions, which no Ultimate project has$/m,
  )

  const brief = autofixBrief({
    ...host,
    name: 'phone',
    path: '/h/Code/ultimate-extensions/phone',
    repo: '/h/Code/ultimate-extensions',
    error: 'Cannot find module ultimate/lib/gone.ts',
    attempt: 2,
    previous: 'SyntaxError',
    clone: '/h/Code/ultimate',
  })
  assert.match(
    brief,
    /^Fawkes autofix, try 2 of 2: the extension `phone` fails to load on fawkes\. Repair it hands-off/,
  )
  assert.match(
    brief,
    /No Ultimate project owns this folder .* Route this goal, word for word, to whoever owns \/h\/Code\/ultimate-extensions/,
  )
  assert.match(brief, /^- Error: Cannot find module ultimate\/lib\/gone\.ts$/m)
  assert.match(brief, /^- The first try did not fix it\. It failed then with: SyntaxError$/m)
  assert.match(brief, /Never merge, pull or reset in the clone/)
  assert.match(brief, /this is the last try/)
  assert.match(giveUpText({ ...host, name: 'phone', path: '/p', error: 'E', tries: 2 }), /after 2 tries[^]*- Error: E/)
})

test('an extension belongs to the project whose root is its repository; one in another repository to none', async (t) => {
  const root = scratch(t)
  const git = gitIn(root)
  const [project, other] = ['project', 'other'].map((name) => join(root, name))
  for (const repo of [project, other]) {
    mkdirSync(join(repo, 'ext'), { recursive: true })
    writeFileSync(join(repo, 'ext', 'server.ts'), '')
    git(repo, 'init', '-q')
  }
  symlinkSync(join(project, 'ext'), join(root, 'link'))
  const projects = [{ name: 'other-home' }, { name: 'project', root: project }]
  assert.deepEqual(await ownerOf(join(root, 'link'), projects), {
    path: join(project, 'ext'),
    repo: project,
    project: 'project',
  })
  assert.deepEqual(await ownerOf(join(other, 'ext'), projects), { path: join(other, 'ext'), repo: other })
  mkdirSync(join(root, 'loose'))
  assert.deepEqual(await ownerOf(join(root, 'loose'), projects), { path: join(root, 'loose') })
})
