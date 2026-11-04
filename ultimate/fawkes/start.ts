#!/usr/bin/env node
/**
 * The manifest's `start`: runs Ultimate from the generation's worktree (the working directory Fawkes starts it in).
 *
 * - `FAWKES_PHASE=live`: Ultimate's own supervisor, `bin/ultimate.ts <project> --no-open`, unchanged.
 * - `FAWKES_PHASE=test`: the core alone (`server/main.ts`), set up the way Ultimate sets up a preview, with the
 *   helpers of the generation under test (`extensions/instance/server.ts`). Fawkes gives the test a copy of the
 *   storage, not of the rest: so it gets a private user home with everything but `.ultimate` linked back, a git guard
 *   that never pushes and moves only its own branches, and `ULTIMATE_INSTANCE_OWNER`, which turns off what a preview
 *   skips. The fawkes extension holds the scheduler there, so nothing in the copied storage resumes.
 *
 * Both name the home they prepared in `FAWKES_ULTIMATE_HOME`: the extension acts only in the Ultimate whose home that
 * is, never in a preview or test server that inherited Fawkes's variables from it (`role.ts`).
 *
 * Usage: `node <this file> <project>`.
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdir, realpath } from 'node:fs/promises'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { testEnv } from './test-env.ts'

const [project] = process.argv.slice(2)
if (!project) {
  console.error('Usage: node start.ts <project>')
  process.exit(2)
}
const code = process.cwd()
const phase = process.env.FAWKES_PHASE

// Until the child runs, a signal ends this process at once; from then on it waits for the child (below).
const signals = ['SIGTERM', 'SIGINT', 'SIGHUP'] as const
let child: ReturnType<typeof spawn> | undefined
let cleanup = async () => {}
for (const signal of signals)
  process.on(signal, () => {
    if (child) return
    void cleanup().finally(() => process.exit(1))
  })

let command: string[]
let env: NodeJS.ProcessEnv
const given = process.env.ULTIMATE_HOME
if (!given) throw new Error('start.ts needs ULTIMATE_HOME: set it to {home} in the manifest')
await mkdir(given, { recursive: true })
const home = await realpath(given)
if (phase === 'test') {
  const instance = await import(pathToFileURL(join(code, 'extensions/instance/server.ts')).href)
  const { cleanPreview } = await import(
    pathToFileURL(join(code, 'extensions/instance/cleanup.ts')).href
  )
  // After a test run: remove what the preview registered in the project (its lander checkout, its branch).
  cleanup = () => cleanPreview(home)
  env = await testEnv(instance, {
    parent: process.env,
    home,
    project: await realpath(resolve(project)),
    userHome: process.env.ULTIMATE_USER_HOME ?? homedir(),
  })
  command = [process.execPath, join(code, 'server/main.ts')]
} else {
  env = { ...process.env }
  command = [process.execPath, join(code, 'bin/ultimate.ts'), resolve(project), '--no-open']
}
env.FAWKES_ULTIMATE_HOME = home
if (!existsSync(command[1])) throw new Error(`${command[1]} is missing: is ${code} an Ultimate worktree?`)

// Fawkes signals the whole process group, so the child gets every SIGTERM itself. This process stays until the child
// has shut down (Fawkes waits on it, its group leader), cleans up after a test run, and then ends the same way.
// The manifest's `stop_timeout` must leave room for both before Fawkes's SIGKILL.
child = spawn(command[0], command.slice(1), { cwd: code, env, stdio: 'inherit' })
child.on('exit', (exitCode, signal) => {
  void cleanup()
    .catch((error: unknown) => console.error(`Cleaning up the test run failed: ${String(error)}`))
    .finally(() => {
      if (signal) {
        process.removeAllListeners(signal)
        process.kill(process.pid, signal)
      } else process.exit(exitCode ?? 1)
    })
})
