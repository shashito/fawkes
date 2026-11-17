/**
 * What the status line shows: a low disk shows when nothing more urgent does, and never hides a pending confirm, a
 * rollback or a failed upgrade.
 */
import assert from 'node:assert/strict'
import test from 'node:test'
import { itemOf } from './item.ts'
import { upgradeRefusal } from './step.ts'
import type { FawkesState, Status } from './types.ts'

type Supervised = Extract<FawkesState, { supervised: true }>

const low = { free: 9.2e9, total: 245e9, low: true }

const status = (more: Partial<Status> = {}) =>
  ({ name: 'u', permanent: 'g6', current: 'g6', pending: null, step: null, busy: null, version: '0.4.0', actions: [], ...more }) as Status

const state = (more: Partial<Supervised> = {}): Supervised => ({
  supervised: true,
  generation: 'g6',
  connected: true,
  generations: [],
  events: [],
  status: status({ disk: low }),
  ...more,
})

test('a low disk shows in the status line, below whatever is more urgent', () => {
  assert.equal(itemOf(state()), 'disk_low')
  assert.equal(itemOf(state({ status: status({ disk: { ...low, low: false } }) })), 'current')
  assert.equal(itemOf(state({ status: status() })), 'current')
  // A new commit to upgrade to is less urgent than the disk that refuses the upgrade.
  assert.equal(itemOf(state({ behind: { ref: 'origin/main', count: 2, commits: [], checkedAt: 0 } })), 'disk_low')

  const pending = { generation: 'g7', previous: 'g6', deadline: 'd' }
  assert.equal(itemOf(state({ status: status({ disk: low, pending, step: 'pending' }) })), 'pending')
  assert.equal(itemOf(state({ status: status({ disk: low, pending, step: 'rolling_back' }) })), 'rolling_back')
  assert.equal(itemOf(state({ status: status({ disk: low, pending, step: 'restore_failed' }) })), 'restore_failed')
  assert.equal(itemOf(state({ status: status({ disk: low, busy: 'stage origin/main' }) })), 'upgrading')
  assert.equal(itemOf(state({ connected: false })), 'disconnected')
  assert.equal(
    itemOf(state({ upgrade: { ref: 'origin/main', state: 'failed', since: 0, at: Date.now() } })),
    'failed',
  )
})

test('without a stage, a low disk is why an upgrade is refused', () => {
  assert.match(upgradeRefusal(status({ disk: low }))!, /^The disk is low \(9\.2 GB free of 245\.0 GB\)/)
  assert.equal(upgradeRefusal(status({ disk: low, actions: ['stage'] })), undefined)
})
