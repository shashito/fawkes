/**
 * What the daemon's status allows a client to do. `pending` stays set while a switch is rolled back or its restore
 * failed, so only `step` says whether a confirm can still happen, and `actions` names every command the daemon would
 * accept right now. Browser-safe.
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

/**
 * Why the daemon is older than this extension expects, or nothing. Every daemon that reports `version` also sends what
 * the extension reads: `step`, `actions`, the event fields and `rollback_cause`.
 */
export function outdated(status: Status | undefined) {
  if (!status || (status.version && status.actions)) return undefined
  return 'The Fawkes daemon is older than this extension expects: its status has no version. Upgrade Fawkes and restart its daemon (a pending switch rolls back); until then this generation still confirms itself, but upgrades are off'
}

/**
 * Whether a switch is under way: `busy` names it (`switch g3`) from its start, and `step` from when its intent is
 * saved. The switch stops the live app's process group, an `upgrade` run from here with it.
 */
export const switching = (status: Status | undefined) =>
  status?.step === 'switching' || !!status?.busy?.startsWith('switch ')

/** Whether the daemon accepts `command` (`rollback`, `confirm g3`) right now, from `status.actions`. */
export const allows = (status: Status | undefined, command: string) => !!status?.actions?.includes(command)

/**
 * Why `fawkes upgrade` cannot run now, or nothing. `actions` lists `stage` while a switch is pending, but that switch
 * refuses the next one, so an upgrade would stage and test only to be refused at its end.
 */
export function upgradeRefusal(status: Status | undefined) {
  if (!status) return 'The Fawkes daemon has not answered yet'
  const old = outdated(status)
  if (old) return old
  if (status.pending) return rollbackNote(status) ?? `${status.pending.generation} still awaits its confirm`
  if (status.step === 'switching') return 'A switch is under way'
  if (!allows(status, 'stage')) return status.busy ? `Fawkes is busy: ${status.busy}` : 'Fawkes does not take a stage now'
  return undefined
}
