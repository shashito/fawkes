/**
 * The environment of a test run: Ultimate's own preview setup, from the generation under test, so it never falls
 * behind what Ultimate itself considers safe for a copy. `instance` is that generation's
 * `extensions/instance/server.ts`; its helpers are called, never copied.
 */
import { existsSync } from 'node:fs'
import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { execFileSync } from 'node:child_process'

export type InstanceHelpers = {
  seedUserHome(from: string, to: string): Promise<void>
  linkHome(from: string, to: string): Promise<void>
  previewBranch(home: string): string
  guardConfig(branch: string): string
  previewEnv(
    parent: NodeJS.ProcessEnv,
    options: {
      home: string
      project: string
      userHome: string
      ownerPid: number
      git?: { common: string; branch: string; config: string }
    },
  ): NodeJS.ProcessEnv
}

const git = (cwd: string, ...args: string[]) =>
  execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim()

/**
 * `home` is the storage copy Fawkes made (`{home}` in the test phase); the private user home goes inside it, so it is
 * removed with it. The project gets a branch of the preview's own prefix, the only kind the guard lets it move.
 */
export async function testEnv(
  instance: InstanceHelpers,
  options: { parent: NodeJS.ProcessEnv; home: string; project: string; userHome: string },
): Promise<NodeJS.ProcessEnv> {
  const { home, project, userHome: realUserHome, parent } = options
  const userHome = join(home, 'user')
  if (!existsSync(userHome)) {
    await mkdir(userHome, { recursive: true })
    await instance.seedUserHome(realUserHome, userHome)
  }
  await instance.linkHome(realUserHome, userHome)
  let guard: { common: string; branch: string; config: string } | undefined
  const top = (() => {
    try {
      return git(project, 'rev-parse', '--show-toplevel')
    } catch {
      // Not a repository: there is nothing for the guard to protect.
      return undefined
    }
  })()
  if (top) {
    const branch = instance.previewBranch(home)
    if (!git(project, 'branch', '--list', branch)) git(project, 'branch', branch)
    const common = git(project, 'rev-parse', '--path-format=absolute', '--git-common-dir')
    const config = join(home, 'preview.gitconfig')
    await writeFile(config, instance.guardConfig(branch))
    guard = { common, branch, config }
  }
  const env = instance.previewEnv(parent, {
    home,
    project,
    userHome,
    // The preview lives as long as this wrapper: Fawkes stops the whole group at the end of the test.
    ownerPid: process.pid,
    git: guard,
  })
  // previewEnv picks a free port; the test checks the spare port Fawkes gave it.
  env.ULTIMATE_PORT = parent.ULTIMATE_PORT
  return env
}
