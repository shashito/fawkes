/**
 * The `fleet` action is one `fawkes status --all --json` call, without the host's `FAWKES_SOCKET` and `FAWKES_MANIFEST`,
 * read as the `FleetView` it prints. A stand-in binary records its arguments and environment and prints a view.
 */
import assert from 'node:assert/strict'
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { describeFleet, fleet } from './fleet.ts'
import type { FleetView, Status } from './types.ts'

/** A binary that records its arguments and environment, prints `stdout` and exits `code`. */
function fakeFawkes(dir: string, stdout: string, code = 0) {
  const bin = join(dir, `fawkes-${code}`)
  writeFileSync(bin, `#!/bin/sh\nprintf '%s\\n' "$@" > '${dir}/args'\nenv > '${dir}/env'\ncat <<'EOF'\n${stdout}\nEOF\nexit ${code}\n`)
  chmodSync(bin, 0o755)
  return bin
}

const status = (over: Partial<Status>): Status => ({
  name: 'fawkes',
  daemon_pid: 40,
  permanent: 'g6',
  current: 'g6',
  pending: null,
  step: null,
  busy: null,
  version: '0.4.0',
  process: { state: 'running', generation: 'g6', pid: 41, port: 4747, started_at: null, uptime: 9, restarts: 0, restart_at: null },
  ...over,
})

test('fleet runs status --all --json without the host socket and reads the view it prints', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'fawkes-fleet-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const apps = [
    { name: 'fawkes', manifest: '/f/fawkes.toml', state: 'running', daemon_pid: 40, failures: 0 },
    { name: 'xana', manifest: '/f/xana.toml', state: 'backoff', restart_at: '2026-11-03T14:02:01Z', failures: 2 },
    { name: 'kept', manifest: '/f/kept.toml', state: 'down', failures: 0 },
  ] as const
  const view: FleetView = {
    supervisor: { pid: 7, version: '0.4.0', socket: '/f/supervisor.sock', apps: [...apps] },
    apps: [
      {
        name: 'fawkes',
        supervisor: apps[0],
        status: status({ pending: { generation: 'g7', previous: 'g6', deadline: 'd' }, step: 'rolling_back' }),
        error: null,
      },
      { name: 'xana', supervisor: apps[1], status: null, error: { code: 'no_answer', message: 'xana did not answer' } },
      { name: 'kept', supervisor: apps[2], status: null, error: { code: 'daemon_unreachable', message: 'no daemon answers' } },
      { name: 'old', supervisor: null, status: null, error: { code: 'daemon_unreachable', message: 'no daemon of old' } },
    ],
  }
  const env = { PATH: process.env.PATH, FAWKES_SOCKET: '/f/run/fawkes.sock', FAWKES_MANIFEST: '/f/fawkes.toml', FAWKES_HOME: '/f' }
  assert.deepEqual(await fleet(fakeFawkes(dir, JSON.stringify(view)), env), view)
  assert.deepEqual(readFileSync(join(dir, 'args'), 'utf8').trim().split('\n'), ['status', '--all', '--json'])
  const seen = readFileSync(join(dir, 'env'), 'utf8')
  assert.doesNotMatch(seen, /^FAWKES_(SOCKET|MANIFEST)=/m)
  assert.match(seen, /^FAWKES_HOME=\/f$/m)

  assert.equal(
    describeFleet(view, 'fawkes'),
    [
      'supervisor pid 7, fawkes 0.4.0, 3 apps',
      '- fawkes (this host): running, g6, pid 41, port 4747, fawkes 0.4.0, rolling back g7, supervised',
      '- xana: no answer, backoff after 2 failures, again at 2026-11-03T14:02:01Z, xana did not answer',
      '- kept: down, no daemon answers',
      '- old: no daemon, no daemon of old',
    ].join('\n'),
  )
  assert.match(describeFleet({ supervisor: null, apps: [] }), /^no supervisor runs; `fawkes service install`/)

  // Under --json a failure is one error object: its message is the error.
  const usage = { code: 'usage', message: '`fawkes status --all` takes the supervisor and every app' }
  await assert.rejects(fleet(fakeFawkes(dir, JSON.stringify(usage), 2), env), { message: usage.message })
  await assert.rejects(fleet(fakeFawkes(dir, 'thread main panicked', 101), env), /answered no JSON/)
})
