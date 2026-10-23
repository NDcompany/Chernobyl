/**
 * Connects the transport to the query cache and routes side effects
 * (local notifications, audit trail). Mounted once at the root layout.
 */
import { useEffect, type ReactNode } from 'react';
import type { ServerMessage, Snapshot } from '@chernobyl/protocol';
import { createE2eTransport, parsePairingPayload } from '@chernobyl/e2e';
import { getTransport, hostUrl, resetTransport, setTransport, type Transport } from './transport';
import { createWsTransport } from './wsTransport';
import { readHostToken, readPairing } from '@/platform/secure';
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
  // Rebuilt whenever the credential changes (a new pairing was saved).
  const epoch = useAppStore((s) => s.transportEpoch);

  useEffect(() => {
    let disposed = false;
    let cleanupTransport: Transport | null = null;
    let unsubMessages: (() => void) | null = null;
    let unsubStatus: (() => void) | null = null;

    void (async () => {
      // Build the transport here (awaiting keystore reads) and install it
      // before any command resolves a transport via getTransport().
      // Order of authority: a saved pairing wins (real E2E handshake against
      // the QR endpoint), then EXPO_PUBLIC_HOST_WS_URL (loopback dev parity),
      // then the mock transport.
      const token = await readHostToken();
      const withToken = (u: string) =>
        token ? `${u}${u.includes('?') ? '&' : '?'}token=${encodeURIComponent(token)}` : u;
      const pairingJson = await readPairing();

      let active: Transport;
      if (pairingJson) {
        try {
          const cred = parsePairingPayload(pairingJson);
          // ordered endpoint list (ADR-011 §2 / S10): every advertised
          // endpoint, in QR order, each carrying the keystore token
          active = createE2eTransport({
            endpoints: cred.endpoints.map(withToken),
            pairing: cred,
          });
          setTransport(active);
        } catch (e) {
          // A corrupt stored pairing must not brick the app — surface it.
          useAppStore
            .getState()
            .addToast(e instanceof Error ? e.message : 'Stored pairing is invalid', 'error');
          const url = hostUrl();
          if (url) {
            active = createWsTransport(withToken(url));
            setTransport(active);
          } else {
            active = getTransport();
          }
        }
      } else {
        const url = hostUrl();
        if (url) {
          active = createWsTransport(withToken(url));
          setTransport(active);
        } else {
          active = getTransport();
        }
      }
      if (disposed) {
        resetTransport();
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
      resetTransport();
    };
  }, [epoch]);

  return <>{children}</>;
}
