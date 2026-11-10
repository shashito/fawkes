import { execFile } from 'node:child_process'
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { promisify } from 'node:util'
import { obj, oneOf, opt, str } from '@earendil-works/pi-schema'
import { defineExtension, defineTool, section } from '@earendil-works/pi-durable'
// Types only: at runtime this folder must not load code from a clone other than the generation it runs in.
import type { ExtensionHost } from 'ultimate/server/extensions.ts'
import type { PauseApi as PauseApiType } from 'ultimate/extensions/pause/api.ts'
import { request, subscribe } from './client.ts'
import { mayConfirm } from './confirm.ts'
import { causeText, eventLine, lastSwitched, outcome, outcomeText, rollbackEvent, storageText } from './facts.ts'
import { deferRelease, releaseHold } from './hold.ts'
import { HELD_MARKER, role } from './role.ts'
import { awaitingConfirm, outdated, restoreFailed, rollbackNote, upgradeRefusal } from './step.ts'
import type { Confirming, FawkesEvent, FawkesState, Generation, Status, Upgrade } from './types.ts'
import { up, UpFailure } from './up.ts'

const run = promisify(execFile)
/** The pause extension's service, looked up by its name like any `want`. */
const PauseApi = { name: 'pause' } as typeof PauseApiType

/** What this process is to Fawkes, from its environment, which never changes while it runs. */
const fawkes = role(process.env)

/** The ref an upgrade stages unless it names another. */
const DEFAULT_REF = 'origin/main'

/** A ref given by the UI or an agent: plain ref characters only; git also gets `--end-of-options` before it. */
function validRef(ref: unknown): string {
  if (ref === undefined) return DEFAULT_REF
  if (typeof ref !== 'string' || !/^[\w./@^~-]+$/.test(ref)) throw new Error('ref must be a git ref')
  return ref
}

type Shared = {
  connected: boolean
  error?: string
  status?: Status
  generations: Generation[]
  events: FawkesEvent[]
  upgrade?: Upgrade
  confirming?: Confirming
  behind?: Extract<FawkesState, { supervised: true }>['behind']
  /** The scheduler is held by this extension until its generation is confirmed. */
  holding: boolean
  stop?: () => void
  confirmRun?: Promise<void>
  upgradeRun?: Promise<void>
}

const shared = (host: ExtensionHost) =>
  host.resource<Shared>('fawkes.state', () => ({ connected: false, generations: [], events: [], holding: false }))

function state(host: ExtensionHost): FawkesState {
  if (fawkes.kind === 'inert') return { supervised: false }
  const s = shared(host)
  return {
    supervised: true,
    generation: fawkes.generation,
    connected: s.connected,
    error: s.error,
    status: s.status,
    generations: s.generations,
    events: s.events,
    upgrade: s.upgrade,
    confirming: s.confirming,
    behind: s.behind,
  }
}

/**
 * Hold the scheduler; in a test run a hold that cannot be taken ends the process before the server resumes scheduling,
 * since the copied storage's work must never start there.
 */
function hold(host: ExtensionHost) {
  const s = shared(host)
  try {
    const harness = host.harness()
    harness.hold()
    s.holding = true
    deferRelease(harness, () => shared(host).holding)
    // Read by health.ts: a test run passes its health check only once this facet held the scheduler. Fawkes removes the
    // test home before every test, so the marker never outlives the run that wrote it.
    if (fawkes.kind === 'test') writeFileSync(join(host.home, HELD_MARKER), new Date().toISOString())
  } catch (error) {
    if (fawkes.kind !== 'test') throw error
    console.error(`fawkes: cannot hold the scheduler in a test run, exiting: ${String(error)}`)
    process.exit(1)
  }
}

/** Release the hold this extension took, unless the user's pause holds the scheduler too (`hold.ts`). */
function release(host: ExtensionHost) {
  const s = shared(host)
  if (!s.holding) return
  s.holding = false
  releaseHold(host.harness(), host.want(PauseApi)?.paused() ?? false)
}

/**
 * Confirm this generation once it is pending and proves it works here, then let the scheduler run. It stops only when
 * the daemon reports this generation is no longer pending (permanent: release; rolled back: this process is stopped);
 * a refused or lost confirm just goes round again on the next status. While the switch is being rolled back it is
 * still pending but no longer confirmable, and the hold stays until the rollback is done.
 */
async function confirmWhenReady(host: ExtensionHost, socket: string, generation: string) {
  const s = shared(host)
  while (s.stop) {
    const status = s.status
    if (status && status.pending?.generation !== generation) {
      if (status.permanent === generation) s.confirming = { state: 'confirmed' }
      release(host)
      return
    }
    // Confirmed already: wait for the status that says so, rather than confirm again and be refused.
    if (status && awaitingConfirm(status) && s.confirming?.state !== 'confirmed') {
      const verdict = await mayConfirm(host)
      if (verdict.ok) {
        try {
          await request(socket, 'confirm', { generation })
          s.confirming = { state: 'confirmed' }
          host.notice(`Fawkes: ${generation} confirmed itself and is now permanent`)
        } catch (error) {
          s.confirming = { state: 'refused', reason: (error as Error).message }
        }
      } else s.confirming = { state: 'checking', detail: verdict.reason }
    }
    await new Promise((resolve) => setTimeout(resolve, 2000))
  }
}

/** The remote a ref like `origin/main` names, if the clone has that remote; nothing to fetch otherwise. */
async function remoteOf(repo: string, ref: string) {
  const { stdout } = await run('git', ['remote'], { cwd: repo, timeout: 10_000 })
  return stdout.split('\n').find((remote) => remote && ref.startsWith(`${remote}/`))
}

/**
 * Count the commits on `ref` that the running generation lacks, fetching its remote first when `fetch` is set. A
 * failure is kept for the popover and rethrown, so an upgrade never stages a ref it could not fetch.
 */
async function check(host: ExtensionHost, fetch: boolean, ref = DEFAULT_REF) {
  const s = shared(host)
  const repo = host.ultimateRoot
  try {
    if (fetch) {
      const remote = await remoteOf(repo, ref)
      if (remote) await run('git', ['fetch', '--quiet', remote], { cwd: repo, timeout: 60_000 })
    }
    const range = `HEAD..${ref}`
    const { stdout } = await run(
      'git',
      ['log', '--format=%h\t%s', '--max-count=20', '--end-of-options', range],
      { cwd: repo, timeout: 10_000 },
    )
    const { stdout: count } = await run('git', ['rev-list', '--count', '--end-of-options', range], {
      cwd: repo,
      timeout: 10_000,
    })
    s.behind = {
      ref,
      count: Number(count.trim()),
      commits: stdout
        .split('\n')
        .filter(Boolean)
        .map((line) => {
          const [sha, ...subject] = line.split('\t')
          return { sha, subject: subject.join('\t') }
        }),
      fetchedAt: Date.now(),
    }
  } catch (error) {
    s.behind = {
      ref,
      count: s.behind?.count ?? 0,
      commits: s.behind?.commits ?? [],
      fetchedAt: Date.now(),
      error: (error as Error).message.split('\n')[0],
    }
    throw error
  }
}

/**
 * Fetch `ref`, then `fawkes up` it: one call that stages, tests and switches. Runs in the background. In the live app
 * the switch stops this process, and the new generation confirms itself after its boot or is rolled back.
 */
function upgrade(host: ExtensionHost, socket: string, ref: string) {
  const s = shared(host)
  if (s.upgradeRun) throw new Error(`An upgrade to ${s.upgrade?.ref} is already running`)
  const refusal = upgradeRefusal(s.status)
  if (refusal) throw new Error(refusal)
  const bin = process.env.FAWKES_BIN
  if (!bin) throw new Error('FAWKES_BIN is not set, though Fawkes sets it for every app it starts')
  const upgrade: Upgrade = { ref, state: 'fetching', at: Date.now() }
  const set = (next: Partial<Upgrade>) => (s.upgrade = Object.assign(upgrade, next, { at: Date.now() }))
  s.upgrade = upgrade
  s.upgradeRun = (async () => {
    try {
      await check(host, true, ref)
    } catch (error) {
      set({ state: 'failed', error: { message: `cannot fetch ${ref}: ${(error as Error).message.split('\n')[0]}` } })
      return
    }
    set({ state: 'running' })
    try {
      set({ state: 'switched', result: await up(bin, socket, ref) })
    } catch (error) {
      set({ state: 'failed', error: error instanceof UpFailure ? error.error : { message: String(error) } })
    }
  })().finally(() => (s.upgradeRun = undefined))
}

const text = (value: string) => ({ output: [{ type: 'text' as const, text: value }] })

/** The upgrade this process started, in one line. */
function upgradeLine({ ref, state, result, error }: Upgrade, status: Status | undefined) {
  if (state === 'fetching') return `Upgrade to ${ref}: fetching it.`
  if (state === 'running') return `Upgrade to ${ref}: fawkes up runs${status?.busy ? ` (${status.busy})` : ''}.`
  if (state === 'switched' && result)
    return `Upgrade to ${ref}: switched to ${result.generation}${result.existing ? ' (already built)' : ''}; it confirms itself by ${result.deadline}.`
  const at = error?.step ? ` at its ${error.step} step` : ''
  return `Upgrade to ${ref} failed${at}: ${error?.code ? `${error.code}: ` : ''}${error?.message ?? 'no reason'}${error?.log ? ` (log: ${error.log})` : ''}`
}

/** How the latest switch ended, with where it went and what became of the storage when it rolled back. */
function lastSwitchLine(state: Extract<FawkesState, { supervised: true }>) {
  const target = lastSwitched(state.generations)
  const decided = target && outcome(state.status, state.generations, target.id)
  if (!decided || decided.state === 'waiting') return undefined
  const event = rollbackEvent(state.events, decided.generation)
  const more = [event?.to && `back to ${event.to}`, event?.storage && storageText(event.storage)].filter(Boolean)
  return `Last switch: ${outcomeText(decided)}${more.length ? `; ${more.join('; ')}` : ''}.`
}

function describe(state: FawkesState) {
  if (!state.supervised) return 'This Ultimate does not run under Fawkes.'
  const { status, generations } = state
  if (!status) return `This Ultimate runs as ${state.generation}; the Fawkes daemon has not answered yet.`
  const pending = awaitingConfirm(status)
  const lines = [
    `Running ${state.generation} (permanent ${status.permanent ?? 'none'}${status.version ? `, daemon ${status.version}` : ''}).`,
    outdated(status),
    pending && `${pending.generation} awaits its confirm by ${pending.deadline}, or rolls back to ${pending.previous ?? 'nothing'}.`,
    rollbackNote(status),
    status.busy && `In progress: ${status.busy}.`,
    status.actions && `Fawkes accepts now: ${status.actions.join(', ') || 'nothing'}.`,
    lastSwitchLine(state),
    state.upgrade && upgradeLine(state.upgrade, status),
  ].filter((line): line is string => !!line)
  for (const g of generations.slice(-6).reverse())
    lines.push(
      `${g.id} ${g.ref}@${g.commit.slice(0, 8)} ${g.state}${g.state === 'rolled_back' ? ` (${causeText(g.rollback_cause, g.rollback_reason)})` : ''}`,
    )
  const recent = state.events.filter((event) => event.kind !== 'progress').slice(-5)
  if (recent.length) lines.push('Recent events:', ...recent.map((event) => `  ${event.at} ${eventLine(event)}`))
  if (state.behind) lines.push(`${state.behind.ref} has ${state.behind.count} commits this generation lacks.`)
  return lines.join('\n')
}

/** What an agent must know right now, for the prompt section: a pending confirm, the latest rollback, an old daemon. */
function attention(state: FawkesState) {
  if (!state.supervised || !state.status) return undefined
  const pending = awaitingConfirm(state.status)
  if (pending) return `${pending.generation} awaits its confirm by ${pending.deadline}.`
  const target = lastSwitched(state.generations)
  const decided = target && outcome(state.status, state.generations, target.id)
  const failed = decided?.state === 'rolled_back' || decided?.state === 'storage_failed'
  return outdated(state.status) ?? rollbackNote(state.status) ?? (failed ? lastSwitchLine(state) : undefined)
}

/** The live generation's socket; every action that moves the app refuses elsewhere (a test run, an inert process). */
function liveSocket() {
  if (fawkes.kind !== 'live') throw new Error('This Ultimate is not the live generation of a Fawkes daemon')
  return fawkes.socket
}

export const actions = {
  /** No arguments → what the status line shows. */
  async state(_args: unknown, { host }: { host: ExtensionHost }) {
    return state(host)
  },
  /** No arguments → fetch the clone and count the commits an upgrade would bring. */
  async check(_args: unknown, { host }: { host: ExtensionHost }) {
    liveSocket()
    // A failure is shown in the popover from the state.
    await check(host, true).catch(() => undefined)
    return state(host)
  },
  /** `{ ref? }` → start an upgrade to `ref` (default origin/main). */
  async upgrade(args: unknown, { host }: { host: ExtensionHost }) {
    upgrade(host, liveSocket(), validRef((args as { ref?: unknown } | null)?.ref))
    return state(host)
  },
  /** No arguments → confirm the pending generation now, without waiting for its own checks. */
  async confirm(_args: unknown, { host }: { host: ExtensionHost }) {
    const socket = liveSocket()
    const pending = awaitingConfirm(shared(host).status)
    if (!pending) throw new Error('No generation awaits a confirm')
    await request(socket, 'confirm', { generation: pending.generation })
    return state(host)
  },
  /** No arguments → roll the pending generation back to the permanent one now (code and storage), or retry a failed restore. */
  async rollback(_args: unknown, { host }: { host: ExtensionHost }) {
    const socket = liveSocket()
    const { status } = shared(host)
    if (!awaitingConfirm(status) && !restoreFailed(status)) throw new Error('No generation awaits a confirm')
    // The rollback stops this process when it is the pending one: its answer may never arrive.
    void request(socket, 'rollback', undefined, { timeout: 600_000 }).catch((error: unknown) =>
      host.notice(`Fawkes rollback: ${String(error)}`),
    )
    return state(host)
  },
}
/** The status line polls it. */
export const pollActions = ['state']
/** Moving the live process is the user's call. */
export const userActions = ['upgrade', 'confirm', 'rollback']

export default function fawkesExtension(host: ExtensionHost) {
  if (fawkes.kind === 'inert') return defineExtension({ name: 'fawkes' })
  const daemon = fawkes
  const s = shared(host)
  host.want(PauseApi)
  // Before the server resumes scheduling (it waits for every opener): a test run never resumes the copied storage's
  // work, and a new live generation resumes it only once it is confirmed, so a rollback forgets nothing that ran.
  // Every open holds again until then: a failed core swap reopens a new harness in this same process.
  host.onOpen(async () => {
    if (daemon.kind === 'test' || s.confirming?.state !== 'confirmed') hold(host)
    if (!s.stop) {
      s.stop = subscribe(daemon.socket, {
        connected: (connected, error) => {
          s.connected = connected
          s.error = connected ? undefined : error
        },
        status: (status) => (s.status = status),
        history: (generations) => (s.generations = generations),
        event: (event) => {
          // A reconnect without `since` (after a facet reload) replays events this process already has.
          const last = s.events.at(-1)?.seq ?? 0
          if (event.seq && event.seq <= last) return
          s.events = [...s.events, event].slice(-30)
        },
      })
    }
    if (daemon.kind === 'live' && !s.confirmRun)
      s.confirmRun = confirmWhenReady(host, daemon.socket, daemon.generation).finally(
        () => (s.confirmRun = undefined),
      )
  })
  if (daemon.kind === 'live') {
    const quiet = () => void check(host, false).catch(() => undefined)
    const first = setTimeout(quiet, 30_000)
    const timer = setInterval(quiet, 5 * 60_000)
    host.onClose(async () => {
      clearTimeout(first)
      clearInterval(timer)
    })
  }
  host.onClose(async () => {
    s.stop?.()
    s.stop = undefined
  })
  if (daemon.kind === 'test') return defineExtension({ name: 'fawkes' })
  return defineExtension({
    name: 'fawkes',
    sections: [
      section('fawkes', () => {
        const now = attention(state(host))
        return `This Ultimate runs under the Fawkes supervisor as a generation: a git worktree of the Ultimate clone at a fixed commit. Never edit the generation's folder (\`${host.ultimateRoot}\`); landed changes go live only through an upgrade (fawkes tool, action upgrade: \`fawkes up\` stages, tests and switches, then the new generation confirms itself, or Fawkes rolls back code and storage). \`restart\` reboots the same generation.${now ? `\nNow: ${now}` : ''}`
      }),
    ],
    tools: [
      defineTool({
        name: 'fawkes',
        replay: 'unsafe',
        description:
          'Ultimate under the Fawkes supervisor. status: this generation, how the latest switch ended (a rollback with its cause), the others, recent events and an upgrade in progress. upgrade {ref?}: `fawkes up` ref (default origin/main) in the background, which stages, tests and switches; the switch restarts this Ultimate and the new generation confirms itself, or is rolled back. After the restart, status says which.',
        parameters: obj({
          action: oneOf(['status', 'upgrade']),
          ref: opt(str({ description: 'Git ref to upgrade to; default origin/main' })),
        }),
        async execute(args) {
          if (args.action === 'upgrade') {
            const ref = validRef(args.ref)
            upgrade(host, daemon.socket, ref)
            return text(`Upgrading to ${ref}; follow it with status.\n${describe(state(host))}`)
          }
          await check(host, false).catch(() => undefined)
          return text(describe(state(host)))
        },
      }),
    ],
  })
}
