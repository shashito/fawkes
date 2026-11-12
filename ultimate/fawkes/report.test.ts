import assert from 'node:assert/strict'
import test from 'node:test'
import { MAX_VALUE, reportInfo, reporter } from './report.ts'

const system = (origins: string[], url = 'http://127.0.0.1:4747') => ({ process: { url }, access: { origins } })

test('url and log, and no remote while there are no origins', () => {
  assert.deepEqual(reportInfo(system([]), '/h/project'), {
    url: 'http://127.0.0.1:4747',
    log: '/h/project/supervisor.log',
  })
})

test('one origin is the remote', () => {
  assert.equal(reportInfo(system(['https://mac.tail.ts.net']), '/h').remote, 'https://mac.tail.ts.net')
})

test('several origins are joined, and extras that would exceed the limit are dropped', () => {
  const two = ['https://a.ts.net', 'https://b.ts.net']
  assert.equal(reportInfo(system(two), '/h').remote, 'https://a.ts.net, https://b.ts.net')
  const many = Array.from({ length: 40 }, (_, i) => `https://host-${i}.tail.ts.net`)
  const remote = reportInfo(system(many), '/h').remote
  assert.ok(remote.length <= MAX_VALUE)
  assert.ok(remote.startsWith('https://host-0.tail.ts.net, https://host-1.tail.ts.net'))
  assert.ok(many.some((origin) => !remote.includes(origin)))
})

test('a first origin that is too long alone leaves the key out', () => {
  assert.ok(!('remote' in reportInfo(system(['https://' + 'a'.repeat(300)]), '/h')))
})

test('the reporter sends on the first tick, on a change and after a reconnect, not otherwise', async () => {
  let info = { url: 'u' }
  const sent: unknown[] = []
  const r = reporter({ read: () => info, send: async (i) => void sent.push(i), fail: () => assert.fail() })
  await r.tick()
  await r.tick()
  assert.equal(sent.length, 1)
  info = { url: 'v' }
  await r.tick()
  assert.equal(sent.length, 2)
  r.connected()
  await r.tick()
  assert.deepEqual(sent, [{ url: 'u' }, { url: 'v' }, { url: 'v' }])
})

test('a failed report is retried on the next tick and told once per message', async () => {
  let fails = 2
  const failed: string[] = []
  let sends = 0
  const r = reporter({
    read: () => ({ url: 'u' }),
    send: async () => {
      sends += 1
      if (fails-- > 0) throw new Error('invalid_state')
    },
    fail: (message) => failed.push(message),
  })
  await r.tick()
  await r.tick()
  await r.tick()
  await r.tick()
  assert.equal(sends, 3)
  assert.deepEqual(failed, ['invalid_state'])
})
