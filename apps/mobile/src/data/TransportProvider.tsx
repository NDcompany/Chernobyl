/**
 * Connects the transport to the query cache and the live transcript store
 * (one message stream, two reducers) and routes side effects (local
 * notifications and their taps, audit trail). Mounted once at the root layout.
 */
import { useEffect, type ReactNode } from 'react';
import { AppState } from 'react-native';
import { router } from 'expo-router';
import {
  FEATURE_TRANSCRIPT,
  createSeedSnapshot,
  type ServerMessage,
  type Snapshot,
} from '@chernobyl/protocol';
import { createE2eTransport, parsePairingPayload } from '@chernobyl/e2e';
import { getTransport, hostUrl, resetTransport, setTransport, type Transport } from './transport';
import { createWsTransport } from './wsTransport';
import { readHostToken, readPairing, rotatePairingPsk } from '@/platform/secure';
import { applyMessage, queryClient, STATE_KEY } from './state';
import { applyLiveMessage } from './live';
import { useAppStore, type Settings } from '@/store/appStore';
import { initNotifications, notify, onNotificationOpen } from '@/platform/notify';
import * as connectionService from '@/platform/connectionService';
import { recordAudit } from '@/platform/storage';
import { agentById, permissionHref } from './selectors';

/**
 * Alerts are on: enabled in Settings, and signed in — sign-out promises the
 * user must sign in again to see approvals, so none are posted after it.
 */
const alertsOn = ({ authed, settings: s }: { authed: boolean; settings: Settings }): boolean =>
  authed && s.push && (s.approvalAlerts || s.sessionAlerts);

function agentName(snapshot: Snapshot | undefined, id: string): string {
  if (!snapshot) return 'Agent';
  return agentById(snapshot, id)?.name ?? 'Agent';
}

async function handleSideEffect(msg: ServerMessage): Promise<void> {
  const { authed, settings } = useAppStore.getState();
  const push = authed && settings.push;

  if (msg.type === 'permission_requested' && push && settings.approvalAlerts) {
    const snapshot = queryClient.getQueryData<Snapshot>(STATE_KEY);
    const live = useAppStore.getState().features.includes(FEATURE_TRANSCRIPT);
    await notify(
      // ADR-018: asked by a session in a terminal or VS Code
      msg.permission.fromPc ? 'Approval needed (PC session)' : 'Approval needed',
      `${agentName(snapshot, msg.permission.agentId)} wants to ${msg.permission.title.toLowerCase()}: ${msg.permission.command}`,
      // the tap opens the conversation at its card (ADR-015 §5), else the request
      snapshot ? permissionHref(snapshot, msg.permission, live) : `/permission/${msg.permission.id}`,
    );
    return;
  }

  if (msg.type === 'session_updated' && msg.session.status === 'completed') {
    if (push && settings.sessionAlerts) {
      await notify('Session finished', msg.session.title, `/session/${msg.session.id}`);
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

const EMPTY_SNAPSHOT: Snapshot = { hosts: [], agents: [], sessions: [], permissions: [], activity: [] };

export function TransportProvider({ children }: { children: ReactNode }) {
  // Rebuilt whenever the credential changes (a new pairing was saved).
  const epoch = useAppStore((s) => s.transportEpoch);

  // a tapped alert opens what it is about, also when it launched the app
  useEffect(() => onNotificationOpen((url) => router.push(url)), []);

  useEffect(() => {
    let disposed = false;
    let cleanupTransport: Transport | null = null;
    let unsubMessages: (() => void) | null = null;
    let unsubStatus: (() => void) | null = null;
    // ADR-011 §4: the Android foreground service exists only to deliver alerts,
    // so it runs only for a real paired transport, while alerts are on and the
    // notification permission lets them post, and never for a credential the
    // companion refused (section 9). Re-decided whenever any of that changes.
    let e2e = false;
    // any real host (E2E pairing or the dev plaintext URL), i.e. not the demo
    let live = false;
    let canPost = false;
    let serviceText = 'Connecting to your desktop companion…';
    const syncService = () => {
      if (disposed) return;
      const want =
        e2e && canPost && alertsOn(useAppStore.getState()) && cleanupTransport?.status() !== 'denied';
      if (!want) {
        connectionService.stop();
      } else if (AppState.currentState === 'active') {
        // Android 12+ refuses a background start: the next 'active' retries,
        // which also covers a start the OS refused. A running service only
        // re-posts its notification.
        connectionService.start('Background connection', serviceText);
      }
    };
    // sign-in / sign-out and the alert settings switch the service
    const unsubStore = useAppStore.subscribe((s, prev) => {
      if (alertsOn(s) !== alertsOn(prev)) syncService();
    });
    const appState = AppState.addEventListener('change', (next) => {
      if (next !== 'active') return;
      // Back in the foreground: a reconnect sitting in backoff (the OS may have
      // frozen timers or dropped the socket while backgrounded) dials now.
      // Only from 'reconnecting' — never re-dial a denied credential (section 9).
      if (cleanupTransport?.status() === 'reconnecting') {
        cleanupTransport.connect();
      }
      // asks nothing: picks up a permission granted in system settings meanwhile
      void initNotifications(false).then((ok) => {
        canPost = ok;
        syncService();
      });
    });

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
            // the companion replaces the QR's key with a device key at first
            // use; the transport acks only once the keystore holds it
            onRekey: (psk) => rotatePairingPsk(cred.deviceId, psk),
          });
          setTransport(active);
          e2e = true;
          live = true;
        } catch (e) {
          // A corrupt stored pairing must not brick the app — surface it.
          useAppStore
            .getState()
            .addToast(e instanceof Error ? e.message : 'Stored pairing is invalid', 'error');
          const url = hostUrl();
          if (url) {
            active = createWsTransport(withToken(url));
            setTransport(active);
            live = true;
          } else {
            active = getTransport();
          }
        }
      } else {
        const url = hostUrl();
        if (url) {
          active = createWsTransport(withToken(url));
          setTransport(active);
          live = true;
        } else {
          active = getTransport();
        }
      }
      if (disposed) {
        resetTransport();
        return;
      }
      cleanupTransport = active;
      // The cache follows the transport (ADR-009): a live host starts empty
      // until its hello_ack snapshot arrives — never with the demo seed shown
      // as live — and demo mode (e.g. after unpairing) gets the seed back.
      queryClient.setQueryData<Snapshot>(STATE_KEY, live ? EMPTY_SNAPSHOT : createSeedSnapshot());
      unsubMessages = active.subscribe((msg) => {
        applyMessage(msg);
        applyLiveMessage(msg);
        if (msg.type === 'hello_ack') useAppStore.getState().setFeatures(msg.features ?? []);
        useAppStore.getState().trackCommand(msg);
        void handleSideEffect(msg);
      });
      unsubStatus = active.subscribeStatus((s) => {
        useAppStore.getState().setConnection(s);
        if (!e2e) return;
        if (s === 'denied') {
          // the ongoing notification would just vanish: say that alerts stopped
          if (alertsOn(useAppStore.getState()) && AppState.currentState !== 'active') {
            void notify(
              'Disconnected from your desktop companion',
              'This phone can no longer connect (revoked, token changed or version mismatch). Approval alerts have stopped. Open CHERNOBYL to pair again.',
            );
          }
          syncService(); // stops it
        } else if (s === 'connected' || s === 'reconnecting') {
          // 'handshaking' keeps the current text (Connecting… / Reconnecting…)
          serviceText =
            s === 'connected'
              ? 'Connected to your desktop companion'
              : 'Reconnecting to your desktop companion…';
          connectionService.update(serviceText);
        }
      });
      active.connect();
      // the permission dialog is shown for a real host only (demo mode never
      // asks); the service waits for the answer
      void initNotifications(live).then((ok) => {
        canPost = ok;
        syncService();
      });
    })();

    return () => {
      disposed = true;
      appState.remove();
      unsubStore();
      unsubMessages?.();
      unsubStatus?.();
      cleanupTransport?.close();
      resetTransport();
      // unpair / re-pair rebuild the transport through this cleanup
      connectionService.stop();
    };
  }, [epoch]);

  return <>{children}</>;
}
