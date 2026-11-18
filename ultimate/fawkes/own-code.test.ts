/** A generation's `node_modules/ultimate` points at the generation itself, whatever was there before. */
import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, readlinkSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { linkOwnCode } from './own-code.ts'

function generation() {
  const code = mkdtempSync(join(tmpdir(), 'fawkes-own-'))
  mkdirSync(join(code, 'node_modules'))
  writeFileSync(join(code, 'package.json'), '{"name": "ultimate"}')
  return code
}

test('links node_modules/ultimate to its own generation, and again after an install or a reuse copy', async () => {
  const code = generation()
  try {
    const link = join(code, 'node_modules/ultimate')
    await linkOwnCode(code)
    assert.equal(readlinkSync(link), '..')
    assert.equal(realpathSync(link), realpathSync(code))
    await linkOwnCode(code)
    assert.equal(readlinkSync(link), '..')

    // A link to another checkout (the shared clone) is replaced.
    rmSync(link)
    symlinkSync('/elsewhere/ultimate', link)
    await linkOwnCode(code)
    assert.equal(readlinkSync(link), '..')
  } finally {
    rmSync(code, { recursive: true, force: true })
  }
})

test('refuses a real folder in its place, and a generation without node_modules', async () => {
  const code = generation()
  try {
    mkdirSync(join(code, 'node_modules/ultimate'))
    await assert.rejects(linkOwnCode(code), /node_modules\/ultimate is not a link .*remove it/)
    rmSync(join(code, 'node_modules'), { recursive: true })
    await assert.rejects(linkOwnCode(code), { code: 'ENOENT' })
  } finally {
    rmSync(code, { recursive: true, force: true })
  }
})
