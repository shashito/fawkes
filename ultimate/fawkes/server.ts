import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { obj, oneOf, opt, str } from '@earendil-works/pi-schema'
import { defineExtension, defineTool, section } from '@earendil-works/pi-durable'
// Types only: at runtime this folder must not load code from a clone other than the generation it runs in.
import type { ExtensionHost } from 'ultimate/server/extensions.ts'
import type { PauseApi as PauseApiType } from 'ultimate/extensions/pause/api.ts'
import { FawkesError, request, subscribe } from './client.ts'
import { mayConfirm } from './confirm.ts'
import type { Confirming, FawkesEvent, FawkesState, Generation, Status, Upgrade } from './types.ts'

const run = promisify(execFile)
/** The pause extension's service, looked up by its name like any `want`. */
const PauseApi = { name: 'pause' } as typeof PauseApiType

/** What Fawkes gives the app it starts (DESIGN.md, "Manifest"); none outside a daemon, where this extension is inert. */
const supervised = () => {
  const socket = process.env.FAWKES_SOCKET
  const generation = process.env.FAWKES_GENERATION
  const phase = process.env.FAWKES_PHASE
  return socket && generation && (phase === 'live' || phase === 'test')
    ? { socket, generation, phase }
    : undefined
}

/** The ref an upgrade stages unless it names another. */
const DEFAULT_REF = 'origin/main'

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
  const fawkes = supervised()
  if (!fawkes) return { supervised: false }
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

/** Release the hold this extension took, unless the user paused the host meanwhile (pause holds it the same way). */
function release(host: ExtensionHost) {
  const s = shared(host)
  if (!s.holding) return
  s.holding = false
  if (!host.want(PauseApi)?.paused()) host.harness().release()
}

/**
 * Confirm this generation once it is pending and proves it works here, then let the scheduler run. Checked again until
 * the deadline: a facet still loading or a page not served yet just waits; Fawkes rolls back if it never passes.
 */
async function confirmWhenReady(host: ExtensionHost, fawkes: NonNullable<ReturnType<typeof supervised>>) {
  const s = shared(host)
  for (;;) {
    const pending = s.status?.pending
    if (s.status && pending?.generation !== fawkes.generation) {
      // Not pending (a restart of the permanent generation, or a confirm from elsewhere): nothing to vouch for.
      s.confirming = s.status.permanent === fawkes.generation ? { state: 'confirmed' } : s.confirming
      release(host)
      return
    }
    if (s.status) {
      const verdict = await mayConfirm(host)
      if (verdict.ok) {
        try {
          await request(fawkes.socket, 'confirm', { generation: fawkes.generation })
          s.confirming = { state: 'confirmed' }
          release(host)
          host.notice(`Fawkes: ${fawkes.generation} confirmed itself and is now permanent`)
          return
        } catch (error) {
          s.confirming = { state: 'refused', reason: (error as Error).message }
          if (error instanceof FawkesError && error.code === 'invalid_state') return
        }
      } else s.confirming = { state: 'checking', detail: verdict.reason }
    }
    await new Promise((resolve) => setTimeout(resolve, 2000))
    if (!supervised() || s.stop === undefined) return
  }
}

/** Commits on the upgrade ref that the running generation does not have (after a fetch of the clone). */
async function check(host: ExtensionHost, fetch: boolean, ref = DEFAULT_REF) {
  const s = shared(host)
  const repo = host.ultimateRoot
  try {
    if (fetch) {
      const remote = ref.includes('/') ? ref.slice(0, ref.indexOf('/')) : undefined
      if (remote) await run('git', ['fetch', '--quiet', remote], { cwd: repo, timeout: 60_000 })
    }
    const { stdout } = await run(
      'git',
      ['log', '--format=%h\t%s', '--max-count=20', `HEAD..${ref}`],
      { cwd: repo, timeout: 10_000 },
    )
    const { stdout: count } = await run('git', ['rev-list', '--count', `HEAD..${ref}`], {
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
  }
}

/** Stage, test and switch the upgrade ref; the new generation confirms itself after its boot. Runs in the background. */
function upgrade(host: ExtensionHost, socket: string, ref: string) {
  const s = shared(host)
  if (s.upgradeRun) throw new Error(`An upgrade to ${s.upgrade?.ref} is already running`)
  if (s.status?.pending) throw new Error(`${s.status.pending.generation} still awaits its confirm`)
  if (s.status?.busy) throw new Error(`Fawkes is busy: ${s.status.busy}`)
  const step = (next: Partial<Upgrade>) => {
    s.upgrade = { ...(s.upgrade ?? { ref, step: 'fetching' }), ...next, at: Date.now() }
  }
  const progress = (event: FawkesEvent) => step({ progress: event.message })
  s.upgrade = { ref, step: 'fetching', at: Date.now() }
  s.upgradeRun = (async () => {
    try {
      await check(host, true, ref)
      step({ step: 'staging', progress: undefined })
      // An install reports while it runs; silence for 10 minutes means it hangs.
      const staged = await request<Generation>(socket, 'stage', { ref }, { progress, timeout: 600_000 })
      step({ step: 'testing', generation: staged.id, progress: undefined })
      await request<Generation>(socket, 'test', { generation: staged.id }, { progress, timeout: 600_000 })
      step({ step: 'switching', progress: undefined })
      // The switch stops this process: its answer may never arrive. The new generation reports from here on.
      await request(socket, 'switch', { generation: staged.id }, { progress, timeout: 600_000 })
      step({ step: 'done' })
    } catch (error) {
      step({ step: 'failed', error: (error as Error).message })
    } finally {
      s.upgradeRun = undefined
    }
  })()
}

const text = (value: string) => ({ output: [{ type: 'text' as const, text: value }] })

function describe(state: FawkesState) {
  if (!state.supervised) return 'This Ultimate does not run under Fawkes.'
  const { status, generations } = state
  if (!status) return `This Ultimate runs as ${state.generation}; the Fawkes daemon has not answered yet.`
  const lines = [
    `Running ${state.generation} (permanent ${status.permanent ?? 'none'}${status.pending ? `, ${status.pending.generation} pending until ${status.pending.deadline}` : ''}${status.busy ? `, busy: ${status.busy}` : ''}).`,
  ]
  for (const g of generations.slice(-6).reverse())
    lines.push(`${g.id} ${g.ref}@${g.commit.slice(0, 8)} ${g.state}${g.rollback_reason ? ` (${g.rollback_reason})` : ''}`)
  if (state.behind) lines.push(`${state.behind.ref} has ${state.behind.count} commits this generation lacks.`)
  if (state.upgrade) lines.push(`Upgrade to ${state.upgrade.ref}: ${state.upgrade.step}${state.upgrade.error ? `: ${state.upgrade.error}` : ''}`)
  return lines.join('\n')
}

export const actions = {
  /** No arguments → what the status line shows. */
  async state(_args: unknown, { host }: { host: ExtensionHost }) {
    return state(host)
  },
  /** No arguments → fetch the clone and count the commits an upgrade would bring. */
  async check(_args: unknown, { host }: { host: ExtensionHost }) {
    if (supervised()?.phase === 'live') await check(host, true)
    return state(host)
  },
  /** `{ ref? }` → start an upgrade to `ref` (default origin/main). */
  async upgrade(args: unknown, { host }: { host: ExtensionHost }) {
    const fawkes = supervised()
    if (fawkes?.phase !== 'live') throw new Error('This Ultimate does not run under a Fawkes daemon')
    const ref = (args as { ref?: unknown } | null)?.ref
    if (ref !== undefined && (typeof ref !== 'string' || !/^[\w./@^~-]+$/.test(ref) || ref.startsWith('-')))
      throw new Error('ref must be a git ref')
    upgrade(host, fawkes.socket, (ref as string | undefined) ?? DEFAULT_REF)
    return state(host)
  },
  /** No arguments → confirm the pending generation now, without waiting for its own checks. */
  async confirm(_args: unknown, { host }: { host: ExtensionHost }) {
    const fawkes = supervised()
    const pending = shared(host).status?.pending
    if (!fawkes || !pending) throw new Error('No generation awaits a confirm')
    await request(fawkes.socket, 'confirm', { generation: pending.generation })
    return state(host)
  },
  /** No arguments → roll the pending generation back to the permanent one now (code and storage). */
  async rollback(_args: unknown, { host }: { host: ExtensionHost }) {
    const fawkes = supervised()
    if (!fawkes || !shared(host).status?.pending) throw new Error('No generation awaits a confirm')
    // The rollback stops this process when it is the pending one: its answer may never arrive.
    void request(fawkes.socket, 'rollback', undefined, { timeout: 600_000 }).catch((error: unknown) =>
      host.notice(`Fawkes rollback: ${String(error)}`),
    )
    return state(host)
  },
}
/** The status line polls it. */
export const pollActions = ['state']
/** Moving the live process is the user's call. */
export const userActions = ['upgrade', 'confirm', 'rollback']

export default function fawkes(host: ExtensionHost) {
  const daemon = supervised()
  if (!daemon) return defineExtension({ name: 'fawkes' })
  const s = shared(host)
  host.want(PauseApi)
  // Before the server resumes scheduling (it waits for every opener): a test run never resumes the copied storage's
  // work, and a new live generation resumes it only once it is confirmed, so a rollback forgets nothing that ran.
  host.onOpen(async () => {
    if (!s.stop) {
      // A facet reload after the confirm opens again in the same process: nothing to hold for then.
      if (s.confirming?.state !== 'confirmed') {
        s.holding = true
        host.harness().hold()
      }
      s.stop = subscribe(daemon.socket, {
        connected: (connected, error) => {
          s.connected = connected
          s.error = connected ? undefined : error
        },
        status: (status) => (s.status = status),
        history: (generations) => (s.generations = generations),
        event: (event) => {
          s.events = [...s.events, event].slice(-30)
          if (s.upgrade && event.generation === s.upgrade.generation && event.kind === 'progress')
            s.upgrade = { ...s.upgrade, progress: event.message, at: Date.now() }
        },
      })
    }
    if (daemon.phase === 'live' && !s.confirmRun)
      s.confirmRun = confirmWhenReady(host, daemon).finally(() => (s.confirmRun = undefined))
  })
  if (daemon.phase === 'live') {
    const timer = setInterval(() => void check(host, false), 5 * 60_000)
    setTimeout(() => void check(host, false), 30_000)
    host.onClose(async () => clearInterval(timer))
  }
  host.onClose(async () => {
    s.stop?.()
    s.stop = undefined
  })
  return defineExtension({
    name: 'fawkes',
    sections:
      daemon.phase === 'live'
        ? [
            section(
              'fawkes',
              () =>
                `This Ultimate runs under the Fawkes supervisor as a generation: a git worktree of the Ultimate clone at a fixed commit. Never edit the generation's folder (\`${host.ultimateRoot}\`); landed changes go live only through an upgrade (fawkes tool, action upgrade: stage, test, switch, then the new generation confirms itself, or Fawkes rolls back code and storage). \`restart\` reboots the same generation.`,
            ),
          ]
        : [],
    tools:
      daemon.phase === 'live'
        ? [
            defineTool({
              name: 'fawkes',
              replay: 'unsafe',
              description:
                'Ultimate under the Fawkes supervisor. status: this generation, the others and an upgrade in progress. upgrade {ref?}: stage, test and switch ref (default origin/main) in the background; the switch restarts this Ultimate and the new generation confirms itself.',
              parameters: obj({
                action: oneOf(['status', 'upgrade']),
                ref: opt(str({ description: 'Git ref to upgrade to; default origin/main' })),
              }),
              async execute(args) {
                if (args.action === 'upgrade') {
                  if (args.ref !== undefined && (!/^[\w./@^~-]+$/.test(args.ref) || args.ref.startsWith('-')))
                    throw new Error('ref must be a git ref')
                  upgrade(host, daemon.socket, args.ref ?? DEFAULT_REF)
                  return text(`Upgrading to ${args.ref ?? DEFAULT_REF}; follow it with status.\n${describe(state(host))}`)
                }
                await check(host, false)
                return text(describe(state(host)))
              },
            }),
          ]
        : [],
  })
}
