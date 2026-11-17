/**
 * What the status line shows, as one word: the order of what matters most. Browser-safe and free of React, so a test
 * can choose the item from a state.
 */
import { lastSwitched, lowDisk, outcome } from './facts.ts'
import { awaitingConfirm, outdated, restoreFailed } from './step.ts'
import type { FawkesState, Upgrade } from './types.ts'

type Supervised = Extract<FawkesState, { supervised: true }>

export const running = (upgrade: Upgrade | undefined) => upgrade?.state === 'fetching' || upgrade?.state === 'running'

/** How the latest switch ended, once it is decided. */
export function lastOutcome(state: Supervised) {
  const target = lastSwitched(state.generations)
  const decided = target && outcome(state.status, state.generations, target.id)
  return decided && decided.state !== 'waiting' ? { target: target!, decided } : undefined
}

/** One word for what the status line shows, in order of what matters most. */
export function itemOf(state: Supervised) {
  const { status, upgrade } = state
  if (!state.connected) return 'disconnected'
  if (awaitingConfirm(status)) return 'pending'
  if (restoreFailed(status)) return 'restore_failed'
  if (status?.pending && status.step === 'rolling_back') return 'rolling_back'
  if (outdated(status)) return 'outdated'
  if (running(upgrade) || status?.busy) return 'upgrading'
  if (upgrade?.state === 'failed' && Date.now() - upgrade.at < 3_600_000) return 'failed'
  const last = lastOutcome(state)
  if (
    last?.decided.state === 'rolled_back' &&
    Date.now() - Date.parse(last.target.rolled_back_at ?? '') < 3_600_000
  )
    return 'rolled_back'
  if (lowDisk(status)) return 'disk_low'
  if (state.behind?.count) return 'behind'
  return 'current'
}
