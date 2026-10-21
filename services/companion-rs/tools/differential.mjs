#!/usr/bin/env node
// Differential wire probe for the CHERNOBYL protocol-v2 server.
//
// Runs one scripted scenario (auth, hello, malformed, create+replay,
// submit refusal, permission decisions, interrupts, version gate, /health)
// against a server and prints a normalized transcript. Volatile fields
// (timestamps, generated ids) are stripped so the transcripts of two
// different implementations can be diffed exactly.
//
// Usage:
//   node tools/differential.mjs --url ws://127.0.0.1:PORT --token T --out a.json
//   node tools/differential.mjs --compare a.json b.json
//   node tools/differential.mjs --churn --url ws://127.0.0.1:PORT --token T --minutes 30
import WebSocket from 'ws';
import { readFileSync, writeFileSync } from 'node:fs';

const args = process.argv.slice(2);
const opt = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};

const VOLATILE = new Set([
  'serverTime', 'occurredAt', 'createdAt', 'lastActivityAt', 'requestedAt',
  'expiresAt', 'updatedAt', 'startedAt', 'completedAt', 'dueAt', 'lastSeenAt',
  'ts', 'decidedAt',
]);
// generated ids look like s_1a15b9b7c371 / evt_…; seed ids (s1, h1, a4) don't
const GEN_ID = /^[a-z]+_[0-9a-zA-Z]{4,}$/;

function normalize(value) {
  if (Array.isArray(value)) return value.map(normalize);
  if (value && typeof value === 'object') {
    const out = {};
    for (const key of Object.keys(value).sort()) {
      if (VOLATILE.has(key)) continue;
      out[key] = normalize(value[key]);
    }
    return out;
  }
  if (typeof value === 'string' && GEN_ID.test(value)) return '<gen>';
  return value;
}

function open(url, token) {
  return new Promise((resolve, reject) => {
    const sep = url.includes('?') ? '&' : '?';
    const ws = new WebSocket(`${url}${sep}token=${encodeURIComponent(token)}`);
    const queue = [];
    const waiters = [];
    let closedCode = null;
    const deliver = (item) => (waiters.length ? waiters.shift()(item) : queue.push(item));
    ws.on('message', (data) => {
      const raw = data.toString();
      let frame;
      try {
        frame = normalize(JSON.parse(raw));
      } catch {
        frame = '<non-json>';
      }
      deliver({ raw, frame });
    });
    ws.on('close', (code) => {
      closedCode = { closed: code };
      deliver(closedCode);
    });
    // reject on connect failure (e.g. server dead) — swallowing it left the
    // caller awaiting forever and drained the event loop into an unsettled
    // top-level await; after 'open' this reject is a harmless no-op
    ws.on('error', (e) => reject(e));
    ws.on('unexpected-response', (_, res) => reject(new Error(`http ${res.statusCode}`)));
    ws.on('open', () =>
      resolve({
        ws,
        send: (obj) => ws.send(typeof obj === 'string' ? obj : JSON.stringify(obj)),
        next: (ms = 5000) =>
          queue.length
            ? Promise.resolve(queue.shift())
            : new Promise((res, rej) => {
                const t = setTimeout(() => rej(new Error('timeout waiting for frame')), ms);
                waiters.push((f) => {
                  clearTimeout(t);
                  res(f);
                });
              }),
        closeNow: () => ws.terminate(),
      }),
    );
  });
}

async function collectUntilAck(c, ms) {
  const frames = [];
  for (let i = 0; i < 64; i++) {
    const f = await c.next(ms);
    frames.push(f.frame);
    if (f.frame && f.frame.type === 'ack') return { frames, ack: f };
  }
  return { frames, ack: null };
}

async function scenario(url, token) {
  const observations = [];

  // 1. wrong token → close 4001
  {
    const bad = await open(url, 'definitely-wrong');
    const f = await bad.next();
    observations.push({ step: 'auth-reject', ...f.frame });
  }

  // 2. good connection, hello v2
  const c = await open(url, token);
  c.send({ type: 'hello', protocol: 2, clientId: 'diff-probe', resumeFrom: null });
  const hello = await c.next();
  observations.push({ step: 'hello', ...hello.frame });

  // 3. malformed text → malformed ack
  c.send('not json at all');
  observations.push({ step: 'malformed', ...(await c.next()).frame });

  // 4. create_session (events precede the ack — collect everything)
  c.send({
    type: 'create_session',
    commandId: 'diff-1',
    agentId: 'a4',
    title: 'Differential',
    prompt: 'run the diff scenario',
  });
  const created = await collectUntilAck(c);
  observations.push({ step: 'create_session', frames: created.frames });
  let sessionId;
  try {
    sessionId = JSON.parse(created.ack.raw).sessionId;
  } catch {
    throw new Error('no create_session ack with sessionId');
  }

  // 5. replay → duplicate ack, no re-execution
  c.send({
    type: 'create_session',
    commandId: 'diff-1',
    agentId: 'a4',
    title: 'Differential',
    prompt: 'run the diff scenario',
  });
  observations.push({ step: 'replay', ...(await c.next()).frame });

  // 6. submit while the plan runs → refused with parity-checked error text
  c.send({ type: 'submit_prompt', commandId: 'diff-2', sessionId, text: 'again' });
  {
    const f = await c.next();
    observations.push({ step: 'submit-while-running', ...f.frame });
  }

  // 7. permission decision: stale revision refused (fail-closed)
  c.send({
    type: 'permission_decision',
    commandId: 'diff-3',
    permissionId: 'p1',
    decision: 'approve-once',
    revision: 2,
  });
  observations.push({ step: 'permission-stale', ...(await c.next()).frame });

  // 8. fresh revision resolves; events precede the ack
  c.send({
    type: 'permission_decision',
    commandId: 'diff-4',
    permissionId: 'p1',
    decision: 'approve-once',
    revision: 1,
  });
  {
    const { frames } = await collectUntilAck(c);
    observations.push({ step: 'permission-fresh', frames });
  }

  // 9. interrupt, then idempotent interrupt on the same session
  c.send({ type: 'interrupt', commandId: 'diff-5', sessionId });
  observations.push({ step: 'interrupt', ...(await c.next()).frame });
  c.send({ type: 'interrupt', commandId: 'diff-6', sessionId });
  observations.push({ step: 'interrupt-again', ...(await c.next()).frame });
  c.closeNow();

  // 10. protocol mismatch → ack + close 4002
  {
    const v1 = await open(url, token);
    v1.send({ type: 'hello', protocol: 1, clientId: 'diff-probe', resumeFrom: null });
    const frames = [];
    for (let i = 0; i < 4; i++) {
      const f = await v1.next(2000).catch(() => null);
      if (!f) break;
      frames.push(f.frame);
      if (f.frame && f.frame.closed !== undefined) break;
    }
    observations.push({ step: 'version-gate', frames });
  }

  // 11. /health stable subset
  {
    const httpBase = url.replace(/^ws/, 'http').replace(/\/\?.*$/, '').replace(/\/$/, '');
    const res = await fetch(`${httpBase}/health`, {
      headers: { authorization: `Bearer ${token}` },
    });
    const h = await res.json();
    observations.push({
      step: 'health',
      body: { ok: h.ok, protocol: h.protocol },
    });
  }

  return observations;
}

function deepDiff(a, b, path = '', out = []) {
  if (out.length >= 25) return out;
  if (JSON.stringify(a) === JSON.stringify(b)) return out;
  if (a === null || b === null || typeof a !== 'object' || typeof b !== 'object') {
    out.push(`${path || '<root>'}: host=${JSON.stringify(a)} rust=${JSON.stringify(b)}`);
    return out;
  }
  const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
  for (const k of keys) deepDiff(a[k], b[k], path ? `${path}.${k}` : k, out);
  return out;
}

// Closed-loop saturation benchmark: every connection sends and waits for the
// reply before sending the next message (no timers, so Windows timer
// resolution can't skew the offered rate). Server CPU is measured externally
// per process — this reports client-side throughput and latency percentiles.
async function bench(url, token, kind, durationSec, conns) {
  const cs = [];
  for (let i = 0; i < conns; i++) {
    const c = await open(url, token);
    c.send({ type: 'hello', protocol: 2, clientId: 'bench', resumeFrom: null });
    await c.next();
    cs.push(c);
  }
  let ok = 0;
  let errs = 0;
  const lats = [];
  const started = Date.now();
  const until = started + durationSec * 1000;
  await Promise.all(
    cs.map(async (c, idx) => {
      let n = 0;
      while (Date.now() < until) {
        const t0 = performance.now();
        if (kind === 'malformed') {
          // transport hot path: frame decode -> JSON parse fail -> error ack,
          // no engine, no persistence, no state growth on either side
          c.send('## not json ##');
        } else {
          n++;
          // full stack: validate -> engine (stale revision, refused every
          // time, state unchanged); on Rust this also persists the command.
          // 'reject' must be a protocol-valid decision or both sides only
          // measure schema-rejection (the first run did exactly that).
          c.send({
            type: 'permission_decision',
            commandId: `bench-${idx}-${n}-${Date.now()}`,
            permissionId: 'p1',
            decision: 'reject',
            revision: 2,
          });
        }
        const f = await c.next(5000).catch(() => null);
        const el = performance.now() - t0;
        if (!f || f.raw === undefined) {
          errs++;
          continue;
        }
        ok++;
        lats.push(el);
      }
    }),
  );
  const elapsedMs = Date.now() - started;
  lats.sort((a, b) => a - b);
  const q = (p) => (lats.length ? lats[Math.min(lats.length - 1, Math.floor((p / 100) * lats.length))] : 0);
  console.log(
    JSON.stringify({
      bench: {
        kind,
        conns,
        durationSec,
        elapsedMs,
        sent: ok,
        errs,
        perSec: +(ok / (elapsedMs / 1000)).toFixed(1),
        p50: +q(50).toFixed(2),
        p95: +q(95).toFixed(2),
        p99: +q(99).toFixed(2),
        max: +(lats[lats.length - 1] ?? 0).toFixed(2),
      },
    }),
  );
  for (const c of cs) c.closeNow();
}

async function churn(url, token, minutes, light) {
  const until = Date.now() + minutes * 60_000;
  let ok = 0;
  let fail = 0;
  while (Date.now() < until) {
    try {
      const c = await open(url, token);
      c.send({ type: 'hello', protocol: 2, clientId: 'churn', resumeFrom: null });
      await c.next();
      if (!light) {
        c.send({
          type: 'create_session',
          commandId: `churn-${Date.now()}`,
          agentId: 'a4',
          title: 'Churn',
          prompt: 'churn cycle',
        });
        await collectUntilAck(c);
      }
      c.closeNow();
      ok++;
    } catch {
      fail++;
    }
    await new Promise((r) => setTimeout(r, 5_000));
  }
  console.log(JSON.stringify({ churn: { ok, fail, minutes, light } }));
}

const compare = args.indexOf('--compare');
if (compare >= 0) {
  const a = JSON.parse(readFileSync(args[compare + 1], 'utf8'));
  const b = JSON.parse(readFileSync(args[compare + 2], 'utf8'));
  const diffs = deepDiff(a, b);
  if (diffs.length === 0) {
    console.log('IDENTICAL');
    process.exit(0);
  }
  console.log(`DIFFS (${diffs.length}${diffs.length >= 25 ? '+' : ''}):`);
  for (const d of diffs) console.log(`  ${d}`);
  process.exit(1);
}

const url = opt('--url');
const token = opt('--token');
if (!url || !token) {
  console.error('usage: --url ws://host:port --token T [--out f.json] [--compare a b] [--bench --kind malformed|command --duration 60 --conns 4] [--churn --light --minutes N]');
  process.exit(2);
}

if (args.includes('--bench')) {
  await bench(
    url,
    token,
    opt('--kind') ?? 'malformed',
    Number(opt('--duration') ?? 60),
    Number(opt('--conns') ?? 4),
  );
} else if (args.includes('--churn')) {
  await churn(url, token, Number(opt('--minutes') ?? 30), args.includes('--light'));
} else {
  const observations = await scenario(url, token);
  const json = JSON.stringify(observations, null, 1);
  const out = opt('--out');
  if (out) {
    writeFileSync(out, json);
    console.log(`wrote ${out} (${observations.length} observations)`);
  } else {
    console.log(json);
  }
}
