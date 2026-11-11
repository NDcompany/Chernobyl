/**
 * ADR-020 opt-in terminal over the SAME transport as everything else. A run
 * is a command (`useCommands`: ack errors toast, a lost ack is an unknown
 * outcome); its `terminal_output` chunks and `terminal_exit` come to this
 * connection only and live in screen state, never persisted (only the
 * command line goes into the local history). Stop goes straight to the
 * transport: it is an ack-only message.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { FEATURE_TERMINAL, nextId, type ServerMessage } from '@chernobyl/protocol';
import { getTransport } from './transport';
import { useCommands } from './commands';
import { useAppStore } from '@/store/appStore';

type Exit = Extract<ServerMessage, { type: 'terminal_exit' }>;

/** The PC turned the terminal on (`hello_ack.features`). The simulation never does. */
export function useTerminalEnabled(): boolean {
  return useAppStore((s) => s.features.includes(FEATURE_TERMINAL));
}

export interface TerminalRun {
  commandId: string;
  command: string;
  output: string;
  /** set once the run ended */
  exit?: Exit;
  /** accepted and not ended yet */
  running: boolean;
  /** refused, not sent, or the outcome is unknown (already toasted) */
  failed?: string;
}

export function useTerminal(workspaceId: string) {
  const send = useCommands();
  const [run, setRun] = useState<TerminalRun | null>(null);
  const off = useRef<(() => void) | null>(null);
  useEffect(() => () => off.current?.(), []);

  const start = useCallback(
    async (command: string) => {
      off.current?.();
      const commandId = nextId('cmd');
      setRun({ commandId, command, output: '', running: true });
      // subscribed before sending: output can arrive before the ack
      off.current = getTransport().subscribe((m) => {
        if (!('commandId' in m) || m.commandId !== commandId) return;
        if (m.type === 'terminal_output') {
          setRun((r) => (r?.commandId === commandId ? { ...r, output: r.output + m.data } : r));
        } else if (m.type === 'terminal_exit') {
          setRun((r) => (r?.commandId === commandId ? { ...r, exit: m, running: false } : r));
        } else if (m.type === 'command_result') {
          // after a reconnect the output and terminal_exit are gone; the outcome is not
          setRun((r) =>
            r?.commandId === commandId && !r.exit
              ? {
                  ...r,
                  running: false,
                  failed: `The connection dropped during the run; it ended: ${m.error ?? m.status}`,
                }
              : r,
          );
          off.current?.();
          off.current = null;
        }
      });
      useAppStore.getState().rememberCommand(workspaceId, command);
      const ack = await send({ type: 'terminal_run', commandId, workspaceId, command });
      if (!ack?.ok) {
        off.current?.();
        off.current = null;
        setRun((r) =>
          r?.commandId === commandId
            ? { ...r, running: false, failed: ack?.error ?? 'Not run: the outcome is unknown or nothing was sent' }
            : r,
        );
      }
    },
    [send, workspaceId],
  );

  const stop = useCallback(async () => {
    try {
      const ack = await getTransport().send({ type: 'terminal_stop', commandId: nextId('cmd'), workspaceId });
      if (!ack.ok) useAppStore.getState().addToast(ack.error ?? 'Could not stop the command', 'error');
    } catch {
      useAppStore.getState().addToast('Could not reach the PC to stop the command', 'error');
    }
  }, [workspaceId]);

  return { run, start, stop };
}
