/**
 * Command layer: builds ClientMessages with fresh commandIds, suppresses
 * identical concurrent sends (client-side duplicate prevention), surfaces
 * ack errors as toasts and reports host-unreachable failures honestly.
 */
import { useCallback } from 'react';
import { nextId, type ClientMessage, type PermissionDecision, type PromptImage } from '@chernobyl/protocol';
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
    /** ADR-015: the chosen option, a reason the model sees, question answers */
    extra?: { optionId?: string; message?: string; answers?: Record<string, string[]> },
  ): ClientMessage => ({
    type: 'permission_decision',
    commandId: nextId('cmd'),
    permissionId,
    decision,
    revision,
    ...extra,
  }),
  /**
   * ADR-017: `images` only for agents whose `live.composer` has 'images'.
   * ADR-018: `fork` continues a session that is live on the PC as a new
   * session (the ack's sessionId); the PC session is untouched.
   */
  submitPrompt: (sessionId: string, text: string, images?: PromptImage[], fork?: boolean): ClientMessage => ({
    type: 'submit_prompt',
    commandId: nextId('cmd'),
    sessionId,
    text,
    ...(images?.length ? { images } : {}),
    ...(fork ? { fork: true } : {}),
  }),
  interrupt: (sessionId: string): ClientMessage => ({
    type: 'interrupt',
    commandId: nextId('cmd'),
    sessionId,
  }),
  createSession: (agentId: string, title: string, prompt: string, project?: string, model?: string): ClientMessage => ({
    type: 'create_session',
    commandId: nextId('cmd'),
    agentId,
    title,
    prompt,
    project,
    ...(model ? { model } : {}),
  }),
  /* ADR-015 live-session commands (session_subscribe/unsubscribe live in data/live.ts: they never toast) */
  setPermissionMode: (sessionId: string, mode: 'default' | 'acceptEdits' | 'plan'): ClientMessage => ({
    type: 'set_permission_mode',
    commandId: nextId('cmd'),
    sessionId,
    mode,
  }),
  endSession: (sessionId: string): ClientMessage => ({
    type: 'session_end',
    commandId: nextId('cmd'),
    sessionId,
  }),
  mcpAuth: (sessionId: string, server: string, flow: 'desktop' | 'phone-callback'): ClientMessage => ({
    type: 'mcp_auth',
    commandId: nextId('cmd'),
    sessionId,
    server,
    flow,
  }),
  mcpAuthCallback: (sessionId: string, server: string, callbackUrl: string): ClientMessage => ({
    type: 'mcp_auth_callback',
    commandId: nextId('cmd'),
    sessionId,
    server,
    callbackUrl,
  }),
  mcpReconnect: (sessionId: string, server: string): ClientMessage => ({
    type: 'mcp_reconnect',
    commandId: nextId('cmd'),
    sessionId,
    server,
  }),
  /* ADR-017 composer (Claude) */
  setModel: (sessionId: string, model: string): ClientMessage => ({
    type: 'set_model',
    commandId: nextId('cmd'),
    sessionId,
    model,
  }),
  renameSession: (sessionId: string, title: string): ClientMessage => ({
    type: 'rename_session',
    commandId: nextId('cmd'),
    sessionId,
    title,
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
