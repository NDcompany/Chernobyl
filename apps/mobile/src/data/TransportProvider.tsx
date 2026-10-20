/**
 * Connects the transport to the query cache and routes side effects
 * (local notifications, audit trail). Mounted once at the root layout.
 */
import { useEffect, type ReactNode } from 'react';
import type { ServerMessage, Snapshot } from '@chernobyl/protocol';
import { getTransport, hostUrl, setTransport, type Transport } from './transport';
import { createWsTransport } from './wsTransport';
import { readHostToken } from '@/platform/secure';
import { applyMessage, queryClient, STATE_KEY } from './state';
import { useAppStore } from '@/store/appStore';
import { initNotifications, notify } from '@/platform/notify';
import { recordAudit } from '@/platform/storage';
import { agentById } from './selectors';

function agentName(snapshot: Snapshot | undefined, id: string): string {
  if (!snapshot) return 'Agent';
  return agentById(snapshot, id)?.name ?? 'Agent';
}

async function handleSideEffect(msg: ServerMessage): Promise<void> {
  const settings = useAppStore.getState().settings;

  if (msg.type === 'permission_requested' && settings.push && settings.approvalAlerts) {
    const snapshot = queryClient.getQueryData<Snapshot>(STATE_KEY);
    await notify(
      'Approval needed',
      `${agentName(snapshot, msg.permission.agentId)} wants to ${msg.permission.title.toLowerCase()}: ${msg.permission.command}`,
    );
    return;
  }

  if (msg.type === 'session_updated' && msg.session.status === 'completed') {
    if (settings.push && settings.sessionAlerts) {
      await notify('Session finished', msg.session.title);
    }
    return;
  }

  if (msg.type === 'permission_resolved') {
    const p = msg.permission;
    const snapshot = queryClient.getQueryData<Snapshot>(STATE_KEY);
    recordAudit({
      permissionId: p.id,
      agent: agentName(snapshot, p.agentId),
      command: p.command,
      decision: p.decision ?? p.status,
      risk: p.risk,
      ts: p.decidedAt ?? Date.now(),
    });
  }
}

export function TransportProvider({ children }: { children: ReactNode }) {
  useEffect(() => {
    let disposed = false;
    let cleanupTransport: Transport | null = null;
    let unsubMessages: (() => void) | null = null;
    let unsubStatus: (() => void) | null = null;

    void (async () => {
      // A secured host expects the keystore token on the connect URL; build the
      // transport here (awaiting secure storage) and install it before any
      // command resolves a transport via getTransport().
      const url = hostUrl();
      let active: Transport;
      if (url) {
        const token = await readHostToken();
        const full = token
          ? `${url}${url.includes('?') ? '&' : '?'}token=${encodeURIComponent(token)}`
          : url;
        active = createWsTransport(full);
        setTransport(active);
      } else {
        active = getTransport();
      }
      if (disposed) {
        active.close();
        return;
      }
      cleanupTransport = active;
      unsubMessages = active.subscribe((msg) => {
        applyMessage(msg);
        useAppStore.getState().trackCommand(msg);
        void handleSideEffect(msg);
      });
      unsubStatus = active.subscribeStatus((s) =>
        useAppStore.getState().setConnection(s),
      );
      active.connect();
      void initNotifications();
    })();

    return () => {
      disposed = true;
      unsubMessages?.();
      unsubStatus?.();
      cleanupTransport?.close();
    };
  }, []);

  return <>{children}</>;
}
