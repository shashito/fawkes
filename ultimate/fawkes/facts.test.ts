/**
 * The outcome of a switch, decided as `fawkes wait` decides it, and the words for it, read from fields: a rollback
 * with each cause, a failed restore, and events whose message says something else than their fields.
 */
import assert from 'node:assert/strict'
import test from 'node:test'
import { causeText, eventLine, lastSwitched, outcome, outcomeText } from './facts.ts'
import type { Cause, FawkesEvent, Generation, Status } from './types.ts'

const generation = (id: string, state: Generation['state'], more: Partial<Generation> = {}): Generation => ({
  id,
  ref: 'origin/main',
  commit: '238c447f00000000',
  state,
  dir: `/f/generations/${id}/code`,
  staged_at: 't',
  test: null,
  switched_at: state === 'staged' || state === 'tested' ? null : 't',
  confirmed_at: null,
  rolled_back_at: null,
  rollback_reason: null,
  rollback_cause: null,
  failed_starts: 0,
  ...more,
})

const status = (more: Partial<Status> = {}) =>
  ({ permanent: 'g2', pending: null, step: null, busy: null, version: '0.1.0', actions: [], ...more }) as Status

/** A switch to g3 that is being rolled back, or whose restore failed: `pending` stays set, `step` says how far. */
const rolling = (step: Status['step']) =>
  status({ pending: { generation: 'g3', previous: 'g2', deadline: 't' }, step })

test('a rolled-back generation names its cause in plain words, for each cause', () => {
  const words: Record<Cause, string> = {
    deadline: 'g3 rolled back: missed its confirm deadline',
    exited: 'g3 rolled back: exited before it confirmed',
    unhealthy: 'g3 rolled back: failed its health check',
    start_failed: 'g3 rolled back: could not be started',
    requested: 'g3 rolled back: a rollback was requested',
    daemon_restart: 'g3 rolled back: the daemon restarted while it was pending',
  }
  for (const [cause, line] of Object.entries(words) as [Cause, string][]) {
    const g3 = generation('g3', 'rolled_back', { rollback_cause: cause, rollback_reason: 'text for people' })
    const decided = outcome(status(), [generation('g2', 'permanent'), g3], 'g3')
    assert.deepEqual(decided, { generation: 'g3', state: 'rolled_back', cause, reason: 'text for people' })
    assert.equal(outcomeText(decided!), line)
  }
  // Rolled back before causes existed, or by a newer daemon with a cause this extension does not know.
  assert.equal(causeText(null, 'missed confirm deadline'), 'missed confirm deadline')
  assert.equal(causeText('cosmic_ray', null), 'cause cosmic_ray')
  assert.equal(causeText(null, null), 'no reason recorded')
})

test('a rollback whose restore failed is storage_failed; one still under way is not decided', () => {
  const gens = [generation('g2', 'permanent'), generation('g3', 'rolled_back', { rollback_cause: 'deadline' })]
  assert.equal(outcome(rolling('restore_failed'), gens, 'g3')?.state, 'storage_failed')
  assert.match(outcomeText(outcome(rolling('restore_failed'), gens, 'g3')!), /storage could not be restored/)
  assert.equal(outcome(rolling('rolling_back'), gens, 'g3')?.state, 'waiting')
  // Once `rollback --keep-storage` or a retried restore ends the step, the rollback is decided.
  assert.equal(outcome(status(), gens, 'g3')?.state, 'rolled_back')
})

test('a confirmed generation is permanent, a pending one waits, one never switched to has no outcome', () => {
  const pending = status({ pending: { generation: 'g3', previous: 'g2', deadline: 't' }, step: 'pending' })
  assert.equal(outcome(pending, [generation('g3', 'current')], 'g3')?.state, 'waiting')
  assert.equal(outcome(status(), [generation('g3', 'permanent')], 'g3')?.state, 'permanent')
  assert.equal(outcome(status(), [generation('g3', 'retired')], 'g3')?.state, 'permanent')
  assert.equal(outcome(status(), [generation('g3', 'tested')], 'g3'), undefined)
  assert.equal(outcome(status({ busy: 'switch g3' }), [generation('g3', 'tested')], 'g3')?.state, 'waiting')
  assert.equal(outcome(status(), [], 'g3'), undefined)
  assert.equal(
    lastSwitched([generation('g2', 'permanent'), generation('g3', 'rolled_back'), generation('g4', 'tested')])?.id,
    'g3',
  )
})

test('an event is said from its fields, not from its message', () => {
  const event = (kind: string, facts: Partial<FawkesEvent>): FawkesEvent => ({
    seq: 1,
    at: 't',
    kind,
    generation: 'g3',
    message: 'a message that says something else',
    ...facts,
  })
  const lines = [
    eventLine(event('started', { pid: 9001, port: 4790 })),
    eventLine(event('exited', { signal: 9, restart_at: '14:02' })),
    eventLine(event('exited', { code: 1 })),
    eventLine(event('switched', { previous: 'g2', deadline: '14:04' })),
    eventLine(event('rolled_back', { to: 'g2', cause: 'deadline', storage: 'restored', reason: 'old words' })),
    eventLine(event('rolled_back', { to: 'g2', cause: 'exited', storage: 'restore_failed' })),
    eventLine(event('tested', { port: 4791, seconds: 2.14 })),
    eventLine(event('daemon', { generation: null, action: 'started', pid: 812, version: '0.1.0' })),
  ]
  assert.deepEqual(lines, [
    'g3 started, pid 9001 on port 4790',
    'g3 exited with signal 9, restarts at 14:02',
    'g3 exited with code 1',
    'switched to g3 from g2, confirm by 14:04',
    'g3 rolled back: missed its confirm deadline; back to g2; storage restored',
    'g3 rolled back: exited before it confirmed; back to g2; storage could not be restored',
    'g3 passed its test on port 4791 in 2.1 s',
    'daemon started, pid 812, version 0.1.0',
  ])
  // An error's code is a field; its message is the only account of what failed.
  assert.equal(
    eventLine(event('error', { generation: null, error_code: 'install_failed', message: 'pnpm exited 1' })),
    'error install_failed: pnpm exited 1',
  )
  // A kind this extension does not know keeps the daemon's message.
  assert.equal(eventLine(event('teleported', {})), 'a message that says something else')
})
