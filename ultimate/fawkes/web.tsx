import {
  ArrowDownIcon,
  CheckIcon,
  LoaderCircleIcon,
  RefreshCwIcon,
  TimerIcon,
  TriangleAlertIcon,
} from 'lucide-react'
import { useEffect, useState } from 'react'
import { Button } from '@/components/ui/button'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover'
import { actionRef, api, cn, defineWebExtension, formatAgo, useAction, useNow } from '@/sdk'
import { awaitingConfirm, restoreFailed, rollbackNote } from './step.ts'
import type { FawkesState, Generation } from './types.ts'

type Supervised = Extract<FawkesState, { supervised: true }>

/** No arguments → what the status line shows. */
const StateAction = actionRef<null, FawkesState>('fawkes', 'state')
/** No arguments → fetch the clone and count what an upgrade would bring. */
const CheckAction = actionRef<null, FawkesState>('fawkes', 'check')
/** `{ ref? }` → start an upgrade (stage, test, switch). */
const UpgradeAction = actionRef<{ ref?: string }, FawkesState>('fawkes', 'upgrade')
/** No arguments → confirm the pending generation now. */
const ConfirmAction = actionRef<null, FawkesState>('fawkes', 'confirm')
/** No arguments → roll the pending generation back now. */
const RollbackAction = actionRef<null, FawkesState>('fawkes', 'rollback')

const statusButton =
  '-mx-1 flex cursor-pointer items-center gap-1.5 rounded-sm px-1 transition-colors hover:bg-accent'
const smallButton = 'h-7 font-sans text-[12px]'

const countdown = (deadline: string, now: number) => {
  const left = Math.max(0, Math.round((Date.parse(deadline) - now) / 1000))
  return `${String(Math.floor(left / 60)).padStart(2, '0')}:${String(left % 60).padStart(2, '0')}`
}

/** One word for what the status line shows, in order of what matters most. */
function itemOf(state: Supervised) {
  const { status, upgrade } = state
  if (!state.connected) return 'disconnected'
  if (awaitingConfirm(status)) return 'pending'
  if (restoreFailed(status)) return 'restore_failed'
  if (status?.pending && status.step === 'rolling_back') return 'rolling_back'
  if (upgrade && !['done', 'failed'].includes(upgrade.step)) return 'upgrading'
  if (upgrade?.step === 'failed' && Date.now() - upgrade.at < 3_600_000) return 'failed'
  const rolled = state.generations.at(-1)
  if (rolled?.state === 'rolled_back' && Date.now() - Date.parse(rolled.rolled_back_at ?? '') < 3_600_000)
    return 'rolled_back'
  if (state.behind?.count) return 'behind'
  return 'current'
}

const stepLabel: Record<string, string> = {
  fetching: 'fetching',
  staging: 'installing',
  testing: 'testing',
  switching: 'switching',
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
      {rows.map((g) => (
        <li className="flex min-w-0 gap-2" key={g.id}>
          <span className={cn('w-7 shrink-0', g.id === state.generation && 'text-brand')}>{g.id}</span>
          <span className="w-24 shrink-0 truncate text-muted-foreground" title={g.commit}>
            {g.commit.slice(0, 8)}
          </span>
          <span className={cn('w-20 shrink-0', genTone[g.state] ?? 'text-muted-foreground')}>
            {g.state.replace('_', ' ')}
          </span>
          <span className="min-w-0 truncate text-muted-foreground" title={g.rollback_reason ?? undefined}>
            {g.rollback_reason ??
              (g.id === state.generation ? 'this one' : g.test ? `test ${g.test.seconds.toFixed(1)}s` : '')}
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
  return (
    <>
      <div className="border-b border-border/70 px-3 py-2 text-[11.5px]">
        <div className="section-label">Fawkes</div>
        <div className="text-muted-foreground">
          {status?.name ?? 'ultimate'} · running {state.generation}
          {status?.permanent && status.permanent !== state.generation && ` · permanent ${status.permanent}`}
        </div>
      </div>
      <div className="space-y-2 px-3 py-2 text-[11.5px] leading-[1.55]">
        {!state.connected && (
          <div className="flex items-start gap-1.5 text-warning">
            <TriangleAlertIcon className="mt-0.5 size-3 shrink-0" />
            <span>The Fawkes daemon does not answer{state.error ? `: ${state.error}` : ''}. Retrying.</span>
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
        {upgrade && (
          <div>
            Upgrade to {upgrade.ref}
            {upgrade.generation && ` as ${upgrade.generation}`}:{' '}
            <span className={upgrade.step === 'failed' ? 'text-error' : 'text-brand'}>
              {stepLabel[upgrade.step] ?? upgrade.step}
            </span>
            {upgrade.progress && !['done', 'failed'].includes(upgrade.step) && (
              <div className="truncate text-muted-foreground" title={upgrade.progress}>
                {upgrade.progress}
              </div>
            )}
            {upgrade.error && (
              <pre className="max-h-32 overflow-auto whitespace-pre-wrap text-error">{upgrade.error}</pre>
            )}
          </div>
        )}
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
            {behind.error && <div className="text-error">Last check failed: {behind.error}</div>}
          </div>
        )}
        <div className="border-t border-border/60 pt-2">
          <Generations state={state} />
        </div>
      </div>
      <div className="flex items-center justify-between gap-2 border-t border-border/70 px-3 py-1.5">
        <span className="text-[10.5px] text-muted-foreground">
          {behind ? `checked ${formatAgo(behind.fetchedAt, Date.now(), { ago: true })}` : ''}
        </span>
        <div className="flex items-center gap-1">
          {pending ? (
            <>
              <Button className={smallButton} disabled={!!busy} onClick={() => act('rollback')} size="sm" variant="ghost">
                {busy === 'rollback' ? 'Rolling back…' : 'Roll back'}
              </Button>
              <Button className={smallButton} disabled={!!busy} onClick={() => act('confirm')} size="sm" variant="outline">
                {busy === 'confirm' ? 'Confirming…' : 'Confirm'}
              </Button>
            </>
          ) : restoreFailed(status) ? (
            <Button className={smallButton} disabled={!!busy} onClick={() => act('rollback')} size="sm" variant="outline">
              {busy === 'rollback' ? 'Rolling back…' : 'Roll back'}
            </Button>
          ) : (
            <>
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
              {!!behind?.count && (
                <Button
                  className={smallButton}
                  disabled={!!busy || !!status?.busy || (upgrade && !['done', 'failed'].includes(upgrade.step))}
                  onClick={() => act('upgrade')}
                  size="sm"
                  variant="outline"
                >
                  Upgrade
                </Button>
              )}
            </>
          )}
        </div>
      </div>
    </>
  )
}

/** The status-line item: quiet while this generation is current, prominent during an upgrade or a pending confirm. */
function Fawkes() {
  const { data, refetch } = useAction(StateAction, null, { onError: () => {} })
  const [busy, setBusy] = useState<string>()
  const [error, setError] = useState<string>()
  const live = data?.supervised && (data.status?.pending || (data.upgrade && !['done', 'failed'].includes(data.upgrade.step)))
  useEffect(() => {
    const timer = setInterval(() => void refetch(), live ? 2000 : 30_000)
    return () => clearInterval(timer)
  }, [refetch, live])
  const now = useNow(1000, Boolean(data?.supervised && awaitingConfirm(data.status)))
  if (!data?.supervised) return null
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
    ) : item === 'upgrading' ? (
      <>
        <LoaderCircleIcon className="size-3 animate-spin" />
        {stepLabel[data.upgrade!.step]} {data.upgrade!.generation ?? ''}
      </>
    ) : item === 'failed' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        upgrade failed
      </>
    ) : item === 'rolled_back' ? (
      <>
        <TriangleAlertIcon className="size-3" />
        {data.generations.at(-1)!.id} rolled back
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
                : item === 'pending'
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
})
