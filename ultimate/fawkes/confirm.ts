/**
 * Whether a new generation may vouch for itself: every built-in extension's server facet loaded (a facet broken by a
 * changed core or a missing package is exactly what an upgrade must not make permanent), and the public page answers.
 *
 * Only built-in extensions run from the generation's own code. A user or project extension resolves `ultimate/...`
 * through its own folder's `node_modules`, which links to one shared clone's working tree, so it loads or fails alike
 * in every generation on that clone: a rollback cannot mend it, and only costs the storage written since the switch.
 */
import type { ExtensionInfo } from 'ultimate/shared/protocol.ts'

export type Verdict = { ok: true } | { ok: false; reason: string }

/**
 * The facets that failed to load, by name: `blocking` the built-in ones, `ignored` those of user and project
 * extensions. Disabled extensions do not count.
 */
export function failedFacets(extensions: ExtensionInfo[]) {
  const failed = extensions.filter(
    (extension) => extension.enabled && extension.facets.server && extension.server?.error,
  )
  return {
    blocking: failed.filter((extension) => extension.source === 'builtin').map((extension) => extension.name),
    ignored: failed.filter((extension) => extension.source !== 'builtin').map((extension) => extension.name),
  }
}

/** Why built-in facets keep a generation from vouching for itself, if any failed. */
export function facetRefusal(extensions: ExtensionInfo[]) {
  const { blocking } = failedFacets(extensions)
  return blocking.length ? `server facets failed to load: ${blocking.join(', ')}` : undefined
}

export function judge(extensions: ExtensionInfo[], page: number | string): Verdict {
  const refusal = facetRefusal(extensions)
  if (refusal) return { ok: false, reason: refusal }
  if (typeof page === 'string') return { ok: false, reason: `the page does not answer: ${page}` }
  if (page >= 400) return { ok: false, reason: `the page answers ${page}` }
  return { ok: true }
}

/** The notice for failed facets that do not block the confirm, or nothing when none failed. */
export function ignoredText(ignored: string[]) {
  if (!ignored.length) return undefined
  return `Fawkes: ${ignored.join(', ')} failed to load; a user or project extension loads the same in every generation, so it does not block the confirm`
}

export async function mayConfirm(host: {
  extensions(): ExtensionInfo[]
  system(): { process: { url: string } }
}): Promise<Verdict> {
  const page = await fetch(host.system().process.url, { signal: AbortSignal.timeout(5000) }).then(
    (response) => response.status,
    (error: unknown) => String(error),
  )
  return judge(host.extensions(), page)
}
