/**
 * The scheduler's hold is one flag, shared with the pause extension, whose Play (pressed, or its auto-play after a rate
 * limit, which a copied storage carries into a test run) would end this extension's hold too. While this extension
 * holds, any other `release()` of the harness is set aside; this extension's own release lets the scheduler run unless
 * the user's pause still holds it.
 */
export type Releasable = { release(): void }

const ownRelease = new WeakMap<Releasable, () => void>()

/** Set aside every `release()` of `harness` while `holding()`. Once per harness: a failed core swap opens a new one. */
export function deferRelease(harness: Releasable, holding: () => boolean) {
  if (ownRelease.has(harness)) return
  const original = harness.release.bind(harness)
  ownRelease.set(harness, original)
  harness.release = () => {
    if (!holding()) original()
  }
}

/** End this extension's hold (the caller has stopped `holding`), unless the user's pause holds the scheduler too. */
export function releaseHold(harness: Releasable, paused: boolean) {
  if (!paused) (ownRelease.get(harness) ?? harness.release.bind(harness))()
}
