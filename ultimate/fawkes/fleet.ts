/**
 * `fawkes status --all --json`: the supervisor and every app of the Fawkes home, for the `fawkes` tool's `fleet` action.
 * The binary puts the view together (`run_status_all` in `src/main.rs`); this reads its one JSON document and says it
 * in a line per app, as `render_fleet` in `src/render.rs` does for a terminal.
 */
import { execFile } from 'node:child_process'
import type { FleetApp, FleetView, Status } from './types.ts'

/**
 * Run `bin status --all --json`; its view, or an error with its message. A host's process inherits `FAWKES_SOCKET` and
 * `FAWKES_MANIFEST`, which name its own daemon; `--all` takes the whole Fawkes home instead, so neither is passed on.
 */
export function fleet(bin: string, env: NodeJS.ProcessEnv = process.env, timeout = 30_000): Promise<FleetView> {
  const own = { ...env }
  delete own.FAWKES_SOCKET
  delete own.FAWKES_MANIFEST
  return new Promise((resolve, reject) => {
    execFile(bin, ['status', '--all', '--json'], { env: own, timeout }, (error, stdout) => {
      let answer: unknown
      try {
        answer = JSON.parse(stdout)
      } catch {
        return reject(new Error(`${bin} status --all answered no JSON${error ? `: ${error.message}` : ''}`))
      }
      // Under `--json` a failure is one error object, `{ code, message }`.
      if (error) return reject(new Error((answer as { message?: string }).message ?? error.message))
      const view = answer as FleetView
      if (!Array.isArray(view?.apps)) return reject(new Error(`${bin} status --all answered no apps`))
      resolve(view)
    })
  })
}

/** What the tool says: the supervisor, then a line per app; `own` (this host's app name) is marked. */
export function describeFleet(view: FleetView, own?: string): string {
  const s = view.supervisor
  const head = s
    ? `supervisor pid ${s.pid}, fawkes ${s.version}, ${s.apps.length} app${s.apps.length === 1 ? '' : 's'}`
    : 'no supervisor runs; `fawkes service install` or `fawkes up --all` starts one'
  const lines = view.apps.map((app) => {
    const down = !app.status && app.supervisor?.state === 'down'
    const facts = app.status
      ? running(app.status)
      : [down ? 'down' : app.error?.code === 'daemon_unreachable' ? 'no daemon' : 'no answer']
    if (app.supervisor && !down) facts.push(supervision(app.supervisor))
    if (!app.status && app.error) facts.push(app.error.message)
    return `- ${app.name}${app.name === own ? ' (this host)' : ''}: ${facts.join(', ')}`
  })
  return [head, ...lines].join('\n')
}

/** A daemon that answers: its process, its build, and the switch or step under way. */
function running(status: Status): string[] {
  const p = status.process
  const facts = [p.state, p.generation ?? 'no generation']
  if (p.pid) facts.push(`pid ${p.pid}`)
  facts.push(`port ${p.port}`)
  if (status.version) facts.push(`fawkes ${status.version}`)
  if (status.step && status.pending) facts.push(`${status.step.replace('_', ' ')} ${status.pending.generation}`)
  if (status.busy) facts.push(`busy: ${status.busy}`)
  return facts
}

/** What the supervisor does with an app; a failing daemon says when it starts again. */
function supervision(app: FleetApp): string {
  if (app.state === 'running') return 'supervised'
  if (app.state === 'backoff')
    return `backoff after ${app.failures} failure${app.failures === 1 ? '' : 's'}${app.restart_at ? `, again at ${app.restart_at}` : ''}`
  return app.state
}
