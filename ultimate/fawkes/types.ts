/**
 * What the Fawkes daemon reports and what the extension shows. Browser-safe. The daemon's side is `src/protocol.rs`
 * (`Status`, `Generation`, `Event`, `GenState`) and DESIGN.md, "Socket protocol": change them together.
 */

export type GenState =
  | 'staging'
  | 'staged'
  | 'test_failed'
  | 'tested'
  | 'current'
  | 'permanent'
  | 'retired'
  | 'rolled_back'

/** Why a rollback was decided (DESIGN.md, "Rollback"). A newer daemon may send a cause this list lacks. */
export type Cause = 'deadline' | 'exited' | 'unhealthy' | 'start_failed' | 'requested' | 'daemon_restart'

/** What became of the storage in a rollback. */
export type StorageOutcome = 'restored' | 'kept' | 'restore_failed' | 'none'

export type TestInfo = { port: number; passed: boolean; at: string; seconds: number; message: string }

export type Generation = {
  id: string
  ref: string
  commit: string
  state: GenState
  dir: string
  staged_at: string
  test: TestInfo | null
  switched_at: string | null
  confirmed_at: string | null
  rolled_back_at: string | null
  rollback_reason: string | null
  /** The reason as a code; null unless rolled back, or rolled back before causes existed. */
  rollback_cause?: Cause | null
  failed_starts: number
}

/** Where the daemon's transaction is (`step` of the `status` result). */
export type Step = 'switching' | 'pending' | 'rolling_back' | 'restore_failed'

export type Status = {
  name: string
  daemon_pid: number
  permanent: string | null
  current: string | null
  /** The live switch. It stays set while the switch is rolled back or its restore failed: read `step`. */
  pending: { generation: string; previous: string | null; deadline: string } | null
  /** Absent from an older daemon's status. */
  step?: Step | null
  busy: string | null
  /** The daemon's build; a status without it is older than this extension expects (`step.ts`, `outdated`). */
  version?: string
  /** The commands the daemon accepts right now, as command lines without `fawkes` (`confirm g3`, `rollback`). */
  actions?: string[]
  process: {
    state: 'running' | 'stopped' | 'failed'
    generation: string | null
    pid: number | null
    port: number
    started_at: string | null
    uptime: number | null
    restarts: number
    restart_at: string | null
  }
}

/** An event: the message is for people, the facts of its kind are fields beside it (DESIGN.md, "Events"). */
export type FawkesEvent = {
  seq: number
  at: string
  kind: string
  generation: string | null
  message: string
  pid?: number
  port?: number
  previous?: string
  deadline?: string
  code?: number
  signal?: number
  restart_at?: string
  seconds?: number
  named?: boolean
  to?: string
  reason?: string
  cause?: Cause
  storage?: StorageOutcome
  removed?: string[]
  error_code?: string
  action?: string
  version?: string
  changed?: string[]
}

/** `fawkes up --json`: the switch's result, plus what the stage and the test learned. */
export type UpResult = {
  generation: string
  previous: string | null
  deadline: string
  /** The stage found the commit already built. */
  existing: boolean
  test: TestInfo | null
}

/** A failed upgrade: `fawkes up`'s error object, with `step` naming the step that failed. */
export type UpError = {
  /** Fawkes's error code; absent when the extension's own fetch of the ref failed, before `up` ran. */
  code?: string
  message: string
  step?: 'stage' | 'test' | 'switch'
  generation?: string
  log?: string
}

/**
 * The upgrade this Ultimate started: the fetch of its ref, then `fawkes up`, whose step the daemon's `status.busy` names
 * while it runs. In the live app it ends with the switch, which stops this process; the generation that runs next reads
 * the outcome (`facts.ts`, `outcome`).
 */
export type Upgrade = {
  ref: string
  state: 'fetching' | 'running' | 'switched' | 'failed'
  result?: UpResult
  error?: UpError
  at: number
}

/**
 * Where a switched generation stands, decided as `fawkes wait` decides it: `permanent` (exit 0), `rolled_back` with its
 * cause, `storage_failed` when the rollback could not restore the storage, or still `waiting` (pending, or its rollback
 * under way).
 */
export type Outcome =
  | { generation: string; state: 'waiting' | 'permanent' | 'storage_failed' }
  | { generation: string; state: 'rolled_back'; cause: Cause | null; reason: string | null }

/** Why this generation has not confirmed itself yet, while it is pending. */
export type Confirming =
  | { state: 'checking'; detail?: string }
  | { state: 'refused'; reason: string }
  | { state: 'confirmed' }

/** The `state` action: everything the status line and its popover show. */
export type FawkesState =
  | { supervised: false }
  | {
      supervised: true
      /** This process's generation (`FAWKES_GENERATION`). */
      generation: string
      connected: boolean
      error?: string
      status?: Status
      generations: Generation[]
      /** Newest last, at most 30. */
      events: FawkesEvent[]
      upgrade?: Upgrade
      confirming?: Confirming
      /** Commits on the upgrade ref that this generation does not have; undefined until checked. */
      behind?: { ref: string; count: number; commits: { sha: string; subject: string }[]; fetchedAt: number; error?: string }
    }
