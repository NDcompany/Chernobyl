/**
 * ADR-015 step 1: live-session validators, golden fixtures and the transcript
 * reducer. The fixtures are the same files services/companion-rs reads with
 * include_str!, so the TS and Rust stacks agree on every shape tested here.
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import {
  FEATURE_TRANSCRIPT,
  applyTranscript,
  emptyTranscriptView,
  isClientMessage,
  isServerMessage,
  type LiveServerMessage,
  type TranscriptOp,
  type TranscriptView,
} from './index';

const fixture = (name: string): unknown =>
  JSON.parse(readFileSync(join(__dirname, '..', 'fixtures', 'live', name), 'utf8'));

/* ------------------------------------------------------------------ */
/* Validators (§3.4)                                                   */
/* ------------------------------------------------------------------ */

test('live client fixtures: valid messages pass, invalid ones fail', () => {
  for (const msg of fixture('client-valid.json') as unknown[]) {
    assert.ok(isClientMessage(msg), JSON.stringify(msg));
  }
  for (const { why, msg } of fixture('client-invalid.json') as Array<{ why: string; msg: unknown }>) {
    assert.equal(isClientMessage(msg), false, why);
  }
});

test('§3.4 bounds: the maximum passes, one more unit fails', () => {
  const mcp = (server: string) => ({ type: 'mcp_auth', commandId: 'c', sessionId: 's', server, flow: 'desktop' });
  assert.ok(isClientMessage(mcp('s'.repeat(256))));
  assert.ok(!isClientMessage(mcp('s'.repeat(257))));
  const reconnect = (server: string) => ({ type: 'mcp_reconnect', commandId: 'c', sessionId: 's', server });
  assert.ok(isClientMessage(reconnect('s'.repeat(256))));
  assert.ok(!isClientMessage(reconnect('s'.repeat(257))));
  const callback = (len: number) => ({
    type: 'mcp_auth_callback',
    commandId: 'c',
    sessionId: 's',
    server: 'x',
    callbackUrl: `http://${'a'.repeat(len - 'http://'.length)}`,
  });
  assert.ok(isClientMessage(callback(4096)));
  assert.ok(!isClientMessage(callback(4097)));
  const epoch = (e: string) => ({ type: 'session_subscribe', commandId: 'c', sessionId: 's', epoch: e });
  assert.ok(isClientMessage(epoch('e'.repeat(64))));
  assert.ok(!isClientMessage(epoch('e'.repeat(65))));
  // submit_prompt: 32,000 UTF-16 units, the phone's zod unit (an emoji counts
  // 2 here; the Rust companion counts chars, so it is never stricter)
  const prompt = (text: string) => ({ type: 'submit_prompt', commandId: 'c', sessionId: 's', text });
  assert.ok(isClientMessage(prompt('p'.repeat(32_000))));
  assert.ok(!isClientMessage(prompt('p'.repeat(32_001))));
  assert.ok(isClientMessage(prompt('é'.repeat(32_000))));
  assert.ok(isClientMessage(prompt('\u{1F600}'.repeat(16_000))));
  assert.ok(!isClientMessage(prompt(`${'\u{1F600}'.repeat(16_000)}x`)));
  // afterSeq: non-negative safe integers only
  const after = (afterSeq: unknown) => ({ type: 'session_subscribe', commandId: 'c', sessionId: 's', afterSeq });
  for (const good of [0, 3, 3.0, Number.MAX_SAFE_INTEGER]) assert.ok(isClientMessage(after(good)), String(good));
  for (const bad of [-1, 1.5, Infinity, NaN, '3', null, Number.MAX_SAFE_INTEGER + 1]) {
    assert.ok(!isClientMessage(after(bad)), String(bad));
  }
});

test('permission_decision accepts optionId, message and answers within bounds', () => {
  const decision = (extra: object) => ({
    type: 'permission_decision',
    commandId: 'c',
    permissionId: 'p',
    decision: 'approve-once',
    revision: 1,
    ...extra,
  });
  assert.ok(isClientMessage(decision({})), 'the legacy v2 shape is unchanged');
  assert.ok(isClientMessage(decision({ optionId: 'o'.repeat(128), message: 'm'.repeat(4096) })));
  assert.ok(!isClientMessage(decision({ optionId: 'o'.repeat(129) })));
  assert.ok(!isClientMessage(decision({ message: 'm'.repeat(4097) })));
  const full = Object.fromEntries(
    Array.from({ length: 8 }, (_, i) => [`q${i}`, Array<string>(16).fill('a'.repeat(1024))]),
  );
  assert.ok(isClientMessage(decision({ answers: full })));
  assert.ok(!isClientMessage(decision({ answers: { ...full, q8: [] } })));
  assert.ok(!isClientMessage(decision({ answers: { q: Array<string>(17).fill('a') } })));
  assert.ok(!isClientMessage(decision({ answers: { q: ['a'.repeat(1025)] } })));
});

test('server fixtures validate: transcript, snapshot, permission extras, hello_ack features', () => {
  for (const name of ['transcript.json', 'transcript-snapshot.json', 'permission-requested.json', 'hello-ack.json']) {
    assert.ok(isServerMessage(fixture(name)), name);
  }
  const hello = fixture('hello-ack.json') as Record<string, unknown>;
  assert.deepEqual(hello.features, [FEATURE_TRANSCRIPT]);
  assert.ok(!isServerMessage({ ...hello, features: FEATURE_TRANSCRIPT }), 'features must be an array');
  const { features: _omitted, ...legacyHello } = hello;
  assert.ok(isServerMessage(legacyHello), 'features stays optional');
  // malformed transcript frames are dropped at the boundary
  const batch = fixture('transcript.json') as Record<string, unknown>;
  assert.ok(!isServerMessage({ ...batch, epoch: '' }));
  assert.ok(!isServerMessage({ ...batch, ops: [{ seq: -1, op: 'item' }] }));
  assert.ok(!isServerMessage({ ...batch, ops: [{ seq: 1 }] }));
  const permission = (fixture('permission-requested.json') as { permission: Record<string, unknown> }).permission;
  const options = permission.options as Array<{ decision: string }>;
  assert.equal(options.length, 4);
  assert.ok(options.some((o) => o.decision === 'reject'), 'options always hold a reject');
});

/* ------------------------------------------------------------------ */
/* applyTranscript                                                     */
/* ------------------------------------------------------------------ */

const EPOCH = '9f86d081884c7d65';
const SNAPSHOT = fixture('transcript-snapshot.json') as LiveServerMessage;
const BATCH = fixture('transcript.json') as LiveServerMessage;

const fromSnapshot = (): TranscriptView => applyTranscript(emptyTranscriptView(), SNAPSHOT).view;
const batch = (...ops: TranscriptOp[]): LiveServerMessage => ({
  type: 'transcript',
  sessionId: 'ses_live_1',
  epoch: EPOCH,
  ops,
});

test('applyTranscript: snapshot replaces', () => {
  const { view, resubscribe } = applyTranscript(emptyTranscriptView(), SNAPSHOT);
  assert.equal(resubscribe, false);
  assert.equal(view.epoch, EPOCH);
  assert.equal(view.seq, 3);
  assert.deepEqual(view.items.map((i) => i.id), ['u_1', 'msg_00#0', 'plan', 'n_1']);
  assert.equal(view.index.get('plan'), 2);
  assert.equal(view.turns.get('turn_1')?.status, 'running');
  assert.equal(view.state.status, 'running');
  // a later snapshot (lag, restart) wins outright, even with a lower seq
  const next = applyTranscript(view, {
    type: 'transcript_snapshot',
    sessionId: 'ses_live_1',
    epoch: 'feedfacecafebeef',
    seq: 0,
    items: [],
    truncatedBefore: true,
    turns: [],
    state: { status: 'idle' },
    mcp: [],
  });
  assert.equal(next.resubscribe, false);
  assert.equal(next.view.epoch, 'feedfacecafebeef');
  assert.equal(next.view.seq, 0);
  assert.equal(next.view.items.length, 0);
  assert.equal(next.view.index.size, 0);
  assert.equal(next.view.truncatedBefore, true);
});

test('applyTranscript: the golden batch applies all five op kinds', () => {
  const { view, resubscribe } = applyTranscript(fromSnapshot(), BATCH);
  assert.equal(resubscribe, false);
  assert.equal(view.seq, 10);
  const text = view.items[view.index.get('msg_01#0')!];
  assert.ok(text.kind === 'text' && text.text === 'Creating a.txt now.');
  const tool = view.items[view.index.get('toolu_01')!];
  assert.ok(tool.kind === 'tool' && tool.tool.output === 'File created successfully');
  assert.equal(view.turns.get('turn_1')?.status, 'completed');
  assert.equal(view.turns.get('turn_1')?.costUsd, 0.0123);
  assert.equal(view.state.status, 'idle');
  assert.deepEqual(view.mcp.map((s) => s.status), ['needs-auth', 'pending', 'connected']);
});

test('applyTranscript: appends', () => {
  const start = applyTranscript(fromSnapshot(), BATCH).view;
  const { view } = applyTranscript(start, batch(
    { seq: 11, op: 'append', id: 'msg_01#0', field: 'text', text: ' Done.' },
    { seq: 12, op: 'append', id: 'toolu_01', field: 'input', text: '!' },
    { seq: 13, op: 'append', id: 'toolu_01', field: 'output', text: '\nok' },
    { seq: 14, op: 'append', id: 'msg_01#0', field: 'output', text: 'wrong field' },
  ));
  assert.equal(view.seq, 14);
  const text = view.items[view.index.get('msg_01#0')!];
  assert.ok(text.kind === 'text' && text.text === 'Creating a.txt now. Done.');
  const tool = view.items[view.index.get('toolu_01')!];
  assert.ok(tool.kind === 'tool');
  assert.equal(tool.tool.input, '{"file_path":"a.txt","content":"hi"}!');
  assert.equal(tool.tool.output, 'File created successfully\nok');
  // pure: the previous view is untouched
  const before = start.items[start.index.get('msg_01#0')!];
  assert.ok(before.kind === 'text' && before.text === 'Creating a.txt now.');
});

test('applyTranscript: upserts', () => {
  const start = fromSnapshot();
  const { view } = applyTranscript(start, batch(
    { seq: 4, op: 'item', item: { id: 'u_1', ts: 1, kind: 'user', text: 'edited prompt' } },
    { seq: 5, op: 'item', item: { id: 'msg_02#0', ts: 2, kind: 'text', text: 'new', done: true } },
    { seq: 6, op: 'turn', turn: { id: 'turn_1', status: 'interrupted', startedAt: 0, endedAt: 9 } },
    { seq: 7, op: 'turn', turn: { id: 'turn_2', status: 'running', startedAt: 10 } },
  ));
  assert.deepEqual(view.items.map((i) => i.id), ['u_1', 'msg_00#0', 'plan', 'n_1', 'msg_02#0']);
  const user = view.items[0];
  assert.ok(user.kind === 'user' && user.text === 'edited prompt', 'replaced in place');
  assert.equal(view.index.get('msg_02#0'), 4);
  assert.equal(view.turns.get('turn_1')?.status, 'interrupted');
  assert.equal(view.turns.get('turn_2')?.status, 'running');
  assert.equal(start.items.length, 4, 'the input view keeps its items');
});

test('applyTranscript: ignores append to unknown item', () => {
  const start = fromSnapshot();
  const { view, resubscribe } = applyTranscript(start, batch(
    { seq: 4, op: 'append', id: 'trimmed-out', field: 'text', text: 'lost' },
  ));
  assert.equal(resubscribe, false);
  assert.equal(view.seq, 4, 'the op still advances the cursor');
  assert.deepEqual(view.items, start.items);
});

test('applyTranscript: gap or new epoch asks to resubscribe', () => {
  const start = fromSnapshot();
  const text = (seq: number): TranscriptOp => ({ seq, op: 'append', id: 'u_1', field: 'text', text: '+' });
  // first fresh op is not seq + 1: nothing applies
  const gap = applyTranscript(start, batch(text(5)));
  assert.deepEqual(gap, { view: start, resubscribe: true });
  // another epoch (companion restart): not applied at all
  const epoch = applyTranscript(start, { ...batch(text(4)), epoch: 'feedfacecafebeef' });
  assert.deepEqual(epoch, { view: start, resubscribe: true });
  // a fresh view has no epoch yet: it waits for its snapshot
  assert.equal(applyTranscript(emptyTranscriptView(), batch(text(1))).resubscribe, true);
  // a gap inside a batch keeps what applied, then asks for the rest
  const partial = applyTranscript(start, batch(text(4), text(6)));
  assert.equal(partial.resubscribe, true);
  assert.equal(partial.view.seq, 4);
  const user = partial.view.items[0];
  assert.ok(user.kind === 'user' && user.text.endsWith('hi+'));
});

test('applyTranscript: duplicate seq skipped', () => {
  const start = fromSnapshot();
  const text = (seq: number): TranscriptOp => ({ seq, op: 'append', id: 'u_1', field: 'text', text: `#${seq}` });
  const replay = applyTranscript(start, batch(text(2), text(3)));
  assert.equal(replay.resubscribe, false);
  assert.equal(replay.view, start, 'all duplicates: the very same view');
  const overlap = applyTranscript(start, batch(text(3), text(4), text(4)));
  assert.equal(overlap.view.seq, 4);
  const user = overlap.view.items[0];
  assert.ok(user.kind === 'user' && user.text === 'Create a.txt containing hi#4');
});

test('applyTranscript: unknown item kinds are kept but flagged', () => {
  const { view } = applyTranscript(fromSnapshot(), batch(
    // a newer companion's kind, and a known kind that lost its body
    { seq: 4, op: 'item', item: { id: 'h_1', ts: 5, kind: 'hologram', depth: 3 } as never },
    { seq: 5, op: 'item', item: { id: 'x_1', ts: 6, kind: 'text' } as never },
    { seq: 6, op: 'append', id: 'h_1', field: 'text', text: 'ignored' },
    { seq: 7, op: 'teleport', id: 'h_1' } as never,
  ));
  assert.equal(view.seq, 7, 'an unknown op kind still counts');
  const unknown = view.items[view.index.get('h_1')!];
  assert.equal(unknown.kind, 'unknown');
  assert.ok(unknown.kind === 'unknown' && unknown.originalKind === 'hologram');
  assert.equal(view.items[view.index.get('x_1')!].kind, 'unknown');
});
