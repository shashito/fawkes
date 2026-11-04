/**
 * Whether a new generation may vouch for itself: every extension's server facet loaded (a facet broken by a changed
 * core or a missing package is exactly what an upgrade must not make permanent), and the public page answers.
 */
import type { ExtensionInfo } from 'ultimate/shared/protocol.ts'

export type Verdict = { ok: true } | { ok: false; reason: string }

/** The facets that failed to load, by name; disabled extensions do not count. */
export function failedFacets(extensions: ExtensionInfo[]) {
  return extensions
    .filter((extension) => extension.enabled && extension.facets.server && extension.server?.error)
    .map((extension) => extension.name)
}

export function judge(extensions: ExtensionInfo[], page: number | string): Verdict {
  const failed = failedFacets(extensions)
  if (failed.length) return { ok: false, reason: `server facets failed to load: ${failed.join(', ')}` }
  if (typeof page === 'string') return { ok: false, reason: `the page does not answer: ${page}` }
  if (page >= 400) return { ok: false, reason: `the page answers ${page}` }
  return { ok: true }
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
