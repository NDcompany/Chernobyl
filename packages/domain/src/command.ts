/**
 * Command lifecycle state machine (PRD §12.2, gate requirement G1).
 *
 * Three phases are distinct and must never be collapsed:
 *  - acceptance   (`ack` on the wire / `created`→`queued`→`accepted`)
 *  - execution    (`running`)
 *  - outcome      (exactly one terminal state)
 *
 * `unknown` is a legitimate terminal: when safe reconciliation is
 * impossible the outcome stays unknown — it must never be coerced into
 * `completed` or `failed` (PRD §12.4).
 */

export type CommandState =
  | 'created'
  | 'queued'
  | 'accepted'
  | 'running'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'expired'
  | 'unknown';

export const TERMINAL_STATES: ReadonlySet<CommandState> = new Set([
  'completed',
  'failed',
  'cancelled',
  'expired',
  'unknown',
]);

const TRANSITIONS: Readonly<Record<CommandState, ReadonlySet<CommandState>>> = {
  created: new Set(['queued', 'accepted']),
  queued: new Set(['accepted', 'cancelled', 'expired']),
  accepted: new Set(['running', 'completed', 'failed', 'cancelled', 'expired', 'unknown']),
  running: new Set(['completed', 'failed', 'cancelled', 'expired', 'unknown']),
  completed: new Set(),
  failed: new Set(),
  cancelled: new Set(),
  expired: new Set(),
  unknown: new Set(),
};

export function isTerminal(state: CommandState): boolean {
  return TERMINAL_STATES.has(state);
}

/** True when the FSM allows `from → to`. Terminal states allow nothing. */
export function canTransition(from: CommandState, to: CommandState): boolean {
  return TRANSITIONS[from].has(to);
}

/** Returns the target state, or null when the transition is illegal. */
export function transitionCommand(from: CommandState, to: CommandState): CommandState | null {
  return canTransition(from, to) ? to : null;
}
