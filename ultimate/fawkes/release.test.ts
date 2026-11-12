/**
 * The release check decided from the tag GitHub's redirect names and the daemon's and the installed binary's
 * versions: when a release is newer, when only a restart is due, and when there is nothing to say.
 */
import assert from 'node:assert/strict'
import test from 'node:test'
import { INSTALL_LINE, newer, releaseNote, releaseText, restartCommand, tagOf, upgradeCommand } from './release.ts'
import { shellWord } from './setup.ts'
import type { Release, Status } from './types.ts'

const status = (version: string | undefined) =>
  ({ name: 'xana', version, manifest: '/Users/me/.fawkes/xana.toml' }) as Status

const release = (latest: string | null, installed?: string): Release => ({
  latest,
  installed,
  command: INSTALL_LINE,
  restart: 'fawkes down --manifest /Users/me/.fawkes/xana.toml && fawkes up --manifest /Users/me/.fawkes/xana.toml',
  checkedAt: 1,
})

test('versions compare by number, and a prerelease comes before its release', () => {
  assert.ok(newer('0.10.0', '0.9.9'))
  assert.ok(newer('v1.0.0', '0.99.0'))
  assert.ok(newer('0.2.0', '0.2.0-rc.1'))
  assert.ok(!newer('0.2.0-rc.1', '0.2.0'))
  assert.ok(!newer('0.2.0', '0.2.0'))
  assert.ok(!newer('0.1.0', '0.2.0'))
  assert.ok(!newer('nightly', '0.1.0'))
  assert.ok(!newer('0.2.0', undefined))
})

test('the redirect names the latest tag; no release yet redirects to the releases page', () => {
  assert.equal(tagOf(302, 'https://github.com/shashito/fawkes/releases/tag/v0.2.0'), 'v0.2.0')
  assert.equal(tagOf(302, 'https://github.com/shashito/fawkes/releases'), null)
  assert.throws(() => tagOf(200, null))
  assert.throws(() => tagOf(404, null))
})

test('a newer release than the installed binary names the command, then the restart', () => {
  assert.deepEqual(releaseNote(release('v0.2.0', '0.1.0'), status('0.1.0')), {
    kind: 'available',
    version: '0.2.0',
    daemon: '0.1.0',
  })
  assert.equal(
    releaseText(release('v0.2.0', '0.1.0'), status('0.1.0')),
    `Fawkes 0.2.0 is available (the daemon runs 0.1.0): upgrade with \`${INSTALL_LINE}\`, then restart the daemon from a shell outside this Ultimate, once no switch is pending (a daemon restart rolls it back): \`fawkes down --manifest /Users/me/.fawkes/xana.toml && fawkes up --manifest /Users/me/.fawkes/xana.toml\`.`,
  )
  // Without the installed version, the daemon's stands in.
  assert.equal(releaseNote(release('v0.2.0'), status('0.1.0'))?.kind, 'available')
})

test('an installed binary newer than the daemon asks only for the restart', () => {
  assert.deepEqual(releaseNote(release('v0.2.0', '0.2.0'), status('0.1.0')), {
    kind: 'restart',
    version: '0.2.0',
    daemon: '0.1.0',
  })
  // Offline: GitHub never answered, the installed binary still says it.
  assert.equal(releaseNote(release(null, '0.2.0'), status('0.1.0'))?.kind, 'restart')
})

test('nothing to say when up to date, without a release, or ahead of it', () => {
  assert.equal(releaseNote(release('v0.1.0', '0.1.0'), status('0.1.0')), undefined)
  assert.equal(releaseNote(release(null, '0.1.0'), status('0.1.0')), undefined)
  assert.equal(releaseNote(release('v0.1.0', '0.2.0-rc.1'), status('0.2.0-rc.1')), undefined)
  assert.equal(releaseNote(release('v0.2.0', '0.1.0'), status(undefined)), undefined)
  assert.equal(releaseNote(undefined, status('0.1.0')), undefined)
})

test('the commands fit the install: curl or a rebuild, launchd or down and up', () => {
  assert.equal(upgradeCommand(undefined, shellWord), INSTALL_LINE)
  assert.equal(
    upgradeCommand('/Users/me/Code/my fawkes', shellWord),
    "cd '/Users/me/Code/my fawkes' && git pull && cargo install --path . --locked",
  )
  assert.equal(
    restartCommand('/m/xana.toml', true, '/u/.local/bin/fawkes', shellWord),
    'fawkes service restart --manifest /m/xana.toml',
  )
  assert.equal(
    restartCommand('xana', false, '/u/.local/bin/fawkes', shellWord),
    '/u/.local/bin/fawkes down --manifest xana && /u/.local/bin/fawkes up --manifest xana',
  )
  assert.equal(restartCommand('xana', false, undefined, shellWord), 'fawkes down --manifest xana && fawkes up --manifest xana')
})
