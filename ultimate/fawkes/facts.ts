/**
 * Plain words for what the daemon reports, read from its fields (DESIGN.md, "Events" and "Rollback"), never from the
 * text of a message, which is for people and may change. Browser-safe: the status line, the popover, the tool and the
 * prompt section share it.
 */
import type { Cause, FawkesEvent, Generation, Outcome, Status, StorageOutcome } from './types.ts'

const causes: Record<Cause, string> = {
  deadline: 'missed its confirm deadline',
  exited: 'exited before it confirmed',
  unhealthy: 'failed its health check',
  start_failed: 'could not be started',
  requested: 'a rollback was requested',
  daemon_restart: 'the daemon restarted while it was pending',
}

/** Why a generation rolled back. A cause this extension does not know, or none, falls back to the daemon's reason. */
export const causeText = (cause: string | null | undefined, reason?: string | null) =>
  causes[cause as Cause] ?? reason ?? (cause ? `cause ${cause}` : 'no reason recorded')

const storages: Record<StorageOutcome, string> = {
  restored: 'storage restored',
  kept: 'storage kept as it was (--keep-storage)',
  restore_failed: 'storage could not be restored',
  none: 'code only (no restore hook)',
}

export const storageText = (storage: string) => storages[storage as StorageOutcome] ?? `storage ${storage}`

/**
 * The generation the latest switch went to, whatever became of it. By `switched_at`, not by the order of the history:
 * a retired generation can be tested and switched to again after a newer one.
 */
export const lastSwitched = (generations: Generation[]) =>
  generations.reduce<Generation | undefined>(
    (latest, g) =>
      g.switched_at && (!latest || Date.parse(g.switched_at) >= Date.parse(latest.switched_at!)) ? g : latest,
    undefined,
  )

/**
 * Where generation `id` stands, as `fawkes wait` decides it from the same `status` and `history` pictures: confirmed
 * (`permanent` or `retired`), `rolled_back` once its rollback has finished, `storage_failed` while its restore failed,
 * `waiting` while it is current or being rolled back. Nothing for a generation that was never switched to.
 */
export function outcome(status: Status | undefined, generations: Generation[], id: string): Outcome | undefined {
  const g = generations.find((each) => each.id === id)
  if (!g) return undefined
  // `pending` stays set while the switch is rolled back; `step` says how far that is.
  const live = status?.pending?.generation === id ? status.step : undefined
  if (g.state === 'permanent' || g.state === 'retired') return { generation: id, state: 'permanent' }
  if (g.state === 'rolled_back') {
    if (live === 'restore_failed') return { generation: id, state: 'storage_failed' }
    if (live) return { generation: id, state: 'waiting' }
    return { generation: id, state: 'rolled_back', cause: g.rollback_cause ?? null, reason: g.rollback_reason }
  }
  // A switch to it that is starting: the target is not `current` until the live app has stopped.
  if (g.state === 'current' || status?.busy === `switch ${id}`) return { generation: id, state: 'waiting' }
  return undefined
}

/** One line for an outcome, as an agent or the popover says it: "g3 rolled back: missed its confirm deadline". */
export function outcomeText(outcome: Outcome) {
  const id = outcome.generation
  switch (outcome.state) {
    case 'permanent':
      return `${id} is permanent`
    case 'waiting':
      return `${id} is not decided yet`
    case 'storage_failed':
      return `${id} rolled back, but its storage could not be restored; the app stays stopped until a rollback retries it`
    case 'rolled_back':
      return `${id} rolled back: ${causeText(outcome.cause, outcome.reason)}`
  }
}

/** The `rolled_back` event of generation `id`, which says where it went and what became of the storage. */
export const rollbackEvent = (events: FawkesEvent[], id: string) =>
  events.findLast((event) => event.kind === 'rolled_back' && event.generation === id)

const exit = (event: FawkesEvent) =>
  event.code !== undefined ? `code ${event.code}` : event.signal !== undefined ? `signal ${event.signal}` : 'no status'

/**
 * One line for an event, from its fields. `time` renders the times it carries (a deadline, a restart). Kinds whose
 * facts are all in `generation`, and a kind this extension does not know, keep the daemon's message.
 */
export function eventLine(event: FawkesEvent, time: (iso: string) => string = (iso) => iso) {
  const g = event.generation ?? ''
  switch (event.kind) {
    case 'started':
      return `${g} started, pid ${event.pid} on port ${event.port}`
    case 'exited':
      return `${g} exited with ${exit(event)}${event.restart_at ? `, restarts at ${time(event.restart_at)}` : ''}`
    case 'switched':
      return `switched to ${g}${event.previous ? ` from ${event.previous}` : ''}${event.deadline ? `, confirm by ${time(event.deadline)}` : ''}`
    case 'confirmed':
      return `${g} confirmed, now permanent`
    case 'rolled_back':
      return [
        `${g} rolled back: ${causeText(event.cause, event.reason)}`,
        event.to ? `back to ${event.to}` : undefined,
        event.storage ? storageText(event.storage) : undefined,
      ]
        .filter(Boolean)
        .join('; ')
    case 'tested':
      return `${g} passed its test on port ${event.port} in ${event.seconds?.toFixed(1)} s`
    case 'test_failed':
      return `${g} failed its test on port ${event.port} after ${event.seconds?.toFixed(1)} s`
    case 'staged':
      return `staged ${g}`
    case 'healthy':
      return `${g} is healthy`
    case 'unhealthy':
      return `${g} failed its health check`
    case 'failed':
      return `${g} was given up: it kept failing to start`
    case 'stopped':
      return `${g} stopped`
    case 'pruned':
      return `pruned ${event.removed?.join(', ') ?? ''}`
    case 'error':
      return `error${event.error_code ? ` ${event.error_code}` : ''}: ${event.message}`
    case 'daemon':
      if (event.action === 'started') return `daemon started, pid ${event.pid}, version ${event.version}`
      if (event.action === 'stopped') return 'daemon stopped'
      if (event.action === 'reloaded')
        return `manifest reloaded: ${event.changed?.length ? event.changed.join(', ') : 'nothing changed'}`
      return event.message
    default:
      return event.message
  }
}
