/**
 * Makes a generation's extensions load that generation's own Ultimate code.
 *
 * Extensions outside Ultimate (`~/Code/ultimate-extensions`, a project's `.ultimate/extensions`) import Ultimate's code
 * as `ultimate/…`. Ultimate loads them through Vite, which resolves every package found in the generation's
 * `node_modules` from there rather than from the importing file (`resolve.dedupe` in `server/vite-extensions.ts`).
 * Without `node_modules/ultimate` in the generation, `ultimate/…` resolves next to the importer instead: the one clone
 * an extension folder links to, shared by every host and moving outside any transaction. With it, each generation (live,
 * under test, or returned to by a rollback) resolves to itself.
 */
import { readlink, symlink, unlink } from 'node:fs/promises'
import { join } from 'node:path'

/** Relative, so the link stays right in a copy of `node_modules` (the manifest's `reuse`). */
const OWN_CODE = '..'

/** Links `<code>/node_modules/ultimate` to `<code>`, replacing a link to anything else. */
export async function linkOwnCode(code: string) {
  const link = join(code, 'node_modules/ultimate')
  const target = await readlink(link).catch((error: NodeJS.ErrnoException) => {
    if (error.code === 'ENOENT') return undefined
    throw new Error(
      `${link} is not a link (${error.code}): remove it, so extensions load this generation's Ultimate`,
      { cause: error },
    )
  })
  if (target === OWN_CODE) return
  if (target !== undefined) await unlink(link)
  await symlink(OWN_CODE, link)
}
