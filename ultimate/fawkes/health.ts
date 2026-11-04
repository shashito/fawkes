#!/usr/bin/env node
/**
 * The manifest's `health.command`: healthy once the page on `ULTIMATE_PORT` answers below 400 and, in a test run, the
 * fawkes extension has held the scheduler (it writes `fawkes-held` into the home). A test run whose hold failed, or
 * whose extension never loaded, never passes, so a generation that breaks the hold never goes live.
 *
 * Usage: `node <this file>`, with the manifest's `[env]` (`ULTIMATE_HOME`, `ULTIMATE_PORT`) and Fawkes's `FAWKES_PHASE`.
 */
import { statSync } from 'node:fs'
import { join } from 'node:path'
import { HELD_MARKER } from './role.ts'

const { ULTIMATE_HOME: home, ULTIMATE_PORT: port, FAWKES_PHASE: phase } = process.env
if (!home || !port) {
  console.error('health.ts needs ULTIMATE_HOME and ULTIMATE_PORT')
  process.exit(2)
}
if (phase === 'test') {
  try {
    statSync(join(home, HELD_MARKER))
  } catch {
    console.error('The fawkes extension has not held the scheduler yet')
    process.exit(1)
  }
}
const status = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(5000) }).then(
  (response) => response.status,
  () => 0,
)
process.exit(status > 0 && status < 400 ? 0 : 1)
