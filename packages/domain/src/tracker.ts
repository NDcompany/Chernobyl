/**
 * Pure client-side command tracker (gate requirement G1).
 *
 * Feeds wire lifecycle messages through the command FSM and keeps a
 * per-command state map. Guarantees:
 *  - a terminal state is final: later/replayed events never move it
 *    (duplicate delivery is safe),
 *  - illegal transitions are ignored, never applied (fail-closed),
 *  - acceptance (`command_progress: accepted`) is never a terminal state.
 *
 * Structural input: anything shaped like the protocol's lifecycle
 * messages, so this module stays dependency-free.
 */

import { canTransition, isTerminal, type CommandState } from './command';

export interface CommandEntry {
  state: CommandState;
  sessionId?: string;
  error?: string;
}

export type CommandTracker = Record<string, CommandEntry>;

export interface LifecycleEvent {
  type: 'command_progress' | 'command_result';
  commandId: string;
  /** command_progress: execution phase so far. */
  state?: string;
  /** command_result: terminal outcome. */
  status?: string;
  sessionId?: string;
  error?: string;
}

function eventState(msg: LifecycleEvent): CommandState | null {
  if (msg.type === 'command_progress') {
    return msg.state === 'accepted' || msg.state === 'running' ? msg.state : null;
  }
  switch (msg.status) {
    case 'completed':
    case 'failed':
    case 'cancelled':
    case 'expired':
    case 'unknown':
      return msg.status;
    default:
      return null;
  }
}

/**
 * Returns the next tracker map. Returns the SAME reference when the event
 * changes nothing, so callers can cheaply detect "ignored".
 */
export function applyCommandLifecycle(
  prev: CommandTracker,
  msg: LifecycleEvent,
): CommandTracker {
  const next = eventState(msg);
  if (!next) return prev;
  const current = prev[msg.commandId];

  // bootstrap: a terminal result may arrive before we ever saw progress
  // (late subscriber) — record it directly.
  if (!current) {
    return {
      ...prev,
      [msg.commandId]: {
        state: next,
        sessionId: msg.sessionId,
        error: msg.error,
      },
    };
  }

  if (isTerminal(current.state)) return prev;
  if (!canTransition(current.state, next)) return prev;

  return {
    ...prev,
    [msg.commandId]: {
      state: next,
      sessionId: msg.sessionId ?? current.sessionId,
      error: msg.error ?? current.error,
    },
  };
}
