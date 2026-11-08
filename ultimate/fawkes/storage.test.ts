/**
 * The storage hooks against real WAL databases in a temp home: a snapshot holds rows that are still only in the
 * `-wal`, a restore is whole or refused and can run again, modes survive, and a copy of a database being written is
 * consistent. The script runs as a child process, as Fawkes runs it, so the exit code is what is checked.
 */
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { chmodSync, existsSync, mkdirSync, mkdtempSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { DatabaseSync } from 'node:sqlite'

const script = join(import.meta.dirname, 'storage.ts')

const hook = (...args: string[]) => spawnSync(process.execPath, [script, ...args], { encoding: 'utf8' })

/** A temp world with a home, a snapshot directory and a test home, removed afterwards. */
async function world(body: (w: { home: string; snap: string; copy: string }) => void | Promise<void>) {
  const root = mkdtempSync(join(tmpdir(), 'fawkes-storage-'))
  const w = { home: join(root, 'home'), snap: join(root, 'snap'), copy: join(root, 'copy') }
  for (const dir of Object.values(w)) mkdirSync(dir)
  try {
    await body(w)
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
}

/** A WAL database whose writes stay in the `-wal` because the connection never checkpoints; keep it open. */
function walDatabase(path: string, rows: number) {
  const db = new DatabaseSync(path)
  db.exec('PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t (n INTEGER)')
  for (let n = 0; n < rows; n++) db.exec(`INSERT INTO t VALUES (${n})`)
  return db
}

const count = (path: string) => {
  const db = new DatabaseSync(path, { readOnly: true })
  try {
    return (db.prepare('SELECT count(*) AS c FROM t').get() as { c: number }).c
  } finally {
    db.close()
  }
}

const mode = (path: string) => statSync(path).mode & 0o777

test('a snapshot holds the rows that are only in the -wal, and a restore brings back exactly those', async () => {
  await world(({ home, snap }) => {
    const live = walDatabase(join(home, 'state.sqlite'), 3)
    assert.ok(statSync(join(home, 'state.sqlite-wal')).size > 0)
    assert.equal(hook('snapshot', home, snap).status, 0)

    live.exec('INSERT INTO t VALUES (99)')
    const later = walDatabase(join(home, 'trace.sqlite'), 1)
    for (const suffix of ['-wal', '-shm']) assert.ok(existsSync(join(home, `trace.sqlite${suffix}`)))
    writeFileSync(join(home, 'trace.sqlite-journal'), 'x')

    const restored = hook('restore', snap, home)
    assert.equal(restored.status, 0, restored.stderr)
    assert.equal(count(join(home, 'state.sqlite')), 3)
    for (const suffix of ['', '-wal', '-shm', '-journal']) {
      assert.equal(existsSync(join(home, `trace.sqlite${suffix}`)), false, `trace.sqlite${suffix} survived`)
    }
    live.close()
    later.close()
  })
})

test('a restore without a usable snapshot fails and touches nothing; run again with the snapshot back, it succeeds', async () => {
  await world(({ home, snap }) => {
    const live = walDatabase(join(home, 'state.sqlite'), 3)
    assert.equal(hook('snapshot', home, snap).status, 0)
    live.exec('INSERT INTO t VALUES (99)')
    live.close()
    const before = { size: statSync(join(home, 'state.sqlite')).size, wal: existsSync(join(home, 'state.sqlite-wal')) }

    const aside = `${snap}.aside`
    renameSync(snap, aside)
    for (const missing of [snap, aside]) {
      if (missing === aside) {
        // No index: the directory exists, the index is not there.
        mkdirSync(snap)
        renameSync(join(aside, 'state.sqlite'), join(snap, 'state.sqlite'))
      }
      const failed = hook('restore', snap, home)
      assert.notEqual(failed.status, 0)
      assert.match(failed.stderr, /no usable snapshot/)
      assert.equal(statSync(join(home, 'state.sqlite')).size, before.size)
      assert.equal(existsSync(join(home, 'state.sqlite-wal')), before.wal)
    }
    rmSync(snap, { recursive: true })
    renameSync(aside, snap)

    // A corrupt index and a file that is not one of the two are refused too.
    writeFileSync(join(snap, 'index.json'), '{ nope')
    assert.notEqual(hook('restore', snap, home).status, 0)
    writeFileSync(join(snap, 'index.json'), JSON.stringify({ files: [{ name: '../x', present: false, mode: 0 }] }))
    assert.match(hook('restore', snap, home).stderr, /not one of/)
    assert.equal(statSync(join(home, 'state.sqlite')).size, before.size)
  })
})

test('a restore retried after a failure succeeds', async () => {
  await world(({ home, snap }) => {
    const live = walDatabase(join(home, 'state.sqlite'), 3)
    assert.equal(hook('snapshot', home, snap).status, 0)
    live.exec('INSERT INTO t VALUES (99)')
    live.close()
    const snapshotFile = join(snap, 'state.sqlite')
    const kept = `${snapshotFile}.kept`
    renameSync(snapshotFile, kept)

    const failed = hook('restore', snap, home)
    assert.notEqual(failed.status, 0)
    assert.match(failed.stderr, /incomplete: state\.sqlite is missing/)
    assert.equal(count(join(home, 'state.sqlite')), 4)

    renameSync(kept, snapshotFile)
    assert.equal(hook('restore', snap, home).status, 0)
    assert.equal(count(join(home, 'state.sqlite')), 3)
    assert.equal(hook('restore', snap, home).status, 0)
    assert.equal(existsSync(join(home, 'state.sqlite.tmp')), false)
  })
})

test('a file keeps its mode through snapshot and restore, a read-only one included', async () => {
  await world(({ home, snap }) => {
    const state = join(home, 'state.sqlite')
    const trace = join(home, 'trace.sqlite')
    walDatabase(state, 2).close()
    walDatabase(trace, 2).close()
    chmodSync(state, 0o600)
    chmodSync(trace, 0o444)
    assert.equal(hook('snapshot', home, snap).status, 0)
    assert.equal(mode(join(snap, 'state.sqlite')), 0o600)
    assert.equal(mode(join(snap, 'trace.sqlite')), 0o444)

    chmodSync(state, 0o644)
    chmodSync(trace, 0o644)
    for (const suffix of ['', '-wal', '-shm']) rmSync(trace + suffix, { force: true })
    walDatabase(trace, 1).close()
    const restored = hook('restore', snap, home)
    assert.equal(restored.status, 0, restored.stderr)
    assert.equal(mode(state), 0o600)
    assert.equal(mode(trace), 0o444)
    assert.equal(count(trace), 2)
    // A second snapshot over the first works even though its files are read-only.
    assert.equal(hook('snapshot', home, snap).status, 0)
  })
})

test('a copy of a database that is being written is consistent and has the rows from before', async () => {
  await world(async ({ home, copy }) => {
    const path = join(home, 'state.sqlite')
    const live = walDatabase(path, 0)
    live.exec(`WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 2000)
      INSERT INTO t SELECT n FROM s`)
    const before = count(path)
    // A writer in another process, as the running app is.
    const writer = spawn(
      process.execPath,
      [
        '-e',
        `const { DatabaseSync } = require('node:sqlite'); const db = new DatabaseSync(process.argv[1]);
         db.exec('PRAGMA busy_timeout=5000; PRAGMA wal_autocheckpoint=0');
         setInterval(() => db.exec('INSERT INTO t VALUES (-1)'), 2)`,
        path,
      ],
      { stdio: 'ignore' },
    )
    try {
      await new Promise((resolve) => setTimeout(resolve, 300))
      const copied = hook('test-copy', home, copy)
      assert.equal(copied.status, 0, copied.stderr)
    } finally {
      writer.kill()
    }
    const db = new DatabaseSync(join(copy, 'state.sqlite'), { readOnly: true })
    assert.equal((db.prepare('PRAGMA integrity_check').get() as { integrity_check: string }).integrity_check, 'ok')
    db.close()
    assert.ok(count(join(copy, 'state.sqlite')) >= before)
    assert.equal(existsSync(join(copy, 'trace.sqlite')), false)
    live.close()
  })
})

test('wrong arguments print the usage and exit 2', () => {
  for (const args of [[], ['snapshot', 'a'], ['nope', 'a', 'b'], ['constructor', 'a', 'b'], ['restore', 'a', 'b', 'c']]) {
    const result = hook(...args)
    assert.equal(result.status, 2)
    assert.match(result.stderr, /usage: node storage\.ts/)
  }
})
