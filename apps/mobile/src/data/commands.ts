/**
 * Command layer: builds ClientMessages with fresh commandIds, suppresses
 * identical concurrent sends (client-side duplicate prevention), surfaces
 * ack errors as toasts and reports host-unreachable failures honestly.
 */
import { useCallback } from 'react';
import { nextId, type ClientMessage, type PermissionDecision } from '@chernobyl/protocol';
import { getTransport, type Ack } from './transport';
import { useAppStore } from '@/store/appStore';

function dedupeKey(msg: ClientMessage): string {
  const clone: Record<string, unknown> = { ...msg };
  delete clone.commandId;
  delete clone.clientId;
  delete clone.protocol;
  return `${msg.type}:${JSON.stringify(clone)}`;
}

export const commands = {
  /**
   * G2: the decision carries the revision it was made against — a record
   * that changed since rendering is refused fail-closed by the host.
   */
  approve: (
    permissionId: string,
    decision: PermissionDecision,
    revision: number,
  ): ClientMessage => ({
    type: 'permission_decision',
    commandId: nextId('cmd'),
    permissionId,
    decision,
    revision,
  }),
  submitPrompt: (sessionId: string, text: string): ClientMessage => ({
    type: 'submit_prompt',
    commandId: nextId('cmd'),
    sessionId,
    text,
  }),
  interrupt: (sessionId: string): ClientMessage => ({
    type: 'interrupt',
    commandId: nextId('cmd'),
    sessionId,
  }),
  createSession: (agentId: string, title: string, prompt: string, project?: string): ClientMessage => ({
    type: 'create_session',
    commandId: nextId('cmd'),
    agentId,
    title,
    prompt,
    project,
  }),
} as const;

export type SendResult = Ack | null;

export function useCommands() {
  const send = useCallback(
    async (msg: ClientMessage, successToast?: string): Promise<SendResult> => {
      const key = dedupeKey(msg);
      const store = useAppStore.getState();
      if (!store.markInFlight(key)) {
        // an identical command is already running — do not send it twice
        return null;
      }
      try {
        const ack = await getTransport().send(msg);
        if (!ack.ok) {
          useAppStore.getState().addToast(ack.error ?? 'Command rejected', 'error');
          return ack;
        }
        if (successToast) useAppStore.getState().addToast(successToast, 'success');
        return ack;
      } catch (err) {
        const isOffline = err instanceof Error && err.message === 'offline';
        if (isOffline) {
          // Rejected before send ('offline') is a definite not-sent state
          useAppStore.getState().addToast('Host unreachable — nothing was sent', 'error');
        } else {
          // Mandate section 9: ack timed out, or connection dropped after send.
          // Outcome is UNKNOWN (the host may have executed it) — never claim failed or not sent.
          if ('commandId' in msg) {
            useAppStore.getState().setCommandUnknown(msg.commandId, {
              sessionId: 'sessionId' in msg ? (msg.sessionId as string) : undefined,
              permissionId: 'permissionId' in msg ? (msg.permissionId as string) : undefined,
            });
          }
          useAppStore.getState().addToast('Outcome unknown — the host may have run it', 'warning');
        }
        return null;
      } finally {
        useAppStore.getState().clearInFlight(key);
      }
    },
    [],
  );

  return send;
}
