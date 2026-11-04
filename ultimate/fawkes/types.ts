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

export type Generation = {
  id: string
  ref: string
  commit: string
  state: GenState
  dir: string
  staged_at: string
  test: { port: number; passed: boolean; at: string; seconds: number; message: string } | null
  switched_at: string | null
  confirmed_at: string | null
  rolled_back_at: string | null
  rollback_reason: string | null
  failed_starts: number
}

export type Status = {
  name: string
  daemon_pid: number
  permanent: string | null
  current: string | null
  pending: { generation: string; previous: string | null; deadline: string } | null
  busy: string | null
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

export type FawkesEvent = {
  seq: number
  at: string
  kind: string
  generation: string | null
  message: string
}

/** The upgrade this Ultimate runs: stage, test and switch of one ref, as the status line follows it. */
export type Upgrade = {
  ref: string
  step: 'fetching' | 'staging' | 'testing' | 'switching' | 'done' | 'failed'
  generation?: string
  /** The latest progress line of the step. */
  progress?: string
  error?: string
  at: number
}

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
