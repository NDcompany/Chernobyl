/**
 * Transport lifecycle over a scripted fake socket, with a mini "server" that
 * performs the server half of the handshake using the same primitives — the
 * exact sequences the real companion produces (server.rs handshake tests).
 * Covers: happy path, mutual-auth failure, hs_err denial, unexpected-close
 * reconnect, in-flight command replay, sealed-violation teardown, heartbeat,
 * single-subscriber semantics across reconnects, and the two-phase device key
 * rotation (hs_rekey → persist → hs_rekey_ack).
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import {
  E2eError,
  SealedSession,
  b64decode,
  b64encode,
  createE2eTransport,
  deriveKeys,
  retryDelayMs,
  handshakeProof,
  performHandshake,
  type HandshakeIo,
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

function harness(opts?: {
  heartbeatMs?: number;
  pongTimeoutMs?: number;
  retryBaseMs?: number;
  retryMaxMs?: number;
  stableMs?: number;
  onRekey?: (pskB64: string) => Promise<void>;
}) {
  const socks: FakeSocket[] = [];
  const t = createE2eTransport({
    url: 'ws://test:8787',
    pairing,
    retryBaseMs: opts?.retryBaseMs ?? 5,
    retryMaxMs: opts?.retryMaxMs ?? 20,
    stableMs: opts?.stableMs,
    heartbeatMs: opts?.heartbeatMs ?? 0,
    pongTimeoutMs: opts?.pongTimeoutMs ?? 50,
    onRekey: opts?.onRekey,
    openSocket: () => {
      const s = new FakeSocket();
      socks.push(s);
      return s;
    },
  });
  return { t, socks };
}

/** The fresh 16-byte client nonce a client's hs_hello carried. */
function cnonceOf(hello: string | undefined): Uint8Array {
  const m = JSON.parse(hello ?? '{}') as { type?: unknown; cnonce?: unknown };
  assert.equal(m.type, 'hs_hello');
  const cnonce = typeof m.cnonce === 'string' ? b64decode(m.cnonce) : null;
  assert.ok(cnonce && cnonce.length === 16, `hs_hello must carry a 16-byte cnonce: ${hello}`);
  return cnonce;
}

/** Server half of the handshake (server.rs semantics), keyed by `psk` (default: the QR's). */
async function serverHandshake(
  sock: FakeSocket,
  opts?: { serverProof?: Uint8Array; err?: string; psk?: Uint8Array },
): Promise<SealedSession | null> {
  const psk = opts?.psk ?? PSK;
  const cnonce = cnonceOf(sock.sent[0]);
  sock.serverSend(JSON.stringify({ type: 'hs_challenge', nonce: b64encode(NONCE), salt: b64encode(SALT) }));
  await tick();
  if (opts?.err !== undefined) {
    sock.serverSend(JSON.stringify({ type: 'hs_err', reason: opts.err }));
    await tick();
    return null;
  }
  const proof = JSON.parse(sock.sent[1] ?? '{}') as { type?: unknown; proof?: unknown };
  assert.equal(proof.type, 'hs_proof');
  assert.equal(proof.proof, b64encode(handshakeProof(psk, 'client', DEVICE, NONCE, SALT, cnonce)), 'client must prove psk');
  const serverProof = opts?.serverProof ?? handshakeProof(psk, 'server', DEVICE, NONCE, SALT, cnonce);
  sock.serverSend(JSON.stringify({ type: 'hs_ok', serverProof: b64encode(serverProof) }));
  await tick();
  return SealedSession.host(deriveKeys(psk, SALT, cnonce));
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
  const wrong = handshakeProof(new Uint8Array(32).fill(8), 'server', DEVICE, NONCE, SALT, cnonceOf(sock.sent[0]));
  sock.serverSend(JSON.stringify({ type: 'hs_ok', serverProof: b64encode(wrong) }));
  await tick();
  assert.equal(t.status(), 'denied');
  assert.notEqual(sock.closedWith, null, 'client must close the socket');
  await sleep(40); // several retry periods (5 ms) — no new socket may appear
  assert.equal(socks.length, 1, 'denied must not auto-retry');
  t.close();
});

test('a replayed server transcript fails as server-proof-mismatch (security review 2026-10-25)', async () => {
  // attempt 1: the genuine host answers; an on-path attacker records its frames
  const sent1: string[] = [];
  const recorded: string[] = [];
  const genuine: HandshakeIo = {
    send: (t) => {
      sent1.push(t);
      const serverProof = handshakeProof(PSK, 'server', DEVICE, NONCE, SALT, cnonceOf(sent1[0]));
      recorded.push(
        JSON.parse(t).type === 'hs_hello'
          ? JSON.stringify({ type: 'hs_challenge', nonce: b64encode(NONCE), salt: b64encode(SALT) })
          : JSON.stringify({ type: 'hs_ok', serverProof: b64encode(serverProof) }),
      );
    },
    recv: async () => recorded[recorded.length - 1], // the reply to the frame just sent
  };
  await performHandshake(genuine, pairing);
  // attempt 2: the attacker answers in the host's place with the recording;
  // the fresh client nonce makes the recorded server proof invalid
  const sent2: string[] = [];
  const replay = [...recorded];
  const attacker: HandshakeIo = { send: (t) => sent2.push(t), recv: async () => replay.shift() ?? '' };
  await assert.rejects(
    performHandshake(attacker, pairing),
    (err) => err instanceof E2eError && err.code === 'server-proof-mismatch',
  );
  assert.notEqual(
    b64encode(cnonceOf(sent2[0])),
    b64encode(cnonceOf(sent1[0])),
    'the client nonce must be fresh per attempt',
  );
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

test('ordered reconnect walks the QR endpoint list in order, restarting each cycle at the primary (ADR-011 §2)', async () => {
  const multi: PairingCredential = { ...pairing, endpoints: ['ws://dead:1', 'ws://live:2'] };
  const dialed: string[] = [];
  const socks: FakeSocket[] = [];
  const t = createE2eTransport({
    pairing: multi,
    retryBaseMs: 5,
    retryMaxMs: 10,
    heartbeatMs: 0,
    handshakeTimeoutMs: 300, // the final un-answered dial must not hold the suite open
    openSocket: (url) => {
      dialed.push(url);
      const s = new FakeSocket();
      socks.push(s);
      if (url === 'ws://dead:1') {
        // dead endpoint: never opens, the dial dies asynchronously
        setTimeout(() => s.serverClose(1006), 0);
      } else {
        setTimeout(() => s.serverOpen(), 0);
      }
      return s;
    },
  });
  // poll (the machine may be loaded — the 24 h soak runs beside the suite);
  // finally-close so a failed assertion can never leave a live retry chain
  const waitFor = async (fn: () => boolean, ms = 2000): Promise<boolean> => {
    const deadline = Date.now() + ms;
    while (!fn()) {
      if (Date.now() > deadline) return false;
      await sleep(5);
    }
    return true;
  };

  try {
    t.connect();
    assert.deepEqual(dialed, ['ws://dead:1'], 'first attempt is the primary endpoint');
    assert.ok(
      await waitFor(() => dialed.length >= 2),
      `failed primary must advance to the second endpoint; dialed=${dialed.join(',')}`,
    );
    assert.equal(
      dialed[1],
      'ws://live:2',
      'a failed attempt advances to the next endpoint in advertised order',
    );
    assert.ok(await waitFor(() => (socks[1]?.sent.length ?? 0) > 0), 'live socket must open');
    const host = await serverHandshake(socks[1]);
    assert.ok(host);
    assert.equal(t.status(), 'connected');

    // the established session drops: the next cycle restarts at the PRIMARY
    // (dead again — one wasted dial — then failover back to the live endpoint)
    socks[1].serverClose();
    assert.ok(
      await waitFor(() => dialed.length >= 3),
      `post-session drop must restart at the primary; dialed=${dialed.join(',')}`,
    );
    assert.equal(dialed[2], 'ws://dead:1', 'after a live session the order restarts from the primary');
    assert.ok(
      await waitFor(() => dialed.length >= 4),
      `cycle must still fail over; dialed=${dialed.join(',')}`,
    );
    assert.equal(dialed[3], 'ws://live:2', 'and still fails over to the second endpoint');
  } finally {
    t.close();
  }
});


test('retryDelayMs: equal jitter inside [cap/2, cap], growing and bounded (audit P5)', () => {
  for (let a = 0; a < 40; a++) {
    for (const r of [0, 0.25, 0.999]) {
      const d = retryDelayMs(a, 1000, 15_000, () => r);
      const cap = Math.min(15_000, 1000 * 2 ** a);
      assert.ok(d >= cap / 2 && d <= cap, `attempt ${a} rand ${r}: ${d}`);
    }
  }
  assert.notEqual(retryDelayMs(3, 1000, 15_000, () => 0), retryDelayMs(3, 1000, 15_000, () => 0.9));
  assert.ok(retryDelayMs(2, 1000, 15_000, () => 0.5) > retryDelayMs(1, 1000, 15_000, () => 0.5));
  assert.equal(retryDelayMs(1e6, 1000, 15_000, () => 1), 15_000);
});

test('a session that drops right after hs_ok keeps its backoff penalty (audit P5)', async () => {
  const { t, socks } = harness({ retryBaseMs: 100, retryMaxMs: 100_000, stableMs: 1e9 });
  try {
    t.connect();
    for (let i = 0; i < 3; i++) {
      const s = socks[i];
      s.serverOpen();
      assert.ok(await serverHandshake(s));
      s.serverClose(1006); // flap: live session, then immediate drop
      const deadline = Date.now() + 3000;
      while (socks.length === i + 1 && i < 2 && Date.now() < deadline) await sleep(5);
    }
    // third retry is attempt 2 → at least 200 ms; a reset-per-handshake
    // backoff would have redialled within 100 ms
    await sleep(190);
    assert.equal(socks.length, 3, 'flapping session must not reset the backoff');
  } finally {
    t.close();
  }
});

test('connect() during a scheduled retry cancels it: no duplicate socket (section 2)', async () => {
  const { t, socks } = harness({ retryBaseMs: 30, retryMaxMs: 30 });
  try {
    t.connect();
    socks[0].serverClose(1006); // retry now scheduled (15-30 ms)
    t.connect(); // e.g. app foregrounded: dial immediately
    assert.equal(socks.length, 2);
    await sleep(80); // well past the old timer
    assert.equal(socks.length, 2, 'the superseded retry must not open a second socket');
    assert.equal(socks[1].closedWith, null, 'the live dial is not orphaned');
  } finally {
    t.close();
  }
});

/* ADR-011 addendum: the companion rotates the QR's PSK to a device key at first use. */
const NEW_PSK = new Uint8Array(32).fill(9);

test('hs_rekey: stored first, acked only after, new key from the next handshake, never surfaced', async () => {
  const stored: string[] = [];
  let release: () => void = () => {};
  const { t, socks } = harness({
    onRekey: (psk) => {
      stored.push(psk);
      return new Promise<void>((r) => {
        release = () => r();
      });
    },
  });
  const seen: string[] = [];
  t.subscribe((m) => seen.push(m.type));
  try {
    t.connect();
    const s1 = socks[0];
    s1.serverOpen();
    const h1 = await serverHandshake(s1);
    assert.ok(h1);
    h1.open(s1.sent[2]); // the sealed hello (client counter 0)
    s1.serverSend(h1.seal(JSON.stringify({ type: 'hs_rekey', psk: b64encode(NEW_PSK) })));
    await tick();
    assert.deepEqual(stored, [b64encode(NEW_PSK)], 'the hook gets the new key');
    assert.equal(s1.sent.length, 3, 'no ack before the key is stored');
    release();
    await tick();
    assert.equal(h1.open(s1.sent[3]), '{"type":"hs_rekey_ack"}');
    assert.deepEqual(seen, [], 'hs_rekey never reaches subscribers');
    // the next connection proves the new key (serverHandshake checks the proof)
    s1.serverClose(1006);
    await sleep(40);
    socks[1].serverOpen();
    assert.ok(await serverHandshake(socks[1], { psk: NEW_PSK }));
    assert.equal(t.status(), 'connected');
  } finally {
    t.close();
  }
});

test('sealed frames delivered in the same tick as hs_ok reach the session in order', async () => {
  // one socket read can carry hs_ok and the frames the companion sends right
  // behind it; they used to stay in the handshake queue, never opened, and the
  // next sealed frame then failed as a counter desync
  const stored: string[] = [];
  const { t, socks } = harness({ onRekey: async (psk) => void stored.push(psk) });
  const seen: string[] = [];
  t.subscribe((m) => seen.push(m.type));
  try {
    t.connect();
    const s = socks[0];
    s.serverOpen();
    const cnonce = cnonceOf(s.sent[0]);
    s.serverSend(JSON.stringify({ type: 'hs_challenge', nonce: b64encode(NONCE), salt: b64encode(SALT) }));
    await tick();
    const host = SealedSession.host(deriveKeys(PSK, SALT, cnonce));
    const serverProof = handshakeProof(PSK, 'server', DEVICE, NONCE, SALT, cnonce);
    s.serverSend(JSON.stringify({ type: 'hs_ok', serverProof: b64encode(serverProof) }));
    s.serverSend(host.seal(JSON.stringify({ type: 'hs_rekey', psk: b64encode(NEW_PSK) })));
    s.serverSend(host.seal(JSON.stringify({ type: 'ack', commandId: 'c1', ok: true })));
    await tick();
    assert.equal(t.status(), 'connected');
    assert.deepEqual(stored, [b64encode(NEW_PSK)], 'hs_rekey was opened');
    assert.deepEqual(seen, ['ack'], 'the frame behind it was delivered');
    s.serverSend(host.seal(JSON.stringify({ type: 'ack', commandId: 'c2', ok: true })));
    assert.equal(s.closedWith, null, 'counters stay in step: no desync teardown');
    assert.deepEqual(seen, ['ack', 'ack']);
  } finally {
    t.close();
  }
});

test('hs_rekey: a key that could not be stored is not acked and the old key stays', async () => {
  const { t, socks } = harness({ onRekey: () => Promise.reject(new Error('keystore write failed')) });
  try {
    t.connect();
    const s1 = socks[0];
    s1.serverOpen();
    const h1 = await serverHandshake(s1);
    assert.ok(h1);
    s1.serverSend(h1.seal(JSON.stringify({ type: 'hs_rekey', psk: b64encode(NEW_PSK) })));
    await sleep(5);
    assert.equal(s1.sent.length, 3, 'no ack for an unstored key');
    assert.equal(s1.closedWith, null, 'the session stays up');
    s1.serverClose(1006);
    await sleep(40);
    socks[1].serverOpen();
    assert.ok(await serverHandshake(socks[1]), 'the QR key still proves');
    assert.equal(t.status(), 'connected');
  } finally {
    t.close();
  }
});

for (const code of [4001, 4002, 4003]) {
  test(`server close ${code} is terminal: denied, in-flight failed, no retry`, async () => {
    const { t, socks } = harness();
    try {
      t.connect();
      socks[0].serverOpen();
      assert.ok(await serverHandshake(socks[0]));
      const inflight = t.send({ type: 'ping', commandId: `c${code}` });
      socks[0].serverClose(code);
      await assert.rejects(inflight);
      assert.equal(t.status(), 'denied');
      await sleep(40); // several retry periods
      assert.equal(socks.length, 1, `close ${code} must not auto-retry`);
    } finally {
      t.close();
    }
  });
}
