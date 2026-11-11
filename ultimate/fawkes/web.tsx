import {
  ArrowDownIcon,
  CheckIcon,
  CopyIcon,
  LoaderCircleIcon,
  RefreshCwIcon,
  TimerIcon,
  TriangleAlertIcon,
  WrenchIcon,
} from 'lucide-react'
import { useEffect, useState } from 'react'
import { Button } from '@/components/ui/button'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover'
import { actionRef, api, cn, defineWebExtension, formatAgo, useAction, useNow } from '@/sdk'
import { causeText, eventLine, lastSwitched, outcome, outcomeText, rollbackEvent, storageText } from './facts.ts'
import { allows, awaitingConfirm, outdated, restoreFailed, rollbackNote, upgradeRefusal } from './step.ts'
import type { FawkesState, Generation, SetupPreview, SetupStep, Upgrade } from './types.ts'

type Supervised = Extract<FawkesState, { supervised: true }>

/** No arguments → what the status line shows. */
const StateAction = actionRef<null, FawkesState>('fawkes', 'state')
/** No arguments → fetch the clone and count what an upgrade would bring. */
const CheckAction = actionRef<null, FawkesState>('fawkes', 'check')
/** `{ ref? }` → start an upgrade (`fawkes upgrade`). */
const UpgradeAction = actionRef<{ ref?: string }, FawkesState>('fawkes', 'upgrade')
/** No arguments → confirm the pending generation now. */
const ConfirmAction = actionRef<null, FawkesState>('fawkes', 'confirm')
/** No arguments → roll the pending generation back now. */
const RollbackAction = actionRef<null, FawkesState>('fawkes', 'rollback')
/** No arguments → the terminal steps that set this host up. */
const SetupPreviewAction = actionRef<null, SetupPreview>('fawkes', 'setupPreview')

const statusButton =
  '-mx-1 flex cursor-pointer items-center gap-1.5 rounded-sm px-1 transition-colors hover:bg-accent'
const smallButton = 'h-7 font-sans text-[12px]'

const countdown = (deadline: string, now: number) => {
  const left = Math.max(0, Math.round((Date.parse(deadline) - now) / 1000))
  return `${String(Math.floor(left / 60)).padStart(2, '0')}:${String(left % 60).padStart(2, '0')}`
}

/** A time the daemon sent, as the user's clock shows it. */
const clock = (iso: string) => new Date(iso).toLocaleTimeString()

const running = (upgrade: Upgrade | undefined) => upgrade?.state === 'fetching' || upgrade?.state === 'running'

/** How the latest switch ended, once it is decided. */
function lastOutcome(state: Supervised) {
  const target = lastSwitched(state.generations)
  const decided = target && outcome(state.status, state.generations, target.id)
  return decided && decided.state !== 'waiting' ? { target: target!, decided } : undefined
}

/** One word for what the status line shows, in order of what matters most. */
function itemOf(state: Supervised) {
  const { status, upgrade } = state
  if (!state.connected) return 'disconnected'
  if (awaitingConfirm(status)) return 'pending'
  if (restoreFailed(status)) return 'restore_failed'
  if (status?.pending && status.step === 'rolling_back') return 'rolling_back'
  if (outdated(status)) return 'outdated'
  if (running(upgrade) || status?.busy) return 'upgrading'
  if (upgrade?.state === 'failed' && Date.now() - upgrade.at < 3_600_000) return 'failed'
  const last = lastOutcome(state)
  if (
    last?.decided.state === 'rolled_back' &&
    Date.now() - Date.parse(last.target.rolled_back_at ?? '') < 3_600_000
  )
    return 'rolled_back'
  if (state.behind?.count) return 'behind'
  return 'current'
}

const genTone: Partial<Record<Generation['state'], string>> = {
  permanent: 'text-success',
  current: 'text-brand',
  rolled_back: 'text-error',
  test_failed: 'text-error',
  tested: 'text-foreground',
}

function Generations({ state }: { state: Supervised }) {
  const rows = state.generations.slice(-5).reverse()
  if (!rows.length) return <div className="text-muted-foreground">No generations yet.</div>
  return (
    <ul className="space-y-0.5">
      {rows.map((g) => {
        const note =
          g.state === 'rolled_back'
            ? causeText(g.rollback_cause, g.rollback_reason)
            : g.id === state.generation
              ? 'this one'
              : g.test
                ? `test ${g.test.seconds.toFixed(1)}s`
                : ''
        return (
          <li className="flex min-w-0 gap-2" key={g.id}>
            <span className={cn('w-7 shrink-0', g.id === state.generation && 'text-brand')}>{g.id}</span>
            <span className="w-24 shrink-0 truncate text-muted-foreground" title={g.commit}>
              {g.commit.slice(0, 8)}
            </span>
            <span className={cn('w-20 shrink-0', genTone[g.state] ?? 'text-muted-foreground')}>
              {g.state.replace('_', ' ')}
            </span>
            <span className="min-w-0 truncate text-muted-foreground" title={g.rollback_reason ?? note}>
              {note}
            </span>
          </li>
        )
      })}
    </ul>
  )
}

/** The upgrade this process started: the step `fawkes upgrade` is on (the daemon's `busy`), or how it failed. */
function UpgradeRow({ state }: { state: Supervised }) {
  const { upgrade, status } = state
  if (!upgrade) return null
  const { error, result } = upgrade
  const progress = running(upgrade)
    ? state.events.findLast((event) => event.kind === 'progress' && event.seq > upgrade.since)
    : undefined
  return (
    <div>
      Upgrade to {upgrade.ref}:{' '}
      {upgrade.state === 'failed' ? (
        <span className="text-error">failed{error?.step ? ` at its ${error.step} step` : ''}</span>
      ) : upgrade.state === 'switched' ? (
        <span className="text-brand">
          switched to {result?.generation}
          {result?.existing ? ' (already built)' : ''}
        </span>
      ) : (
        <span className="text-brand">{upgrade.state === 'fetching' ? 'fetching' : (status?.busy ?? 'starting')}</span>
      )}
      {progress && (
        <div className="truncate text-muted-foreground" title={progress.message}>
          {progress.message}
        </div>
      )}
      {error && (
        <pre className="max-h-32 overflow-auto whitespace-pre-wrap text-error">
          {error.code ? `${error.code}: ` : ''}
          {error.message}
        </pre>
      )}
      {error?.log && <div className="truncate text-muted-foreground" title={error.log}>log: {error.log}</div>}
    </div>
  )
}

/** How the latest switch ended, when it rolled back: its cause in words, where it went, what became of the storage. */
function LastSwitch({ state }: { state: Supervised }) {
  const last = lastOutcome(state)
  if (!last || last.decided.state === 'permanent') return null
  const event = rollbackEvent(state.events, last.target.id)
  const details = [
    event?.to && `back to ${event.to}`,
    event?.storage && storageText(event.storage),
    last.target.rolled_back_at && clock(last.target.rolled_back_at),
  ].filter(Boolean)
  return (
    <div className="flex items-start gap-1.5 text-error">
      <TriangleAlertIcon className="mt-0.5 size-3 shrink-0" />
      <span>
        {outcomeText(last.decided)}
        {details.length > 0 && <span className="text-muted-foreground"> · {details.join(' · ')}</span>}
      </span>
    </div>
  )
}

/** The latest events that changed something, newest first, each said from its fields. */
function Recent({ state }: { state: Supervised }) {
  const events = state.events.filter((event) => event.kind !== 'progress').slice(-4).reverse()
  if (!events.length) return null
  return (
    <ul className="space-y-0.5 border-t border-border/60 pt-2 text-muted-foreground">
      {events.map((event) => (
        <li className="flex min-w-0 gap-2" key={event.seq}>
          <span className="shrink-0">{clock(event.at)}</span>
          <span className="min-w-0 truncate" title={event.message}>
            {eventLine(event, clock)}
          </span>
        </li>
      ))}
    </ul>
  )
}

function Popup({ state, act, busy }: { state: Supervised; act: (name: string) => void; busy?: string }) {
  const { status, upgrade, behind, confirming } = state
  const pending = awaitingConfirm(status)
  const now = useNow(1000, Boolean(pending))
  const rollbackText = rollbackNote(status)
  const old = outdated(status)
  // Only what the daemon would accept right now (`status.actions`). An older daemon sends no actions: its pending
  // switch keeps both ways out, as the server actions accept them.
  const canConfirm = !!pending && (old ? true : allows(status, `confirm ${pending.generation}`))
  const canRollback = old ? !!pending || restoreFailed(status) : allows(status, 'rollback')
  const canUpgrade = !!behind?.count && !running(upgrade) && !upgradeRefusal(status)
  // The ref still points at the commit the latest switch rolled back from (`behind` has short shas, newest first).
  const last = lastOutcome(state)
  const head = behind?.commits[0]?.sha
  const retry =
    last?.decided.state === 'rolled_back' && head && last.target.commit.startsWith(head) ? last : undefined
  return (
    <>
      <div className="border-b border-border/70 px-3 py-2 text-[11.5px]">
        <div className="section-label">Fawkes</div>
        <div className="text-muted-foreground">
          {status?.name ?? 'ultimate'} · running {state.generation}
          {status?.permanent && status.permanent !== state.generation && ` · permanent ${status.permanent}`}
          {status?.version && ` · daemon ${status.version}`}
        </div>
      </div>
      <div className="space-y-2 px-3 py-2 text-[11.5px] leading-[1.55]">
        {!state.connected && (
          <div className="flex items-start gap-1.5 text-warning">
            <TriangleAlertIcon className="mt-0.5 size-3 shrink-0" />
            <span>The Fawkes daemon does not answer{state.error ? `: ${state.error}` : ''}. Retrying.</span>
          </div>
        )}
        {old && (
          <div className="flex items-start gap-1.5 text-warning">
            <TriangleAlertIcon className="mt-0.5 size-3 shrink-0" />
            <span>{old}.</span>
          </div>
        )}
        {rollbackText && (
          <div className={cn('flex items-start gap-1.5', restoreFailed(status) ? 'text-error' : 'text-warning')}>
            <TriangleAlertIcon className="mt-0.5 size-3 shrink-0" />
            <span>{rollbackText}.</span>
          </div>
        )}
        {pending && (
          <div>
            <TimerIcon className="mr-1 inline size-3 text-warning" />
            {pending.generation} must be confirmed within{' '}
            <span className="text-warning">{countdown(pending.deadline, now)}</span>, or it rolls back to{' '}
            {pending.previous ?? 'nothing'} (code and storage).
            {pending.generation === state.generation && confirming?.state === 'checking' && (
              <div className="text-muted-foreground">
                Confirms itself once its checks pass{confirming.detail ? `; ${confirming.detail}` : ''}.
              </div>
            )}
            {confirming?.state === 'refused' && <div className="text-error">{confirming.reason}</div>}
          </div>
        )}
        {!pending && !rollbackText && <LastSwitch state={state} />}
        <UpgradeRow state={state} />
        {!pending && behind && (
          <div>
            {behind.count ? (
              <>
                {behind.ref} has {behind.count} {behind.count === 1 ? 'commit' : 'commits'} this generation
                lacks
                <ul className="mt-0.5 space-y-0.5">
                  {behind.commits.slice(0, 5).map((commit) => (
                    <li className="flex min-w-0 gap-2" key={commit.sha}>
                      <span className="shrink-0 text-muted-foreground">{commit.sha}</span>
                      <span className="truncate text-foreground/85" title={commit.subject}>
                        {commit.subject}
                      </span>
                    </li>
                  ))}
                  {behind.count > 5 && <li className="text-muted-foreground">… {behind.count - 5} more</li>}
                </ul>
              </>
            ) : (
              <>
                <span className="text-success">Up to date</span> with {behind.ref}
              </>
            )}
            {retry && (
              <div className="text-warning">
                That is the commit {retry.target.id} rolled back from; an upgrade builds it again.
              </div>
            )}
            {behind.error && <div className="text-error">Last check failed: {behind.error}</div>}
          </div>
        )}
        <div className="border-t border-border/60 pt-2">
          <Generations state={state} />
        </div>
        <Recent state={state} />
      </div>
      <div className="flex items-center justify-between gap-2 border-t border-border/70 px-3 py-1.5">
        <span className="text-[10.5px] text-muted-foreground">
          {behind ? `checked ${formatAgo(behind.fetchedAt, Date.now(), { ago: true })}` : ''}
        </span>
        <div className="flex items-center gap-1">
          {!pending && (
            <Button
              className={cn(smallButton, 'text-muted-foreground')}
              disabled={!!busy}
              onClick={() => act('check')}
              size="sm"
              variant="ghost"
            >
              {busy === 'check' ? <LoaderCircleIcon className="size-3 animate-spin" /> : <RefreshCwIcon className="size-3" />}
              {busy === 'check' ? 'Checking…' : 'Check now'}
            </Button>
          )}
          {canRollback && (
            <Button
              className={smallButton}
              disabled={!!busy}
              onClick={() => act('rollback')}
              size="sm"
              variant={canConfirm ? 'ghost' : 'outline'}
            >
              {busy === 'rollback' ? 'Rolling back…' : 'Roll back'}
            </Button>
          )}
          {canConfirm && (
            <Button className={smallButton} disabled={!!busy} onClick={() => act('confirm')} size="sm" variant="outline">
              {busy === 'confirm' ? 'Confirming…' : 'Confirm'}
            </Button>
          )}
          {canUpgrade && (
            <Button className={smallButton} disabled={!!busy} onClick={() => act('upgrade')} size="sm" variant="outline">
              Upgrade
            </Button>
          )}
        </div>
      </div>
    </>
  )
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <Button
      className="size-6 shrink-0 text-muted-foreground"
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true)
          setTimeout(() => setCopied(false), 1500)
        })
      }}
      size="icon"
      title="Copy"
      variant="ghost"
    >
      {copied ? <CheckIcon className="size-3 text-success" /> : <CopyIcon className="size-3" />}
    </Button>
  )
}

function SetupSteps({ steps }: { steps: SetupStep[] }) {
  return (
    <>
      <ol className="space-y-1.5 border-t border-border/60 pt-2">
        {steps.map((step, index) => (
          <li key={step.label}>
            <div>
              <span className="text-muted-foreground">{index + 1}.</span> {step.label}
            </div>
            {step.command && (
              <div className="mt-0.5 flex items-center gap-1 rounded-sm bg-muted/50 py-0.5 pl-2 pr-0.5">
                <code className="min-w-0 flex-1 overflow-x-auto whitespace-nowrap">{step.command}</code>
                <CopyButton text={step.command} />
              </div>
            )}
          </li>
        ))}
      </ol>
      <div className="text-muted-foreground">
        The first start tests the running version on a spare port, then runs it. If that test fails, nothing changes and{' '}
        <code>ultimate</code> can be started as before.
      </div>
    </>
  )
}

const manifestBox = 'max-h-56 overflow-auto whitespace-pre-wrap break-words text-muted-foreground'

/**
 * First-time setup: the terminal steps that set this project up and start it under Fawkes, which `init.ts` runs; the
 * card writes nothing. Everything shown is read from the server, so a reload or a closed popover loses nothing.
 */
function SetupCard() {
  const preview = useAction(SetupPreviewAction, null, { onError: () => {} })
  const { existing, steps, missing, error } = preview.data ?? {}
  return (
    <>
      <div className="border-b border-border/70 px-3 py-2 text-[11.5px]">
        <div className="section-label">Fawkes</div>
        <div className="text-muted-foreground">
          {preview.data?.steps && !missing
            ? 'Set up: start Fawkes from a terminal to finish'
            : 'Run this Ultimate under Fawkes: upgrades that roll back'}
        </div>
      </div>
      <div className="space-y-2 px-3 py-2 text-[11.5px] leading-[1.55]">
        {preview.loading && !preview.data && <div className="text-muted-foreground">Reading the Fawkes folder…</div>}
        {error && (
          <div className="flex items-start gap-1.5 text-warning">
            <TriangleAlertIcon className="mt-0.5 size-3 shrink-0" />
            <span className="whitespace-pre-line">{error}</span>
          </div>
        )}
        {existing && (
          <div className="break-all">
            <CheckIcon className="mr-1 inline size-3 text-success" />
            {existing.file}
          </div>
        )}
        {missing && (
          <div className="text-muted-foreground">
            The first step writes the Fawkes manifest, links this extension into the project, turns the built-in update
            extension off for it and trusts the project's Ultimate setup (.ultimate/: agents, skills, settings).
          </div>
        )}
        {steps && <SetupSteps steps={steps} />}
        {existing && (
          <details>
            <summary className="cursor-pointer text-muted-foreground">The manifest</summary>
            <pre className={manifestBox}>{existing.text}</pre>
          </details>
        )}
      </div>
    </>
  )
}

/**
 * Muted, like the item of a current generation: a host not under Fawkes offers to be set up, and once `init.ts` has run,
 * says it is ready for `fawkes up`.
 */
function SetupItem({ ready }: { ready: boolean }) {
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          className={cn(statusButton, 'text-muted-foreground/70 hover:text-foreground')}
          data-fawkes-setup
          title={
            ready
              ? 'Set up for Fawkes: start `fawkes up` in a terminal to finish'
              : 'Run this Ultimate under the Fawkes supervisor'
          }
          type="button"
        >
          <WrenchIcon className="size-3" />
          fawkes<span className="max-md:hidden"> · {ready ? 'ready' : 'set up'}</span>
        </button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-[26rem] max-w-[calc(100vw-1rem)] p-0 font-mono" side="top">
        <SetupCard />
      </PopoverContent>
    </Popover>
  )
}

/** The status-line item: quiet while this generation is current, prominent during an upgrade or a pending confirm. */
function Fawkes() {
  const { data, refetch } = useAction(StateAction, null, { onError: () => {} })
  const [busy, setBusy] = useState<string>()
  const [error, setError] = useState<string>()
  const live = data?.supervised && (data.status?.pending || data.status?.busy || running(data.upgrade))
  useEffect(() => {
    const timer = setInterval(() => void refetch(), live ? 2000 : 30_000)
    return () => clearInterval(timer)
  }, [refetch, live])
  const now = useNow(1000, Boolean(data?.supervised && awaitingConfirm(data.status)))
  if (!data?.supervised) return data?.setup ? <SetupItem ready={data.ready} /> : null
  const act = (name: string) => {
    const ref = { check: CheckAction, upgrade: UpgradeAction, confirm: ConfirmAction, rollback: RollbackAction }[name]
    if (!ref) return
    setBusy(name)
    setError(undefined)
    api
      .action(ref as typeof CheckAction)
      .catch((failure: unknown) => setError(failure instanceof Error ? failure.message : String(failure)))
      .finally(() => {
        setBusy(undefined)
        void refetch()
      })
  }
  const item = itemOf(data)
  const pending = awaitingConfirm(data.status)
  const label =
    item === 'disconnected' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        fawkes
      </>
    ) : item === 'pending' ? (
      <>
        <TimerIcon className="size-3" />
        {pending!.generation === data.generation ? 'confirming' : pending!.generation} {countdown(pending!.deadline, now)}
      </>
    ) : item === 'rolling_back' ? (
      <>
        <LoaderCircleIcon className="size-3 animate-spin" />
        rolling back {data.status!.pending!.generation}
      </>
    ) : item === 'restore_failed' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        restore failed
      </>
    ) : item === 'outdated' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        fawkes too old
      </>
    ) : item === 'upgrading' ? (
      <>
        <LoaderCircleIcon className="size-3 animate-spin" />
        {data.upgrade?.state === 'fetching' ? 'fetching' : (data.status?.busy ?? 'upgrading')}
      </>
    ) : item === 'failed' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        upgrade failed
      </>
    ) : item === 'rolled_back' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        {lastSwitched(data.generations)!.id} rolled back
      </>
    ) : item === 'behind' ? (
      <>
        <ArrowDownIcon className="size-3" />
        {data.behind!.count} new<span className="max-md:hidden"> · upgrade</span>
      </>
    ) : (
      <>
        <CheckIcon className="size-3" />
        {data.generation}
      </>
    )
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          className={cn(
            statusButton,
            item === 'current'
              ? 'text-muted-foreground/70 hover:text-foreground'
              : ['failed', 'rolled_back', 'restore_failed', 'disconnected'].includes(item)
                ? 'text-error'
                : item === 'pending' || item === 'outdated'
                  ? 'text-warning'
                  : 'text-brand',
          )}
          data-fawkes
          title="Generations of this Ultimate under Fawkes"
          type="button"
        >
          {label}
        </button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-[26rem] max-w-[calc(100vw-1rem)] p-0 font-mono" side="top">
        <Popup act={act} busy={busy} state={data} />
        {error && <div className="break-words px-3 pb-2 text-[11.5px] text-error">{error}</div>}
      </PopoverContent>
    </Popover>
  )
}

export default defineWebExtension({
  slots: { 'statusline.right': [{ component: Fawkes, order: -2 }] },
  // The server's per-input note for the model; the status line already shows the same.
  entries: { 'fawkes.note': () => null },
})
