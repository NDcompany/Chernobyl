/**
 * ADR-016 workspace files and git over the SAME transport as everything else.
 * Reads go straight to the transport (a screen shows their errors inline, so
 * they never toast); their `workspace_result` arrives right before the ack.
 * Changes go through `useCommands` (ack errors toast, a lost ack becomes a
 * visible unknown outcome) and resolve with their one `command_result`.
 * Results are screen state only: never persisted (threat model T2).
 */
import { useCallback, useEffect, useState } from 'react';
import {
  FEATURE_WORKSPACE,
  nextId,
  type ClientMessage,
  type WorkspaceClientMessage,
  type WorkspaceResult,
} from '@chernobyl/protocol';
import { getTransport } from './transport';
import { useCommands } from './commands';
import { useAppStore } from '@/store/appStore';

type Omit2<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;
type Read = Extract<WorkspaceClientMessage, { type: 'fs_list' | 'fs_read' | 'git_status' | 'git_diff' }>;
type Change = Exclude<WorkspaceClientMessage, Read>;
export type WorkspaceRead = Omit2<Read, 'commandId'>;
export type WorkspaceChange = Omit2<Change, 'commandId'>;

/** A push can take its full 60 s on the PC; past this the outcome stays unknown. */
const RESULT_TIMEOUT_MS = 150_000;

/** The host speaks ADR-016 (`hello_ack.features`). The simulation does not. */
export function useWorkspaceEnabled(): boolean {
  return useAppStore((s) => s.features.includes(FEATURE_WORKSPACE));
}

/** One read; rejects with the companion's own reason ("not a git repository", "binary file", …). */
export async function workspaceQuery(msg: WorkspaceRead): Promise<WorkspaceResult> {
  const transport = getTransport();
  const commandId = nextId('ws');
  let result: WorkspaceResult | undefined;
  const off = transport.subscribe((m) => {
    if (m.type === 'workspace_result' && m.commandId === commandId) result = m.result;
  });
  try {
    const ack = await transport.send({ ...msg, commandId } as ClientMessage);
    if (!ack.ok) throw new Error(ack.error ?? 'refused by the companion');
    if (!result) throw new Error('the companion sent no result');
    return result;
  } finally {
    off();
  }
}

export interface WorkspaceReadState {
  result?: WorkspaceResult;
  error?: string;
  loading: boolean;
  reload: () => void;
}

/**
 * `msg` read while connected, again on reconnect and on `reload()`. The last
 * result stays on screen while offline or reloading; null reads nothing.
 */
export function useWorkspaceRead(msg: WorkspaceRead | null): WorkspaceReadState {
  const connected = useAppStore((s) => s.connection === 'connected');
  const key = msg ? JSON.stringify(msg) : '';
  const [nonce, setNonce] = useState(0);
  const [state, setState] = useState<{ key: string; nonce: number; result?: WorkspaceResult; error?: string }>();
  useEffect(() => {
    if (!key || !connected) return;
    let live = true;
    workspaceQuery(JSON.parse(key) as WorkspaceRead).then(
      (result) => live && setState({ key, nonce, result }),
      (e: unknown) => live && setState({ key, nonce, error: e instanceof Error ? e.message : String(e) }),
    );
    return () => {
      live = false;
    };
  }, [key, nonce, connected]);
  const mine = state?.key === key ? state : undefined;
  return {
    result: mine?.result,
    error: mine?.error,
    loading: Boolean(key) && connected && (mine === undefined || mine.nonce !== nonce),
    reload: useCallback(() => setNonce((n) => n + 1), []),
  };
}

export interface ChangeOutcome {
  ok: boolean;
  /** git's own summary when it failed (push rejected, hook failed, …) */
  error?: string;
  /** `git_pull` only: changed agent-configuration paths (see the protocol) */
  agentConfigChanged?: string[];
}

/**
 * Sends one git change; resolves with its `command_result`, or `null` when
 * there is none to show: refused (already toasted), not sent, outcome
 * unknown, a replayed duplicate, or no result within the timeout.
 */
export function useGitChange(): (msg: WorkspaceChange) => Promise<ChangeOutcome | null> {
  const send = useCommands();
  return useCallback(
    async (msg) => {
      const commandId = nextId('cmd');
      let settle: (o: ChangeOutcome | null) => void = () => undefined;
      const outcome = new Promise<ChangeOutcome | null>((resolve) => {
        settle = resolve;
      });
      // subscribed before sending: a fast git action can finish before its ack arrives
      const off = getTransport().subscribe((m) => {
        if (m.type === 'command_result' && m.commandId === commandId) {
          settle({
            ok: m.status === 'completed',
            error: m.error,
            agentConfigChanged: m.agentConfigChanged,
          });
        }
      });
      const timer = setTimeout(() => settle(null), RESULT_TIMEOUT_MS);
      try {
        const ack = await send({ ...msg, commandId } as ClientMessage);
        if (!ack?.ok || ack.duplicate) return null;
        return await outcome;
      } finally {
        clearTimeout(timer);
        off();
      }
    },
    [send],
  );
}
