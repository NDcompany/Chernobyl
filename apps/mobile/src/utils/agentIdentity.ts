/**
 * Pure helpers behind agent identity (`src/ui/agentMark.tsx`) and the
 * conversation's working indicator (`WorkingRow` in `src/ui/transcript.tsx`):
 * which mark an agent kind gets, what a running turn is doing, the rotating
 * status verb and the stable screen-reader label. No React Native imports and
 * type-only protocol imports, so `node --test` runs it (agentIdentity.test.ts).
 */
import type { PermissionRequest, TranscriptItem, TurnState } from '@chernobyl/protocol';

/** One original mark per runtime family; everything else shares the generic one. */
export type MarkId = 'claude' | 'codex' | 'opencode' | 'antigravity' | 'generic';

export type MarkState = 'idle' | 'thinking' | 'working' | 'waiting' | 'done' | 'error';

/** `Agent.kind` → mark. Takes any string: a newer companion may send kinds this app does not know. */
export function markFor(kind?: string): MarkId {
  switch (kind) {
    case 'claude-code':
      return 'claude';
    case 'codex':
      return 'codex';
    case 'opencode':
      return 'opencode';
    case 'antigravity':
      return 'antigravity';
    default:
      return 'generic';
  }
}

/** Shown while the model streams; one every VERB_MS, in the CLI's spirit. */
export const WORKING_VERBS = [
  'Thinking',
  'Pondering',
  'Brewing',
  'Reticulating',
  'Mulling',
  'Tinkering',
  'Noodling',
  'Percolating',
  'Untangling',
  'Sketching',
  'Simmering',
  'Distilling',
  'Conjuring',
  'Whittling',
  'Puzzling',
  'Assembling',
  'Calibrating',
  'Composing',
  'Sifting',
  'Kneading',
  'Weaving',
  'Musing',
] as const;

export const VERB_MS = 3000;

/** The verb for this moment of a turn; `seed` (the turn id) picks where each turn starts. */
export function workingVerb(elapsedMs: number, seed: string): string {
  let h = 0;
  for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) >>> 0;
  const step = Math.floor(Math.max(0, elapsedMs) / VERB_MS);
  return WORKING_VERBS[(h + step) % WORKING_VERBS.length];
}

export interface WorkingStatus {
  state: 'thinking' | 'working' | 'waiting';
  turn: TurnState;
  /** state 'working': the running tool's name, verbatim */
  tool?: string;
}

/**
 * What the newest running turn is doing: waiting for the user when a request
 * is pending (or one of its tools awaits approval), running a tool when one of
 * its tools is pending/running (the newest names it), otherwise thinking (streaming text or
 * reasoning). Null when no turn runs, so the indicator disappears and the
 * turn footer remains.
 */
export function workingStatus(
  items: TranscriptItem[],
  turns: TurnState[],
  permissions: PermissionRequest[],
): WorkingStatus | null {
  let turn: TurnState | undefined;
  for (const t of turns) if (t.status === 'running' && (!turn || t.startedAt >= turn.startedAt)) turn = t;
  if (!turn) return null;
  if (permissions.some((p) => p.status === 'pending')) return { state: 'waiting', turn };
  // Any tool of the open turn decides, not the last item: notices (usage
  // warnings) and other items carry the running turn's id and can land after
  // the tool card while it still runs.
  let running: string | undefined;
  for (let i = items.length - 1; i >= 0; i--) {
    const it = items[i];
    if (it.turnId !== turn.id || it.kind !== 'tool') continue;
    if (it.tool.status === 'awaiting-approval') return { state: 'waiting', turn };
    if (running === undefined && (it.tool.status === 'running' || it.tool.status === 'pending')) running = it.tool.name;
  }
  return running === undefined ? { state: 'thinking', turn } : { state: 'working', turn, tool: running };
}

/** The visible line: "Pondering…", "Running Bash…" or "Waiting for you"; `seed` = the turn id. */
export function workingLabel(state: WorkingStatus['state'], elapsedMs: number, seed: string, tool?: string): string {
  switch (state) {
    case 'waiting':
      return 'Waiting for you';
    case 'working':
      return `Running ${tool || 'a tool'}…`;
    default:
      return `${workingVerb(elapsedMs, seed)}…`;
  }
}

/**
 * The screen-reader label: changes only between working and waiting, so the
 * polite live region is not re-read for every verb, tick or tool call.
 */
export function workingA11yLabel(state: WorkingStatus['state'], agentName: string): string {
  return state === 'waiting' ? `${agentName} is waiting for you` : `${agentName} is working`;
}

/** "8s", "1m 05s", "1h 02m": whole seconds, for a ticking counter. */
export function formatElapsed(ms: number): string {
  const s = Math.floor(Math.max(0, ms) / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor(s / 60) % 60).padStart(2, '0')}m`;
}
