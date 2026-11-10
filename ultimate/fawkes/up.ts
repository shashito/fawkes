/**
 * `fawkes up <ref> --json`: stage, test and switch in one call, as Fawkes composes them (retry-safe stage, each step's
 * refusals, `existing`, `test`, and `step` on a failure). Its stdout is one JSON document, the result or the error
 * object. Its progress lines on stderr are not read: every subscriber of the daemon gets the same events.
 *
 * In the live app the switch stops this process's group, the `up` child with it, so a successful `up` answers only
 * when it runs outside the app; the generation that runs next reads the outcome from the daemon.
 */
import { spawn } from 'node:child_process'
import type { UpError, UpResult } from './types.ts'

export class UpFailure extends Error {
  readonly error: UpError
  constructor(error: UpError) {
    super(error.message)
    this.error = error
  }
}

/** The end of a child's stderr kept for a failure that printed no error object (a binary that crashed). */
const TAIL = 2000

/** Run `bin up ref --json` against `socket`; the `up` result, or an `UpFailure` with its error object. */
export function up(bin: string, socket: string, ref: string): Promise<UpResult> {
  return new Promise((resolve, reject) => {
    // After `--`, a ref that starts with `-` is still the ref.
    const child = spawn(bin, ['up', '--json', '--socket', socket, '--', ref], { stdio: ['ignore', 'pipe', 'pipe'] })
    let stdout = ''
    let stderr = ''
    child.stdout.setEncoding('utf8').on('data', (chunk: string) => (stdout += chunk))
    child.stderr.setEncoding('utf8').on('data', (chunk: string) => (stderr = (stderr + chunk).slice(-TAIL)))
    child.on('error', (error) => reject(new UpFailure({ message: `cannot run ${bin}: ${error.message}` })))
    child.on('close', (code, signal) => {
      let answer: unknown
      try {
        answer = JSON.parse(stdout)
      } catch {
        const how = signal ? `was ended by ${signal}` : `exited ${code}`
        const tail = stderr.trim().split('\n').at(-1)
        return reject(new UpFailure({ message: `${bin} up ${how} without an answer${tail ? `: ${tail}` : ''}` }))
      }
      if (code === 0) resolve(answer as UpResult)
      else reject(new UpFailure(answer as UpError))
    })
  })
}
