import assert from 'node:assert/strict';
import test from 'node:test';
import {
  MockEngine,
  parseClientMessage,
  type ServerMessage,
} from '@chernobyl/protocol';

type Ack = Extract<ServerMessage, { type: 'ack' }>;

function makeEngine(): { engine: MockEngine; events: ServerMessage[] } {
  const events: ServerMessage[] = [];
  const engine = new MockEngine((m) => events.push(m));
  return { engine, events };
}

function ackOf(msg: ServerMessage | undefined): Ack | undefined {
  return msg && msg.type === 'ack' ? msg : undefined;
}

test('duplicate command ids do not create duplicate sessions', () => {
  const { engine } = makeEngine();
  const before = engine.snapshot().sessions.length;
  const cmd = {
    type: 'create_session' as const,
    commandId: 'cmd-1',
    agentId: 'a4',
    title: 'Duplicate test',
    prompt: 'Say hello',
  };

  const first = ackOf(engine.handle(cmd));
  const second = ackOf(engine.handle(cmd));

  assert.equal(first?.ok, true);
  assert.equal(second?.ok, true);
  assert.equal(second?.duplicate, true);
  assert.equal(first?.sessionId, second?.sessionId);
  assert.equal(engine.snapshot().sessions.length, before + 1);
  engine.dispose();
});

test('approving a request resolves it and resumes the session', () => {
  const { engine, events } = makeEngine();
  const permission = engine.snapshot().permissions.find((p) => p.status === 'pending');
  assert.ok(permission);

  const reply = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-1',
      permissionId: permission.id,
      decision: 'approve-once',
    }),
  );

  assert.equal(reply?.ok, true);
  assert.equal(permission.status, 'approved');
  assert.equal(permission.decision, 'approve-once');
  const session = engine.snapshot().sessions.find((s) => s.id === permission.sessionId);
  assert.equal(session?.status, 'running');
  assert.ok(events.some((e) => e.type === 'permission_resolved'));
  engine.dispose();
});

test('a stale permission request can no longer be decided', () => {
  const { engine } = makeEngine();
  const permission = engine.snapshot().permissions.find((p) => p.status === 'pending');
  assert.ok(permission);
  permission.expiresAt = Date.now() - 1;

  engine.tick();
  assert.equal(permission.status, 'stale');

  const reply = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-2',
      permissionId: permission.id,
      decision: 'approve-once',
    }),
  );
  assert.equal(reply?.ok, false);
  assert.equal(permission.status, 'stale');
  engine.dispose();
});

test('rejecting a request interrupts its session', () => {
  const { engine } = makeEngine();
  const permission = engine.snapshot().permissions.find(
    (p) => p.status === 'pending' && p.sessionId === 's1',
  );
  assert.ok(permission);

  const reply = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-3',
      permissionId: permission.id,
      decision: 'reject',
    }),
  );

  assert.equal(reply?.ok, true);
  assert.equal(permission.status, 'rejected');
  const session = engine.snapshot().sessions.find((s) => s.id === 's1');
  assert.equal(session?.status, 'interrupted');
  assert.ok(session?.output.some((l) => l.stream === 'system' && l.text.includes('Rejected')));
  engine.dispose();
});

test('malformed frames are rejected at the protocol boundary', () => {
  assert.equal(parseClientMessage('{"type":"interrupt"}'), null);
  assert.equal(parseClientMessage('not json'), null);
  assert.equal(parseClientMessage('{"type":"nope"}'), null);
  assert.ok(parseClientMessage('{"type":"ping","commandId":"c1"}'));
});

test('a freshly created session starts running its plan immediately', () => {
  const { engine } = makeEngine();
  const ack = ackOf(
    engine.handle({
      type: 'create_session',
      commandId: 'cmd-run',
      agentId: 'a4',
      title: 'Run test',
      prompt: 'Do the thing',
    }),
  );
  assert.equal(ack?.ok, true);

  const session = engine.snapshot().sessions.find((s) => s.id === ack?.sessionId);
  assert.equal(session?.status, 'running');

  // the plan is already executing — a second prompt is refused, not queued
  const second = ackOf(
    engine.handle({
      type: 'submit_prompt',
      commandId: 'cmd-run-2',
      sessionId: ack!.sessionId!,
      text: 'again',
    }),
  );
  assert.equal(second?.ok, false);
  assert.match(second?.error ?? '', /already running/);
  engine.dispose();
});

test('interrupting a waiting session expires its pending ask', () => {
  const { engine } = makeEngine();
  const snap = engine.snapshot();
  const waiting = snap.sessions.find((s) => s.status === 'waiting');
  assert.ok(waiting);
  const ask = snap.permissions.find(
    (p) => p.sessionId === waiting.id && p.status === 'pending',
  );
  assert.ok(ask);

  const ack = ackOf(
    engine.handle({ type: 'interrupt', commandId: 'cmd-int', sessionId: waiting.id }),
  );
  assert.equal(ack?.ok, true);

  const after = engine.snapshot();
  assert.equal(after.sessions.find((s) => s.id === waiting.id)?.status, 'interrupted');
  assert.equal(after.permissions.find((p) => p.id === ask.id)?.status, 'stale');
  engine.dispose();
});
