/**
 * A client of the Fawkes daemon's Unix socket (DESIGN.md, "Socket protocol"): newline-delimited JSON, one request per
 * connection. `request` answers one command; `subscribe` keeps the live stream and reconnects with `since`.
 */
import { connect } from 'node:net'
import type { Socket } from 'node:net'
import type { FawkesEvent, Generation, Status } from './types.ts'

export class FawkesError extends Error {
  readonly code: string
  constructor(code: string, message: string) {
    super(message)
    this.code = code
  }
}

/** Split a byte stream into lines, each parsed as JSON; a line that is not JSON is skipped. */
function lines(socket: Socket, line: (value: Record<string, unknown>) => void) {
  let buffer = ''
  socket.setEncoding('utf8')
  socket.on('data', (chunk: string) => {
    buffer += chunk
    let index: number
    while ((index = buffer.indexOf('\n')) !== -1) {
      const text = buffer.slice(0, index)
      buffer = buffer.slice(index + 1)
      if (!text.trim()) continue
      try {
        line(JSON.parse(text) as Record<string, unknown>)
      } catch {
        // The daemon writes only JSON lines; a torn one means the connection is ending anyway.
      }
    }
  })
}

/**
 * Send one command and resolve its `result`, or reject with the daemon's error. `progress` gets each event the step
 * reports while it runs. `timeout` bounds silence on the connection (a stage's install can take minutes but reports).
 */
export function request<T>(
  socket: string,
  command: string,
  args?: Record<string, unknown>,
  options: { progress?: (event: FawkesEvent) => void; timeout?: number; signal?: AbortSignal } = {},
): Promise<T> {
  return new Promise((resolve, reject) => {
    const connection = connect(socket)
    let done = false
    const finish = (error?: Error, result?: T) => {
      if (done) return
      done = true
      connection.destroy()
      options.signal?.removeEventListener('abort', aborted)
      if (error) reject(error)
      else resolve(result as T)
    }
    const aborted = () => finish(new FawkesError('aborted', `${command} was cancelled`))
    options.signal?.addEventListener('abort', aborted, { once: true })
    connection.setTimeout(options.timeout ?? 10_000, () =>
      finish(new FawkesError('no_answer', `The Fawkes daemon did not answer ${command}`)),
    )
    connection.on('error', (error) =>
      finish(new FawkesError('daemon_unreachable', `No Fawkes daemon answers on ${socket}: ${error.message}`)),
    )
    connection.on('close', () =>
      finish(new FawkesError('daemon_unreachable', `The Fawkes daemon closed ${command} without an answer`)),
    )
    lines(connection, (line) => {
      if ('event' in line) options.progress?.(line.event as FawkesEvent)
      else if (line.ok === true) finish(undefined, line.result as T)
      else if (line.ok === false) {
        const error = line.error as { code: string; message: string }
        finish(new FawkesError(error.code, error.message))
      }
    })
    connection.write(JSON.stringify({ command, ...(args ? { args } : {}) }) + '\n')
  })
}

export type Stream = {
  status?: (status: Status) => void
  history?: (generations: Generation[]) => void
  event?: (event: FawkesEvent) => void
  connected?: (connected: boolean, error?: string) => void
}

/**
 * Follow the daemon: replay the last `limit` events, then `status` and `history`, then every change. A dropped
 * connection reconnects after `retry` ms with the last `seq` seen, so no event is lost or repeated. Returns a stop.
 */
export function subscribe(
  socket: string,
  handlers: Stream,
  options: { limit?: number; retry?: number } = {},
) {
  let stopped = false
  let since: number | undefined
  let current: Socket | undefined
  let timer: ReturnType<typeof setTimeout> | undefined
  const open = () => {
    if (stopped) return
    const connection = connect(socket)
    current = connection
    let failure: string | undefined
    connection.on('connect', () => {
      handlers.connected?.(true)
      const args = since === undefined ? { limit: options.limit ?? 50 } : { since }
      connection.write(JSON.stringify({ command: 'subscribe', args }) + '\n')
    })
    connection.on('error', (error) => (failure = error.message))
    connection.on('close', () => {
      if (current === connection) current = undefined
      if (stopped) return
      handlers.connected?.(false, failure)
      timer = setTimeout(open, options.retry ?? 2000)
    })
    lines(connection, (line) => {
      if ('event' in line) {
        const event = line.event as FawkesEvent
        if (event.seq) since = event.seq
        handlers.event?.(event)
      } else if ('status' in line) handlers.status?.(line.status as Status)
      else if ('history' in line)
        handlers.history?.((line.history as { generations: Generation[] }).generations)
    })
  }
  open()
  return () => {
    stopped = true
    clearTimeout(timer)
    current?.destroy()
  }
}
