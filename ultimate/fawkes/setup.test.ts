/**
 * The manifest setup writes for a host, the spare ports it picks beside the manifests already there, the name it
 * derives and the manifest it recognises as this host's own.
 */
import assert from 'node:assert/strict'
import test from 'node:test'
import { buildManifest, findExisting, freeName, manifestName, readManifest, shellWord, spareBlock } from './setup.ts'
import type { SetupInput } from './setup.ts'

const host: SetupInput = {
  name: 'xana',
  project: '/Users/me/Code/xana',
  repo: '/Users/me/Code/ultimate',
  home: '/Users/me/.ultimate/projects/xana-0010d262',
  port: 4749,
  spare: [4790, 4799],
  extension: '/Users/me/Code/fawkes/ultimate/fawkes',
}

test('the manifest of a sample host has every field, absolute paths and bare templates', () => {
  assert.equal(
    buildManifest(host),
    `# xana's Ultimate host under Fawkes, written by init.ts of the Ultimate extension (ultimate/fawkes/README.md).
name = "xana"
project = "/Users/me/Code/xana"
repo = "/Users/me/Code/ultimate"
home = "/Users/me/.ultimate/projects/xana-0010d262"
port = 4749
spare_ports = [4790, 4799]
adopt = true
install = "pnpm install --frozen-lockfile"
reuse = ["node_modules"]
keep = 5
start = "node /Users/me/Code/fawkes/ultimate/fawkes/start.ts /Users/me/Code/xana"
deadline = 120
stop_timeout = 30

[health]
command = "node /Users/me/Code/fawkes/ultimate/fawkes/health.ts"
timeout = 120

[hooks]
snapshot = "node /Users/me/Code/fawkes/ultimate/fawkes/storage.ts snapshot {home} {snapshot}"
restore = "node /Users/me/Code/fawkes/ultimate/fawkes/storage.ts restore {snapshot} {home}"
test_copy = "node /Users/me/Code/fawkes/ultimate/fawkes/storage.ts test-copy {home} {test_home}"

[env]
ULTIMATE_HOME = "{home}"
ULTIMATE_PORT = "{port}"
`,
  )
})

test('a path with a space or a quote is one shell word in the command and an escaped TOML string', () => {
  assert.equal(shellWord('/a b/it\'s'), `'/a b/it'\\''s'`)
  const text = buildManifest({ ...host, project: '/Users/me/my "app"', extension: '/Users/me/fawkes ext' })
  assert.match(text, /^project = "\/Users\/me\/my \\"app\\""$/m)
  assert.match(text, /^start = "node '\/Users\/me\/fawkes ext\/start.ts' '\/Users\/me\/my \\"app\\"'"$/m)
  // What setup wrote is what it reads back.
  assert.equal(readManifest(text).project, '/Users/me/my "app"')
})

test('the name keeps letters, digits, underscore and hyphen of the folder name', () => {
  assert.equal(manifestName('/Users/me/Code/xana'), 'xana')
  assert.equal(manifestName('/Users/me/Code/My App.v2_x-y/'), 'MyAppv2_x-y')
  assert.equal(manifestName('/'), 'ultimate')
  assert.equal(manifestName('/Users/me/ünï'), 'n')
})

test('the spare block is the first of ten from 4790 that no other manifest touches, nor the host port', () => {
  assert.deepEqual(spareBlock([], 4747), [4790, 4799])
  // A range, a lone port and a manifest without spare_ports (which owns the ten after its port).
  const others = [
    readManifest('port = 4747\nspare_ports = [4790, 4799]\n'),
    readManifest('port = 4800\nspare_ports = [4801, 4805]\n'),
    readManifest('port = 4812\n'),
  ]
  assert.deepEqual(spareBlock(others, 4749), [4830, 4839])
  // A block is skipped for any overlap, a single port included, and for the host's own port.
  assert.deepEqual(spareBlock([{ port: 4795, spare: [4796, 4796] }], 4749), [4800, 4809])
  assert.deepEqual(spareBlock([], 4795), [4800, 4809])
  // A manifest read without a port takes nothing.
  assert.deepEqual(spareBlock([readManifest('# nothing')], 4747), [4790, 4799])
})

test('a manifest is read from its top level only', () => {
  const facts = readManifest(`# c
name = "xana"   # the app
home = '/h/xana'
port = 4749
spare_ports = [4790, 4799]

[env]
port = 1
name = "other"
`)
  assert.deepEqual(facts, { name: 'xana', home: '/h/xana', port: 4749, spare: [4790, 4799] })
})

test('an existing manifest is found by its project or its home, never by its file name alone', () => {
  const xana = { file: 'xana.toml', text: buildManifest(host) }
  // Hand-written: no `project`, only a home.
  const byHand = { file: 'ultimate.toml', text: `name = "ultimate"\nhome = "${host.home}"\nport = 4750\n` }
  const other = {
    file: 'other.toml',
    text: 'name = "other"\nproject = "/Users/me/Code/other"\nhome = "/Users/me/.ultimate/projects/other-1"\nport = 4751\n',
  }
  const me = { project: host.project, home: host.home }
  assert.equal(findExisting([other], me), undefined)
  assert.equal(findExisting([other, xana], me), xana)
  // Another project of the same folder name: its manifest is not this host's, and this one gets the next free name.
  const namesake = [{ file: 'xana.toml', text: 'name = "xana"\nproject = "/Users/me/play/xana"\n' }]
  assert.equal(findExisting(namesake, me), undefined)
  assert.equal(freeName(namesake, 'xana'), 'xana-2')
  assert.equal(freeName([...namesake, { file: 'xana-2.toml', text: '' }], 'xana'), 'xana-3')
  assert.equal(freeName(namesake, 'other'), 'other')
  // Same project under another file name.
  assert.equal(findExisting([other, { file: 'x2.toml', text: xana.text }], me)?.file, 'x2.toml')
  // Same home, no project.
  assert.equal(findExisting([other, byHand], me), byHand)
})
