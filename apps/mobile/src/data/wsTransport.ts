/**
 * WebSocket transport with exponential-backoff reconnection.
 *
 * On reconnect it re-sends `hello` (full snapshot resume) and replays every
 * unresolved command with its ORIGINAL commandId — the host suppresses
 * duplicates, so a command that already landed is never executed twice.
 */
import {
  PROTOCOL_VERSION,
  nextId,
  parseServerMessage,
  type ClientMessage,
  type ServerMessage,
} from '@chernobyl/protocol';
import type { Ack, Transport, TransportStatus } from './transport';

const MAX_BACKOFF_MS = 15_000;
const ACK_TIMEOUT_MS = 10_000;
const clientId = nextId('app');

interface Pending {
  msg: ClientMessage;
  resolve: (ack: Ack) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

export function createWsTransport(url: string): Transport {
  const msgSubs = new Set<(m: ServerMessage) => void>();
  const statusSubs = new Set<(s: TransportStatus) => void>();
  const pending = new Map<string, Pending>();
  let ws: WebSocket | null = null;
  let status: TransportStatus = 'offline';
  let attempts = 0;
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  let stopped = true;

  const setStatus = (s: TransportStatus) => {
    if (status === s) return;
    status = s;
    statusSubs.forEach((fn) => fn(s));
  };
  const emit = (m: ServerMessage) => msgSubs.forEach((fn) => fn(m));

  function failPending(reason: string): void {
    for (const [, p] of pending) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    pending.clear();
  }

  function open(): void {
    if (stopped) return;
    setStatus(attempts === 0 ? 'connecting' : 'reconnecting');
    let socket: WebSocket;
    try {
      socket = new WebSocket(url);
    } catch {
      scheduleRetry();
      return;
    }
    ws = socket;

    socket.onopen = () => {
      if (stopped) return;
      attempts = 0;
      setStatus('connected');
      socket.send(JSON.stringify({ type: 'hello', clientId, protocol: PROTOCOL_VERSION }));
      for (const [, p] of pending) socket.send(JSON.stringify(p.msg));
    };

    socket.onmessage = (event) => {
      const msg = parseServerMessage(String(event.data));
      if (!msg) return;
      if (msg.type === 'ack') {
        const p = pending.get(msg.commandId);
        if (p) {
          clearTimeout(p.timer);
          pending.delete(msg.commandId);
          p.resolve(msg);
        }
      }
      emit(msg);
    };

    socket.onerror = () => {
      /* onclose always follows */
    };

    socket.onclose = () => {
      if (ws !== socket) return;
      ws = null;
      if (stopped) {
        setStatus('offline');
        return;
      }
      // pending commands stay queued for replay on the next open
      setStatus('reconnecting');
      scheduleRetry();
    };
  }

  function scheduleRetry(): void {
    if (stopped || retryTimer) return;
    const delay = Math.min(MAX_BACKOFF_MS, 1000 * 2 ** attempts);
    attempts += 1;
    retryTimer = setTimeout(() => {
      retryTimer = null;
      open();
    }, delay);
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
      if (!ws) open();
    },

    close() {
      stopped = true;
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
      failPending('transport closed');
      const socket = ws;
      ws = null;
      try {
        socket?.close();
      } catch {
        /* already closed */
      }
      setStatus('offline');
    },

    send(msg: ClientMessage) {
      return new Promise<Ack>((resolve, reject) => {
        if ('commandId' in msg && pending.has(msg.commandId)) {
          reject(new Error('command already in flight'));
          return;
        }
        if (!ws || ws.readyState !== WebSocket.OPEN) {
          reject(new Error('offline'));
          return;
        }
        const key = 'commandId' in msg ? msg.commandId : `_${Date.now()}`;
        const timer = setTimeout(() => {
          pending.delete(key);
          reject(new Error('timed out'));
        }, ACK_TIMEOUT_MS);
        pending.set(key, { msg, resolve, reject, timer });
        ws.send(JSON.stringify(msg));
      });
    },
  };
}
