#!/usr/bin/env node
/**
 * Sets a project up for the Fawkes supervisor, whether or not it ever ran Ultimate. Four pieces, each checked before anything is
 * written, each left alone when it is there:
 *
 * - the **manifest** in the Fawkes home (`setup.ts`), from which the project's daemon adopts the running Ultimate;
 * - the **link** `<project>/.ultimate/extensions/fawkes` to this folder, so the project's Ultimate loads the extension;
 * - **`update` off** in `<project>/.ultimate/settings.json`: the built-in `update` would move a clone Fawkes runs
 *   generations from;
 * - the **trust** of the project's Ultimate setup, without which Ultimate ignores the link and the settings.
 *
 * Ultimate decides all four (where a project's home is, whose decision a worktree shares, how settings and trust are
 * written), so the helpers are loaded from the clone at run time, never copied. A piece that cannot be written without
 * overriding something (a link elsewhere, a declined project) is refused, and then nothing at all is written.
 *
 * As a module (`inspect`, `resolveClone`, `setupSteps`) the extension's server facet shows the same plan and the same
 * terminal steps on its card. As a program:
 * `node init.ts [--ultimate <clone>] [--port <n>] <project>`; exit 0 done, 1 failed or refused, 2 usage.
 */
import {
  accessSync,
  chmodSync,
  constants,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { createConnection, createServer } from 'node:net'
import { homedir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { parseArgs } from 'node:util'
import type { JsonObject } from '@earendil-works/pi-durable'
import type * as PiCodingAgent from '@earendil-works/pi-coding-agent'
// Types only: the code itself is loaded from the clone being set up (`loadClone`).
import type * as Lock from 'ultimate/lib/lock.ts'
import type * as ProjectPaths from 'ultimate/lib/project-paths.ts'
import type * as ProjectSetup from 'ultimate/lib/project-setup.ts'
import type * as Settings from 'ultimate/lib/settings.ts'
import { request } from './client.ts'
import { buildManifest, claimedPorts, findExisting, freeName, manifestName, readManifest, shellWord, spareBlock } from './setup.ts'
import type { ManifestFile } from './setup.ts'
import type { SetupStep } from './types.ts'

/** The real path of this folder, where `start.ts`, `health.ts` and `storage.ts` live beside this file. */
export const extensionFolder = () => realpathSync(dirname(fileURLToPath(import.meta.url)))

// ------------------------------------------------ the plan

export type PieceName = 'manifest' | 'link' | 'update' | 'trust'

/** One piece: whether it is there, to be written, or refused (`reason`); `write` is set exactly when it is to be written. */
export type Piece = {
  name: PieceName
  state: 'there' | 'write' | 'refused'
  /** What it is, with its path. */
  what: string
  /** Why it is refused. */
  reason?: string
  /** What writing it means beyond the file. */
  note?: string
  write?: () => Promise<void>
}

export type Inspection = {
  /** The real path of the project. */
  project: string
  /** The manifest that already sets the project up, with its path. */
  existing?: ManifestFile
  /** In the order they are written. */
  pieces: Piece[]
}

export type InspectOptions = {
  project: string
  /** The Ultimate clone the project runs from; it must have `node_modules`. */
  clone: string
  userHome: string
  fawkesHome: string
  /** The port the manifest gives the host; by default the one it serves on, or the first free one. */
  port?: number
}

/** The clone's own helpers. */
type Helpers = {
  projectHome: typeof ProjectPaths.projectHome
  decisionRoot: typeof ProjectSetup.decisionRoot
  projectDecision: typeof ProjectSetup.projectDecision
  projectDiagnostics: typeof ProjectSetup.projectDiagnostics
  readSettings: typeof Settings.readSettings
  updateSettings: typeof Settings.updateSettings
  readOwner: typeof Lock.readOwner
  alive: typeof Lock.alive
  ProjectTrustStore: typeof PiCodingAgent.ProjectTrustStore
}

/** Imports the helpers from the clone, the way `start.ts` imports `extensions/instance/server.ts`. */
async function loadClone(clone: string): Promise<Helpers> {
  // `@vite-ignore`: Ultimate's web build reads this file, and cannot analyze an import whose path is a variable.
  const load = (path: string) => import(/* @vite-ignore */ pathToFileURL(join(clone, path)).href)
  // The package exports only an `import` condition, which `require.resolve` cannot follow.
  const agent = join(clone, 'node_modules/@earendil-works/pi-coding-agent')
  const entry = (JSON.parse(readFileSync(join(agent, 'package.json'), 'utf8')) as { exports: Record<string, { import: string }> })
    .exports['.'].import
  const [projectPaths, projectSetup, settings, lock, pi] = await Promise.all([
    load('lib/project-paths.ts'),
    load('lib/project-setup.ts'),
    load('lib/settings.ts'),
    load('lib/lock.ts'),
    import(/* @vite-ignore */ pathToFileURL(join(agent, entry)).href),
  ])
  return {
    projectHome: projectPaths.projectHome,
    decisionRoot: projectSetup.decisionRoot,
    projectDecision: projectSetup.projectDecision,
    projectDiagnostics: projectSetup.projectDiagnostics,
    readSettings: settings.readSettings,
    updateSettings: settings.updateSettings,
    readOwner: lock.readOwner,
    alive: lock.alive,
    ProjectTrustStore: pi.ProjectTrustStore,
  }
}

/** Writes nothing (reading the trust store takes its lock): what each of the four pieces needs. */
export async function inspect(options: InspectOptions): Promise<Inspection> {
  const { clone, userHome } = options
  const project = realpathSync(options.project)
  const helpers = await loadClone(clone)
  const extension = extensionFolder()
  const manifest = await planManifest({ ...options, project, helpers, extension })
  return {
    project,
    existing: manifest.existing,
    pieces: [
      manifest.piece,
      planLink(project, extension),
      await planUpdate(helpers, project),
      planTrust(helpers, project, userHome),
    ],
  }
}

const refused = (name: PieceName, what: string, reason: string): Piece => ({ name, state: 'refused', what, reason })

// ------------------------------------------------ the manifest

/** The manifests of the Fawkes home, as Fawkes's own discovery reads them: an entry that cannot be read sets nothing up. */
export function readManifests(directory: string): ManifestFile[] {
  const read = (entry: string) => {
    try {
      return [{ file: entry, text: readFileSync(join(directory, entry), 'utf8') }]
    } catch {
      return []
    }
  }
  try {
    return readdirSync(directory)
      .filter((entry) => entry.endsWith('.toml'))
      .toSorted()
      .flatMap(read)
  } catch {
    // No Fawkes home yet, or one this process cannot list: writing creates it or reports why it cannot.
    return []
  }
}

/** The lowest port from `from` up that no range in `taken` holds and `listening` says is free. */
export async function pickPort(
  taken: [number, number][],
  listening: (port: number) => Promise<boolean>,
  from = 4747,
): Promise<number> {
  for (let port = from; port <= 65535; port++) {
    if (taken.some(([low, high]) => port >= low && port <= high)) continue
    if (!(await listening(port))) return port
  }
  throw new Error(`No free port from ${from} up; pass one with --port`)
}

/**
 * Whether something listens on 127.0.0.1:`port`, or the port cannot be bound at all. Binding alone misses a listener on
 * all interfaces (macOS lets 127.0.0.1 be bound beside it), which a connection reaches.
 */
const listeningOn = (port: number) =>
  new Promise<boolean>((done) => {
    const probe = createConnection({ port, host: '127.0.0.1' })
    probe.once('connect', () => {
      probe.destroy()
      done(true)
    })
    probe.once('error', () => {
      const server = createServer()
      server.once('error', () => done(true))
      server.listen(port, '127.0.0.1', () => server.close(() => done(false)))
    })
  })

/**
 * The port the host is opened on: the one `<home>/ultimate.lock` records while its Ultimate lives (the lock the project
 * list reads for `running.url`); otherwise the first free one that no manifest, no lock of any project (running or not)
 * and no listener holds.
 */
async function choosePort(helpers: Helpers, home: string, userHome: string, manifests: ManifestFile[]) {
  const owner = (path: string) => helpers.readOwner(path).catch(() => undefined)
  const own = await owner(join(home, 'ultimate.lock'))
  if (own && helpers.alive(own.pid) && Number.isInteger(own.port) && own.port! >= 1 && own.port! <= 65535) return own.port!
  const projects = join(userHome, '.ultimate/projects')
  const names = (() => {
    try {
      return readdirSync(projects)
    } catch {
      return []
    }
  })()
  const recorded = await Promise.all(names.map((name) => owner(join(projects, name, 'ultimate.lock'))))
  const taken = claimedPorts(manifests.map(({ text }) => readManifest(text)))
  for (const lock of recorded) if (typeof lock?.port === 'number') taken.push([lock.port, lock.port])
  return pickPort(taken, listeningOn)
}

async function planManifest(
  o: InspectOptions & { helpers: Helpers; extension: string },
): Promise<{ piece: Piece; existing?: ManifestFile }> {
  const { project, clone, userHome, fawkesHome, helpers, extension } = o
  const home = helpers.projectHome(project, userHome)
  const manifests = readManifests(fawkesHome)
  const found = findExisting(manifests, { project, home })
  if (found) {
    const file = join(fawkesHome, found.file)
    return { existing: { file, text: found.text }, piece: { name: 'manifest', state: 'there', what: `manifest ${file}` } }
  }
  const file = join(fawkesHome, `${freeName(manifests, manifestName(project))}.toml`)
  const what = `manifest ${file}`
  let port: number
  let spare: [number, number]
  try {
    port = o.port ?? (await choosePort(helpers, home, userHome, manifests))
    spare = spareBlock(
      manifests.map(({ text }) => readManifest(text)),
      port,
    )
  } catch (error) {
    return { piece: refused('manifest', what, (error as Error).message) }
  }
  const text = buildManifest({ name: basename(file, '.toml'), project, repo: clone, home, port, spare, extension })
  return {
    piece: {
      name: 'manifest',
      state: 'write',
      what,
      note: `port ${port}, spare ports ${spare[0]}-${spare[1]}`,
      write: async () => {
        mkdirSync(fawkesHome, { recursive: true, mode: 0o700 })
        // `wx`: a manifest created since the inspection is never overwritten.
        writeFileSync(file, text, { flag: 'wx', mode: 0o644 })
        // The umask may have narrowed the mode.
        chmodSync(file, 0o644)
      },
    },
  }
}

// ------------------------------------------------ the link

function planLink(project: string, extension: string): Piece {
  const path = join(project, '.ultimate/extensions/fawkes')
  const what = `link ${path} -> ${extension}`
  let stat: ReturnType<typeof lstatSync>
  try {
    stat = lstatSync(path)
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
    return {
      name: 'link',
      state: 'write',
      what,
      write: async () => {
        mkdirSync(dirname(path), { recursive: true })
        symlinkSync(extension, path)
      },
    }
  }
  try {
    if (realpathSync(path) === extension) return { name: 'link', state: 'there', what }
  } catch {
    // A dangling link points elsewhere too.
  }
  const is = stat.isSymbolicLink() ? `a link to ${readlinkSync(path)}` : 'not a link'
  return refused(
    'link',
    what,
    `${path} is ${is}, not to ${extension}; init never replaces it. Remove it, or link ${extension} there yourself.`,
  )
}

// ------------------------------------------------ update off

/** The `extensions.enable` and `extensions.disable` lists of a settings object, checked as onboarding checks them. */
function lists(value: JsonObject) {
  const extensions = value.extensions ?? {}
  if (typeof extensions !== 'object' || Array.isArray(extensions)) throw new Error('extensions must be a JSON object')
  for (const key of ['enable', 'disable']) {
    const names = extensions[key]
    if (names !== undefined && (!Array.isArray(names) || names.some((name) => typeof name !== 'string')))
      throw new Error(`extensions.${key} must be an array of names`)
  }
  value.extensions = extensions
  return extensions
}

async function planUpdate(helpers: Helpers, project: string): Promise<Piece> {
  const path = join(project, '.ultimate/settings.json')
  const what = `update off in ${path}`
  let disabled: boolean
  try {
    disabled = ((lists(structuredClone(await helpers.readSettings(path))).disable as string[] | undefined) ?? []).includes('update')
  } catch (error) {
    return refused('update', what, `${path} cannot be changed: ${(error as Error).message}`)
  }
  if (disabled) return { name: 'update', state: 'there', what }
  return {
    name: 'update',
    state: 'write',
    what,
    write: () =>
      helpers.updateSettings(path, (value) => {
        const extensions = lists(value)
        extensions.disable = [...new Set([...((extensions.disable as string[] | undefined) ?? []), 'update'])]
        if (extensions.enable !== undefined)
          extensions.enable = (extensions.enable as string[]).filter((name) => name !== 'update')
      }),
  }
}

// ------------------------------------------------ trust

function planTrust(helpers: Helpers, project: string, userHome: string): Piece {
  const root = helpers.decisionRoot(project)
  const file = join(userHome, '.ultimate/trust.json')
  const what = `trust for ${root} in ${file}`
  // No trust store yet: undecided, without taking the store's lock, which creates `<userHome>/.ultimate`.
  if (!existsSync(file)) return writeTrust(helpers, project, root, userHome, what)
  // `projectDecision` reads an unreadable store as undecided; its write would then fail after the other pieces.
  const [unreadable] = helpers.projectDiagnostics(project, userHome)
  if (unreadable) return refused('trust', what, `${unreadable.replace(/^Error: /, '')}; fix or remove ${file}, and run init again.`)
  const decision = helpers.projectDecision(project, userHome)
  if (decision === true) return { name: 'trust', state: 'there', what }
  if (decision === false)
    return refused(
      'trust',
      what,
      `you declined this project's Ultimate setup in ${file}; init never overrides that. Remove the entry for ${root} there, or trust the project in Ultimate, and run init again.`,
    )
  return writeTrust(helpers, project, root, userHome, what)
}

function writeTrust(helpers: Helpers, project: string, root: string, userHome: string, what: string): Piece {
  return {
    name: 'trust',
    state: 'write',
    what,
    note: `trusts the whole ${join(project, '.ultimate')}/ (agents, skills, settings)`,
    write: async () => new helpers.ProjectTrustStore(join(userHome, '.ultimate')).set(root, true),
  }
}

// ------------------------------------------------ the clone

/** The clone behind a launcher on PATH: a link to `<clone>/bin/ultimate.ts`, or a pnpm shim naming it. */
function cloneOnPath(path: string | undefined) {
  const cloneOf = (entry: string) =>
    basename(entry) === 'ultimate.ts' && basename(dirname(entry)) === 'bin' ? dirname(dirname(entry)) : undefined
  for (const directory of (path ?? '').split(':')) {
    const launcher = join(directory || '.', 'ultimate')
    try {
      accessSync(launcher, constants.X_OK)
      if (!statSync(launcher).isFile()) continue
      const real = realpathSync(launcher)
      const linked = cloneOf(real)
      if (linked || statSync(real).size > 65536) return linked
      const target = /^# cmd-shim-target=(.+)$/m.exec(readFileSync(real, 'utf8'))?.[1]
      return target && cloneOf(resolve(dirname(launcher), target))
    } catch {
      continue
    }
  }
  return undefined
}

/**
 * The Ultimate clone to set up for: `--ultimate`, else `$ULTIMATE`, else the clone behind `ultimate` on PATH, else
 * `<userHome>/Code/ultimate`. The first that names something is the one: if it is not a clone, that is an error, never
 * a reason to try the next.
 */
export function resolveClone({ option, env, userHome }: { option?: string; env: NodeJS.ProcessEnv; userHome: string }) {
  const onPath = cloneOnPath(env.PATH)
  const [from, path] = option
    ? ['--ultimate', option]
    : env.ULTIMATE
      ? ['$ULTIMATE', env.ULTIMATE]
      : onPath
        ? ['the ultimate on PATH', onPath]
        : ['the default location', join(userHome, 'Code/ultimate')]
  const clone = resolve(path)
  if (!existsSync(join(clone, 'bin/ultimate.ts')) || !existsSync(join(clone, 'node_modules')))
    throw new Error(
      `${from} names ${clone}, which is not an Ultimate clone with node_modules (bin/ultimate.ts and node_modules are expected). Pass the clone with --ultimate <clone>.`,
    )
  return realpathSync(clone)
}

// ------------------------------------------------ the terminal steps

/** Whether a `fawkes` binary is on `path`. */
export function fawkesOnPath(path = process.env.PATH) {
  return (path ?? '').split(':').some((directory) => {
    try {
      const file = join(directory || '.', 'fawkes')
      accessSync(file, constants.X_OK)
      return statSync(file).isFile()
    } catch {
      return false
    }
  })
}

/**
 * The step that puts `fawkes` on PATH; none when one is there. From a Fawkes checkout (`<repo>/ultimate/fawkes`) it
 * installs from it; a release installed by install.sh has `fawkes` in ~/.local/bin, which PATH may lack.
 */
function installSteps(extension: string, path: string | undefined): SetupStep[] {
  if (fawkesOnPath(path)) return []
  const repo = dirname(dirname(extension))
  return [
    existsSync(join(repo, 'Cargo.toml'))
      ? { label: 'Install fawkes', command: `cargo install --path ${shellWord(repo)} --locked` }
      : { label: 'Put fawkes on PATH (install.sh puts it in ~/.local/bin, or $FAWKES_INSTALL_DIR)' },
  ]
}

/** The part of the supervisor's `status` (`FleetStatus` in `src/protocol.rs`) the steps read. */
type Supervised = { apps: { name: string; manifest: string; state: string }[] }

/**
 * The `status` of the supervisor of `fawkesHome` (`fawkes up --all`, under launchd or in a terminal), or nothing when
 * none answers on its socket. 5 s: a supervisor that re-executes answers only once it runs its new binary, and taking
 * it for none would offer the `service install` that restarts every app.
 */
const supervisorStatus = (fawkesHome: string) =>
  request<Supervised>(join(fawkesHome, 'supervisor.sock'), 'status', undefined, { timeout: 5000 }).catch(() => undefined)

export type StepOptions = {
  extension: string
  fawkesHome: string
  /** The manifest that already sets the project up: the supervisor may hold its app down. */
  manifest?: string
  /** The command that runs `init.ts`, while it has a piece to write: the card's first step. */
  init?: string
  path?: string
  platform?: NodeJS.Platform
}

/**
 * The terminal steps that set a project up, for the card and for `init.ts`'s own last words: `init.ts`, `fawkes` on PATH,
 * stopping the Ultimate that runs on the project, and the supervisor. The supervisor starts every manifest of the
 * Fawkes home, a new one within seconds, so with one running nothing is left to start: the project's daemon adopts
 * the Ultimate once its port is free, retrying at most 30 s apart until then; only an app it holds down since
 * `fawkes down` waits for `fawkes up`. Without one, `fawkes service install` starts it, once per Mac; only then, since
 * installing again over a loaded agent restarts every app.
 */
export async function setupSteps(o: StepOptions): Promise<SetupStep[]> {
  const { extension, fawkesHome, init, manifest, path = process.env.PATH, platform = process.platform } = o
  const supervisor = await supervisorStatus(fawkesHome)
  const name = manifest && basename(manifest, '.toml')
  const down = supervisor?.apps.some((app) => app.state === 'down' && (app.manifest === manifest || app.name === name))
  const start: SetupStep = supervisor
    ? down
      ? {
          label: 'Start this project under the Fawkes supervisor, which holds it down since `fawkes down`',
          command: `fawkes up --manifest ${shellWord(name!)}`,
        }
      : {
          label:
            'Nothing to start: the Fawkes supervisor runs, starts this project from its manifest and adopts this Ultimate once it stopped (`fawkes status --all` shows it)',
        }
    : platform === 'darwin'
      ? {
          label:
            'Start the Fawkes supervisor, once per Mac: the launchd agent dev.fawkes, which runs `fawkes up --all` (every app in the Fawkes home) at login',
          command: 'fawkes service install',
        }
      : {
          label: 'Start the Fawkes supervisor, once per machine, under your init system: it runs every app in the Fawkes home',
          command: 'fawkes up --all',
        }
  return [
    ...(init ? [{ label: 'Set up this project (run once)', command: init }] : []),
    ...installSteps(extension, path),
    { label: 'Stop the Ultimate running on this project, if any (Ctrl-C in its terminal)' },
    start,
  ]
}

// ------------------------------------------------ the program

const USAGE = 'Usage: node init.ts [--ultimate <clone>] [--port <n>] <project>'

const parse = (args: string[]) =>
  parseArgs({ args, allowPositionals: true, options: { ultimate: { type: 'string' }, port: { type: 'string' } } })

async function main(argv: string[], env: NodeJS.ProcessEnv) {
  let parsed: ReturnType<typeof parse>
  try {
    parsed = parse(argv)
  } catch (error) {
    console.error(`${(error as Error).message}\n${USAGE}`)
    return 2
  }
  const [project, ...extra] = parsed.positionals
  const port = parsed.values.port === undefined ? undefined : Number(parsed.values.port)
  if (!project || extra.length || (port !== undefined && !(Number.isInteger(port) && port >= 1 && port <= 65535))) {
    console.error(USAGE)
    return 2
  }
  try {
    const userHome = env.ULTIMATE_USER_HOME || homedir()
    const clone = resolveClone({ option: parsed.values.ultimate, env, userHome })
    // Where `fawkes` itself looks: `$FAWKES_HOME`, else `$HOME/.fawkes`.
    const fawkesHome = env.FAWKES_HOME || join(env.HOME || homedir(), '.fawkes')
    const plan = await inspect({ project, clone, userHome, fawkesHome, port })
    const refusals = plan.pieces.filter((piece) => piece.state === 'refused')
    if (refusals.length) {
      for (const piece of refusals) console.error(`${piece.name}: ${piece.reason}`)
      console.error('Nothing was written.')
      return 1
    }
    for (const piece of plan.pieces) {
      try {
        await piece.write?.()
      } catch (error) {
        console.error(`${piece.name}: cannot write ${piece.what}: ${(error as Error).message}`)
        return 1
      }
      console.log(`${piece.write ? 'wrote' : 'already'} ${piece.what}`)
      if (piece.write && piece.note) console.log(`  ${piece.note}`)
    }
    const manifest = plan.existing?.file
    const steps = await setupSteps({ extension: extensionFolder(), fawkesHome, manifest, path: env.PATH })
    console.log('\nNext, in a terminal:')
    steps.forEach((step, index) => console.log(`  ${index + 1}. ${step.label}${step.command ? `\n       ${step.command}` : ''}`))
    return 0
  } catch (error) {
    console.error(`init: ${(error as Error).message}`)
    return 1
  }
}

if (import.meta.main) process.exitCode = await main(process.argv.slice(2), process.env)
