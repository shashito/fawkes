/**
 * The environment of a test run: Ultimate's own preview setup, from the generation under test, so it follows what
 * Ultimate itself considers safe for a copy. `instance` is that generation's `extensions/instance/server.ts`: its
 * helpers are called, and the git part mirrors its private `previewGit` (same checks, same paths), which it does not
 * export.
 */
import { existsSync } from 'node:fs'
import { mkdir, realpath, writeFile } from 'node:fs/promises'
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

const git = (cwd: string, ...args: string[]) => {
  try {
    return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim()
  } catch {
    // previewGit's checks read a failure as "no": not a repository, no branch, no commit.
    return ''
  }
}

/** previewGit: a checked-out branch with a commit gets the preview branch and the guard; anything else none. */
async function previewGit(instance: InstanceHelpers, project: string, home: string) {
  const top = git(project, 'rev-parse', '--show-toplevel')
  if (!top || !git(top, 'symbolic-ref', '--quiet', '--short', 'HEAD')) return undefined
  if (!git(top, 'rev-parse', '--verify', '--quiet', 'HEAD')) return undefined
  const branch = instance.previewBranch(home)
  if (!git(project, 'branch', '--list', branch)) git(project, 'branch', branch)
  const common = git(project, 'rev-parse', '--path-format=absolute', '--git-common-dir')
  const config = join(home, 'preview.gitconfig')
  await writeFile(config, instance.guardConfig(branch))
  return { common: await realpath(common), branch, config }
}

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
  const env = instance.previewEnv(parent, {
    home,
    project,
    userHome,
    // The preview lives as long as this wrapper: Fawkes stops the whole group at the end of the test.
    ownerPid: process.pid,
    git: await previewGit(instance, project, home),
  })
  // previewEnv asks for any free port (0); the test checks the spare port Fawkes gave it.
  env.ULTIMATE_PORT = parent.ULTIMATE_PORT
  return env
}
