/**
 * Integration tests: a real WebSocket client against the exact server the
 * entry point runs — auth gate, version gate, snapshots, command acks,
 * cross-client broadcast, and duplicate suppression over the wire.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import WebSocket from 'ws';
import {
  PROTOCOL_VERSION,
  type ClientMessage,
  type ServerMessage,
} from '@chernobyl/protocol';
import { createHost } from './host';

type Ack = Extract<ServerMessage, { type: 'ack' }>;
type HelloAck = Extract<ServerMessage, { type: 'hello_ack' }>;

interface TestClient {
  ws: WebSocket;
  messages: ServerMessage[];
  closed: Promise<{ code: number; reason: string }>;
  /** Strings are sent raw (malformed-frame tests); objects are JSON-encoded. */
  send(msg: string | object): void;
}

async function connect(url: string, headers?: Record<string, string>): Promise<TestClient> {
  const ws = new WebSocket(url, headers ? { headers } : undefined);
  const messages: ServerMessage[] = [];
  let resolveClosed!: (v: { code: number; reason: string }) => void;
  const closed = new Promise<{ code: number; reason: string }>((r) => (resolveClosed = r));
  ws.on('message', (raw) => messages.push(JSON.parse(raw.toString()) as ServerMessage));
  ws.on('close', (code, reason) => resolveClosed({ code, reason: reason.toString() }));
  ws.on('error', () => {}); // close handler reports the outcome
  await new Promise<void>((resolve, reject) => {
    ws.once('open', () => resolve());
    ws.once('error', reject);
  });
  return {
    ws,
    messages,
    closed,
    send: (m) => ws.send(typeof m === 'string' ? m : JSON.stringify(m)),
  };
}

function waitFor<T extends ServerMessage>(
  messages: ServerMessage[],
  pred: (m: ServerMessage) => boolean,
  timeoutMs = 2000,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const started = Date.now();
    const iv = setInterval(() => {
      const hit = messages.find(pred) as T | undefined;
      if (hit) {
        clearInterval(iv);
        resolve(hit);
      } else if (Date.now() - started > timeoutMs) {
        clearInterval(iv);
        reject(new Error('timed out waiting for a server message'));
      }
    }, 10);
  });
}

async function hello(c: TestClient): Promise<HelloAck> {
  c.send({ type: 'hello', clientId: 'itest-client', protocol: PROTOCOL_VERSION });
  return waitFor<HelloAck>(c.messages, (m) => m.type === 'hello_ack');
}

test('hello receives the full snapshot', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const c = await connect(host.url);
  const ack = await hello(c);

  assert.equal(ack.protocol, PROTOCOL_VERSION);
  assert.ok(ack.snapshot.hosts.length > 0);
  assert.ok(ack.snapshot.sessions.length > 0);
});

test('HTTP health endpoint reports counters', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const res = await fetch(`http://127.0.0.1:${host.port}/health`);
  const body = (await res.json()) as { ok: boolean; protocol: number };
  assert.equal(res.status, 200);
  assert.equal(body.ok, true);
  assert.equal(body.protocol, PROTOCOL_VERSION);
});

test('malformed frames get an error ack', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const c = await connect(host.url);
  c.send('this is not json');
  const ack = await waitFor<Ack>(c.messages, (m) => m.type === 'ack');
  assert.equal(ack.ok, false);
  assert.match(ack.error ?? '', /malformed/);
});

test('protocol mismatch is rejected with 4002', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const c = await connect(host.url);
  c.send({ type: 'hello', clientId: 'future-client', protocol: 999 });
  const ack = await waitFor<Ack>(c.messages, (m) => m.type === 'ack');
  assert.equal(ack.ok, false);
  assert.match(ack.error ?? '', /protocol mismatch/);
  assert.equal((await c.closed).code, 4002);
});

test('fractional and older protocol versions are rejected with 4002 (D7)', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  // 2.5 used to slip past the Rust gate via u32 truncation — both sides
  // must reject it, plus any version older than PROTOCOL_VERSION
  for (const protocol of [2.5, 1]) {
    const c = await connect(host.url);
    c.send({ type: 'hello', clientId: `d7-client-${protocol}`, protocol });
    const ack = await waitFor<Ack>(c.messages, (m) => m.type === 'ack');
    assert.equal(ack.ok, false, `v${protocol} must be rejected`);
    assert.match(ack.error ?? '', /protocol mismatch/);
    assert.equal((await c.closed).code, 4002, `v${protocol} must close 4002`);
  }
});

test('token gate: missing token closes 4001, query and header both pass', async (t) => {
  const host = await createHost({ port: 0, token: 'sekrit' });
  t.after(() => host.close());

  const refused = await connect(host.url);
  assert.equal((await refused.closed).code, 4001);

  const byQuery = await connect(`${host.url}/?token=sekrit`);
  assert.equal((await hello(byQuery)).protocol, PROTOCOL_VERSION);

  const byHeader = await connect(host.url, { authorization: 'Bearer sekrit' });
  assert.equal((await hello(byHeader)).protocol, PROTOCOL_VERSION);
});

test('commands are acked and broadcast to every other client', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const a = await connect(host.url);
  const b = await connect(host.url);
  await hello(a);
  await hello(b);

  const cmd: ClientMessage = {
    type: 'create_session',
    commandId: 'itest-1',
    agentId: 'a4',
    title: 'Integration task',
    prompt: 'Say hello over a real socket',
  };
  a.send(cmd);

  const ack = await waitFor<Ack>(a.messages, (m) => m.type === 'ack');
  assert.equal(ack.ok, true);
  assert.ok(ack.sessionId);

  const update = await waitFor<Extract<ServerMessage, { type: 'session_created' }>>(
    b.messages,
    (m) => m.type === 'session_created' && m.session.id === ack.sessionId,
  );
  assert.equal(update.session.title, 'Integration task');
});

test('replaying a commandId over the socket never runs it twice', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const c = await connect(host.url);
  await hello(c);

  const cmd = {
    type: 'create_session',
    commandId: 'itest-dup',
    agentId: 'a4',
    title: 'Dup task',
    prompt: 'x',
  };
  c.send(cmd);
  c.send(cmd);

  // both frames are answered with the SAME sessionId, and the second is
  // flagged as a duplicate — the command executed exactly once
  await waitFor<Ack>(c.messages, (m) => m.type === 'ack' && m.duplicate === true);
  await waitFor<Extract<ServerMessage, { type: 'session_created' }>>(
    c.messages,
    (m) => m.type === 'session_created',
  );
  const acks = c.messages.filter((m): m is Ack => m.type === 'ack' && m.commandId === 'itest-dup');
  assert.equal(acks.length, 2);
  assert.ok(acks[0].sessionId);
  assert.equal(acks[0].sessionId, acks[1].sessionId);
  assert.equal(acks.filter((a) => a.duplicate).length, 1);
});

/* ------------------------------------------------------------------ */
/* Phase 1 gate requirements (docs/decisions/PHASE1-GATE-REQUIREMENTS) */
/* ------------------------------------------------------------------ */

test('G1: the ack and the terminal outcome are separate frames', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const c = await connect(host.url);
  const helloAck = await hello(c);

  c.send({
    type: 'create_session',
    commandId: 'itest-life',
    agentId: 'a4',
    title: 'Lifecycle task',
    prompt: 'over a real socket',
  });

  // acceptance: persisted + routed — nothing about execution
  const ack = await waitFor<Ack>(c.messages, (m) => m.type === 'ack' && m.commandId === 'itest-life');
  assert.equal(ack.ok, true);

  // outcome: exactly one terminal, correlated by commandId
  const result = await waitFor<Extract<ServerMessage, { type: 'command_result' }>>(
    c.messages,
    (m) => m.type === 'command_result' && m.commandId === 'itest-life',
  );
  assert.equal(result.status, 'completed');
  assert.equal(result.sessionId, ack.sessionId);

  // envelope: strictly increasing seqs, all newer than the snapshot's lastSeq
  const enveloped = c.messages.filter((m): m is ServerMessage & { seq: number } => 'seq' in m);
  assert.ok(enveloped.length > 0);
  for (let i = 1; i < enveloped.length; i++) {
    assert.ok(enveloped[i].seq > enveloped[i - 1].seq, `seq went backwards at ${i}`);
  }
  assert.ok(enveloped.every((m) => m.seq > helloAck.lastSeq));
});

test('G1/G2: a permission decision over the wire is revision-checked', async (t) => {
  const host = await createHost({ port: 0 });
  t.after(() => host.close());

  const c = await connect(host.url);
  const { snapshot } = await hello(c);
  const permission = snapshot.permissions.find((p) => p.status === 'pending');
  assert.ok(permission);

  // native fields ride the wire (G2)
  assert.equal(permission.nativeRequestId, permission.id);
  assert.ok(permission.supportedDecisions.includes('reject'));
  assert.equal(permission.revision, 1);

  // stale revision → refused fail-closed, and NO resolution event is emitted
  c.send({
    type: 'permission_decision',
    commandId: 'itest-stale-rev',
    permissionId: permission.id,
    decision: 'reject',
    revision: permission.revision + 1,
  });
  const stale = await waitFor<Ack>(
    c.messages,
    (m) => m.type === 'ack' && m.commandId === 'itest-stale-rev',
  );
  assert.equal(stale.ok, false);
  assert.equal(
    c.messages.some((m) => m.type === 'permission_resolved'),
    false,
  );

  // fresh revision → resolved, and the bumped revision rides back on the wire
  c.send({
    type: 'permission_decision',
    commandId: 'itest-fresh-rev',
    permissionId: permission.id,
    decision: 'reject',
    revision: permission.revision,
  });
  const ack = await waitFor<Ack>(
    c.messages,
    (m) => m.type === 'ack' && m.commandId === 'itest-fresh-rev',
  );
  assert.equal(ack.ok, true);
  const resolved = await waitFor<Extract<ServerMessage, { type: 'permission_resolved' }>>(
    c.messages,
    (m) => m.type === 'permission_resolved' && m.permission.id === permission.id,
  );
  assert.equal(resolved.permission.status, 'rejected');
  assert.equal(resolved.permission.revision, 2);
});
