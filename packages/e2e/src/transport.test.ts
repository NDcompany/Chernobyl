/**
 * Transport lifecycle over a scripted fake socket, with a mini "server" that
 * performs the server half of the handshake using the same primitives — the
 * exact sequences the real companion produces (server.rs handshake tests).
 * Covers: happy path, mutual-auth failure, hs_err denial, unexpected-close
 * reconnect, in-flight command replay, sealed-violation teardown, heartbeat,
 * and single-subscriber semantics across reconnects.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import {
  SealedSession,
  b64encode,
  createE2eTransport,
  deriveKeys,
  handshakeProof,
  type PairingCredential,
  type SocketLike,
} from './index.js';

const PSK = new Uint8Array(32).fill(7);
const NONCE = new Uint8Array(16).fill(1);
const SALT = new Uint8Array(16).fill(2);
const DEVICE = 'aabbccddeeff0011';

const pairing: PairingCredential = {
  host: '0123456789abcdef',
  endpoints: ['ws://test:8787'],
  deviceId: DEVICE,
  psk: PSK,
  challenge: new Uint8Array(16),
  expiresAtSec: 4_000_000_000,
};

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const tick = () => sleep(0);

class FakeSocket implements SocketLike {
  sent: string[] = [];
  closedWith: number | null = null;
  onopen: ((ev?: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code?: number }) => void) | null = null;
  onerror: ((ev?: unknown) => void) | null = null;

  send(data: string): void {
    if (this.closedWith !== null) throw new Error('send on closed socket');
    this.sent.push(data);
  }
  close(): void {
    this.serverClose(1006);
  }
  serverOpen(): void {
    this.onopen?.({});
  }
  serverSend(text: string): void {
    if (this.closedWith === null) this.onmessage?.({ data: text });
  }
  serverClose(code = 1006): void {
    if (this.closedWith !== null) return;
    this.closedWith = code;
    this.onclose?.({ code });
  }
}

function harness(opts?: { heartbeatMs?: number; pongTimeoutMs?: number }) {
  const socks: FakeSocket[] = [];
  const t = createE2eTransport({
    url: 'ws://test:8787',
    pairing,
    retryBaseMs: 5,
    retryMaxMs: 20,
    heartbeatMs: opts?.heartbeatMs ?? 0,
    pongTimeoutMs: opts?.pongTimeoutMs ?? 50,
    openSocket: () => {
      const s = new FakeSocket();
      socks.push(s);
      return s;
    },
  });
  return { t, socks };
}

/** Server half of the handshake (server.rs semantics). */
async function serverHandshake(
  sock: FakeSocket,
  opts?: { serverProof?: Uint8Array; err?: string },
): Promise<SealedSession | null> {
  assert.match(sock.sent[0] ?? '', /"hs_hello"/);
  sock.serverSend(JSON.stringify({ type: 'hs_challenge', nonce: b64encode(NONCE), salt: b64encode(SALT) }));
  await tick();
  if (opts?.err !== undefined) {
    sock.serverSend(JSON.stringify({ type: 'hs_err', reason: opts.err }));
    await tick();
    return null;
  }
  assert.match(sock.sent[1] ?? '', /"hs_proof"/);
  const serverProof = opts?.serverProof ?? handshakeProof(PSK, 'server', DEVICE, NONCE, SALT);
  sock.serverSend(JSON.stringify({ type: 'hs_ok', serverProof: b64encode(serverProof) }));
  await tick();
  return SealedSession.host(deriveKeys(PSK, SALT));
}

test('connect: handshake, sealed hello, ack correlation, ping/pong', async () => {
  const { t, socks } = harness();
  const statuses: string[] = [];
  t.subscribeStatus((s) => statuses.push(s));
  t.connect();
  const sock = socks[0];
  assert.equal(sock.sent.length, 0, 'nothing before the socket opens');
  sock.serverOpen();
  const host = await serverHandshake(sock);
  assert.ok(host);
  assert.equal(t.status(), 'connected');
  assert.ok(statuses.includes('handshaking') && statuses.includes('connected'));
  // client frame order: 0 hs_hello, 1 hs_proof, then sealed hello + …
  const hello = host.open(sock.sent[2]);
  assert.match(hello, /"type":"hello"/);
  assert.match(hello, /"protocol":/);
  const ping = t.send({ type: 'ping', commandId: 'p1' });
  assert.equal(host.open(sock.sent[3]), '{"type":"ping","commandId":"p1"}');
  sock.serverSend(host.seal(JSON.stringify({ type: 'pong', commandId: 'p1' })));
  sock.serverSend(host.seal(JSON.stringify({ type: 'ack', commandId: 'p1', ok: true })));
  const ack = await ping;
  assert.equal(ack.ok, true);
  t.close();
  assert.equal(t.status(), 'offline');
});

test('server proof mismatch is terminal: denied, socket closed, no retry', async () => {
  const { t, socks } = harness();
  t.connect();
  const sock = socks[0];
  sock.serverOpen();
  sock.serverSend(JSON.stringify({ type: 'hs_challenge', nonce: b64encode(NONCE), salt: b64encode(SALT) }));
  await tick();
  const wrong = handshakeProof(new Uint8Array(32).fill(8), 'server', DEVICE, NONCE, SALT);
  sock.serverSend(JSON.stringify({ type: 'hs_ok', serverProof: b64encode(wrong) }));
  await tick();
  assert.equal(t.status(), 'denied');
  assert.notEqual(sock.closedWith, null, 'client must close the socket');
  await sleep(40); // several retry periods (5 ms) — no new socket may appear
  assert.equal(socks.length, 1, 'denied must not auto-retry');
  t.close();
});

test('hs_err (revoked/stale credential) is terminal and not retried', async () => {
  const { t, socks } = harness();
  t.connect();
  const sock = socks[0];
  sock.serverOpen();
  sock.serverSend(JSON.stringify({ type: 'hs_challenge', nonce: b64encode(NONCE), salt: b64encode(SALT) }));
  await tick();
  sock.serverSend(JSON.stringify({ type: 'hs_err', reason: 'device revoked' }));
  await tick();
  assert.equal(t.status(), 'denied');
  assert.notEqual(sock.closedWith, null);
  await sleep(40);
  assert.equal(socks.length, 1, 'hs_err must not auto-retry');
  // explicit connect() is the retry path (after the user repairs credentials)
  t.connect();
  assert.equal(t.status(), 'connecting');
  assert.equal(socks.length, 2, 'explicit connect() reopens');
  socks[1].serverOpen();
  const host = await serverHandshake(socks[1]);
  assert.ok(host);
  assert.equal(t.status(), 'connected');
  t.close();
});

test('unexpected close reconnects and re-handshakes with fresh counters', async () => {
  const { t, socks } = harness();
  t.connect();
  const s1 = socks[0];
  s1.serverOpen();
  const h1 = await serverHandshake(s1);
  assert.ok(h1);
  s1.serverClose(1006);
  assert.equal(t.status(), 'reconnecting');
  await sleep(40); // retry period elapsed
  const s2 = socks[1];
  s2.serverOpen();
  const h2 = await serverHandshake(s2);
  assert.ok(h2);
  assert.equal(t.status(), 'connected');
  // fresh session: the client's first sealed frame is counter 0 again
  const first = s2.sent.find((f) => f.startsWith('{"n"'));
  assert.ok(first);
  assert.match(first, /^\{"n":0,/);
  t.close();
});

test('in-flight command is replayed with the ORIGINAL commandId after reconnect', async () => {
  const { t, socks } = harness();
  t.connect();
  const s1 = socks[0];
  s1.serverOpen();
  await serverHandshake(s1);
  const inflight = t.send({ type: 'ping', commandId: 'p-replay' });
  s1.serverClose(1006);
  await sleep(40);
  const s2 = socks[1];
  s2.serverOpen();
  const h2 = await serverHandshake(s2);
  assert.ok(h2);
  const resent = s2.sent
    .map((f) => {
      try {
        return h2.open(f);
      } catch {
        return null;
      }
    })
    .find((p) => p !== null && p.includes('p-replay'));
  assert.ok(resent, 'the pending command must be re-sent on the new connection');
  s2.serverSend(h2.seal(JSON.stringify({ type: 'ack', commandId: 'p-replay', ok: true })));
  const ack = await inflight;
  assert.equal(ack.ok, true);
  t.close();
});

test('a replayed sealed frame tears the connection down; retry recovers', async () => {
  const { t, socks } = harness();
  t.connect();
  const s1 = socks[0];
  s1.serverOpen();
  const h1 = await serverHandshake(s1);
  assert.ok(h1);
  const f = h1.seal(JSON.stringify({ type: 'pong', commandId: 'x' }));
  s1.serverSend(f); // counter 0 — accepted
  s1.serverSend(f); // replayed counter 0 — client must reject and close
  assert.notEqual(s1.closedWith, null, 'violation must close the connection');
  await sleep(40);
  const s2 = socks[1];
  s2.serverOpen();
  const h2 = await serverHandshake(s2);
  assert.ok(h2);
  assert.equal(t.status(), 'connected');
  t.close();
});

test('heartbeat: answered pong keeps the connection, missing pong reconnects', async () => {
  const { t, socks } = harness({ heartbeatMs: 30, pongTimeoutMs: 200 });
  t.connect();
  const s1 = socks[0];
  s1.serverOpen();
  const h1 = await serverHandshake(s1);
  assert.ok(h1);
  h1.open(s1.sent[2]); // consume the sealed hello (client counter 0) first
  await sleep(40); // exactly one heartbeat ping fired
  const pingEnv = s1.sent.slice(3).find((f) => f.startsWith('{"n"'));
  assert.ok(pingEnv, 'heartbeat must send a sealed ping');
  const pingMsg = JSON.parse(h1.open(pingEnv)) as { type: string; commandId: string };
  assert.equal(pingMsg.type, 'ping');
  s1.serverSend(h1.seal(JSON.stringify({ type: 'pong', commandId: pingMsg.commandId })));
  await sleep(80); // well past the ping, well within the pong deadline
  assert.equal(s1.closedWith, null, 'answered heartbeat must keep the connection open');
  assert.equal(t.status(), 'connected');
  t.close();

  // now stop answering: the client must force-close and reconnect
  const b = harness({ heartbeatMs: 10, pongTimeoutMs: 20 });
  b.t.connect();
  const q1 = b.socks[0];
  q1.serverOpen();
  assert.ok(await serverHandshake(q1));
  await sleep(60); // ping sent, no pong → force close
  assert.notEqual(q1.closedWith, null, 'missing pong must force a reconnect');
  await sleep(40);
  assert.equal(b.socks.length, 2, 'a new connection attempt follows');
  b.t.close();
});

test('one subscription, no duplicate events across reconnects', async () => {
  const { t, socks } = harness();
  const seen: string[] = [];
  t.subscribe((m) => seen.push(m.type));
  t.connect();
  const s1 = socks[0];
  s1.serverOpen();
  const h1 = await serverHandshake(s1);
  assert.ok(h1);
  s1.serverSend(h1.seal(JSON.stringify({ type: 'ack', commandId: 'c1', ok: true })));
  await tick();
  s1.serverClose(1006);
  await sleep(40);
  const s2 = socks[1];
  s2.serverOpen();
  const h2 = await serverHandshake(s2);
  assert.ok(h2);
  s2.serverSend(h2.seal(JSON.stringify({ type: 'ack', commandId: 'c2', ok: true })));
  await tick();
  assert.deepEqual(seen, ['ack', 'ack'], 'exactly one event per frame, none duplicated');
  t.close();
});

test('close() fails in-flight commands and stops all retrying', async () => {
  const { t, socks } = harness();
  t.connect();
  const s1 = socks[0];
  s1.serverOpen();
  await serverHandshake(s1);
  const inflight = t.send({ type: 'ping', commandId: 'doomed' });
  t.close();
  await assert.rejects(inflight, /transport closed/);
  await sleep(40);
  assert.equal(socks.length, 1, 'no reconnect after close()');
  assert.equal(t.status(), 'offline');
});

