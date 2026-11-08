/**
 * The hold and the pause extension's Play: one flag on the harness, so Play (or its auto-play) must not end the hold
 * this extension took, and this extension's release must not end the user's pause.
 */
import assert from 'node:assert/strict'
import test from 'node:test'
import { deferRelease, releaseHold } from './hold.ts'

/** A harness whose scheduler is held, as `hold()` leaves it; `release()` is what Play calls. */
const heldHarness = () => {
  const harness = {
    held: true,
    releases: 0,
    release() {
      harness.held = false
      harness.releases += 1
    },
  }
  return harness
}

test('Play while the hold is in place is set aside, and works again once the hold has ended', () => {
  const harness = heldHarness()
  let holding = true
  deferRelease(harness, () => holding)
  harness.release()
  assert.equal(harness.held, true, 'Play ended the hold')

  holding = false
  releaseHold(harness, false)
  assert.equal(harness.held, false)
  harness.held = true
  harness.release()
  assert.equal(harness.held, false, 'Play after the hold does nothing')
})

test("the hold's own release leaves the scheduler held while the user's pause holds it, until Play", () => {
  const harness = heldHarness()
  let holding = true
  deferRelease(harness, () => holding)
  holding = false
  releaseHold(harness, true)
  assert.equal(harness.held, true, "the release ended the user's pause")
  harness.release()
  assert.equal(harness.held, false)
})

test('a harness is wrapped once, so a second hold does not stack another wrapper', () => {
  const harness = heldHarness()
  let holding = true
  deferRelease(harness, () => holding)
  deferRelease(harness, () => true)
  holding = false
  harness.release()
  assert.deepEqual([harness.held, harness.releases], [false, 1])
})
