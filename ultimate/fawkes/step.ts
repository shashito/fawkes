/**
 * What the daemon's transaction step means for a client. `pending` stays set while a switch is rolled back or its
 * restore failed, so only `step` says whether a confirm can still happen. Browser-safe.
 */
import type { Status } from './types.ts'

/** The switch that awaits its confirm. An older daemon sends no `step`, and its `pending` does. */
export function awaitingConfirm(status: Status | undefined) {
  return status?.pending && (!status.step || status.step === 'pending') ? status.pending : null
}

/** Whether the app waits for `rollback` to retry a failed storage restore. */
export const restoreFailed = (status: Status | undefined) => !!status?.pending && status.step === 'restore_failed'

/** Plain words for a switch that is being rolled back or whose restore failed; nothing otherwise. */
export function rollbackNote(status: Status | undefined) {
  const { pending, step } = status ?? {}
  if (!pending) return undefined
  if (step === 'rolling_back') return `${pending.generation} is being rolled back to ${pending.previous ?? 'nothing'}`
  if (step === 'restore_failed')
    return `${pending.generation}'s storage could not be restored and the app is stopped; rolling back retries it`
  return undefined
}
