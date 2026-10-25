/**
 * E2E WebSocket transport: performs the `hs_*` handshake on every connection,
 * then speaks sealed frames only. Mirrors the proven lifecycle semantics of
 * the plaintext `wsTransport` (pending-command replay on reconnect with the
 * ORIGINAL commandId — the host dedupes — exponential backoff, single
 * subscription list across reconnects), and adds what the E2E contract needs:
 *
 * - authorization is a visible state: `handshaking` between socket-open and
 *   `hs_ok`, `denied` (terminal until an explicit `connect()`) for hs_err /
 *   close 4001 / 4002 / 4003 (revoked live) — stale or revoked credentials never spin a
 *   reconnect loop;
 * - a heartbeat ping every `heartbeatMs` (server's dead-peer window is 3×15 s,
 *   ADR-011 / server.rs) whose missing pong forces a reconnect;
 * - any sealed-frame violation (replay/desync/tamper) tears the connection
 *   down; the next connection re-handshakes from scratch with fresh counters;
 * - ordered reconnect over the QR's endpoint list (ADR-011 §2 / S10): each
 *   failed attempt advances to the next endpoint in the advertised order, a
 *   successful handshake resets to the first. The list is dial order only —
 *   endpoints are not credentials (see docs/decisions/PHASE5-THREAT-MODEL.md §3.9).
 */
import { PROTOCOL_VERSION, nextId, parseServerMessage } from '@chernobyl/protocol';
import type { ClientMessage, ServerMessage } from '@chernobyl/protocol';
import { E2eError } from './errors.js';
import { performHandshake } from './handshake.js';
import type { PairingCredential } from './pairing.js';
import { SealedSession } from './session.js';

export type TransportStatus =
  | 'offline'
  | 'connecting'
  | 'handshaking'
  | 'connected'
  | 'reconnecting'
  | 'denied';

export type Ack = Extract<ServerMessage, { type: 'ack' }>;

/** Structural subset of the WebSocket surface used (RN + ws both adapt). */
export interface SocketLike {
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev?: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code?: number }) => void) | null;
  onerror: ((ev?: unknown) => void) | null;
}

export interface E2eTransportOptions {
  /** Single endpoint (tests/simple callers). Ignored when `endpoints` is given. */
  url?: string;
  /**
   * Ordered endpoint list from the pairing payload (or token-appended copies
   * of it). Defaults to `pairing.endpoints` when neither is provided.
   */
  endpoints?: string[];
  pairing: PairingCredential;
  /** Injected for tests/Node (`ws`); defaults to the runtime's global WebSocket. */
  openSocket?: (url: string) => SocketLike;
  ackTimeoutMs?: number;
  handshakeTimeoutMs?: number;
  retryBaseMs?: number;
  retryMaxMs?: number;
  /**
   * A live session must stay up this long before the backoff resets; a
   * connection that handshakes then drops at once keeps its penalty, so a
   * flapping server is not hammered at the base delay (audit P5).
   */
  stableMs?: number;
  /** App-level ping interval; 0 disables the heartbeat. */
  heartbeatMs?: number;
  pongTimeoutMs?: number;
}

export interface E2eTransport {
  readonly kind: 'websocket';
  connect(): void;
  close(): void;
  send(msg: ClientMessage): Promise<Ack>;
  subscribe(cb: (m: ServerMessage) => void): () => void;
  subscribeStatus(cb: (s: TransportStatus) => void): () => void;
  status(): TransportStatus;
}

interface Pending {
  msg: ClientMessage;
  resolve: (ack: Ack) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

/** Server close codes that mean the credential or protocol is bad: retrying cannot help. */
const TERMINAL_CLOSE: Record<number, string> = {
  4001: 'unauthorized (bad or missing token)',
  4002: 'protocol version mismatch',
  4003: 'device revoked',
};

const DEFAULTS = {
  ackTimeoutMs: 10_000,
  handshakeTimeoutMs: 10_000,
  retryBaseMs: 1_000,
  retryMaxMs: 15_000,
  stableMs: 30_000,
  heartbeatMs: 15_000,
  pongTimeoutMs: 10_000,
};

/**
 * Bounded exponential backoff with equal jitter (audit P5): half the capped
 * step is fixed, half is random, so clients that lost the same server at the
 * same instant spread their retries instead of reconnecting in lockstep.
 * Always within [min(base, max) / 2, max].
 */
export function retryDelayMs(
  attempt: number,
  baseMs: number,
  maxMs: number,
  rand: () => number = Math.random,
): number {
  const cap = Math.min(maxMs, baseMs * 2 ** Math.min(attempt, 30));
  return cap / 2 + rand() * (cap / 2);
}

/**
 * The Origin the app sends (D1): React Native's Android WebSocket otherwise
 * adds a default "http://host:port" Origin, which the companion refuses as a
 * browser. Browsers cannot forge Origin, so this non-web value is the one
 * the companion accepts (server.rs APP_ORIGIN).
 */
export const APP_ORIGIN = 'chernobyl://app';

function defaultOpenSocket(url: string): SocketLike {
  // the third argument is React Native's options bag; browsers ignore it
  const ctor = (
    globalThis as {
      WebSocket?: new (url: string, protocols?: string[], options?: { headers: Record<string, string> }) => SocketLike;
    }
  ).WebSocket;
  if (!ctor) throw new Error('no WebSocket implementation available');
  return new ctor(url, undefined, { headers: { origin: APP_ORIGIN } });
}

export function createE2eTransport(opts: E2eTransportOptions): E2eTransport {
  const ackTimeoutMs = opts.ackTimeoutMs ?? DEFAULTS.ackTimeoutMs;
  const handshakeTimeoutMs = opts.handshakeTimeoutMs ?? DEFAULTS.handshakeTimeoutMs;
  const retryBaseMs = opts.retryBaseMs ?? DEFAULTS.retryBaseMs;
  const retryMaxMs = opts.retryMaxMs ?? DEFAULTS.retryMaxMs;
  const stableMs = opts.stableMs ?? DEFAULTS.stableMs;
  const heartbeatMs = opts.heartbeatMs ?? DEFAULTS.heartbeatMs;
  const pongTimeoutMs = opts.pongTimeoutMs ?? DEFAULTS.pongTimeoutMs;
  const openSocket = opts.openSocket ?? defaultOpenSocket;
  const { pairing } = opts;
  const endpoints: string[] = opts.endpoints ?? (opts.url ? [opts.url] : pairing.endpoints);
  if (endpoints.length === 0) throw new Error('e2e transport: no endpoint to dial');
  const clientId = nextId('app');

  const msgSubs = new Set<(m: ServerMessage) => void>();
  const statusSubs = new Set<(s: TransportStatus) => void>();
  const pending = new Map<string, Pending>();
  const pendingPongs = new Map<string, ReturnType<typeof setTimeout>>();

  let ws: SocketLike | null = null;
  let session: SealedSession | null = null;
  let status: TransportStatus = 'offline';
  let attempts = 0;
  // when the current session reached hs_ok; decides the backoff reset on close
  let liveSince = 0;
  // position in the QR endpoint list; advanced per failed attempt, reset to
  // the primary endpoint on a successful handshake (ordered reconnect)
  let endpointIdx = 0;
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  let heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  let stopped = true;
  // frames that arrive before/while the handshake coroutine awaits recv()
  const hsQueue: string[] = [];

  const setStatus = (s: TransportStatus): void => {
    if (status === s) return;
    status = s;
    statusSubs.forEach((fn) => fn(s));
  };
  const emit = (m: ServerMessage): void => msgSubs.forEach((fn) => fn(m));

  function failPending(reason: string): void {
    for (const [, p] of pending) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    pending.clear();
  }

  function stopHeartbeat(): void {
    if (heartbeatTimer) {
      clearInterval(heartbeatTimer);
      heartbeatTimer = null;
    }
    for (const t of pendingPongs.values()) clearTimeout(t);
    pendingPongs.clear();
  }

  function startHeartbeat(socket: SocketLike, s: SealedSession): void {
    if (heartbeatMs <= 0) return;
    heartbeatTimer = setInterval(() => {
      const id = nextId('ping');
      const timer = setTimeout(() => {
        // server missed our pong — the connection is not healthy; reconnect
        pendingPongs.delete(id);
        socket.close();
      }, pongTimeoutMs);
      pendingPongs.set(id, timer);
      try {
        socket.send(s.seal(JSON.stringify({ type: 'ping', commandId: id })));
      } catch {
        socket.close();
      }
    }, heartbeatMs);
  }

  function scheduleRetry(): void {
    if (stopped || retryTimer || status === 'denied') return;
    const delay = retryDelayMs(attempts, retryBaseMs, retryMaxMs);
    attempts += 1;
    retryTimer = setTimeout(() => {
      retryTimer = null;
      open();
    }, delay);
  }

  function deny(reason: string): void {
    // Terminal until the user calls connect() again (e.g. after repairing the
    // stored pairing): hs_err / 4001 / 4002 mean the credential itself is bad.
    setStatus('denied');
    failPending(reason);
  }

  function handleSealed(socket: SocketLike, text: string): void {
    const s = session;
    if (!s) return;
    let inner: string;
    try {
      inner = s.open(text);
    } catch {
      // Sealed-frame violation on an established session (replay/desync/tamper):
      // tear this connection down; the retry re-handshakes with fresh counters.
      socket.close();
      return;
    }
    const msg = parseServerMessage(inner);
    if (!msg) return; // unknown message types are ignored (parity with wsTransport)
    if (msg.type === 'pong') {
      // heartbeat bookkeeping only — not surfaced to subscribers
      const t = pendingPongs.get(msg.commandId);
      if (t) {
        clearTimeout(t);
        pendingPongs.delete(msg.commandId);
      }
      return;
    }
    if (msg.type === 'ack') {
      const p = pending.get(msg.commandId);
      if (p) {
        clearTimeout(p.timer);
        pending.delete(msg.commandId);
        p.resolve(msg);
      }
    }
    emit(msg);
  }

  function open(): void {
    if (stopped || status === 'denied') return;
    setStatus(attempts === 0 ? 'connecting' : 'reconnecting');
    let socket: SocketLike;
    try {
      socket = openSocket(endpoints[endpointIdx]);
    } catch {
      endpointIdx = (endpointIdx + 1) % endpoints.length;
      scheduleRetry();
      return;
    }
    ws = socket;

    let handshakeDone = false;
    const hsWaiters: Array<{ resolve: (t: string) => void; reject: (e: Error) => void }> = [];

    socket.onopen = () => {
      if (stopped || ws !== socket) return;
      setStatus('handshaking');
      void (async () => {
        try {
          const s = await performHandshake(
            {
              send: (t) => socket.send(t),
              recv: () => {
                const q = hsQueue.shift();
                if (q !== undefined) return Promise.resolve(q);
                return new Promise<string>((res, rej) => hsWaiters.push({ resolve: res, reject: rej }));
              },
            },
            pairing,
            { timeoutMs: handshakeTimeoutMs },
          );
          if (stopped || ws !== socket) return; // superseded while handshaking
          session = s;
          handshakeDone = true;
          liveSince = Date.now();
          endpointIdx = 0; // live session → future reconnects start from the primary
          setStatus('connected');
          try {
            socket.send(s.seal(JSON.stringify({ type: 'hello', clientId, protocol: PROTOCOL_VERSION })));
            // replay commands that were in flight when the previous connection
            // died — the host dedupes by commandId, so nothing runs twice
            for (const [, p] of pending) socket.send(s.seal(JSON.stringify(p.msg)));
          } catch {
            socket.close();
            return;
          }
          startHeartbeat(socket, s);
        } catch (err) {
          if (ws !== socket) return;
          if (
            err instanceof E2eError &&
            (err.code === 'handshake-rejected' || err.code === 'server-proof-mismatch')
          ) {
            // Terminal: the server rejected the credential (hs_err) or cannot
            // prove PSK knowledge (deterministic mismatch / MITM). Retrying
            // cannot succeed — the user must repair the stored pairing.
            deny(err.message);
            socket.close(); // server keeps the socket open after hs_err
            return;
          }
          // handshake timeout / malformed handshake frames / closed mid-way
          socket.close();
        }
      })();
    };

    socket.onmessage = (ev) => {
      if (ws !== socket) return;
      if (handshakeDone) {
        if (typeof ev.data === 'string') handleSealed(socket, ev.data);
        return;
      }
      if (typeof ev.data !== 'string') return; // binary frames are not part of the contract
      const w = hsWaiters.shift();
      if (w) w.resolve(ev.data);
      else hsQueue.push(ev.data);
    };

    socket.onclose = (ev) => {
      if (ws !== socket) return;
      ws = null;
      session = null;
      stopHeartbeat();
      for (const w of hsWaiters.splice(0)) w.reject(new Error('connection closed'));
      if (stopped) {
        setStatus('offline');
        return;
      }
      const terminal = ev?.code !== undefined ? TERMINAL_CLOSE[ev.code] : undefined;
      if (terminal) deny(terminal);
      if (status === 'denied') return; // terminal — no auto-retry
      // Ordered reconnect over the QR endpoint list (ADR-011 §2 / S10):
      // a cycle that never reached a session advances to the NEXT endpoint
      // (this one refused/died mid-handshake); a drop after a live session
      // restarts the cycle from the primary (wraps; 1 endpoint = no-op).
      endpointIdx = handshakeDone
        ? 0
        : (endpointIdx + 1) % endpoints.length;
      // only a session that stayed up earns a fresh backoff (audit P5)
      if (handshakeDone && Date.now() - liveSince >= stableMs) attempts = 0;
      setStatus('reconnecting');
      scheduleRetry();
    };

    socket.onerror = () => {
      /* onclose always follows */
    };
  }

  return {
    kind: 'websocket',
    status: () => status,

    subscribe(cb) {
      msgSubs.add(cb);
      return () => msgSubs.delete(cb);
    },
    subscribeStatus(cb) {
      statusSubs.add(cb);
      cb(status);
      return () => statusSubs.delete(cb);
    },

    connect() {
      stopped = false;
      if (status === 'denied') setStatus('offline'); // explicit retry clears the terminal state
      endpointIdx = 0; // an explicit connect starts at the primary endpoint
      // an explicit connect supersedes any scheduled retry: without this the
      // old timer would later open a second socket and orphan this one
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
      attempts = 0;
      if (!ws) open();
    },

    close() {
      stopped = true;
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
      stopHeartbeat();
      failPending('transport closed');
      const socket = ws;
      ws = null;
      session = null;
      try {
        socket?.close();
      } catch {
        /* already closed */
      }
      setStatus('offline');
    },

    send(msg) {
      return new Promise<Ack>((resolve, reject) => {
        if ('commandId' in msg && pending.has(msg.commandId)) {
          reject(new Error('command already in flight'));
          return;
        }
        const socket = ws;
        const s = session;
        if (!socket || !s || status !== 'connected') {
          reject(new Error('offline'));
          return;
        }
        const key = 'commandId' in msg ? msg.commandId : `_t${Date.now()}`;
        const timer = setTimeout(() => {
          pending.delete(key);
          reject(new Error('timed out'));
        }, ackTimeoutMs);
        pending.set(key, { msg, resolve, reject, timer });
        try {
          socket.send(s.seal(JSON.stringify(msg)));
        } catch {
          clearTimeout(timer);
          pending.delete(key);
          reject(new Error('send failed'));
        }
      });
    },
  };
}
