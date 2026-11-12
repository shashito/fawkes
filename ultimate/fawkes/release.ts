/**
 * Whether a newer Fawkes is out, and what the user runs to get it. The server asks GitHub's `releases/latest` redirect
 * for the newest tag (not the API, which limits unauthenticated clients to 60 requests an hour); everything here
 * decides from that tag and the versions of the daemon and of the installed binary. Browser-safe.
 */
import type { Release, Status } from './types.ts'

export const RELEASES = 'https://github.com/shashito/fawkes/releases'

/** What `install.sh` documents: running it again installs the latest release over the old one. */
export const INSTALL_LINE = `curl -LsSf ${RELEASES}/latest/download/install.sh | sh`

type Version = { parts: [number, number, number]; prerelease: boolean }

/** `0.2.0`, `v0.2.0` or `fawkes 0.2.0-rc.1` (what `fawkes --version` prints); undefined for anything else. */
export function parseVersion(text: string): Version | undefined {
  const match = /^(?:fawkes )?v?(\d+)\.(\d+)\.(\d+)(-\S+)?$/.exec(text.trim())
  if (!match) return undefined
  return { parts: [Number(match[1]), Number(match[2]), Number(match[3])], prerelease: !!match[4] }
}

/** Whether `a` is a later version than `b`; a prerelease comes before its release. False when either is unreadable. */
export function newer(a: string | undefined, b: string | undefined) {
  const [x, y] = [a ? parseVersion(a) : undefined, b ? parseVersion(b) : undefined]
  if (!x || !y) return false
  for (let i = 0; i < 3; i++) if (x.parts[i] !== y.parts[i]) return x.parts[i] > y.parts[i]
  return y.prerelease && !x.prerelease
}

/**
 * The tag `releases/latest` redirects to (`…/releases/tag/v0.2.0` → `v0.2.0`), or null when there is no release yet: it
 * then redirects to the releases page. Anything but a redirect throws.
 */
export function tagOf(status: number, location: string | null): string | null {
  if (status < 300 || status >= 400 || !location) throw new Error(`GitHub answered ${status} without a redirect`)
  const tag = /\/releases\/tag\/([^/?#]+)$/.exec(location)?.[1]
  return tag ? decodeURIComponent(tag) : null
}

/** How this Fawkes is upgraded: the curl line for a release install, a rebuild for a source checkout (`repo`). */
export const upgradeCommand = (repo: string | undefined, quote: (word: string) => string) =>
  repo ? `cd ${quote(repo)} && git pull && cargo install --path . --locked` : INSTALL_LINE

/**
 * The daemon restart that runs the installed binary: its launchd agent's when it has one (the agent names the binary
 * itself), else `down` and `up` with the daemon's own binary `bin`, since another `fawkes` may come first on PATH.
 */
export function restartCommand(
  manifest: string,
  launchd: boolean,
  bin: string | undefined,
  quote: (word: string) => string,
) {
  const named = `--manifest ${quote(manifest)}`
  if (launchd) return `fawkes service restart ${named}`
  const fawkes = bin ? quote(bin) : 'fawkes'
  return `${fawkes} down ${named} && ${fawkes} up ${named}`
}

/**
 * What to tell the user, or nothing: a release newer than what is installed (`available`), else an installed binary
 * newer than the daemon that runs (`restart`). The daemon's version stands in for an installed one that could not be
 * read.
 */
export function releaseNote(release: Release | undefined, status: Status | undefined) {
  const daemon = status?.version
  if (!release || !daemon) return undefined
  const installed = release.installed ?? daemon
  const latest = release.latest?.replace(/^v/, '')
  if (latest && newer(latest, installed) && newer(latest, daemon))
    return { kind: 'available' as const, version: latest, daemon }
  if (newer(installed, daemon)) return { kind: 'restart' as const, version: installed, daemon }
  return undefined
}

/**
 * Why the restart waits and where it runs: it stops this Ultimate, so it comes from a shell outside it, and a daemon
 * restart rolls a pending switch back.
 */
export const RESTART_WHEN = 'from a shell outside this Ultimate, once no switch is pending (a daemon restart rolls it back)'

/** The note in one line, for an agent. */
export function releaseText(release: Release | undefined, status: Status | undefined) {
  const note = releaseNote(release, status)
  if (!note || !release) return undefined
  const restart = `restart the daemon ${RESTART_WHEN}: \`${release.restart}\``
  return note.kind === 'available'
    ? `Fawkes ${note.version} is available (the daemon runs ${note.daemon}): upgrade with \`${release.command}\`, then ${restart}.`
    : `Fawkes ${note.version} is installed but the daemon still runs ${note.daemon}: ${restart}.`
}
