/**
 * Who may act and when a generation may vouch for itself: the two decisions that keep a broken upgrade from becoming
 * permanent. A preview of the live host inherits Fawkes's variables but runs with a home of its own, so it stays inert.
 */
import assert from 'node:assert/strict'
import { mkdtempSync, realpathSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import type { ExtensionInfo } from 'ultimate/shared/protocol.ts'
import { judge } from './confirm.ts'
import { role } from './role.ts'
import { awaitingConfirm, restoreFailed, rollbackNote } from './step.ts'
import type { Status, Step } from './types.ts'

const extension = (name: string, error?: string): ExtensionInfo => ({
  name,
  enabled: true,
  source: 'builtin',
  layer: 'default',
  path: `/x/${name}`,
  actions: [],
  facets: { server: true, web: false },
  server: { tools: [], sections: [], hooks: 0, docs: [], ...(error ? { error } : {}) },
})

test('a generation vouches for itself only with every server facet loaded and its page served', () => {
  assert.deepEqual(judge([extension('a'), extension('b')], 200), { ok: true })
  assert.deepEqual(judge([extension('a'), extension('broken', 'boom')], 200), {
    ok: false,
    reason: 'server facets failed to load: broken',
  })
  assert.equal(judge([{ ...extension('off', 'boom'), enabled: false }], 200).ok, true)
  assert.equal(judge([extension('a')], 503).ok, false)
  assert.equal(judge([extension('a')], 'fetch failed').ok, false)
})

test('a confirm is offered only while the switch awaits it, though pending stays set during a rollback', () => {
  const status = (step?: Step | null, pending = true) =>
    ({ pending: pending ? { generation: 'g2', previous: 'g1', deadline: 't' } : null, step }) as Status
  assert.equal(awaitingConfirm(status('pending'))?.generation, 'g2')
  // An older daemon sends no step.
  assert.equal(awaitingConfirm(status())?.generation, 'g2')
  assert.equal(awaitingConfirm(status('rolling_back')), null)
  assert.equal(awaitingConfirm(status('restore_failed')), null)
  assert.equal(awaitingConfirm(status(null, false)), null)
  assert.equal(awaitingConfirm(undefined), null)
  assert.equal(restoreFailed(status('restore_failed')), true)
  assert.equal(restoreFailed(status('rolling_back')), false)
  assert.equal(rollbackNote(status('pending')), undefined)
  assert.match(rollbackNote(status('rolling_back'))!, /g2 is being rolled back to g1/)
  assert.match(rollbackNote(status('restore_failed'))!, /g2's storage could not be restored/)
})

test('only the Ultimate start.ts prepared acts on Fawkes; a preview that inherited its variables is inert', () => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fawkes-role-')))
  try {
    const fawkes = { FAWKES_SOCKET: '/s', FAWKES_GENERATION: 'g2', FAWKES_PHASE: 'live' }
    assert.deepEqual(role({ ...fawkes, FAWKES_ULTIMATE_HOME: root, ULTIMATE_HOME: root }), {
      kind: 'live',
      socket: '/s',
      generation: 'g2',
    })
    // A preview: Fawkes's variables and the live home's marker, but its own ULTIMATE_HOME.
    const preview = join(root, 'preview')
    assert.deepEqual(role({ ...fawkes, FAWKES_ULTIMATE_HOME: root, ULTIMATE_HOME: preview }), { kind: 'inert' })
    assert.deepEqual(role({ ...fawkes, ULTIMATE_HOME: root }), { kind: 'inert' })
    assert.deepEqual(role({ ULTIMATE_HOME: root, FAWKES_ULTIMATE_HOME: root }), { kind: 'inert' })
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})
