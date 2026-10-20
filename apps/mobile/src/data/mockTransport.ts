/**
 * Offline transport: runs the exact same MockEngine the host service uses,
 * in-process. The app is fully interactive with no host at all.
 */
import {
  MockEngine,
  PROTOCOL_VERSION,
  nextId,
  type ClientMessage,
  type ServerMessage,
} from '@chernobyl/protocol';
import type { Ack, Transport, TransportStatus } from './transport';

const MIN_LATENCY = 160;
const MAX_LATENCY = 380;
const CONNECT_MS = 260;

export function createMockTransport(): Transport {
  const msgSubs = new Set<(m: ServerMessage) => void>();
  const statusSubs = new Set<(s: TransportStatus) => void>();
  const timers = new Set<ReturnType<typeof setTimeout>>();
  const clientId = nextId('mock');
  let status: TransportStatus = 'offline';

  const setStatus = (s: TransportStatus) => {
    if (status === s) return;
    status = s;
    statusSubs.forEach((fn) => fn(s));
  };
  const emit = (m: ServerMessage) => msgSubs.forEach((fn) => fn(m));
  const engine = new MockEngine(emit);

  const schedule = (fn: () => void) => {
    const delay = MIN_LATENCY + Math.random() * (MAX_LATENCY - MIN_LATENCY);
    const t = setTimeout(() => {
      timers.delete(t);
      fn();
    }, delay);
    timers.add(t);
  };

  return {
    kind: 'mock',
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
      setStatus('connecting');
      const t = setTimeout(() => {
        timers.delete(t);
        setStatus('connected');
        const hello = engine.handle({ type: 'hello', clientId, protocol: PROTOCOL_VERSION });
        if (hello) emit(hello);
      }, CONNECT_MS);
      timers.add(t);
    },

    close() {
      for (const t of timers) clearTimeout(t);
      timers.clear();
      setStatus('offline');
    },

    send(msg: ClientMessage) {
      return new Promise<Ack>((resolve, reject) => {
        if (status !== 'connected') {
          reject(new Error('offline'));
          return;
        }
        schedule(() => {
          const reply = engine.handle(msg);
          if (reply) emit(reply);
          if (reply && reply.type === 'ack') {
            resolve(reply);
            return;
          }
          resolve({
            type: 'ack',
            commandId: 'commandId' in msg ? msg.commandId : 'unknown',
            ok: true,
          });
        });
      });
    },
  };
}
