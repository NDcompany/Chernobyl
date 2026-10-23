#!/usr/bin/env node
// soak.mjs — CHERNOBYL companion soak-test harness (repeatability over time).
//
// Spawns the REAL companion binary (loopback, token auth, isolated temp
// home), drives it through lifecycle stress phases, samples resources, and
// writes timestamped JSONL events plus a machine-readable summary.json.
//
//   node services/companion-rs/tools/soak.mjs                      # smoke (~2-3 min)
//   node services/companion-rs/tools/soak.mjs --mode long --duration 86400
//   node services/companion-rs/tools/soak.mjs --inject server-kill  # failure-detection demo
//   npm run soak:smoke / npm run soak:long                          # from repo root
//
// Options (defaults in brackets):
//   --mode smoke|long        [smoke]  smoke = one full pass; long = pass + load loop
//   --duration <seconds>     [long: 86400] wall-clock cap for mode long
//   --clients <n>            [smoke 5, long 10] FIFO-phase concurrency
//   --churn-cycles <n>       [smoke 10] connect/disconnect cycles (long: per-loop lite)
//   --out <dir>              [services/companion-rs/target/soak/<ts>-<mode>] artifacts
//   --inject server-kill     []       induce a fault to prove detection works
//   --progress-secs <n>      [smoke 10, long 60] progress line cadence
//
// PASS/FAIL criteria — evaluated from observations, never from "the process
// was still alive". Any failure ⇒ exit 1:
//   1. unexpected_server_exit  — server exited without a harness-requested stop
//   2. ordering_violation      — a pong arrived out of order / missing / wrong id
//   3. unauthorized_accepted   — a wrong-token client got any data (must get none)
//   4. peer_leak               — /health clients != 0 after a phase that must
//                                release every peer (churn, saturation, dead-peer,
//                                writer-failure, final quiesce)
//   5. d8_saturation_teardown  — a stopped-consuming client stayed registered after
//                                the reply flood (bounded-queue regression)
//   6. dead_peer_not_released  — a silent paused peer survived past the heartbeat
//                                bound (HEARTBEAT_MS 15s x MISS_LIMIT 3 + slack)
//   7. graceful_shutdown_failed— --exit-after instance exited non-zero or hung
//   8. restart_recovery_failed — respawned server failed to serve reconnecting clients
//   9. resource_growth         — quiesced RSS above the limits below
//  10. connection_failure      — an expected connect/hello/pong that did not happen
//
// Threshold provenance (stated in summary.thresholds):
//   EXISTING LIMITS (from source, not invented):
//     OUTBOUND_QUEUE_MAX_MESSAGES=256, OUTBOUND_QUEUE_MAX_BYTES=4 MiB,
//     WRITE_TIMEOUT=10s, HEARTBEAT_MS=15s, HEARTBEAT_MISS_LIMIT=3,
//     AUTH_FAILURE_LIMIT=5, /health rssBytes (server self-report)
//   PROPOSED (new for the soak; documented for review):
//     rssGrowthLimitBytes = 64 MiB   (quiesced growth over baseline; absorbs
//                                     allocator retention — a leak that matters
//                                     shows far more over a long run)
//     rssAbsoluteLimitBytes = 512 MiB (any time; catches runaway independent
//                                     of baseline)
//     releaseTimeouts: normal 30s, saturation 120s, dead-peer 150s (each above
//                                     the source-derived teardown bound so a
//                                     timeout means "stuck", not "slow")
//
// Honesty notes: this is a loopback harness — it does NOT simulate a phone,
// does NOT exercise E2E-sealed sessions (the Rust e2e matrix covers those),
// does NOT bind beyond loopback (LAN exposure is forbidden), and mode labels
// are literal: a run ends when its real duration has elapsed.

import { WebSocket } from 'ws';
import {
  spawn,
  spawnSync,
} from 'node:child_process';
import {
  appendFileSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/* ------------------------------------------------------------------ */
/* config                                                              */
/* ------------------------------------------------------------------ */

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, '..', '..', '..');
const CARGO_TOML = join(REPO, 'services', 'companion-rs', 'Cargo.toml');
const BIN =
  join(REPO, 'services', 'companion-rs', 'target', 'debug',
    'chernobyl-companion' + (process.platform === 'win32' ? '.exe' : ''));

const argv = process.argv.slice(2);
function flag(name, def) {
  // last occurrence wins, so a later flag can override an earlier default
  const key = '--' + name;
  for (let i = argv.length - 1; i >= 0; i--) {
    if (argv[i] === key && i + 1 < argv.length) return argv[i + 1];
  }
  return def;
}
const MODE = flag('mode', 'smoke');
const LONG = MODE === 'long';
if (MODE !== 'smoke' && MODE !== 'long') {
  console.error(`unknown --mode ${MODE} (smoke|long)`);
  process.exit(2);
}
const DURATION_S = Number(flag('duration', LONG ? 86400 : 0));
const CLIENTS = Number(flag('clients', LONG ? 10 : 5));
const CHURN_CYCLES = Number(flag('churn-cycles', LONG ? 40 : 10));
const INJECT = flag('inject', '');
const PROGRESS_S = Number(flag('progress-secs', LONG ? 60 : 10));
const RUN_STAMP = new Date().toISOString().replace(/[:.]/g, '-');
const OUT = flag('out', join(REPO, 'services', 'companion-rs', 'target', 'soak', `${RUN_STAMP}-${MODE}`));
const TOKEN = 'soak-secret';
const PROTOCOL_VERSION = 2; // confirmed from /health at boot

const LIMITS = {
  rssGrowthLimitBytes: 64 * 1024 * 1024, // PROPOSED
  rssAbsoluteLimitBytes: 512 * 1024 * 1024, // PROPOSED
  releaseNormalMs: 30_000, // PROPOSED (above source-derived bounds)
  releaseSaturationMs: 120_000, // PROPOSED
  releaseDeadPeerMs: 150_000, // PROPOSED (4+ beats of 15s + slack)
  outboundMaxMessages: 256, // EXISTING (server.rs)
  outboundMaxBytes: 4 * 1024 * 1024, // EXISTING (server.rs)
  authFailureLimit: 5, // EXISTING (server.rs)
  heartbeatMs: 15_000, // EXISTING (protocol.rs)
  heartbeatMissLimit: 3, // EXISTING (protocol.rs)
};

const T0 = Date.now();
let ABORT = false;
const failures = [];
const counts = {
  connectsCompleted: 0,
  connectsFailed: 0,
  pongsExpected: 0,
  pongsReceived: 0,
  orderingViolations: 0,
  unauthorizedOutcomes: {},
  disconnectReasons: {},
  teardowns: [],
};
const serverLog = { unexpectedExits: [], requestedStops: [], graceful: null, restart: null };
const samples = []; // {t, rss, cpuS|null}
let rssBaseline = null;
let rssPeak = null;
let rssFinal = null;

/* ------------------------------------------------------------------ */
/* small utilities                                                     */
/* ------------------------------------------------------------------ */

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const elapsedS = () => Math.round((Date.now() - T0) / 100) / 10;

function ev(phase, event, fields = {}) {
  appendFileSync(
    join(OUT, 'events.jsonl'),
    JSON.stringify({ ts: new Date().toISOString(), t: Date.now(), phase, event, ...fields }) + '\n',
  );
}

function fail(phase, code, detail) {
  failures.push({ phase, code, detail: String(detail) });
  ev(phase, 'FAIL', { code, detail: String(detail) });
  console.error(`  FAIL [${phase}] ${code}: ${detail}`);
}

function progress(msg) {
  console.log(`[t+${elapsedS()}s] ${msg}`);
}

class SoakErr extends Error {
  constructor(kind, message, extra = {}) {
    super(message);
    this.kind = kind;
    Object.assign(this, extra);
  }
}

function withTimeout(promise, ms, what) {
  let timer;
  return Promise.race([
    promise.finally(() => clearTimeout(timer)),
    new Promise((_, rej) => {
      timer = setTimeout(() => rej(new SoakErr('timeout', `${what} timed out after ${ms}ms`)), ms);
    }),
  ]);
}

async function pollUntil(fn, timeoutMs, intervalMs = 250) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    if (ABORT) throw new SoakErr('aborted', 'run aborted');
    if (await fn()) return true;
    if (Date.now() > end) return false;
    await sleep(intervalMs);
  }
}

async function runPhase(name, fn) {
  if (ABORT) return null;
  ev(name, 'phase_start');
  progress(`phase ${name} …`);
  const start = Date.now();
  try {
    const r = await fn();
    ev(name, 'phase_ok', { ms: Date.now() - start });
    return r;
  } catch (e) {
    if (e?.kind !== 'aborted') fail(name, 'exception', e?.stack || e);
    return null;
  }
}

/* ------------------------------------------------------------------ */
/* process / resource helpers                                          */
/* ------------------------------------------------------------------ */

function gitHead() {
  try {
    return spawnSync('git', ['rev-parse', 'HEAD'], { cwd: REPO, encoding: 'utf8' }).stdout?.trim() || null;
  } catch {
    return null;
  }
}

// Server CPU seconds (cumulative), best effort per platform.
function sampleCpu(pid) {
  try {
    if (process.platform === 'win32') {
      const r = spawnSync(
        'powershell',
        ['-NoProfile', '-Command', `(Get-Process -Id ${pid} -ErrorAction Stop).CPU`],
        { encoding: 'utf8', timeout: 5000 },
      );
      const v = parseFloat(r.stdout);
      return Number.isFinite(v) ? v : null;
    }
    const r = spawnSync('ps', ['-o', 'cputime=', '-p', String(pid)], { encoding: 'utf8', timeout: 5000 });
    const s = (r.stdout || '').trim();
    if (!s) return null;
    const m = /(?:(\d+):)?(\d+):(\d+)/.exec(s);
    if (!m) return null;
    return (+(m[1] || 0)) * 3600 + (+m[2]) * 60 + (+m[3]);
  } catch {
    return null;
  }
}

async function health(port) {
  const url = `http://127.0.0.1:${port}/health?token=${encodeURIComponent(TOKEN)}`;
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), 5000);
  try {
    const res = await fetch(url, { signal: ac.signal });
    if (!res.ok) throw new SoakErr('health', `HTTP ${res.status}`);
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}

async function waitHealth(port) {
  let lastErr = null;
  for (let i = 0; i < 100; i++) {
    try {
      return await health(port);
    } catch (e) {
      lastErr = e;
      await sleep(100);
    }
  }
  throw lastErr || new SoakErr('health', 'server /health never answered');
}

/* ------------------------------------------------------------------ */
/* server child                                                        */
/* ------------------------------------------------------------------ */

function spawnServer({ name, home, exitAfterMs = null }) {
  mkdirSync(home, { recursive: true });
  const logPath = join(OUT, `server-${name}.log`);
  const log = createWriteStream(logPath, { flags: 'a' });
  const env = { ...process.env };
  delete env.PORT;
  delete env.HOST_TOKEN;
  env.CHERNOBYL_HOME = home;
  const args = [
    '--bind', '127.0.0.1',
    '--port', '0',
    '--diagnostics-port', '0',
    '--token', TOKEN,
  ];
  // failsafe: a forgotten long run still exits on its own (documented stop condition)
  args.push('--exit-after', String(exitAfterMs ?? (DURATION_S + 3600) * 1000));
  const child = spawn(BIN, args, { env, stdio: ['ignore', 'pipe', 'pipe'] });

  const server = {
    name,
    child,
    pid: child.pid,
    port: null,
    diagPort: null,
    requested: false,
    logPath,
    stderrBuf: '',
    exited: false,
    exitInfo: null,
  };

  child.stdout.on('data', (d) => log.write(d));
  child.stderr.on('data', (d) => {
    log.write(d);
    server.stderrBuf += d.toString();
    // each stderr line can arrive as its own chunk — parse per field
    if (server.port === null) {
      const m = /listening on [0-9.]+:(\d+)/.exec(server.stderrBuf);
      if (m) server.port = +m[1];
    }
    if (server.diagPort === null) {
      const dm = /diagnostics on 127\.0\.0\.1:(\d+)/.exec(server.stderrBuf);
      if (dm) server.diagPort = +dm[1];
    }
  });

  child.on('exit', (code, sig) => {
    server.exited = true;
    server.exitInfo = { code, sig, t: Date.now() };
    if (!server.requested) {
      // criterion 1: the server must never go away on its own
      serverLog.unexpectedExits.push({ server: name, code, sig, at: Date.now() - T0 });
      fail('server', 'unexpected_server_exit', `${name} exited code=${code} sig=${sig} without a harness request`);
      ABORT = true;
    } else {
      serverLog.requestedStops.push({ server: name, code, sig, at: Date.now() - T0 });
    }
    ev('server', 'exit', { server: name, code, sig, requested: server.requested });
  });
  child.on('error', (e) => fail('server', 'spawn_error', `${name}: ${e}`));

  server.waitForPorts = () =>
    withTimeout(
      pollUntil(async () => server.port !== null && server.diagPort !== null, 15_000, 50)
        .then((ok) => {
          if (!ok) throw new SoakErr('spawn', `ports never announced; log: ${server.logPath}`);
        }),
      16_000,
      'port announcement',
    );
  server.stop = async (ms = 15_000) => {
    if (server.exited) return server.exitInfo;
    server.requested = true;
    server.child.kill();
    return withTimeout(server.exitedPoll(), ms, `stop ${name}`);
  };
  server.exitedPoll = async () => {
    await pollUntil(async () => server.exited, 15_000, 50);
    return server.exitInfo;
  };
  server.expectExit = async (ms) => {
    server.requested = true; // the --exit-after shutdown IS the expected path
    return withTimeout(server.exitedPoll(), ms, `expected exit ${name}`);
  };
  return server;
}

/* ------------------------------------------------------------------ */
/* WS client                                                           */
/* ------------------------------------------------------------------ */

class Client {
  constructor(label) {
    this.label = label;
    this.texts = []; // arrival-ordered raw texts
    this.objs = []; // parsed in order (null when not JSON)
    this.pongSeq = []; // commandIds of pong messages, arrival order
    this.closeInfo = null;
    this.errors = [];
    this.open = false;
    this.ws = null;
    this._waiters = [];
  }

  attach(ws) {
    this.ws = ws;
    ws.on('message', (data) => {
      const text = data.toString();
      this.texts.push(text);
      let obj = null;
      try {
        obj = JSON.parse(text);
      } catch {
        /* keep null */
      }
      this.objs.push(obj);
      if (obj?.type === 'pong') this.pongSeq.push(obj.commandId);
      for (let i = this._waiters.length - 1; i >= 0; i--) {
        if (this._waiters[i].pred(obj, text)) {
          const w = this._waiters.splice(i, 1)[0];
          clearTimeout(w.timer);
          w.resolve({ obj, text });
        }
      }
    });
    ws.on('close', (code, reason) => {
      this.closeInfo = { code, reason: reason?.toString() || '', t: Date.now() };
      for (const w of this._waiters.splice(0)) {
        clearTimeout(w.timer);
        w.resolve({ closed: this.closeInfo });
      }
    });
    ws.on('error', (e) => this.errors.push(String(e?.message || e)));
  }

  next(pred, ms, what) {
    for (let i = 0; i < this.objs.length; i++) {
      const obj = this.objs[i];
      if (pred(obj, this.texts[i])) {
        const text = this.texts[i];
        this.objs.splice(i, 1);
        this.texts.splice(i, 1);
        return Promise.resolve({ obj, text });
      }
    }
    if (this.closeInfo) return Promise.reject(new SoakErr('closed', `${this.label}: closed (${this.closeInfo.code}) while waiting for ${what}`));
    return new Promise((resolve, reject) => {
      const w = {
        pred,
        resolve: (v) => (v.closed ? reject(new SoakErr('closed', `${this.label}: closed (${v.closed.code}) while waiting for ${what}`)) : resolve(v)),
        timer: setTimeout(() => {
          const idx = this._waiters.indexOf(w);
          if (idx >= 0) this._waiters.splice(idx, 1);
          reject(new SoakErr('timeout', `${this.label}: no ${what} within ${ms}ms (got ${this.objs.length} buffered msgs)`));
        }, ms),
      };
      this._waiters.push(w);
    });
  }

  send(obj) {
    this.ws.send(JSON.stringify(obj));
  }

  async hello(protocol) {
    this.send({ type: 'hello', clientId: this.label, protocol });
    const { obj } = await this.next((m) => m?.type === 'hello_ack', 8000, 'hello_ack');
    return obj;
  }

  async pingAndWait(id, ms = 8000) {
    this.send({ type: 'ping', commandId: id });
    const { obj } = await this.next((m) => m?.type === 'pong' && m.commandId === id, ms, `pong ${id}`);
    return obj;
  }

  async waitClose(ms) {
    if (this.closeInfo) return this.closeInfo;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new SoakErr('timeout', `${this.label}: no close within ${ms}ms`)),
        ms,
      );
      this._waiters.push({
        pred: () => false,
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v.closed);
        },
        timer,
      });
    });
  }

  pause() {
    this.ws.pause();
  }

  resume() {
    this.ws.resume();
  }

  closeClean() {
    try {
      this.ws.close(1000);
    } catch {
      /* already gone */
    }
  }

  kill() {
    try {
      this.ws.terminate();
    } catch {
      /* already gone */
    }
  }
}

const LIVE = new Set();

function connect(port, token, label, timeoutMs = 8000) {
  return new Promise((resolve, reject) => {
    const c = new Client(label);
    const url = `ws://127.0.0.1:${port}/?token=${encodeURIComponent(token)}`;
    const ws = new WebSocket(url, { handshakeTimeout: timeoutMs });
    c.attach(ws);
    let settled = false;
    const settle = (fn, v) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      fn(v);
    };
    const timer = setTimeout(() => settle(reject, new SoakErr('timeout', `${label}: connect timed out`)), timeoutMs);
    ws.on('unexpected-response', (_req, res) => {
      res.resume();
      settle(reject, new SoakErr('http', `${label}: HTTP ${res.statusCode}`, { status: res.statusCode }));
    });
    ws.on('open', () => {
      c.open = true;
      LIVE.add(c);
      counts.connectsCompleted++;
      settle(resolve, c);
    });
    ws.on('close', (code) => {
      if (!c.open) settle(reject, new SoakErr('closed', `${label}: closed before open (code ${code})`, { code }));
    });
    ws.on('error', (e) => {
      if (!c.open) settle(reject, new SoakErr('error', `${label}: ${e?.message || e}`));
    });
  });
}

function cleanupClients() {
  for (const c of LIVE) c.kill();
  LIVE.clear();
}

/* ------------------------------------------------------------------ */
/* phases                                                              */
/* ------------------------------------------------------------------ */

// Criterion 3: wrong tokens must yield outcomes by design (4001/429) and
// must never receive a single byte of protocol data.
async function phaseAuth(port) {
  const attempt = async (label, wanted) => {
    try {
      const c = await connect(port, wanted, label);
      const info = await c.waitClose(8000);
      LIVE.delete(c);
      return { outcome: `closed-${info.code}`, texts: c.texts.length };
    } catch (e) {
      if (e.kind === 'http') return { outcome: `http-${e.status}`, texts: 0 };
      if (e.kind === 'closed') return { outcome: `closed-${e.code}`, texts: 0 };
      throw e;
    }
  };

  // failures 0→3: three wrong tokens close 4001, no data
  for (let i = 1; i <= 3; i++) {
    const r = await attempt(`auth-wrong-${i}`, 'wrong-token');
    counts.unauthorizedOutcomes[r.outcome] = (counts.unauthorizedOutcomes[r.outcome] || 0) + 1;
    if (r.outcome !== 'closed-4001') fail('auth', 'unauthorized_accepted', `wrong token #${i} got ${r.outcome} (expected closed-4001)`);
    if (r.texts !== 0) fail('auth', 'unauthorized_accepted', `wrong token #${i} received ${r.texts} messages`);
  }
  // good token resets the counter
  {
    const c = await connect(port, TOKEN, 'auth-good-1');
    const ack = await c.hello(PROTOCOL_VERSION);
    if (ack?.type !== 'hello_ack') fail('auth', 'connection_failure', 'good token did not get hello_ack');
    c.closeClean();
    await c.waitClose(5000).catch(() => {});
    LIVE.delete(c);
  }
  // failures 1..5 → five more 4001s, 6th attempt → 429 (AUTH_FAILURE_LIMIT=5)
  for (let i = 1; i <= 6; i++) {
    const r = await attempt(`auth-wrong-b-${i}`, 'wrong-token');
    counts.unauthorizedOutcomes[r.outcome] = (counts.unauthorizedOutcomes[r.outcome] || 0) + 1;
    const expected = i < 6 ? 'closed-4001' : 'http-429';
    if (r.outcome !== expected) fail('auth', 'unauthorized_accepted', `wrong token b#${i} got ${r.outcome} (expected ${expected})`);
    if (r.texts !== 0) fail('auth', 'unauthorized_accepted', `wrong token b#${i} received ${r.texts} messages`);
  }
  // recovery after the cooldown trip
  {
    const c = await connect(port, TOKEN, 'auth-good-2');
    await c.hello(PROTOCOL_VERSION);
    await c.pingAndWait('auth-recover-ping');
    c.closeClean();
    LIVE.delete(c);
  }
  ev('auth', 'auth_outcomes', counts.unauthorizedOutcomes);
}

// Criterion 2: pipelined pings per client must come back in send order.
async function phaseFifo(port, clients, pings) {
  const cs = [];
  for (let i = 0; i < clients; i++) {
    const c = await connect(port, TOKEN, `fifo-${i}`);
    await c.hello(PROTOCOL_VERSION);
    cs.push(c);
  }
  const expected = cs.map((_, i) => Array.from({ length: pings }, (__, j) => `f${i}-${j}`));
  cs.forEach((c, i) => expected[i].forEach((id) => c.send({ type: 'ping', commandId: id })));
  counts.pongsExpected += clients * pings;

  for (let i = 0; i < cs.length; i++) {
    const c = cs[i];
    const ok = await pollUntil(() => c.pongSeq.length >= pings, 15_000, 50);
    if (!ok) {
      fail('fifo', 'connection_failure', `${c.label}: got ${c.pongSeq.length}/${pings} pongs`);
    }
    counts.pongsReceived += Math.min(c.pongSeq.length, pings);
    const got = c.pongSeq.slice(0, pings);
    const want = expected[i];
    if (got.length === want.length && got.some((v, j) => v !== want[j])) {
      const j = got.findIndex((v, k) => v !== want[k]);
      counts.orderingViolations++;
      fail('fifo', 'ordering_violation', `${c.label}: at ${j} expected ${want[j]} got ${got[j]}`);
    }
    c.closeClean();
    await c.waitClose(5000).catch(() => {});
    LIVE.delete(c);
  }
}

// Criterion 4 (churn slice): every peer must come back to 0 after churn.
async function phaseChurn(port, cycles, perCycle) {
  for (let cy = 0; cy < cycles; cy++) {
    for (let k = 0; k < perCycle; k++) {
      const label = `churn-${cy}-${k}`;
      try {
        const c = await connect(port, TOKEN, label);
        await c.hello(PROTOCOL_VERSION);
        await c.pingAndWait(label + '-p');
        if (k % 2 === 0) {
          c.closeClean();
          const info = await c.waitClose(5000).catch(() => null);
          const reason = info ? `close-${info.code}` : 'closed';
          counts.disconnectReasons[reason] = (counts.disconnectReasons[reason] || 0) + 1;
        } else {
          c.kill(); // abrupt mid-session disappearance
          counts.disconnectReasons.terminated = (counts.disconnectReasons.terminated || 0) + 1;
        }
        LIVE.delete(c);
      } catch (e) {
        counts.connectsFailed++;
        fail('churn', 'connection_failure', `${label}: ${e.message}`);
      }
    }
  }
  const released = await pollUntil(
    async () => (await health(port)).clients === 0,
    LIMITS.releaseNormalMs,
    250,
  );
  if (!released) {
    const h = await health(port).catch(() => ({}));
    fail('churn', 'peer_leak', `${h.clients ?? '?'} clients still registered after churn (limit ${LIMITS.releaseNormalMs}ms)`);
  }
  ev('churn', 'released', { cycles, perCycle });
}

// Recovery: a 4001'd client reconnects with the right token and works.
async function phaseReconnect(port) {
  try {
    await connect(port, 'wrong-token', 'recon-wrong');
  } catch {
    /* 4001/429 expected */
  }
  const c = await connect(port, TOKEN, 'recon-good');
  await c.hello(PROTOCOL_VERSION);
  await c.pingAndWait('recon-ping');
  c.closeClean();
  LIVE.delete(c);
}

async function probeStateSize(port, label) {
  const c = await connect(port, TOKEN, label);
  await c.hello(PROTOCOL_VERSION);
  c.send({ type: 'list_state', commandId: `${label}-probe` });
  const { text } = await c.next((m) => m?.type === 'state', 8000, 'state reply');
  const bytes = Buffer.byteLength(text);
  c.closeClean();
  await c.waitClose(5000).catch(() => {});
  LIVE.delete(c);
  return bytes;
}

// Criterion 5: stopped-consuming client + reply flood → D8 teardown must be
// observed via /health release, then the same client reconnects (recovery).
async function phaseSaturation(port, label, n, replyBytes, expectedBound) {
  const c = await connect(port, TOKEN, label);
  await c.hello(PROTOCOL_VERSION);
  c.pause(); // stop consuming BEFORE the flood: no replies are ever read
  for (let j = 0; j < n; j++) c.send({ type: 'list_state', commandId: `${label}-${j}` });
  const t0 = Date.now();
  const released = await pollUntil(
    async () => (await health(port)).clients === 0,
    LIMITS.releaseSaturationMs,
    500,
  );
  const teardownMs = Date.now() - t0;
  if (!released) {
    fail(label, 'd8_saturation_teardown', `still registered ${LIMITS.releaseSaturationMs}ms after ${n} list_states (server must tear down, not buffer)`);
  } else {
    counts.teardowns.push({ label, n, replyBytes, expectedBound, teardownMs });
    ev(label, 'saturation_teardown', { n, replyBytes, expectedBound, teardownMs });
    progress(`${label}: D8 teardown in ${teardownMs}ms (n=${n}, reply=${replyBytes}B, expected bound: ${expectedBound})`);
  }
  c.resume();
  c.kill();
  LIVE.delete(c);
  // recovery after a server-side disconnect
  const c2 = await connect(port, TOKEN, label + '-recovery');
  await c2.hello(PROTOCOL_VERSION);
  await c2.pingAndWait(label + '-recovery-ping');
  c2.closeClean();
  await c2.waitClose(5000).catch(() => {});
  LIVE.delete(c2);
  return teardownMs;
}

// Grow the snapshot so one state reply exceeds 4 MiB / 256 = 16 KiB,
// which makes the BYTE bound reachable ahead of the count bound.
async function seedSessions(port, targetBytes, maxSessions) {
  const c = await connect(port, TOKEN, 'seed');
  const ack = await c.hello(PROTOCOL_VERSION);
  const agents = (ack?.snapshot?.agents || []).map((a) => a.id);
  if (agents.length === 0) {
    c.closeClean();
    LIVE.delete(c);
    return { ok: false, reason: 'snapshot has no agents', created: 0, stateBytes: 0 };
  }
  // find an agent that accepts new sessions (one probe per candidate)
  let chosen = null;
  for (const agentId of agents) {
    c.send({
      type: 'create_session',
      commandId: `seed-probe-${agentId}`,
      agentId,
      title: 'Soak session probe',
      prompt: 'soak seed',
    });
    const { obj } = await c.next((m) => m?.type === 'ack' && m.commandId === `seed-probe-${agentId}`, 8000, 'create ack');
    if (obj?.ok) {
      chosen = agentId;
      break;
    }
  }
  if (!chosen) {
    c.closeClean();
    LIVE.delete(c);
    return { ok: false, reason: 'no agent accepted a session', created: 0, stateBytes: 0 };
  }
  let created = 1;
  let stateBytes = 0;
  while (created < maxSessions) {
    const batch = Math.min(50, maxSessions - created);
    for (let j = 0; j < batch; j++) {
      c.send({
        type: 'create_session',
        commandId: `seed-${chosen}-${created + j}`,
        agentId: chosen,
        title: `Soak session ${created + j}`,
        prompt: 'soak seed',
      });
    }
    for (let j = 0; j < batch; j++) {
      await c.next(
        (m) => m?.type === 'ack' && m.commandId === `seed-${chosen}-${created + j}`,
        15_000,
        'create ack batch',
      );
    }
    created += batch;
    c.send({ type: 'list_state', commandId: 'seed-probe' });
    const { text } = await c.next((m) => m?.type === 'state', 8000, 'state');
    stateBytes = Buffer.byteLength(text);
    if (stateBytes >= targetBytes) break;
  }
  c.closeClean();
  await c.waitClose(5000).catch(() => {});
  LIVE.delete(c);
  return { ok: true, created, stateBytes, agentId: chosen };
}

// Criterion 4/6: a silent peer that never answers heartbeats must be
// released by the heartbeat path (HEARTBEAT_MS x MISS_LIMIT ≈ 45-60s).
async function phaseDeadPeer(port) {
  const c = await connect(port, TOKEN, 'dead-peer');
  await c.hello(PROTOCOL_VERSION);
  c.pause(); // no reads → no pongs → server counts heartbeat misses
  const t0 = Date.now();
  const released = await pollUntil(
    async () => (await health(port)).clients === 0,
    LIMITS.releaseDeadPeerMs,
    1000,
  );
  const ms = Date.now() - t0;
  if (!released) {
    fail('dead-peer', 'dead_peer_not_released', `still registered after ${LIMITS.releaseDeadPeerMs}ms`);
  } else {
    ev('dead-peer', 'released', { ms });
    progress(`dead-peer released in ${Math.round(ms / 1000)}s`);
  }
  c.resume();
  c.kill();
  LIVE.delete(c);
  return ms;
}

// Criterion 4: abrupt client death mid-reply-stream must not wedge the
// server or leak the registry entry.
async function phaseWriterFailure(port) {
  const c = await connect(port, TOKEN, 'writer-fail');
  await c.hello(PROTOCOL_VERSION);
  for (let j = 0; j < 500; j++) c.send({ type: 'list_state', commandId: `wf-${j}` });
  c.kill(); // RST while the server is mid-reply
  LIVE.delete(c);
  const released = await pollUntil(
    async () => (await health(port)).clients === 0,
    LIMITS.releaseNormalMs,
    250,
  );
  if (!released) fail('writer-fail', 'peer_leak', 'client still registered after abrupt death');
  // the server must still serve a fresh client afterwards
  const c2 = await connect(port, TOKEN, 'writer-fail-after');
  await c2.hello(PROTOCOL_VERSION);
  await c2.pingAndWait('wf-after-ping');
  c2.closeClean();
  await c2.waitClose(5000).catch(() => {});
  LIVE.delete(c2);
}

async function phaseQuiesce(port) {
  const released = await pollUntil(
    async () => (await health(port)).clients === 0,
    LIMITS.releaseNormalMs,
    250,
  );
  const h = await health(port).catch(() => null);
  if (!released) fail('quiesce', 'peer_leak', `${h?.clients ?? '?'} clients still registered at quiesce`);
  if (!h) {
    fail('quiesce', 'connection_failure', '/health unavailable at quiesce');
    return null;
  }
  rssFinal = h.rssBytes ?? null;
  ev('quiesce', 'health', {
    clients: h.clients, rssBytes: h.rssBytes, commands: h.commands,
    unresolved: h.unresolved, malformed: h.malformed, events: h.events,
    sessions: h.sessions,
  });
  return h;
}

// Criterion 7: --exit-after shutdown with live clients + queued work must
// exit 0 and log its shutdown sequence.
async function phaseGraceful() {
  const home = join(OUT, 'home-graceful');
  const s = spawnServer({ name: 'graceful', home, exitAfterMs: 8000 });
  await s.waitForPorts();
  const cs = [];
  for (let i = 0; i < 3; i++) {
    const c = await connect(s.port, TOKEN, `graceful-${i}`);
    await c.hello(PROTOCOL_VERSION);
    cs.push(c);
  }
  for (const c of cs) for (let j = 0; j < 30; j++) c.send({ type: 'ping', commandId: `g-${c.label}-${j}` });
  // queued work in flight; the timed shutdown must still be clean
  const info = await s.expectExit(8000 + 20_000);
  const logged = existsSync(s.logPath) ? readFileSync(s.logPath, 'utf8') : '';
  serverLog.graceful = {
    code: info?.code,
    hasShuttingDown: logged.includes('shutting down'),
    hasBye: logged.includes('[companion] bye'),
    ms: Date.now() - T0,
  };
  if (info?.code !== 0) fail('graceful', 'graceful_shutdown_failed', `exit code ${info?.code} (expected 0)`);
  if (!serverLog.graceful.hasShuttingDown) fail('graceful', 'graceful_shutdown_failed', 'stderr never logged "shutting down"');
  for (const c of cs) {
    await c.waitClose(10_000).catch(() => {});
    LIVE.delete(c);
  }
}

// Criterion 8: restart on the same data dir must serve reconnecting clients.
async function phaseRestart(main) {
  await main.stop(15_000);
  const s = spawnServer({ name: 'restart', home: join(OUT, 'home-a') });
  await s.waitForPorts();
  const ok = [];
  for (let i = 0; i < 2; i++) {
    try {
      const c = await connect(s.port, TOKEN, `restart-${i}`);
      await c.hello(PROTOCOL_VERSION);
      await c.pingAndWait(`restart-ping-${i}`);
      c.closeClean();
      await c.waitClose(5000).catch(() => {});
      LIVE.delete(c);
      ok.push(true);
    } catch (e) {
      ok.push(false);
      fail('restart', 'restart_recovery_failed', `client ${i}: ${e.message}`);
    }
  }
  serverLog.restart = { ok: ok.filter(Boolean).length, total: ok.length };
  await s.stop(15_000);
}

/* ------------------------------------------------------------------ */
/* resource sampler                                                    */
/* ------------------------------------------------------------------ */

let sampler = null;
function startSampler(port, pid) {
  let cpuDue = 0;
  sampler = setInterval(async () => {
    try {
      const h = await health(port);
      if (typeof h.rssBytes === 'number') {
        rssPeak = rssPeak === null ? h.rssBytes : Math.max(rssPeak, h.rssBytes);
        if (rssBaseline === null) rssBaseline = h.rssBytes;
      }
      let cpuS = null;
      if (Date.now() >= cpuDue) {
        cpuS = sampleCpu(pid);
        cpuDue = Date.now() + 120_000;
      }
      samples.push({ t: Date.now() - T0, rss: h.rssBytes ?? null, cpuS });
      ev('sample', 'resource', { rss: h.rssBytes ?? null, cpuS, clients: h.clients });
    } catch (e) {
      samples.push({ t: Date.now() - T0, rss: null, cpuS: null, error: String(e.message || e) });
    }
  }, 30_000);
  sampler.unref?.();
}

function stopSampler() {
  if (sampler) clearInterval(sampler);
  sampler = null;
}

/* ------------------------------------------------------------------ */
/* summary                                                             */
/* ------------------------------------------------------------------ */

function buildSummary(mode, extra = {}) {
  const growth = rssBaseline !== null && rssFinal !== null ? rssFinal - rssBaseline : null;
  const criteria = [
    {
      id: 'unexpected_server_exit',
      requirement: 'server never exits without a harness-requested stop',
      observed: `${serverLog.unexpectedExits.length} unexpected exit(s)`,
      pass: serverLog.unexpectedExits.length === 0,
    },
    {
      id: 'ordering_violation',
      requirement: 'every expected pong arrives in send order (per client)',
      observed: `${counts.pongsReceived}/${counts.pongsExpected} pongs, ${counts.orderingViolations} order violations`,
      pass: counts.orderingViolations === 0 && counts.pongsReceived === counts.pongsExpected,
    },
    {
      id: 'unauthorized_accepted',
      requirement: 'wrong-token clients get only 4001/429 and never protocol data',
      observed: JSON.stringify(counts.unauthorizedOutcomes),
      pass: !failures.some((f) => f.code === 'unauthorized_accepted'),
    },
    {
      id: 'peer_leak',
      requirement: 'clients gauge returns to 0 after churn / writer failure / quiesce',
      observed: failures.some((f) => f.code === 'peer_leak') ? 'see failures[]' : 'all phases released to 0',
      pass: !failures.some((f) => f.code === 'peer_leak'),
    },
    {
      id: 'd8_saturation_teardown',
      requirement: 'stopped-consuming clients are torn down (bounded queue), then reconnect fine',
      observed: `${counts.teardowns.length} teardown(s): ${counts.teardowns.map((t) => `${t.label} ${t.teardownMs}ms`).join(', ') || 'none'}`,
      pass: !failures.some((f) => f.code === 'd8_saturation_teardown'),
    },
    {
      id: 'dead_peer_not_released',
      requirement: 'silent paused peer released within the heartbeat bound',
      observed: failures.some((f) => f.code === 'dead_peer_not_released') ? 'NOT released (see failures)' : 'released',
      pass: !failures.some((f) => f.code === 'dead_peer_not_released'),
    },
    {
      id: 'graceful_shutdown_failed',
      requirement: '--exit-after shutdown with live clients exits 0 and logs shutdown',
      observed: serverLog.graceful ? JSON.stringify(serverLog.graceful) : 'not run',
      pass: serverLog.graceful !== null && serverLog.graceful.code === 0 && serverLog.graceful.hasShuttingDown
        && !failures.some((f) => f.code === 'graceful_shutdown_failed'),
    },
    {
      id: 'restart_recovery_failed',
      requirement: 'respawned server serves reconnecting clients on the same data dir',
      observed: serverLog.restart ? `${serverLog.restart.ok}/${serverLog.restart.total}` : 'not run',
      pass: serverLog.restart !== null && serverLog.restart.ok === serverLog.restart.total
        && !failures.some((f) => f.code === 'restart_recovery_failed'),
    },
    {
      id: 'resource_growth',
      requirement: `quiesced RSS growth ≤ ${LIMITS.rssGrowthLimitBytes}B and absolute ≤ ${LIMITS.rssAbsoluteLimitBytes}B`,
      observed: `baseline=${rssBaseline} final=${rssFinal} growth=${growth} peak=${rssPeak}`,
      pass:
        (growth === null || growth <= LIMITS.rssGrowthLimitBytes)
        && (rssPeak === null || rssPeak <= LIMITS.rssAbsoluteLimitBytes)
        && rssFinal !== null,
    },
    {
      id: 'connection_failure',
      requirement: 'every expected connect/hello/pong/ack happens (failures listed)',
      observed: `${failures.filter((f) => f.code === 'connection_failure' || f.code === 'exception').length} failure(s)`,
      pass: !failures.some((f) => f.code === 'connection_failure' || f.code === 'exception'),
    },
  ];
  return {
    harness: 'services/companion-rs/tools/soak.mjs',
    mode,
    startedAt: new Date(T0).toISOString(),
    endedAt: new Date().toISOString(),
    durationS: Math.round((Date.now() - T0) / 100) / 10,
    gitHead: gitHead(),
    node: process.version,
    platform: `${process.platform} ${process.arch}`,
    config: { durationS: DURATION_S, clients: CLIENTS, churnCycles: CHURN_CYCLES, inject: INJECT, out: OUT },
    thresholds: {
      existingLimits: {
        outboundMaxMessages: LIMITS.outboundMaxMessages,
        outboundMaxBytes: LIMITS.outboundMaxBytes,
        authFailureLimit: LIMITS.authFailureLimit,
        heartbeatMs: LIMITS.heartbeatMs,
        heartbeatMissLimit: LIMITS.heartbeatMissLimit,
        source: 'services/companion-rs/src/server.rs, src/protocol.rs',
      },
      proposedLimits: {
        rssGrowthLimitBytes: LIMITS.rssGrowthLimitBytes,
        rssAbsoluteLimitBytes: LIMITS.rssAbsoluteLimitBytes,
        releaseNormalMs: LIMITS.releaseNormalMs,
        releaseSaturationMs: LIMITS.releaseSaturationMs,
        releaseDeadPeerMs: LIMITS.releaseDeadPeerMs,
        note: 'newly proposed by the soak harness; release timeouts sit above the source-derived teardown bounds so a timeout means stuck, not slow',
      },
    },
    counts,
    servers: serverLog,
    resources: {
      baselineRssBytes: rssBaseline,
      peakRssBytes: rssPeak,
      finalRssBytes: rssFinal,
      growthBytes: rssBaseline !== null && rssFinal !== null ? rssFinal - rssBaseline : null,
      sampleCount: samples.length,
      samplesEveryMs: 30_000,
    },
    criteria,
    failures,
    pass: failures.length === 0 && criteria.every((c) => c.pass),
    ...extra,
  };
}

function writeSummary(summary) {
  writeFileSync(join(OUT, 'summary.json'), JSON.stringify(summary, null, 2));
  console.log('\n=== soak summary ===');
  console.log(`mode=${summary.mode} duration=${summary.durationS}s head=${summary.gitHead?.slice(0, 12)}`);
  console.log(`rss baseline=${summary.resources.baselineRssBytes} peak=${summary.resources.peakRssBytes} final=${summary.resources.finalRssBytes} growth=${summary.resources.growthBytes}`);
  for (const c of summary.criteria) {
    console.log(`  ${c.pass ? 'PASS' : 'FAIL'}  ${c.id}: ${c.observed}`);
  }
  console.log(`failures=${summary.failures.length} pass=${summary.pass}`);
  console.log(`artifacts: ${OUT}`);
}

/* ------------------------------------------------------------------ */
/* main                                                                */
/* ------------------------------------------------------------------ */

async function buildBinary() {
  const t0 = Date.now();
  const r = spawnSync('cargo', ['build', '--manifest-path', CARGO_TOML], { encoding: 'utf8' });
  if (r.status !== 0) {
    console.error('cargo build failed:\n' + (r.stderr || r.stdout || ''));
    process.exit(2);
  }
  if (!existsSync(BIN)) {
    console.error(`binary not found at ${BIN}`);
    process.exit(2);
  }
  return Date.now() - t0;
}

async function main() {
  mkdirSync(OUT, { recursive: true });
  const buildMs = await buildBinary();
  progress(`run dir ${OUT} (build ${buildMs}ms)`);

  // failure-injection demo: kill the server mid-workload, unannounced
  let injectAt = null;
  if (INJECT === 'server-kill') injectAt = 15;
  else if (INJECT) {
    console.error(`unknown --inject ${INJECT}`);
    process.exit(2);
  }

  const main1 = spawnServer({ name: 'main', home: join(OUT, 'home-a') });
  await main1.waitForPorts();
  progress(`server up on :${main1.port} (diag :${main1.diagPort})`);
  const h0 = await waitHealth(main1.port);
  rssBaseline = h0.rssBytes ?? null;
  ev('boot', 'health', h0);
  if (h0.protocol !== PROTOCOL_VERSION) {
    fail('boot', 'connection_failure', `unexpected protocol ${h0.protocol}`);
  }
  startSampler(main1.port, main1.pid);

  await runPhase('auth', () => phaseAuth(main1.port));

  await runPhase('fifo', async () => {
    if (injectAt) {
      // let real traffic flow first, then pull the plug without announcing it
      await phaseFifo(main1.port, Math.min(CLIENTS, 3), injectAt);
      progress('INJECT: killing server without a harness stop (expect unexpected_server_exit)');
      main1.child.kill(); // deliberately NOT via server.stop → must be detected
      await sleep(1500);
      return;
    }
    await phaseFifo(main1.port, CLIENTS, LONG ? 80 : 40);
  });

  await runPhase('churn', () => phaseChurn(main1.port, CHURN_CYCLES, 8));
  await runPhase('reconnect', () => phaseReconnect(main1.port));

  if (!ABORT && !LONG) {
    await runPhase('saturation', async () => {
      const L0 = await probeStateSize(main1.port, 'probe-a');
      // 40 MiB of replies ≈ surely beyond any socket buffer; count-clamped
      const nA = Math.min(40_000, Math.max(512, Math.ceil((40 * 1024 * 1024) / Math.max(L0, 512)) + 1024));
      const boundA = 256 * L0 > LIMITS.outboundMaxBytes ? 'bytes' : 'count';
      await phaseSaturation(main1.port, 'sat-count', nA, L0, boundA);

      const seed = await seedSessions(main1.port, 17_000, 1000);
      ev('saturation', 'seed', seed);
      if (seed.ok && seed.stateBytes > 0) {
        const L2 = seed.stateBytes;
        const nB = Math.min(40_000, Math.max(512, Math.ceil((40 * 1024 * 1024) / L2) + 1024));
        const boundB = 256 * L2 > LIMITS.outboundMaxBytes ? 'bytes' : 'count';
        await phaseSaturation(main1.port, 'sat-bytes', nB, L2, boundB);
        ev('saturation', 'byte_path', { stateBytes: L2, reachable: boundB === 'bytes' });
      } else {
        ev('saturation', 'byte_path', { reachable: false, reason: seed.reason });
        console.log(`  note: byte-bound path not reachable (${seed.reason}); the count path was exercised — byte-bound proof lives in the outbound_queue_bounds_total_bytes unit test`);
      }
    });

    await runPhase('dead-peer', () => phaseDeadPeer(main1.port));
    await runPhase('writer-failure', () => phaseWriterFailure(main1.port));
    await runPhase('quiesce', () => phaseQuiesce(main1.port));
  }

  if (ABORT && INJECT === 'server-kill') {
    // expected for the detection demo: fall through to the summary below
    progress('injected fault detected — writing summary');
  } else if (ABORT) {
    progress('ABORTED — writing summary');
  } else if (LONG) {
    await runPhase('load-loop', async () => {
      let cycle = 0;
      while ((Date.now() - T0) / 1000 < DURATION_S && !ABORT) {
        cycle++;
        await phaseChurn(main1.port, 3, 8);
        await phaseFifo(main1.port, CLIENTS, 60);
        if (cycle % 3 === 0) {
          const L = await probeStateSize(main1.port, `probe-${cycle}`);
          const n = Math.min(40_000, Math.max(512, Math.ceil((40 * 1024 * 1024) / Math.max(L, 512)) + 1024));
          await phaseSaturation(main1.port, `sat-cycle-${cycle}`, n, L, 256 * L > LIMITS.outboundMaxBytes ? 'bytes' : 'count');
        }
        if (cycle % 10 === 0) await phaseDeadPeer(main1.port);
        const left = Math.round(DURATION_S - (Date.now() - T0) / 1000);
        progress(`load-loop cycle ${cycle} complete, ${left}s remaining`);
        ev('load-loop', 'cycle', { cycle, remainingS: left });
      }
      const target = DURATION_S;
      const actual = (Date.now() - T0) / 1000;
      ev('load-loop', 'duration', { targetS: target, actualS: Math.round(actual) });
      console.log(`  load loop: target ${target}s, ran ${Math.round(actual)}s`);
    });
    await runPhase('saturation', async () => {
      const L = await probeStateSize(main1.port, 'probe-long-sat');
      const n = Math.min(40_000, Math.max(512, Math.ceil((40 * 1024 * 1024) / Math.max(L, 512)) + 1024));
      await phaseSaturation(main1.port, 'sat-final', n, L, 256 * L > LIMITS.outboundMaxBytes ? 'bytes' : 'count');
    });
    await runPhase('dead-peer', () => phaseDeadPeer(main1.port));
    await runPhase('writer-failure', () => phaseWriterFailure(main1.port));
    await runPhase('quiesce', () => phaseQuiesce(main1.port));
  }

  if (!ABORT && INJECT !== 'server-kill') {
    await runPhase('graceful', () => phaseGraceful());
    await runPhase('restart', () => phaseRestart(main1));
  }

  stopSampler();
  cleanupClients();
  if (!main1.exited) await main1.stop(10_000).catch(() => {});

  const summary = buildSummary(MODE, {
    injected: INJECT || null,
    injectedFaultDetected: INJECT === 'server-kill'
      ? serverLog.unexpectedExits.length > 0
      : null,
  });
  writeSummary(summary);
  if (INJECT === 'server-kill') {
    // demo run: exit 1 iff the fault WAS detected (detection is the point)
    process.exit(summary.injectedFaultDetected ? 1 : 3);
  }
  process.exit(summary.pass ? 0 : 1);
}

main().catch((e) => {
  try {
    stopSampler();
    cleanupClients();
    const summary = buildSummary(MODE, { fatal: String(e?.stack || e) });
    summary.pass = false;
    writeSummary(summary);
  } catch {
    console.error('summary write failed after fatal error', e);
  }
  process.exit(1);
});
