/**
 * The upgrade is one `fawkes up --json` call: its result keeps `existing` and `test`, its error object `code`, `step`,
 * `generation` and `log`. A stand-in binary prints what the real one does under `--json` and records its arguments.
 */
import assert from 'node:assert/strict'
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { up, UpFailure } from './up.ts'

/** A binary that records its arguments, prints `stdout` and `stderr`, and exits `code`. */
function fakeFawkes(dir: string, stdout: string, code: number, stderr = '') {
  const bin = join(dir, `fawkes-${code}`)
  writeFileSync(
    bin,
    `#!/bin/sh\nprintf '%s\\n' "$@" > '${dir}/args'\nprintf '%s' '${stderr}' >&2\nprintf '%s\\n' '${stdout}'\nexit ${code}\n`,
  )
  chmodSync(bin, 0o755)
  return bin
}

const failure = (promise: Promise<unknown>) =>
  promise.then(
    () => assert.fail('up succeeded'),
    (error: unknown) => {
      assert.ok(error instanceof UpFailure)
      return error.error
    },
  )

test("an upgrade runs fawkes up once and keeps the fields of its result and of its step's error", async () => {
  const dir = mkdtempSync(join(tmpdir(), 'fawkes-up-'))
  try {
    const result = {
      generation: 'g4',
      previous: 'g3',
      deadline: '2026-11-03T14:02:01Z',
      existing: true,
      test: { port: 4791, passed: true, at: 't', seconds: 2.1, message: 'healthy' },
    }
    assert.deepEqual(await up(fakeFawkes(dir, JSON.stringify(result), 0), '/s/fawkes.sock', '-odd/ref'), result)
    // One call, the socket named, and the ref after `--` so a ref that starts with `-` stays the ref.
    assert.deepEqual(readFileSync(join(dir, 'args'), 'utf8').trim().split('\n'), [
      'up',
      '--json',
      '--socket',
      '/s/fawkes.sock',
      '--',
      '-odd/ref',
    ])

    const tested = {
      code: 'health_failed',
      message: 'g4 failed its health check',
      generation: 'g4',
      log: '/f/generations/g4/logs/app.log',
      step: 'test',
    }
    assert.deepEqual(await failure(up(fakeFawkes(dir, JSON.stringify(tested), 1), '/s', 'main')), tested)
    const unreachable = { code: 'daemon_unreachable', message: 'no daemon answers on /s', step: 'stage' }
    assert.deepEqual(await failure(up(fakeFawkes(dir, JSON.stringify(unreachable), 3), '/s', 'main')), unreachable)

    // No error object: the end of stderr says why.
    const crashed = await failure(up(fakeFawkes(dir, '', 101, 'thread main panicked'), '/s', 'main'))
    assert.match(crashed.message, /up exited 101 without an answer: thread main panicked/)
    assert.equal(crashed.code, undefined)
    const missing = await failure(up(join(dir, 'no-such-fawkes'), '/s', 'main'))
    assert.match(missing.message, /cannot run .*no-such-fawkes/)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
