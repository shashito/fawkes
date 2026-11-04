/**
 * A test run's environment comes from the generation's own preview helpers: a private user home with the rest linked
 * back, the git guard on a branch of the preview's prefix, and `ULTIMATE_INSTANCE_OWNER`. Runs against the clone in
 * `$ULTIMATE` (default ~/Code/ultimate), so a change there that breaks the isolation fails here.
 */
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'
import { testEnv } from './test-env.ts'

const clone = process.env.ULTIMATE ?? join(homedir(), 'Code/ultimate')

test('a test run gets a private user home, the git guard and an owner, like a preview', async () => {
  const instance = await import(pathToFileURL(join(clone, 'extensions/instance/server.ts')).href)
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fawkes-ext-')))
  try {
    const userHome = join(root, 'me')
    mkdirSync(join(userHome, '.ultimate'), { recursive: true })
    writeFileSync(join(userHome, '.ultimate/settings.json'), '{"real": true}')
    mkdirSync(join(userHome, '.ssh'))
    const project = join(root, 'project')
    mkdirSync(project)
    const git = (...args: string[]) =>
      execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@example.com', ...args], { cwd: project })
    git('init', '-q', '-b', 'main')
    git('commit', '-q', '--allow-empty', '-m', 'first')
    const home = join(root, 'test-home')
    mkdirSync(home)

    const env = await testEnv(instance, {
      parent: { PATH: process.env.PATH, ULTIMATE_PORT: '4791', FAWKES_PHASE: 'test' },
      home,
      project,
      userHome,
    })

    const privateHome = join(home, 'user')
    assert.equal(env.HOME, privateHome)
    assert.equal(env.ULTIMATE_USER_HOME, privateHome)
    assert.equal(env.ULTIMATE_HOME, home)
    assert.equal(env.ULTIMATE_PORT, '4791')
    assert.equal(env.ULTIMATE_INSTANCE_OWNER, String(process.pid))
    // ~/.ultimate is a copy, so the test run's writes never reach the real one; the rest is linked back.
    assert.equal(lstatSync(join(privateHome, '.ultimate')).isSymbolicLink(), false)
    assert.equal(readFileSync(join(privateHome, '.ultimate/settings.json'), 'utf8'), '{"real": true}')
    assert.equal(lstatSync(join(privateHome, '.ssh')).isSymbolicLink(), true)
    // The guard: pushes go nowhere, and only the preview's own branches move.
    const config = readFileSync(join(home, 'preview.gitconfig'), 'utf8')
    assert.match(config, /pushInsteadOf/)
    assert.match(config, /reference-transaction/)
    assert.equal(env.ULTIMATE_PREVIEW_BRANCH, instance.previewBranch(home))
    assert.ok(git('branch', '--list', env.ULTIMATE_PREVIEW_BRANCH!).toString().trim())
    assert.ok(existsSync(join(home, 'preview.gitconfig')))
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})
