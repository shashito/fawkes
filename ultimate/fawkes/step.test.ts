/**
 * `projectProgress` for Ultimate's `status` extension: a valid `{ label, index, total }` from the daemon's typed `step`
 * alone, or null. `busy` is a sentence for people and never counts.
 */
import assert from 'node:assert/strict'
import test from 'node:test'
import { progress } from './step.ts'
import type { Status } from './types.ts'

const status = (step: Status['step'], busy: string | null = null) =>
  ({
    name: 'fawkes',
    daemon_pid: 40,
    permanent: 'g6',
    current: 'g7',
    pending: { generation: 'g7', previous: 'g6', deadline: 'd' },
    step,
    busy,
    version: '0.4.0',
    actions: [],
  }) as unknown as Status

/** What `extensions/status` accepts (its README): a nonempty label, a positive safe total, an index from 0 through it. */
const valid = (p: { label: string; index: number; total: number }) =>
  p.label.length > 0 && Number.isSafeInteger(p.total) && p.total > 0 && Number.isSafeInteger(p.index) && p.index >= 0 && p.index <= p.total

test('progress names the step of the transaction, from step alone', () => {
  const seen = (['switching', 'pending', 'rolling_back', 'restore_failed'] as const).map((step) => progress(status(step)))
  assert.deepEqual(seen, [
    { label: 'switch', index: 2, total: 4 },
    { label: 'confirm', index: 3, total: 4 },
    { label: 'rollback', index: 3, total: 4 },
    { label: 'restore failed', index: 3, total: 4 },
  ])
  for (const p of seen) assert.ok(p && valid(p), JSON.stringify(p))

  // A stage, a test or a switch that `busy` names before its intent is saved: no step, no progress.
  for (const busy of ['stage origin/main', 'test g7', 'switch g7']) assert.equal(progress(status(null, busy)), null)
  assert.deepEqual(progress(status('pending', 'stage origin/main')), { label: 'confirm', index: 3, total: 4 })
  // An older daemon sends no step, a newer one may send a step this extension does not know, and before the first
  // status there is nothing.
  assert.equal(progress(status(undefined)), null)
  assert.equal(progress(status('reticulating' as Status['step'])), null)
  assert.equal(progress(undefined), null)
})
