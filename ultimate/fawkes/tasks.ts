/**
 * What a host asks an @ultimate to do after an upgrade, and when an extension the user keeps working stops loading: the
 * user's settings for it, the failures that call for an investigation, the autofix decision, and every text, each
 * naming its facts plainly. Pure: the server facet reads the daemon and the shared files (`store.ts`) and delivers.
 */
import { diskText, eventLine, gigabytes, outcome, outcomeText, lastSwitched, rollbackEvent, storageText } from './facts.ts'
import { shellWord } from './setup.ts'
import type { Cause, Disk, FawkesEvent, Generation, History, Status } from './types.ts'

// ------------------------------------------------ settings

/** `true` runs the default instructions, `false` turns the task off, a string replaces the default instructions. */
export type Instructions = boolean | string

export type TaskSettings = {
  /** The project whose @ultimate gets the upgrade tasks and the autofix give-ups; this host's own when absent. */
  project?: string
  success: Instructions
  failure: Instructions
  /** The extensions repaired hands-off when they fail to load. */
  autofix: string[]
}

const where = 'in ~/.ultimate/settings.json'
const shown = (value: unknown) => JSON.stringify(value) ?? String(value)

function instructions(key: 'success' | 'failure', value: unknown): Instructions {
  if (value === undefined) return true
  if (typeof value === 'boolean' || (typeof value === 'string' && value.trim())) return value
  throw new Error(
    `fawkes.afterUpgrade.${key} ${where} must be true, false or the instructions as text; it is ${shown(value)}`,
  )
}

/** `afterUpgrade` and `autofix` of the user's `fawkes` settings; a value they do not allow throws, naming the setting. */
export function parseTaskSettings(fawkes: { afterUpgrade?: unknown; autofix?: unknown }): TaskSettings {
  const { afterUpgrade, autofix = [] } = fawkes
  // Names become file names in the shared folder (`autofix-<name>.json`), so only the characters of a folder name.
  if (!Array.isArray(autofix) || !autofix.every((name) => typeof name === 'string' && /^[\w.-]+$/.test(name)))
    throw new Error(`fawkes.autofix ${where} must be a list of extension names; it is ${shown(autofix)}`)
  if (afterUpgrade === false) return { success: false, failure: false, autofix }
  if (afterUpgrade === undefined) return { success: true, failure: true, autofix }
  if (
    typeof afterUpgrade !== 'object' ||
    afterUpgrade === null ||
    Array.isArray(afterUpgrade) ||
    Object.keys(afterUpgrade).some((key) => !['project', 'success', 'failure'].includes(key))
  )
    throw new Error(
      `fawkes.afterUpgrade ${where} must be false or an object of project, success and failure; it is ${shown(afterUpgrade)}`,
    )
  const { project, success, failure } = afterUpgrade as Record<string, unknown>
  if (project !== undefined && (typeof project !== 'string' || !project.trim()))
    throw new Error(`fawkes.afterUpgrade.project ${where} must be a project's name; it is ${shown(project)}`)
  return {
    ...(project !== undefined && { project: project as string }),
    success: instructions('success', success),
    failure: instructions('failure', failure),
    autofix,
  }
}

// ------------------------------------------------ failures

/** How long after it happened a failure still calls for an investigation. */
export const FAILURE_WINDOW = 24 * 3_600_000

/** An upgrade that failed: its test, its switch (rolled back), or its stage. */
export type Failure = {
  kind: 'test' | 'rollback' | 'stage'
  generation: string
  /** The commit it was built from; a failed stage removed its generation, and the commit with it. */
  commit?: string
  /** Its claim in the shared folder: one investigation per failed commit, whichever host saw it first. */
  key: string
  /** The event that says it: `test_failed`, or the `error` of the stage. */
  event?: FawkesEvent
}

/** Rollbacks that are no failure of the generation: the user asked, or the daemon restarted under a pending switch. */
const notFailures = new Set<Cause | null>(['requested', 'daemon_restart'])

/**
 * A failed stage logs an `error` naming the generation it then removes, as "stage of <ref> failed: …" (the daemon's
 * `stage` in `daemon/transactions.rs`): DESIGN.md gives the event no field of its own, so its words and the missing
 * generation tell it from the other errors.
 */
const stageFailed = (event: FawkesEvent, commit: string | undefined) =>
  event.kind === 'error' && !commit && event.message.startsWith('stage of ')

/** Since when a failure calls for an investigation: the last `FAILURE_WINDOW`, and not before the tasks first ran. */
export const failingSince = (installed: number, now: number) => Math.max(installed, now - FAILURE_WINDOW)

/** The failed upgrades since `since`, one per claim key, a rollback before the test of its commit. */
export function failures(
  host: string,
  status: Status,
  generations: Generation[],
  events: FawkesEvent[],
  since: number,
): Failure[] {
  const recent = (at: string | null | undefined) => Date.parse(at ?? '') >= since
  const commitOf = (id: string) => generations.find((g) => g.id === id)?.commit
  // Generation ids are the daemon's own: without a commit, the host's name keeps them apart.
  const key = (id: string, commit: string | undefined) => `failure-${commit ?? `${host}-${id}`}`
  const found: Failure[] = []
  const last = lastSwitched(generations)
  const decided = last && outcome(status, generations, last.id)
  if (last && decided?.state === 'rolled_back' && !notFailures.has(decided.cause) && recent(last.rolled_back_at))
    found.push({ kind: 'rollback', generation: last.id, commit: last.commit, key: key(last.id, last.commit) })
  for (const event of events) {
    const id = event.generation
    if (!id || !recent(event.at)) continue
    const commit = commitOf(id)
    if (event.kind === 'test_failed') found.push({ kind: 'test', generation: id, commit, key: key(id, commit), event })
    else if (stageFailed(event, commit)) found.push({ kind: 'stage', generation: id, key: key(id, undefined), event })
  }
  return found.filter((failure, index) => found.findIndex((other) => other.key === failure.key) === index)
}

/** A failure in a few words, for the notice that says who investigates it. */
export const failureName = (failure: Failure) =>
  `${failure.generation}'s ${{ test: 'failed test', rollback: 'rollback', stage: 'failed stage' }[failure.kind]}`

// ------------------------------------------------ autofix

/** How long a try is given before the next, or before Fawkes gives up. */
export const AUTOFIX_WAIT = 30 * 60_000
export const AUTOFIX_TRIES = 2

/**
 * What the shared folder keeps for one extension under repair (`autofix-<name>.json`); `host` is the one whose failure
 * the last try was for.
 */
export type AutofixRecord = { error: string; tries: number; lastTry: number; host: string; gaveUp?: boolean }

export type AutofixStep = 'try' | 'wait' | 'give-up' | 'recovered' | 'idle'

/**
 * What to do for a listed extension: a first try when it fails without a record, another once the last try had its
 * time, giving up (once) after the last try, forgetting the record once it loads again on `host`. Only the host that
 * failed can say so: one that loaded the extension before it broke, and has not reloaded it since, would otherwise
 * forget the record every minute, and the failing host try again at once.
 */
export function autofixStep(
  record: AutofixRecord | undefined,
  failing: boolean,
  now: number,
  host: string,
): AutofixStep {
  if (!failing) return record?.host === host ? 'recovered' : 'idle'
  if (!record) return 'try'
  if (record.gaveUp) return 'idle'
  if (now - record.lastTry < AUTOFIX_WAIT) return 'wait'
  return record.tries < AUTOFIX_TRIES ? 'try' : 'give-up'
}

// ------------------------------------------------ low disk

/** The file in the shared folder that says a low-disk episode was told: its claim, removed when the episode ends. */
export const DISK_LOW = 'disk-low'

export type DiskStep = 'claim' | 'clear' | 'idle'

/**
 * What a low disk calls for, from the daemon's `status.disk` and whether the episode's claim file exists: claim it
 * while the disk is low and nobody has, forget it once the disk is not low. No `disk` (an older daemon, or a failed
 * measurement) says nothing about the episode.
 */
export function diskStep(disk: Disk | null | undefined, claimed: boolean): DiskStep {
  if (!disk) return 'idle'
  if (disk.low) return claimed ? 'idle' : 'claim'
  return claimed ? 'clear' : 'idle'
}

// ------------------------------------------------ texts

/** The host a text comes from. */
export type HostFacts = { host: string; manifest: string }

/** Who owns an extension's folder: the git repository it is in, and the Ultimate project of that repository. */
export type Owner = { path: string; repo?: string; project?: string }

/** An extension this host loads beyond the built-in ones. */
export type Owned = Owner & { name: string; source: string }

const short = (commit: string) => commit.slice(0, 8)

const ownerText = (owner: Owner) =>
  owner.project
    ? `project ${owner.project}`
    : owner.repo
      ? `the repository ${owner.repo}, which no Ultimate project has`
      : 'in no git repository'

const historyCommand = (manifest: string) => `\`fawkes history --manifest ${shellWord(manifest)} --json\``

/** A fenced block that no run of backticks in the text can close. */
function fenced(text: string) {
  const longest = Math.max(2, ...[...text.matchAll(/`+/g)].map((match) => match[0].length))
  const fence = '`'.repeat(longest + 1)
  return `${fence}text\n${text}\n${fence}`
}

/** The last `count` lines of a log tail, without its trailing newline. */
const lastLines = (tail: string, count: number) => tail.trimEnd().split('\n').slice(-count).join('\n')

export type ReviewFacts = HostFacts & {
  generation: string
  ref: string
  /** The Ultimate clone the generations come from. */
  clone: string
  /** The base of the review's diff: where the newest reviewed tip left the history of `to`. */
  from: string
  to: string
  /** The reviewed tips: the commits to review are those of `to` in none of them. */
  reviewed: string[]
  count: number
  inventory: Owned[]
}

export function successFacts(facts: ReviewFacts) {
  const { host, manifest, generation, ref, clone, from, to, reviewed, count, inventory } = facts
  const range = [to, ...reviewed.map((tip) => `^${tip}`)].join(' ')
  return [
    'Facts:',
    `- Host: ${host}, manifest ${manifest}`,
    `- Generation: ${generation}, ${ref}@${short(to)}, now permanent`,
    `- Ultimate clone: ${clone}`,
    `- Commits to review (<range>): \`${range}\`, ${count} commit${count === 1 ? '' : 's'}`,
    `- Base of their diff (<from>..<to>): \`${from}..${to}\``,
    '- Extensions this host loads beyond the built-in ones:',
    ...(inventory.length
      ? inventory.map((each) => `  - ${each.name} (${each.source}): ${each.path}, ${ownerText(each)}`)
      : ['  - none']),
  ].join('\n')
}

/** The success task: the review of what the new Ultimate commits offer every extension. */
export function successTask(instructions: string | true, folder: string, facts: ReviewFacts) {
  const what =
    instructions === true
      ? `Review what these Ultimate commits break, deprecate or newly offer every extension below: follow \`${folder}/review.md\` over the range in the facts. Propose only: change nothing without the user's go.`
      : instructions
  const lead = `Fawkes: ${facts.host} now runs Ultimate ${short(facts.to)} as ${facts.generation}, ${facts.count} commit${facts.count === 1 ? '' : 's'} since the last review.`
  return `${lead}\n\n${what}\n\n${successFacts(facts)}`
}

/** What the daemon knows about a failure: its status, its history (with log tails) and its recent events. */
export type Daemon = { status: Status; history: History; events: FawkesEvent[] }

/** The failure in words, with its cause, reason and storage outcome, from the daemon's fields. */
function failureWords(failure: Failure, daemon: Daemon) {
  const g = daemon.history.generations.find((each) => each.id === failure.generation)
  if (failure.kind === 'rollback') {
    const decided = outcome(daemon.status, daemon.history.generations, failure.generation)
    const event = rollbackEvent(daemon.events, failure.generation)
    return {
      what: event ? eventLine(event) : decided ? outcomeText(decided) : `${failure.generation} rolled back`,
      cause: g?.rollback_cause ?? event?.cause,
      reason: g?.rollback_reason ?? event?.reason,
      storage: event?.storage && storageText(event.storage),
    }
  }
  const event = failure.event
  if (failure.kind === 'test')
    return {
      what: event ? eventLine(event) : `${failure.generation} failed its test`,
      cause: 'test_failed',
      reason: g?.test?.message,
    }
  return {
    what: event ? eventLine(event) : `the stage of ${failure.generation} failed`,
    cause: event?.error_code,
    reason: event?.message,
  }
}

export function failureFacts(host: HostFacts, failure: Failure, daemon: Daemon) {
  const { generations, failed_install_tail } = daemon.history
  const g = generations.find((each) => each.id === failure.generation)
  const permanent = generations.find((each) => each.id === daemon.status.permanent)
  const { what, cause, reason, storage } = failureWords(failure, daemon)
  const confirm = daemon.events.findLast(
    (event) => event.kind === 'reported' && event.generation === failure.generation,
  )?.info?.confirm
  const [logName, tail] =
    failure.kind === 'stage'
      ? ['install-failed.log, the install output of the failed stage', failed_install_tail]
      : [`${failure.generation}'s logs/app.log`, g?.log_tail]
  const lines = [
    'Facts:',
    `- Host: ${host.host}, manifest ${host.manifest}`,
    `- Generation: ${failure.generation}${g ? `, ${g.ref}@${short(g.commit)}` : ', removed by its failed stage'}`,
    `- What happened: ${what}`,
    `- Cause: ${cause ?? 'none recorded'}`,
    `- Reason: ${reason ?? 'none recorded'}`,
    storage && `- Storage: ${storage}`,
    `- Its last confirm report: ${confirm ?? 'none'}`,
    g && permanent && permanent.id !== g.id
      ? `- The Ultimate commits it brought over ${permanent.id}: ${permanent.commit}..${g.commit}`
      : undefined,
    `- The rest (every generation, its log tail, the failed install's): ${historyCommand(host.manifest)}`,
    '',
    tail ? `The last 20 lines of ${logName}:\n${fenced(lastLines(tail, 20))}` : `${logName}: empty or missing.`,
  ]
  return lines.filter((line) => line !== undefined).join('\n')
}

/** The failure task: an investigation right away. */
export function failureTask(instructions: string | true, host: HostFacts, failure: Failure, daemon: Daemon) {
  const what =
    instructions === true
      ? [
          "Investigate it now. Find the root cause with evidence: the facts below, `fawkes history`, the daemon's events",
          '(`fawkes log --manifest <manifest> --json`) and the Ultimate commits the generation brought. Say whether it lies',
          'in Ultimate core, an extension or Fawkes, and check the other hosts on the same commit (`fawkes status',
          '--manifest <manifest> --json` for each manifest in `~/.fawkes`). Propose the fix as a branch prompt, and change',
          "nothing without the user's go.",
        ].join(' ')
      : instructions
  const lead = `Fawkes: on ${host.host}, ${failureWords(failure, daemon).what}.`
  return `${lead}\n\n${what}\n\n${failureFacts(host, failure, daemon)}`
}

export type AutofixFacts = HostFacts &
  Owner & {
    name: string
    error: string
    /** 1 or 2. */
    attempt: number
    /** The error the first try left, on the second. */
    previous?: string
    clone: string
  }

/** The hands-off goal of one autofix try, for the owning project's @ultimate (or the coordinator, to route it). */
export function autofixBrief(facts: AutofixFacts) {
  const { name, path, repo, project, error, attempt, previous, clone, host, manifest } = facts
  const landing = project
    ? "- Pass the extension's own checks and a reviewer, then land through the project's lander."
    : repo
      ? `- Pass the extension's own checks and a reviewer, then commit in ${repo} and push it if it has a remote.`
      : `- The folder is in no repository: copy it aside first (to ${path}.before-autofix), fix it in place, and remove the copy once the facet loads.`
  const routing = project
    ? []
    : [
        '',
        `No Ultimate project owns this folder (it is ${ownerText(facts)}). Route this goal, word for word, to whoever owns ${facts.repo ?? path}; if nobody can, do it yourself under the same rules.`,
      ]
  return [
    `Fawkes autofix, try ${attempt} of ${AUTOFIX_TRIES}: the extension \`${name}\` fails to load on ${host}. Repair it hands-off: build, verify and land the fix without asking the user. You decide in the user's place.`,
    ...routing,
    '',
    'Facts:',
    `- Extension: ${name}, folder ${path}`,
    `- Owner: ${ownerText(facts)}`,
    `- Host: ${host}, manifest ${manifest}`,
    `- Ultimate clone: ${clone}; \`ultimate/...\` in a user or project extension resolves against its working tree`,
    `- Error:\n${fenced(error)}`,
    ...(previous ? [`- The first try did not fix it. It failed then with:\n${fenced(previous)}`] : []),
    '',
    'Rules:',
    `- Edit only the extension's folder, ${path}. Leave everything else alone: the rest of the Ultimate clone (${clone}) and Ultimate core, every Fawkes generation, and the rest of ~/.ultimate.`,
    '- Never merge, pull or reset in the clone: the break usually comes from its working tree moving, and the fix goes against that working tree as it is.',
    '- Never force push, never disable the extension or remove a feature to make it load, and add no dependency.',
    landing,
    `- Verify that the facet loads on ${host}: \`ultimate.extensions()\` shows no \`server.error\` for ${name}. The fix hot-reloads it on every host, so nothing else is needed.`,
    attempt < AUTOFIX_TRIES
      ? '- If you cannot fix it, say so and why in your final answer: Fawkes tries once more in 30 minutes, then tells the user.'
      : '- If you cannot fix it, say so and why in your final answer: this is the last try, and Fawkes then tells the user.',
  ].join('\n')
}

/** The coordinator's message once autofix gave up: tell the user. */
export function giveUpText(facts: HostFacts & { name: string; path: string; error: string; tries: number }) {
  return [
    `Fawkes autofix gave up: the extension \`${facts.name}\` still fails to load on ${facts.host} after ${facts.tries} tries to repair it. Tell the user it needs them, with these facts:`,
    '',
    `- Extension: ${facts.name}, folder ${facts.path}`,
    `- Error:\n${fenced(facts.error)}`,
    `- Tries: ${facts.tries}, ${AUTOFIX_WAIT / 60_000} minutes apart`,
    `- Fawkes tries no more until it loads again, or until ~/.ultimate/fawkes/autofix-${facts.name}.json is removed`,
  ].join('\n')
}

/** The coordinator's message when the disk runs low: find what takes the space and tell the user. */
export function diskLowText(facts: HostFacts & { disk: Disk; stateDir: string }) {
  const { host, manifest, disk, stateDir } = facts
  return [
    `Fawkes: the disk is low on ${host}: ${diskText(disk)} (the volume of ${stateDir}). Upgrades are refused below ${gigabytes(disk.min_free)} (the manifest's min_free). Find what takes the space (large files in $TMPDIR, caches, build folders, old worktrees) and tell the user what you found; delete nothing without their go.`,
    '',
    'Facts:',
    `- Host: ${host}, manifest ${manifest}`,
    `- Volume of: ${stateDir}`,
    `- Free: ${disk.free} bytes of ${disk.total}`,
    '- Fawkes tells you once per episode, for all hosts on this machine; it tells you again only after the disk has had room.',
  ].join('\n')
}
