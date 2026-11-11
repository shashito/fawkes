/**
 * First-time setup: the Fawkes manifest for an Ultimate host that does not run under Fawkes yet, from facts the server
 * facet reads (the project, the home, the public port, the manifests already in the Fawkes home). Pure and browser-safe:
 * nothing here reads a file, so the text a card previews is the text it writes. The manifest follows
 * `examples/ultimate.toml`; DESIGN.md, "Manifest", is the contract.
 */

/** What the top level of an existing manifest says that setup compares with. */
export type ManifestFacts = {
  name?: string
  project?: string
  home?: string
  port?: number
  /** Inclusive `[low, high]`, as in the manifest. */
  spare?: [number, number]
}

/** A manifest file in the Fawkes home: its file name and its text. */
export type ManifestFile = { file: string; text: string }

/** The part of the manifest a host decides; everything else is the same for every Ultimate host. */
export type SetupInput = {
  name: string
  project: string
  repo: string
  home: string
  port: number
  spare: [number, number]
  /** The real path of this extension's folder, where `start.ts`, `health.ts` and `storage.ts` live. */
  extension: string
}

/** The first spare port block; below it lies Ultimate's own default range of ports. */
const SPARE_FROM = 4790
const SPARE_SIZE = 10

/** The project folder's name reduced to what a manifest name allows (it becomes a file name). */
export const manifestName = (project: string) =>
  project.split('/').filter(Boolean).at(-1)?.replace(/[^A-Za-z0-9_-]/g, '') || 'ultimate'

/** A TOML basic string. */
const quote = (value: string) => JSON.stringify(value).replace(/\x7f/g, '\\u007f')

/** A path as one shell word, bare when it has nothing the shell would read. */
export const shellWord = (value: string) =>
  /^[\w@%+=:,./-]+$/.test(value) ? value : `'${value.replaceAll("'", `'\\''`)}'`

/** The top-level keys of a manifest that setup reads; a value it cannot read is left out. */
export function readManifest(text: string): ManifestFacts {
  const facts: ManifestFacts = {}
  for (const line of text.split('\n')) {
    if (line.startsWith('[')) break
    const match = /^([A-Za-z0-9_-]+)\s*=\s*(.*)$/.exec(line.trim())
    if (!match) continue
    const [, key, value] = match
    const string = /^"((?:[^"\\]|\\.)*)"/.exec(value) ?? /^'([^']*)'/.exec(value)
    if (key === 'name' || key === 'project' || key === 'home') {
      if (!string) continue
      try {
        facts[key] = value.startsWith('"') ? (JSON.parse(`"${string[1]}"`) as string) : string[1]
      } catch {
        // An escape JSON does not know: the value is not comparable.
      }
    } else if (key === 'port') {
      const port = /^\d+/.exec(value)
      if (port) facts.port = Number(port[0])
    } else if (key === 'spare_ports') {
      const range = /^\[\s*(\d+)\s*,\s*(\d+)\s*\]/.exec(value)
      if (range) facts.spare = [Number(range[1]), Number(range[2])]
    }
  }
  return facts
}

/**
 * The first block of ten ports from 4790 up that overlaps no `port` or `spare_ports` of the other manifests and does not
 * hold `port`. A manifest without `spare_ports` owns the ten ports after its `port`, as the daemon defaults them.
 */
export function spareBlock(others: ManifestFacts[], port: number): [number, number] {
  const taken: [number, number][] = [[port, port]]
  for (const other of others) {
    if (other.port === undefined) continue
    taken.push([other.port, other.port], other.spare ?? [other.port + 1, other.port + SPARE_SIZE])
  }
  for (let low = SPARE_FROM; low + SPARE_SIZE - 1 <= 65535; low += SPARE_SIZE) {
    const high = low + SPARE_SIZE - 1
    if (!taken.some(([from, to]) => low <= to && high >= from)) return [low, high]
  }
  throw new Error('No block of ten free ports above 4790 is left; free some in the other manifests')
}

/**
 * The manifest in `manifests` that already sets this host up: one whose `project` is this project, or one whose `home`
 * is this host's home (a hand-written manifest has no `project`). A file of the same name alone is another app's.
 */
export function findExisting(
  manifests: ManifestFile[],
  host: { project: string; home: string },
): ManifestFile | undefined {
  return manifests.find(({ text }) => {
    const facts = readManifest(text)
    return facts.project === host.project || facts.home === host.home
  })
}

/** `name`, or `name-2`, `name-3`, … when another app's manifest already has that file name. */
export function freeName(manifests: ManifestFile[], name: string): string {
  const taken = new Set(manifests.map(({ file }) => file))
  let candidate = name
  for (let n = 2; taken.has(`${candidate}.toml`); n++) candidate = `${name}-${n}`
  return candidate
}

/** The manifest text for a host. */
export function buildManifest(input: SetupInput): string {
  const { name, project, repo, home, port, spare, extension } = input
  const command = (script: string, ...args: string[]) =>
    quote(['node', shellWord(`${extension}/${script}`), ...args].join(' '))
  return [
    `# ${name}'s Ultimate host under Fawkes, written by the setup card of the Ultimate extension (ultimate/fawkes/README.md).`,
    `name = ${quote(name)}`,
    `project = ${quote(project)}`,
    `repo = ${quote(repo)}`,
    `home = ${quote(home)}`,
    `port = ${port}`,
    `spare_ports = [${spare[0]}, ${spare[1]}]`,
    'adopt = true',
    'install = "pnpm install --frozen-lockfile"',
    'reuse = ["node_modules"]',
    'keep = 5',
    `start = ${command('start.ts', shellWord(project))}`,
    'deadline = 120',
    'stop_timeout = 30',
    '',
    '[health]',
    `command = ${command('health.ts')}`,
    'timeout = 120',
    '',
    '[hooks]',
    `snapshot = ${command('storage.ts', 'snapshot', '{home}', '{snapshot}')}`,
    `restore = ${command('storage.ts', 'restore', '{snapshot}', '{home}')}`,
    `test_copy = ${command('storage.ts', 'test-copy', '{home}', '{test_home}')}`,
    '',
    '[env]',
    'ULTIMATE_HOME = "{home}"',
    'ULTIMATE_PORT = "{port}"',
    '',
  ].join('\n')
}
