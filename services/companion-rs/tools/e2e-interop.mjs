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
 *   7. revoked device       -> --revoke against the RUNNING companion closes the
 *      live session with 4003 (transport 'denied', D4/ADR-014); after a
 *      restart the device is still refused (hs_err 'device revoked')
 *   8. S10 ordered endpoints -> multi --endpoint payload keeps CLI order, and
 *      the client fails over from a dead first endpoint to the live one
 *   9. client freshness (security review 2026-10-25) -> an hs_hello without a
 *      client nonce gets hs_err, and a transcript recorded from the real
 *      companion replayed to a new client attempt fails server-proof-mismatch
 *  10. device key rotation at first use (ADR-011 addendum, checklist row 2.2)
 *      -> a client that ignores hs_rekey keeps connecting on the QR key; once
 *      a client stores the rotated key and acks, the QR key is refused and
 *      the rotated key connects
 *  11. ADR-015 live transcripts (step 10) -> a second companion whose
 *      companion.json configures the `codex` adapter against the fake
 *      app-server (tools/fake-codex-app-server.mjs, real spawn path):
 *      session_subscribe gets a snapshot then live ops, every frame sealed;
 *      a reconnect with the cursor replays with no gap or duplicate; a stale
 *      cursor gets a snapshot; an inline decision by optionId + revision is
 *      accepted and a stale revision refused; a client that never subscribes
 *      receives zero transcript frames; the fake logs no protocol violation;
 *      with `terminal.enabled` a sealed terminal_run streams its output to
 *      the starting client only, then terminal_exit (ADR-020)
 *
 * No device claim: this is Node-vs-real-companion evidence only.
 */
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocket } from 'ws';
import {
  b64decode,
  createE2eTransport,
  parsePairingPayload,
  performHandshake,
} from '../../../packages/e2e/dist/index.js';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const BIN = resolve(HERE, '..', 'target', 'debug', 'chernobyl-companion.exe');
const TOKEN = 'interop-token-0123456789abcdef0123456789abcdef';
const FAKE_CODEX = resolve(HERE, 'fake-codex-app-server.mjs');

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

/** A free port in 1024..9999 (4 digits) — keeps a two-endpoint QR payload inside QR v10-M's 213-byte cap. */
async function freePort4() {
  for (let i = 0; i < 40; i += 1) {
    const p = 1024 + Math.floor(Math.random() * 8976);
    const ok = await new Promise((res) => {
      const s = createServer();
      s.on('error', () => res(false));
      s.listen(p, '127.0.0.1', () => s.close(() => res(true)));
    });
    if (ok) return p;
  }
  throw new Error('no free 4-digit port available');
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

function runPair(home, { endpoints, expectQrOverflow = false } = {}) {
  const env = { ...process.env };
  delete env.PORT;
  delete env.HOST_TOKEN;
  env.CHERNOBYL_HOME = home;
  const args = ['--pair', ...endpoints.flatMap((e) => ['--endpoint', e])];
  const r = spawnSync(BIN, args, { env, encoding: 'utf8' });
  const m = /^payload: (.+)$/m.exec(r.stdout);
  assert.ok(m, `no payload line in pair output:\n${r.stdout}\n${r.stderr}`);
  if (expectQrOverflow) {
    // documented: payload prints first, QR failure is reported (exit 1) so
    // the operator knows to paste instead of scan
    assert.equal(r.status, 1, `QR overflow expected (exit 1) but got ${r.status}: ${r.stderr}`);
    assert.match(r.stderr, /too large for QR/, 'QR failure must be reported');
  } else {
    assert.equal(r.status, 0, `pair failed: ${r.stderr}`);
  }
  return parsePairingPayload(m[1]);
}

/** --revoke routed to the running server's diagnostics listener (D4). */
function revoke(home, deviceId, srv) {
  const env = { ...process.env };
  env.CHERNOBYL_HOME = home;
  const m = /diagnostics on 127\.0\.0\.1:(\d+)/.exec(srv.stderrBuf);
  assert.ok(m, `no diagnostics port in server log:
${srv.stderrBuf}`);
  const r = spawnSync(BIN, ['--diagnostics-port', m[1], '--revoke', deviceId], { env, encoding: 'utf8' });
  assert.equal(r.status, 0, `revoke failed: ${r.stderr}`);
  return r.stdout;
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
  // 4-digit server port: two `ws://127.0.0.1:PORT` entries in one QR payload
  // then land at exactly 213 bytes = QR v10-M capacity (see pair checks below)
  const port = await freePort4();
  const url = `ws://127.0.0.1:${port}`;

  // Pairing offers must exist in the journal BEFORE the daemon starts
  // (the running store is loaded once at startup — see establish_session).
  const pairA = runPair(home, { endpoints: [url] });
  const pairB = runPair(home, { endpoints: [url] });
  const pairD = runPair(home, { endpoints: [url] }); // scenario 10 (key rotation)
  // S10: dead endpoint FIRST (a second 4-digit port — two 4-digit entries =
  // exactly 213 bytes = QR v10-M capacity), live second. Distinct from the
  // server port or the "dead" dial would hit the live daemon.
  let deadPort = await freePort4();
  while (deadPort === port) deadPort = await freePort4();
  const deadUrl = `ws://127.0.0.1:${deadPort}`;
  const pairC = runPair(home, { endpoints: [deadUrl, url] });
  check('pair payload carries the redemption device id (ADR-012 dev field)', /^[0-9a-f]{16}$/.test(pairA.deviceId), pairA.deviceId);
  check(
    'S10: multi-endpoint QR payload keeps CLI order (dead, live)',
    JSON.stringify(pairC.endpoints) === JSON.stringify([deadUrl, url]),
    pairC.endpoints.join(', '),
  );
  // Two full endpoints (LAN + tunnel, ~250 bytes) fit since the QR grew to
  // v15-M (412 bytes)
  const pairTwo = runPair(home, { endpoints: [url, `ws://127.0.0.1:${await freePort()}`] });
  check('S10: two endpoints still render a QR', pairTwo.endpoints.length === 2, pairTwo.endpoints.join(', '));
  // Capacity edge: past 412 bytes the payload must still print for paste, QR
  // failure reported
  const longHost = `wss://${['a', 'b', 'c', 'd'].map((x) => x.repeat(60)).join('.')}.example.com`;
  const pairOver = runPair(home, { endpoints: [url, longHost], expectQrOverflow: true });
  check(
    'S10: QR overflow keeps a pasteable payload with endpoint order intact',
    pairOver.endpoints.length === 2 && pairOver.endpoints[0] === url,
    pairOver.endpoints.join(', '),
  );

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
    // ~2.5s with 500 ms heartbeats: count every status change, not one
    // sample — a session that dies and silently re-handshakes between two
    // samples once hid a nonce-layout bug that killed it on frame 2
    const changes = [];
    const unsubChurn = t.subscribeStatus((s) => changes.push(s));
    changes.length = 0; // drop the immediate replay of the current status
    await sleep(2500);
    unsubChurn();
    check(
      'heartbeat pongs keep the session alive (no churn)',
      t.status() === 'connected' && changes.length === 0,
      `status=${t.status()} changes=[${changes.join(',')}]`,
    );
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
    // 7a: device B holds a live session
    const tLive = createE2eTransport({ url, pairing: pairB, retryBaseMs: 200, retryMaxMs: 400 });
    transports.push(tLive);
    tLive.connect();
    check('device B is live before revocation', await waitFor(() => tLive.status() === 'connected', 6000));
    // 7b: --revoke reaches the running companion, which closes B's session (4003)
    const out = revoke(home, pairB.deviceId, srv);
    check('--revoke used the running companion', /via running companion/.test(out), out.trim());
    const cut = await waitFor(() => tLive.status() === 'denied', 6000);
    check('D4: revoked live session is terminated (4003, denied)', cut, `status=${tLive.status()}`);
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

  /* 8. S10: ordered failover across the advertised endpoint list (real binary) */
  {
    const t = createE2eTransport({
      pairing: pairC, // defaults to the payload's own ordered list (dead, live)
      retryBaseMs: 200,
      retryMaxMs: 400,
      heartbeatMs: 0,
      handshakeTimeoutMs: 1500, // a random 4-digit port that answers as a non-companion service rotates quickly
    });
    transports.push(t);
    t.connect();
    const connected = await waitFor(() => t.status() === 'connected', 8000);
    check(
      'S10: dead first endpoint fails over to the live second endpoint',
      connected,
      `status=${t.status()}`,
    );
    await sleep(600);
    check('S10: session stays on the live endpoint (no churn)', t.status() === 'connected', `status=${t.status()}`);
    t.close();
  }

  /* 9. client freshness (security review 2026-10-25) */
  {
    const legacy = rawClient(url);
    await legacy.opened;
    legacy.send(JSON.stringify({ type: 'hs_hello', deviceId: pairA.deviceId }));
    const reply = JSON.parse(await legacy.recv());
    check('hs_hello without a client nonce is refused (hs_err)', reply.type === 'hs_err', JSON.stringify(reply));
    legacy.close();

    // record a real handshake's server frames, then replay them to a new attempt
    const live = rawClient(url);
    await live.opened;
    const recorded = [];
    const recv = async () => {
      const t = await live.recv();
      recorded.push(t);
      return t;
    };
    await performHandshake({ send: live.send, recv }, pairA, { timeoutMs: 5000 });
    live.close();
    const err = await performHandshake({ send: () => {}, recv: async () => recorded.shift() ?? '' }, pairA, {
      timeoutMs: 2000,
    }).then(
      () => null,
      (e) => e,
    );
    check(
      'replayed real server transcript fails mutual auth (server-proof-mismatch)',
      err?.code === 'server-proof-mismatch',
      err ? `${err.code}: ${err.message}` : 'replay was accepted',
    );
  }

  /* 10. device key rotation at first use (ADR-011 addendum, checklist row 2.2) */
  {
    // a client without onRekey drops hs_rekey and never acks: the QR key stays valid
    for (const round of [1, 2]) {
      const old = createE2eTransport({ url, pairing: pairD, retryBaseMs: 200, retryMaxMs: 400 });
      transports.push(old);
      old.connect();
      const ok = await waitFor(() => old.status() === 'connected', 6000);
      check(`rotation: a client that ignores hs_rekey keeps the QR key (connection ${round})`, ok, `status=${old.status()}`);
      old.close();
    }
    let stored = null;
    const t = createE2eTransport({
      url,
      pairing: pairD,
      retryBaseMs: 200,
      retryMaxMs: 400,
      onRekey: async (psk) => {
        stored = psk;
      },
    });
    transports.push(t);
    t.connect();
    check('rotation: a rotating client is offered and stores a new key', await waitFor(() => stored !== null, 6000));
    // frames are handled in order: this command's ack proves the hs_rekey_ack
    // the transport sent before it was processed
    const barrier = await t.send({ type: 'interrupt', commandId: 'rekey-barrier', sessionId: 'none' }).then(
      () => true,
      () => false,
    );
    t.close();
    const handshake = (cred) =>
      rawSession(url, cred).then(
        ({ c }) => {
          c.close();
          return 'connected';
        },
        (e) => e?.code ?? String(e),
      );
    const replay = await handshake(pairD);
    check('rotation: the QR key is refused once the rotated key was acked (row 2.2)', barrier && replay === 'handshake-rejected', `barrier=${barrier} replay=${replay}`);
    const rotated = stored ? await handshake({ ...pairD, psk: b64decode(stored) }) : 'no key';
    check('rotation: the rotated key connects', rotated === 'connected', rotated);
  }

  /* 11. ADR-015 live transcripts against the Codex runtime (fake app-server) */
  {
    const home2 = mkdtempSync(join(tmpdir(), 'chernobyl-e2e-codex-'));
    const ws = mkdtempSync(join(tmpdir(), 'chernobyl-e2e-codex-ws-'));
    const fakeLog = join(home2, 'fake-codex.log');
    writeFileSync(
      join(home2, 'companion.json'),
      JSON.stringify({
        version: 1,
        name: 'interop codex',
        dataDir: home2,
        adapters: [
          {
            id: 'codex-1',
            type: 'codex',
            enabled: true,
            workspaces: [ws],
            binary: process.execPath,
            args: [FAKE_CODEX, '--transcript', fakeLog],
          },
        ],
        retention: { events: 5000, eventsMaxAgeMs: 604800000 },
        diagnostics: { port: 0 },
        probeIntervalMs: 15000,
        terminal: { enabled: true },
      }),
    );
    const port2 = await freePort();
    const url2 = `ws://127.0.0.1:${port2}`;
    const pairLive = runPair(home2, { endpoints: [url2] });
    const srv2 = spawnServer(home2, port2);
    await waitListening(srv2, port2);
    const live = [];
    const open = (onMsg) => {
      const t = createE2eTransport({ url: url2, pairing: pairLive, retryBaseMs: 200, retryMaxMs: 400 });
      live.push(t);
      transports.push(t);
      t.subscribe(onMsg);
      t.connect();
      return t;
    };
    const isTranscript = (m) => m.type === 'transcript' || m.type === 'transcript_snapshot';

    // a client that never subscribes: events yes, transcript frames never
    const observed = [];
    const observer = open((m) => observed.push(m));
    // the main client
    const msgs = [];
    const a = open((m) => msgs.push(m));
    check(
      'codex: clients connect',
      await waitFor(() => a.status() === 'connected' && observer.status() === 'connected', 8000),
    );
    await waitFor(() => msgs.some((m) => m.type === 'hello_ack'), 5000);
    const helloAck = msgs.find((m) => m.type === 'hello_ack');
    const agent = helloAck?.snapshot.agents.find((x) => x.id === 'codex-1');
    const wsId = agent?.live?.workspaces?.[0]?.id;
    check(
      'codex: hello_ack advertises transcript.v1 and the codex agent with its workspace',
      (helloAck?.features ?? []).includes('transcript.v1') && agent?.live?.promptWhileBusy === 'merge' && typeof wsId === 'string',
      JSON.stringify(agent?.live ?? null),
    );

    // ADR-016: a sealed workspace read answers with its result, then the ack
    writeFileSync(join(ws, '.env'), 'API_KEY=interop-secret-value\n');
    const lsAck = await a.send({ type: 'fs_list', commandId: 'ws-ls', workspaceId: wsId });
    const lsAt = msgs.findIndex((m) => m.type === 'workspace_result' && m.commandId === 'ws-ls');
    const lsAckAt = msgs.findIndex((m) => m.type === 'ack' && m.commandId === 'ws-ls');
    const listing = msgs[lsAt]?.result;
    check(
      'workspace: hello_ack advertises workspace.v1; fs_list answers with the listing before its ack',
      (helloAck?.features ?? []).includes('workspace.v1') &&
        lsAck.ok &&
        lsAt >= 0 &&
        lsAt < lsAckAt &&
        listing?.entries?.some((e) => e.name === '.env'),
      JSON.stringify(listing ?? null),
    );
    await a.send({ type: 'fs_read', commandId: 'ws-env', workspaceId: wsId, path: '.env' });
    const env = msgs.find((m) => m.type === 'workspace_result' && m.commandId === 'ws-env')?.result;
    check(
      'workspace: a .env file reaches the phone masked',
      env?.sensitive === true && !env.content.includes('interop-secret-value'),
      JSON.stringify(env ?? null),
    );
    const escape = await a.send({ type: 'fs_read', commandId: 'ws-esc', workspaceId: wsId, path: '../x' });
    const notRepo = await a.send({ type: 'git_status', commandId: 'ws-git', workspaceId: wsId });
    check(
      'workspace: escapes are refused; a folder without git says so',
      !escape.ok && !notRepo.ok && notRepo.error === 'not a git repository',
      `${escape.error} | ${notRepo.error}`,
    );
    check(
      'workspace: an observer connection never sees another client\'s results',
      !observed.some((m) => m.type === 'workspace_result'),
    );

    // ADR-020: the opt-in terminal, sealed like everything else
    const termAck = await a.send({ type: 'terminal_run', commandId: 'term-1', workspaceId: wsId, command: 'echo interop-term' });
    await waitFor(() => msgs.some((m) => m.type === 'command_result' && m.commandId === 'term-1'), 10000);
    const termOut = msgs
      .filter((m) => m.type === 'terminal_output' && m.commandId === 'term-1')
      .map((m) => m.data)
      .join('');
    const termExit = msgs.findIndex((m) => m.type === 'terminal_exit' && m.commandId === 'term-1');
    const termResult = msgs.findIndex((m) => m.type === 'command_result' && m.commandId === 'term-1');
    check(
      'terminal: advertised when enabled; a sealed run streams its output, exit code 0, then its command_result; observers see no output',
      (helloAck?.features ?? []).includes('terminal.v1') &&
        termAck.ok &&
        termOut.includes('interop-term') &&
        termExit >= 0 &&
        msgs[termExit].exitCode === 0 &&
        termExit < termResult &&
        msgs[termResult].status === 'completed' &&
        !observed.some((m) => m.type === 'terminal_output' || m.type === 'terminal_exit'),
      `${termAck.error ?? ''} | ${JSON.stringify(termOut)} | ${termExit} < ${termResult}`,
    );

    const created = await a.send({
      type: 'create_session',
      commandId: 'cx-create',
      agentId: 'codex-1',
      title: 'interop',
      prompt: 'run edit slow',
      project: wsId,
    });
    const sid = created.sessionId;
    check('codex: create_session acks with the thread as session id', created.ok && typeof sid === 'string', JSON.stringify(created));

    // follows one session's stream the way applyTranscript does: any gap,
    // duplicate or epoch change is recorded
    const follow = (out) => {
      const view = { epoch: null, seq: null, snapshots: 0, batches: 0, first: null, problems: [] };
      out.view = view;
      return (m) => {
        if (!isTranscript(m) || m.sessionId !== sid) return;
        view.first ??= m.type;
        if (m.type === 'transcript_snapshot') {
          Object.assign(view, { epoch: m.epoch, seq: m.seq, snapshots: view.snapshots + 1 });
          return;
        }
        view.batches += 1;
        if (m.epoch !== view.epoch) view.problems.push(`epoch ${m.epoch} != ${view.epoch}`);
        for (const op of m.ops) {
          if (op.seq !== view.seq + 1) view.problems.push(`seq ${op.seq} after ${view.seq}`);
          view.seq = op.seq;
        }
      };
    };
    const s1 = {};
    a.subscribe(follow(s1));
    const sub = await a.send({ type: 'session_subscribe', commandId: 'cx-sub-1', sessionId: sid });
    check('codex: session_subscribe is acked', sub.ok, JSON.stringify(sub));
    const snapshotFirst = await waitFor(() => s1.view.first === 'transcript_snapshot', 8000);

    // inline approvals: a stale revision is refused, optionId + revision accepted
    const requests = () => msgs.filter((m) => m.type === 'permission_requested' && m.permission.sessionId === sid);
    check('codex: the command approval arrives', await waitFor(() => requests().length >= 1, 8000));
    const p1 = requests()[0].permission;
    const stale = await a.send({
      type: 'permission_decision',
      commandId: 'cx-d0',
      permissionId: p1.id,
      decision: 'approve-once',
      optionId: 'accept',
      revision: p1.revision - 1,
    });
    check('codex: a decision on a stale revision is refused', stale.ok === false, JSON.stringify(stale));
    const d1 = await a.send({
      type: 'permission_decision',
      commandId: 'cx-d1',
      permissionId: p1.id,
      decision: 'approve-once',
      optionId: 'accept',
      revision: p1.revision,
    });
    check(
      'codex: an inline decision by optionId + revision is accepted',
      d1.ok && (p1.options ?? []).some((o) => o.id === 'accept') && typeof p1.toolItemId === 'string',
      JSON.stringify(d1),
    );
    check('codex: the file-change approval arrives', await waitFor(() => requests().length >= 2, 8000));
    const p2 = requests()[1].permission;
    const d2 = await a.send({
      type: 'permission_decision',
      commandId: 'cx-d2',
      permissionId: p2.id,
      decision: 'approve-session',
      optionId: 'acceptForSession',
      revision: p2.revision,
    });
    check('codex: a session-scoped option is accepted', d2.ok, JSON.stringify(d2));
    // the turn goes on past the approvals: live ops follow the snapshot
    check(
      'codex: subscribe gets a snapshot, then contiguous live ops',
      snapshotFirst && (await waitFor(() => s1.view.batches > 0, 8000)) && s1.view.problems.length === 0,
      JSON.stringify(s1.view),
    );

    // every frame on the wire is sealed (a raw client of the same device)
    {
      const { c, session } = await rawSession(url2, pairLive);
      c.send(session.seal(JSON.stringify({ type: 'session_subscribe', commandId: 'cx-raw', sessionId: sid })));
      const frames = [];
      const deadline = Date.now() + 1500;
      while (Date.now() < deadline) {
        const t = await Promise.race([c.recv(), sleep(deadline - Date.now()).then(() => null)]);
        if (t === null) break;
        frames.push(t);
      }
      c.close();
      let sealed = 0;
      let transcripts = 0;
      for (const f of frames) {
        try {
          if (!/^\{"n":\d+,"d":"/.test(f)) continue;
          const m = JSON.parse(session.open(f));
          sealed += 1;
          if (isTranscript(m)) transcripts += 1;
        } catch {
          /* not sealed, or not ours */
        }
      }
      check(
        'codex: every transcript frame is sealed',
        frames.length > 0 && sealed === frames.length && transcripts > 0,
        `frames=${frames.length} sealed=${sealed} transcript=${transcripts}`,
      );
    }

    // reconnect with the cursor: replay with no gap or duplicate
    const cursor = { epoch: s1.view.epoch, seq: s1.view.seq };
    a.close();
    await sleep(800); // the slow reply keeps streaming meanwhile
    const msgs2 = [];
    const s2 = {};
    const b = open((m) => msgs2.push(m));
    b.subscribe(follow(s2));
    s2.view.epoch = cursor.epoch;
    s2.view.seq = cursor.seq;
    await waitFor(() => b.status() === 'connected', 8000);
    const resub = await b.send({
      type: 'session_subscribe',
      commandId: 'cx-sub-2',
      sessionId: sid,
      epoch: cursor.epoch,
      afterSeq: cursor.seq,
    });
    const again = await b.send({ type: 'submit_prompt', commandId: 'cx-p1', sessionId: sid, text: 'hello again' });
    check(
      'codex: reconnect with the cursor replays with no gap or duplicate',
      resub.ok &&
        again.ok &&
        (await waitFor(() => s2.view.batches > 0, 8000)) &&
        s2.view.first === 'transcript' &&
        s2.view.snapshots === 0 &&
        s2.view.problems.length === 0,
      JSON.stringify({ cursor, ...s2.view }),
    );
    const results = () => [...msgs, ...msgs2].filter((m) => m.type === 'command_result');
    check(
      'codex: the session prompt and the steered follow-up complete',
      await waitFor(
        () => ['cx-create', 'cx-p1'].every((id) => results().some((m) => m.commandId === id && m.status === 'completed')),
        15_000,
      ),
      JSON.stringify(results().map((m) => [m.commandId, m.status])),
    );

    // a stale cursor gets a snapshot
    const s3 = {};
    const c3 = open(follow(s3));
    await waitFor(() => c3.status() === 'connected', 8000);
    const resub3 = await c3.send({
      type: 'session_subscribe',
      commandId: 'cx-sub-3',
      sessionId: sid,
      epoch: 'not-the-epoch',
      afterSeq: 1,
    });
    check(
      'codex: a stale cursor gets a snapshot',
      resub3.ok && (await waitFor(() => s3.view.first === 'transcript_snapshot', 5000)),
      JSON.stringify(s3.view),
    );

    check(
      'codex: a client that never subscribes receives zero transcript frames',
      observed.some((m) => m.type === 'permission_requested') && !observed.some(isTranscript),
      `messages=${observed.length}`,
    );
    const violations = readFileSync(fakeLog, 'utf8')
      .split('\n')
      .filter((l) => l.includes(' !! '));
    check('codex: the fake app-server logged no protocol violation', violations.length === 0, violations.join(' | '));

    for (const t of live) t.close();
    srv2.child.kill();
    await sleep(300);
    for (const dir of [home2, ws]) {
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch {
        /* best effort: the fake may hold its log for a moment */
      }
    }
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
