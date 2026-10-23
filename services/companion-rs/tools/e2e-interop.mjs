/**
 * Cross-language E2E interop (Phase 6, ADR-011/012):
 * spawns the REAL Rust companion binary and drives it with the real TS client
 * (@chernobyl/e2e) over a real WebSocket — the exact wire the phone will use.
 *
 * Scenarios (each prints PASS/FAIL; exit 0 only if all pass):
 *   1. wrong PSK            -> hs_err -> transport 'denied' (no auto-retry)
 *   2. happy path           -> handshake, sealed hello, hello_ack observed,
 *                              heartbeat pongs keep the session alive
 *   3. replayed sealed frame-> server tears the connection down
 *   4. tampered sealed frame-> server tears the connection down
 *   5. redeem second offer  -> second device pairs (single-use offers work)
 *   6. server restart       -> connected transport re-handshakes (stored PSK)
 *   7. revoked device       -> after --revoke + restart: hs_err 'device revoked'
 *      (also asserts today's D4 semantic: revocation takes effect at restart,
 *       not live — documented, deliberately not changed here)
 *
 * No device claim: this is Node-vs-real-companion evidence only.
 */
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocket } from 'ws';
import {
  createE2eTransport,
  parsePairingPayload,
  performHandshake,
} from '../../../packages/e2e/dist/index.js';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const BIN = resolve(HERE, '..', 'target', 'debug', 'chernobyl-companion.exe');
const TOKEN = 'interop-token';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, ms, step = 25) {
  const deadline = Date.now() + ms;
  for (;;) {
    if (fn()) return true;
    if (Date.now() > deadline) return false;
    await sleep(step);
  }
}

let failures = 0;
function check(name, ok, detail = '') {
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
  if (!ok) failures += 1;
}

async function freePort() {
  return new Promise((res, rej) => {
    const s = createServer();
    s.on('error', rej);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => res(port));
    });
  });
}

/* ---------------------------------------------------------------- */
/* process plumbing                                                  */
/* ---------------------------------------------------------------- */

function spawnServer(home, port) {
  const env = { ...process.env };
  delete env.PORT;
  delete env.HOST_TOKEN;
  env.CHERNOBYL_HOME = home;
  const child = spawn(
    BIN,
    ['--bind', '127.0.0.1', '--port', String(port), '--diagnostics-port', '0', '--exit-after', '180000'],
    { env, stdio: ['ignore', 'pipe', 'pipe'] },
  );
  const srv = { child, stderrBuf: '' };
  child.stderr.on('data', (d) => {
    srv.stderrBuf += d.toString();
  });
  child.stdout.resume();
  return srv;
}

async function waitListening(srv, port) {
  const ok = await waitFor(() => srv.stderrBuf.includes(`listening on 127.0.0.1:${port}`), 15_000);
  assert.ok(ok, `server did not report listening; stderr so far:\n${srv.stderrBuf}`);
}

function runPair(home, endpoint) {
  const env = { ...process.env };
  delete env.PORT;
  delete env.HOST_TOKEN;
  env.CHERNOBYL_HOME = home;
  const r = spawnSync(BIN, ['--pair', '--endpoint', endpoint], { env, encoding: 'utf8' });
  assert.equal(r.status, 0, `pair failed: ${r.stderr}`);
  const m = /^payload: (.+)$/m.exec(r.stdout);
  assert.ok(m, `no payload line in pair output:\n${r.stdout}`);
  return parsePairingPayload(m[1]);
}

function revoke(home, deviceId) {
  const env = { ...process.env };
  env.CHERNOBYL_HOME = home;
  const r = spawnSync(BIN, ['--revoke', deviceId], { env, encoding: 'utf8' });
  assert.equal(r.status, 0, `revoke failed: ${r.stderr}`);
}

/* raw-socket handshake helper (for the tear-down scenarios) */
function rawClient(url) {
  const ws = new WebSocket(url);
  const queue = [];
  const waiters = [];
  ws.on('message', (data) => {
    const t = data.toString();
    const w = waiters.shift();
    if (w) w(t);
    else queue.push(t);
  });
  const opened = new Promise((res, rej) => {
    ws.on('open', () => res());
    ws.on('error', (e) => rej(e));
  });
  const closed = new Promise((res) => ws.on('close', (code, reason) => res({ code, reason: reason?.toString() ?? '' })));
  return {
    opened,
    closed,
    send: (t) => ws.send(t),
    recv: () => (queue.length ? Promise.resolve(queue.shift()) : new Promise((r) => waiters.push(r))),
    close: () => ws.close(),
  };
}

async function rawSession(url, cred) {
  const c = rawClient(url);
  await c.opened;
  const session = await performHandshake(
    { send: c.send, recv: c.recv },
    cred,
    { timeoutMs: 5000 },
  ).catch((e) => {
    c.close();
    throw e;
  });
  return { c, session };
}

/* ---------------------------------------------------------------- */
/* main                                                              */
/* ---------------------------------------------------------------- */

async function main() {
  console.log('[interop] building the companion binary…');
  const build = spawnSync('cargo', ['build', '--manifest-path', join(HERE, '..', 'Cargo.toml')], {
    stdio: ['ignore', 'pipe', 'pipe'],
    encoding: 'utf8',
  });
  assert.equal(build.status, 0, `cargo build failed:\n${build.stdout}\n${build.stderr}`);

  const home = mkdtempSync(join(tmpdir(), 'chernobyl-e2e-interop-'));
  const port = await freePort();
  const url = `ws://127.0.0.1:${port}`;

  // Pairing offers must exist in the journal BEFORE the daemon starts
  // (the running store is loaded once at startup — see establish_session).
  const pairA = runPair(home, url);
  const pairB = runPair(home, url);
  check('pair payload carries the redemption device id (ADR-012 dev field)', /^[0-9a-f]{16}$/.test(pairA.deviceId), pairA.deviceId);

  let srv = spawnServer(home, port);
  await waitListening(srv, port);
  console.log(`[interop] server up on ${url} (home ${home})`);

  const t0 = Date.now();
  const transports = [];

  /* 1. wrong PSK -> hs_err -> denied, no auto-retry */
  {
    const bad = { ...pairA, psk: Uint8Array.from(pairA.psk, (b) => b ^ 0xff) };
    const t = createE2eTransport({ url, pairing: bad, retryBaseMs: 200, retryMaxMs: 400 });
    transports.push(t);
    t.connect();
    const denied = await waitFor(() => t.status() === 'denied', 6000);
    check('wrong PSK is terminal (denied, not retried)', denied, `status=${t.status()}`);
    await sleep(500);
    check('wrong PSK did not reconnect in the background', t.status() === 'denied');
    t.close();
  }

  /* 2. happy path: handshake + sealed hello + hello_ack + heartbeat stability */
  let transportA;
  {
    const msgs = [];
    const t = createE2eTransport({
      url,
      pairing: pairA,
      retryBaseMs: 200,
      retryMaxMs: 800,
      heartbeatMs: 500,
      pongTimeoutMs: 2500,
    });
    transports.push(t);
    transportA = t;
    t.subscribe((m) => msgs.push(m.type));
    t.connect();
    const connected = await waitFor(() => t.status() === 'connected', 6000);
    check('real handshake reaches connected', connected, `status=${t.status()}`);
    const gotAck = await waitFor(() => msgs.includes('hello_ack'), 5000);
    check('sealed hello produced hello_ack (server decrypted + replied)', gotAck, msgs.join(','));
    // ~2.5s with 500 ms heartbeats: unanswered pongs would force a reconnect
    await sleep(2500);
    check('heartbeat pongs keep the session alive (no churn)', t.status() === 'connected', `status=${t.status()}`);
  }

  /* 3. replayed sealed frame -> server tears the connection down */
  {
    const { c, session } = await rawSession(url, pairA);
    const hello = session.seal(JSON.stringify({ type: 'ping', commandId: 'replay-probe' }));
    c.send(hello);
    c.send(hello); // exact replay
    const r = await Promise.race([c.closed, sleep(6000).then(() => null)]);
    check('replayed sealed frame tears the connection down', r !== null, r ? `close ${r.code} ${r.reason}` : 'still open after 6s');
    c.close();
  }

  /* 4. tampered sealed frame -> server tears the connection down */
  {
    const { c, session } = await rawSession(url, pairA);
    const hello = session.seal(JSON.stringify({ type: 'ping', commandId: 'tamper-probe' }));
    // flip a character in the middle of the base64 body: {"n":N,"d":"…"}
    const at = 11 + Math.floor((hello.length - 13) / 2);
    const flipped = hello.slice(0, at) + (hello[at] === 'A' ? 'B' : 'A') + hello.slice(at + 1);
    c.send(flipped);
    const r = await Promise.race([c.closed, sleep(6000).then(() => null)]);
    check('tampered sealed frame tears the connection down', r !== null, r ? `close ${r.code} ${r.reason}` : 'still open after 6s');
    c.close();
  }

  /* 5. second device redeems its own offer (single-use, two devices) */
  {
    const t = createE2eTransport({ url, pairing: pairB, retryBaseMs: 200, retryMaxMs: 400 });
    transports.push(t);
    t.connect();
    const connected = await waitFor(() => t.status() === 'connected', 6000);
    check('second device pairs and connects', connected, `status=${t.status()}`);
    t.close();
  }

  /* 6. server restart: device A transport re-handshakes with its stored PSK */
  {
    // transportA is still subscribed/connected from scenario 2
    srv.child.kill();
    await waitFor(() => srv.child.exitCode !== null, 5000);
    srv = spawnServer(home, port);
    await waitListening(srv, port);
    const back = await waitFor(() => transportA.status() === 'connected', 15_000);
    check('transport reconnects through a full server restart', back, `status=${transportA.status()}`);
    transportA.close();
  }

  /* 7. revocation: redeem-check, --revoke, D4 restart semantic, refused handshake */
  {
    // 7a: revoke device B while the server runs (journal write)
    revoke(home, pairB.deviceId);
    // 7b: D4 semantic — today revocation applies at (re)start, not live:
    const tLive = createE2eTransport({ url, pairing: pairB, retryBaseMs: 200, retryMaxMs: 400 });
    transports.push(tLive);
    tLive.connect();
    const stillConnects = await waitFor(() => tLive.status() === 'connected', 6000);
    check('D4 documented semantic: revoke takes effect at restart (still connects live)', stillConnects);
    tLive.close();

    // 7c: after restart the revoked device is refused with hs_err
    srv.child.kill();
    await waitFor(() => srv.child.exitCode !== null, 5000);
    srv = spawnServer(home, port);
    await waitListening(srv, port);
    const tRevoked = createE2eTransport({ url, pairing: pairB, retryBaseMs: 200, retryMaxMs: 400 });
    transports.push(tRevoked);
    tRevoked.connect();
    const denied = await waitFor(() => tRevoked.status() === 'denied', 6000);
    check('revoked device is refused after restart (hs_err, denied)', denied, `status=${tRevoked.status()}`);
    await sleep(500);
    check('revoked device does not auto-retry', tRevoked.status() === 'denied');
    tRevoked.close();
  }

  for (const t of transports) t.close();
  srv.child.kill();
  await sleep(300);
  try {
    rmSync(home, { recursive: true, force: true });
  } catch {
    /* best effort */
  }

  console.log(`[interop] done in ${((Date.now() - t0) / 1000).toFixed(1)}s: ${failures === 0 ? 'ALL PASS' : `${failures} FAILURE(S)`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error('[interop] fatal:', e);
  process.exit(1);
});
