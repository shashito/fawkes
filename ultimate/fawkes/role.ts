/**
 * What a process is to Fawkes. Fawkes's variables are inherited by everything the app starts: a preview Ultimate starts,
 * the lander's test servers, an agent's shell. Only the process `start.ts` ran for this generation's home acts on them:
 * `start.ts` names that home in `FAWKES_ULTIMATE_HOME`, and every other Ultimate runs with a home of its own.
 */
import { realpathSync } from 'node:fs'

/** The file a test run's facet writes once it holds the scheduler; the manifest's health check waits for it. */
export const HELD_MARKER = 'fawkes-held'

export type Role =
  | { kind: 'inert' }
  | { kind: 'live' | 'test'; socket: string; generation: string }

const real = (path: string) => {
  try {
    return realpathSync(path)
  } catch {
    // A home that does not exist yet is not the one start.ts prepared.
    return undefined
  }
}

export function role(env: NodeJS.ProcessEnv): Role {
  const { FAWKES_SOCKET: socket, FAWKES_GENERATION: generation, FAWKES_PHASE: phase } = env
  if (!socket || !generation || (phase !== 'live' && phase !== 'test')) return { kind: 'inert' }
  const ours = env.FAWKES_ULTIMATE_HOME
  if (!ours || !env.ULTIMATE_HOME || real(env.ULTIMATE_HOME) !== ours) return { kind: 'inert' }
  return { kind: phase, socket, generation }
}
