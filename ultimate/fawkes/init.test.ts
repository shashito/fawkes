/**
 * `init.ts` run as a user runs it, on a fresh project in a throwaway home, against the Ultimate clone in `$ULTIMATE`
 * (`ultimate/check` sets it): all four pieces are written, and a second run changes nothing.
 */
import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, realpathSync, rmSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import test from 'node:test'
import { fileURLToPath, pathToFileURL } from 'node:url'

const folder = realpathSync(dirname(fileURLToPath(import.meta.url)))
const clone = process.env.ULTIMATE ?? join(homedir(), 'Code/ultimate')

test('init writes the manifest, the link, update off and the trust, and a second run changes nothing', async (t) => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fkinit-')))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const home = join(root, 'home')
  const project = join(root, 'proj')
  mkdirSync(home)
  // Only these reach the child: the variables an app under Fawkes or a running Ultimate exports must not.
  const env = {
    PATH: process.env.PATH ?? '',
    HOME: home,
    ULTIMATE_USER_HOME: home,
    FAWKES_HOME: join(home, '.fawkes'),
    GIT_CONFIG_GLOBAL: '/dev/null',
  }
  execFileSync('git', ['init', '-q', project], { env })
  const init = () =>
    spawnSync(process.execPath, [join(folder, 'init.ts'), '--ultimate', clone, '--port', '24747', project], {
      env,
      encoding: 'utf8',
    })
  const files = () => {
    const manifests = readdirSync(env.FAWKES_HOME)
    return {
      manifests,
      manifest: readFileSync(join(env.FAWKES_HOME, manifests[0]), 'utf8'),
      link: readlinkSync(join(project, '.ultimate/extensions/fawkes')),
      settings: readFileSync(join(project, '.ultimate/settings.json'), 'utf8'),
      trust: readFileSync(join(home, '.ultimate/trust.json'), 'utf8'),
    }
  }

  const first = init()
  assert.equal(first.status, 0, first.stderr)
  assert.deepEqual(
    first.stdout.split('\n').filter((line) => line.startsWith('wrote ')).map((line) => line.split(' ')[1]),
    ['manifest', 'link', 'update', 'trust'],
  )
  const written = files()
  const { projectHome } = await import(pathToFileURL(join(clone, 'server/project.ts')).href)
  assert.equal(written.manifests.length, 1)
  assert.match(written.manifest, /^adopt = true$/m)
  assert.match(written.manifest, new RegExp(`^project = ${JSON.stringify(project)}$`, 'm'))
  assert.match(written.manifest, new RegExp(`^home = ${JSON.stringify(projectHome(project, home))}$`, 'm'))
  assert.match(written.manifest, /^port = 24747$/m)
  assert.equal(realpathSync(join(project, '.ultimate/extensions/fawkes')), folder)
  assert.deepEqual(JSON.parse(written.settings), { extensions: { disable: ['update'] } })
  assert.deepEqual(JSON.parse(written.trust), { [project]: true })

  const second = init()
  assert.equal(second.status, 0, second.stderr)
  assert.deepEqual(
    second.stdout.split('\n').filter((line) => /^(wrote|already) /.test(line)).map((line) => line.split(' ')[0]),
    ['already', 'already', 'already', 'already'],
  )
  assert.deepEqual(files(), written)
})
