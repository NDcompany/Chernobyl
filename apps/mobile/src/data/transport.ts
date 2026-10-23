/**
 * Transport abstraction: mock (default, offline demo) or WebSocket host API.
 * Both emit the same ServerMessage stream and resolve the same command acks.
 */
import type { ClientMessage, ServerMessage } from '@chernobyl/protocol';
import type { TransportStatus as E2eStatus } from '@chernobyl/e2e';
import { createMockTransport } from './mockTransport';
import { createWsTransport } from './wsTransport';

/**
 * All transport statuses, including the E2E handshake's honest authorization
 * states ('handshaking' = authorizing with the host, 'denied' = credential
 * refused — terminal, not retried). Mock/ws emit a subset.
 */
export type TransportStatus = E2eStatus;

export type Ack = Extract<ServerMessage, { type: 'ack' }>;

export interface Transport {
  readonly kind: 'mock' | 'websocket';
  connect(): void;
  close(): void;
  /** Sends a command; rejects when the host is unreachable or it times out. */
  send(msg: ClientMessage): Promise<Ack>;
  subscribe(cb: (m: ServerMessage) => void): () => void;
  subscribeStatus(cb: (s: TransportStatus) => void): () => void;
  status(): TransportStatus;
}

let transport: Transport | null = null;

/** Set EXPO_PUBLIC_HOST_WS_URL (e.g. ws://10.0.4.21:8787) to use the real host. */
export function hostUrl(): string | undefined {
  return process.env.EXPO_PUBLIC_HOST_WS_URL;
}

/**
 * Install the active transport. The provider calls this at boot (appending the
 * keystore token for a secured host) before any command resolves a transport.
 */
export function setTransport(next: Transport): void {
  transport = next;
}

export function getTransport(): Transport {
  if (!transport) {
    const url = hostUrl();
    transport = url ? createWsTransport(url) : createMockTransport();
  }
  return transport;
}

/** The active transport's kind — 'mock' (simulation) or 'websocket' (live). */
export function transportKind(): Transport['kind'] {
  return getTransport().kind;
}

/**
 * Tear down the active transport so the next getTransport() rebuilds it.
 * The provider calls this when a new pairing is saved (the credential
 * changed, so the connection must too).
 */
export function resetTransport(): void {
  transport?.close();
  transport = null;
}
