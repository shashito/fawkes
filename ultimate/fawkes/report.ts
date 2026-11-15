/**
 * What the live host tells the Fawkes daemon about itself (`report`): the lines `fawkes up` and `fawkes status` print,
 * which would otherwise be lost, since Ultimate's own dashboard writes to a log file under a supervisor.
 */
import { join } from 'node:path'
import type { Confirming } from './types.ts'

export type Info = Record<string, string>

/** The daemon refuses a value longer than this, so a longer one is left out. */
export const MAX_VALUE = 256
/** How often the host is asked whether its info changed: system info has no change notification for extensions. */
export const INTERVAL = 5_000

/** What the host knows (`host.system()`, in part). */
export type Known = { process: { url: string }; access: { origins: string[] } }

/** The origins joined by `, `, as many as fit in a value; the first alone may already be too long. */
function remote(origins: string[]) {
  let joined = ''
  for (const origin of origins) {
    const next = joined ? `${joined}, ${origin}` : origin
    if (next.length > MAX_VALUE) break
    joined = next
  }
  return joined
}

/**
 * A value the daemon accepts: no control or invisible formatting character (it prints values to a terminal), at most
 * `MAX_VALUE` characters, as the daemon counts them.
 */
function value(text: string) {
  const plain = text.replace(/[\p{Cc}\p{Cf}\s]+/gu, ' ').trim()
  const chars = [...plain]
  return chars.length > MAX_VALUE ? `${chars.slice(0, MAX_VALUE - 1).join('')}…` : plain
}

/** Why this generation has not confirmed itself yet, or nothing once it did or before it checked. */
function confirmText(confirming: Confirming | undefined) {
  if (confirming?.state === 'checking') return confirming.detail && value(`waiting: ${confirming.detail}`)
  if (confirming?.state === 'refused') return value(`refused: ${confirming.reason}`)
  return undefined
}

/**
 * `url` (the address the host serves), `remote` (its Tailscale HTTPS URLs, when it has any), `log` (the supervisor
 * log in the host's home, where `bin/ultimate.ts` writes it) and `confirm` (why a pending generation has not confirmed
 * itself yet, which the daemon logs as a `reported` event before any rollback). A key with nothing to say is left out.
 */
export function reportInfo(system: Known, home: string, confirming?: Confirming): Info {
  const info: Info = {}
  const { url } = system.process
  if (url && url.length <= MAX_VALUE) info.url = url
  const origins = remote(system.access.origins)
  if (origins) info.remote = origins
  const log = join(home, 'supervisor.log')
  if (log.length <= MAX_VALUE) info.log = log
  const confirm = confirmText(confirming)
  if (confirm) info.confirm = confirm
  return info
}

/**
 * Keeps the daemon's copy of the info current. `tick` sends the info when it differs from what the daemon last
 * accepted; `connected` forgets that, since a daemon that was away has lost it. A failed send is not remembered, so the
 * next tick retries it, and is told to `fail` once per distinct message.
 */
export function reporter(options: {
  read: () => Info
  send: (info: Info) => Promise<unknown>
  fail: (message: string) => void
}) {
  let accepted: string | undefined
  let failure: string | undefined
  let sending = false
  // A send that was under way when the stream reconnected reached the daemon that was there before.
  let connections = 0
  return {
    connected() {
      connections += 1
      accepted = undefined
    },
    async tick() {
      if (sending) return
      sending = true
      try {
        const info = options.read()
        const json = JSON.stringify(info)
        if (json === accepted) return
        const connection = connections
        await options.send(info)
        if (connection === connections) accepted = json
        failure = undefined
      } catch (error) {
        const message = String((error as Error).message ?? error)
        if (message !== failure) options.fail(message)
        failure = message
      } finally {
        sending = false
      }
    },
  }
}
