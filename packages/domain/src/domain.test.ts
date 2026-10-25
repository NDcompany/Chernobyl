import assert from 'node:assert/strict';
import test from 'node:test';
import {
  applyCommandLifecycle,
  canTransition,
  evaluatePermissionDecision,
  isTerminal,
  isUsable,
  isValidCapability,
  legacyDecisions,
  transitionCommand,
  type CommandTracker,
  type PermissionDecisionCode,
  type PermissionDecisionOption,
  type PermissionDecisionRequest,
} from './index';

/* ------------------------------------------------------------------ */
/* Command FSM                                                         */
/* ------------------------------------------------------------------ */

const TERMINAL = ['completed', 'failed', 'cancelled', 'expired', 'unknown'] as const;

test('command FSM accepts the documented lifecycle paths', () => {
  assert.ok(canTransition('created', 'queued'));
  assert.ok(canTransition('queued', 'accepted'));
  assert.ok(canTransition('accepted', 'running'));
  assert.ok(canTransition('accepted', 'completed')); // instant commands
  assert.ok(canTransition('running', 'unknown')); // unsafe reconciliation
  assert.ok(canTransition('running', 'cancelled'));
});

test('command FSM rejects illegal jumps and anything out of terminal states', () => {
  assert.equal(transitionCommand('created', 'running'), null);
  assert.equal(transitionCommand('accepted', 'queued'), null);
  assert.equal(transitionCommand('running', 'accepted'), null);
  assert.equal(transitionCommand('completed', 'running'), null);
  for (const t of TERMINAL) {
    assert.ok(isTerminal(t));
    for (const to of ['created', 'queued', 'accepted', 'running'] as const) {
      assert.equal(transitionCommand(t, to), null, `${t} → ${to} must be illegal`);
    }
  }
});

test('acceptance is never terminal', () => {
  assert.ok(!isTerminal('accepted'));
  assert.ok(!isTerminal('created'));
  assert.ok(!isTerminal('queued'));
  assert.ok(!isTerminal('running'));
});

/* ------------------------------------------------------------------ */
/* Permission evaluation (fail-closed)                                 */
/* ------------------------------------------------------------------ */

function pending(overrides: Partial<PermissionDecisionRequest> = {}): PermissionDecisionRequest {
  return {
    status: 'pending',
    expiresAt: Date.now() + 60_000,
    nativeRequestId: 'per_native_1',
    supportedDecisions: ['approve-once', 'approve-session', 'reject'],
    revision: 3,
    ...overrides,
  };
}

test('a complete, fresh request with matching revision decides', () => {
  const v = evaluatePermissionDecision(pending(), { decision: 'approve-once', revision: 3 });
  assert.deepEqual(v, { ok: true });
});

test('missing native metadata fails closed before anything else', () => {
  assert.equal(
    evaluatePermissionDecision(pending({ nativeRequestId: undefined }), {
      decision: 'reject',
      revision: 3,
    }).ok,
    false,
  );
  assert.equal(
    evaluatePermissionDecision(pending({ supportedDecisions: undefined }), {
      decision: 'reject',
      revision: 3,
    }).ok,
    false,
  );
  assert.equal(
    evaluatePermissionDecision(pending({ revision: undefined }), {
      decision: 'reject',
      revision: 3,
    }).ok,
    false,
  );
  const v = evaluatePermissionDecision(pending({ supportedDecisions: undefined }), {
    decision: 'reject',
    revision: 3,
  });
  assert.equal(!v.ok && v.code, 'incomplete-metadata');
});

test('resolved, expired and stale-revision requests are refused', () => {
  const resolved = evaluatePermissionDecision(pending({ status: 'approved' }), {
    decision: 'approve-once',
    revision: 3,
  });
  assert.ok(!resolved.ok && resolved.code === 'not-pending');

  const expired = evaluatePermissionDecision(
    pending({ expiresAt: Date.now() - 1 }),
    { decision: 'approve-once', revision: 3 },
    Date.now(),
  );
  assert.ok(!expired.ok && expired.code === 'expired');

  const stale = evaluatePermissionDecision(pending({ revision: 4 }), {
    decision: 'approve-once',
    revision: 3,
  });
  assert.ok(!stale.ok && stale.code === 'stale-revision');
});

test('a decision the runtime never offered is refused, never translated', () => {
  const limited = pending({ supportedDecisions: ['reject'] });
  const v = evaluatePermissionDecision(limited, { decision: 'approve-once', revision: 3 });
  assert.ok(!v.ok && v.code === 'unsupported-decision');
  // even with the right revision, the unsupported decision stays refused
  assert.equal(v.ok, false);
});

/* ADR-015 options — the same cases as the Rust authority tests */

function opt(
  id: string,
  decision: PermissionDecisionCode,
  scope: PermissionDecisionOption['scope'],
  takesMessage?: true,
): PermissionDecisionOption {
  return takesMessage ? { id, decision, scope, takesMessage } : { id, decision, scope };
}

/** Codex-shaped: a persistent amendment sits before the session grant; the plain deny takes a message. */
const CODEX_OPTIONS = [
  opt('accept', 'approve-once', 'once'),
  opt('amend', 'approve-session', 'persistent'),
  opt('session', 'approve-session', 'session'),
  opt('decline', 'reject', 'once', true),
  opt('cancel', 'reject', 'once'),
];

function withOptions(options: readonly PermissionDecisionOption[] = CODEX_OPTIONS) {
  return pending({ options, supportedDecisions: legacyDecisions(options) });
}

/** The id of the option a decision selects (undefined: no options), or the refusal code. */
function pick(
  request: PermissionDecisionRequest,
  decision: PermissionDecisionCode,
  optionId?: string,
  message?: string,
): string | undefined {
  const v = evaluatePermissionDecision(request, { decision, revision: 3, optionId, message });
  return v.ok ? v.option?.id : v.code;
}

test('option decision must match its bucket', () => {
  const p = withOptions();
  assert.equal(pick(p, 'approve-once', 'accept'), 'accept');
  // an explicit persistent grant is fine, even outside the legacy buckets
  const noSession = withOptions(CODEX_OPTIONS.filter((o) => o.id !== 'session'));
  assert.deepEqual(noSession.supportedDecisions, ['approve-once', 'reject']);
  assert.equal(pick(noSession, 'approve-session', 'amend'), 'amend');
  // the option's bucket must equal the decision sent
  assert.equal(pick(p, 'approve-once', 'decline'), 'unsupported-decision');
  assert.equal(pick(p, 'reject', 'amend'), 'unsupported-decision');
  assert.equal(pick(p, 'approve-session', 'accept'), 'unsupported-decision');
});

test('unknown option is unsupported', () => {
  assert.equal(pick(withOptions(), 'approve-once', 'nope'), 'unsupported-decision');
  // a request without options has no option to name
  assert.equal(pick(pending(), 'approve-once', 'accept'), 'unsupported-decision');
});

test('legacy decision never selects a persistent option', () => {
  const p = withOptions();
  // the first NON-persistent option of the bucket, skipping the amendment
  assert.equal(pick(p, 'approve-session'), 'session');
  assert.equal(pick(p, 'approve-once'), 'accept');
  // takesMessage does not disqualify: the message is optional
  assert.equal(pick(p, 'reject'), 'decline');
  // only a persistent grant left: refused even when the record (wrongly) lists approve-session
  const persistentOnly = pending({ options: CODEX_OPTIONS.filter((o) => o.id !== 'session') });
  assert.ok(persistentOnly.supportedDecisions?.includes('approve-session'));
  assert.equal(pick(persistentOnly, 'approve-session'), 'unsupported-decision');
  // requests without options behave exactly as before
  assert.equal(pick(pending(), 'approve-session'), undefined);
});

test('supported decisions are the legacy buckets', () => {
  assert.deepEqual(legacyDecisions(CODEX_OPTIONS), ['approve-once', 'approve-session', 'reject']);
  assert.deepEqual(
    legacyDecisions([
      opt('allow', 'approve-once', 'once'),
      opt('always', 'approve-session', 'persistent'),
      opt('deny', 'reject', 'once', true),
    ]),
    ['approve-once', 'reject'],
  );
});

test('a message needs an option that takes one, or a plain reject', () => {
  const p = withOptions();
  assert.equal(pick(p, 'reject', 'decline', 'use staging'), 'decline');
  // the legacy reject selects `decline`, which takes the message
  assert.equal(pick(p, 'reject', undefined, 'why'), 'decline');
  assert.equal(pick(p, 'reject', 'cancel', 'why'), 'unsupported-decision');
  assert.equal(pick(p, 'approve-once', undefined, 'why'), 'unsupported-decision');
  // without options: a reject may carry a message, an approval may not
  assert.equal(pick(pending(), 'reject', undefined, 'no'), undefined);
  assert.equal(pick(pending(), 'approve-once', undefined, 'ok'), 'unsupported-decision');
});

test('answers must fit the question detail', () => {
  const question = pending({
    detail: {
      kind: 'question',
      questions: [
        { id: 'q1', options: [{ label: 'Postgres' }, { label: 'SQLite' }], multiSelect: false, allowFreeText: false },
        { id: 'q2', options: [{ label: 'Unit tests' }, { label: 'Lint' }], multiSelect: true, allowFreeText: true },
      ],
    },
  });
  const verdict = (request: PermissionDecisionRequest, answers: Record<string, string[]>) =>
    evaluatePermissionDecision(request, { decision: 'approve-once', revision: 3, answers });
  const refused = { ok: false, code: 'unsupported-decision' };
  const ok = { q1: ['Postgres'], q2: ['Unit tests', 'Lint', 'e2e too'] };
  assert.deepEqual(verdict(question, ok), { ok: true });
  const bad: Record<string, string[]>[] = [
    { q3: ['Postgres'] }, // unknown question id
    { q1: ['MySQL'] }, // not an option, no free text
    { q1: ['Postgres', 'SQLite'] }, // one answer unless multiSelect
  ];
  for (const answers of bad) {
    assert.deepEqual(verdict(question, answers), refused, JSON.stringify(answers));
  }
  // answers only for a question detail
  assert.deepEqual(verdict(pending(), ok), refused);
  assert.deepEqual(verdict(pending({ detail: { kind: 'shell' } }), ok), refused);
  assert.deepEqual(verdict(pending({ detail: { kind: 'shell' } }), {}), { ok: true });
});

/* ------------------------------------------------------------------ */
/* Client tracker                                                      */
/* ------------------------------------------------------------------ */

test('tracker records acceptance, then running, then exactly one terminal', () => {
  let map: CommandTracker = {};
  map = applyCommandLifecycle(map, { type: 'command_progress', commandId: 'c1', state: 'accepted' });
  assert.equal(map.c1.state, 'accepted');
  map = applyCommandLifecycle(map, { type: 'command_progress', commandId: 'c1', state: 'running' });
  assert.equal(map.c1.state, 'running');
  map = applyCommandLifecycle(map, {
    type: 'command_result',
    commandId: 'c1',
    status: 'completed',
    sessionId: 's9',
  });
  assert.equal(map.c1.state, 'completed');
  assert.equal(map.c1.sessionId, 's9');
});

test('a terminal state is final — replayed or late events cannot move it', () => {
  const done: CommandTracker = {
    c1: { state: 'completed' },
  };
  const afterRunning = applyCommandLifecycle(done, {
    type: 'command_progress',
    commandId: 'c1',
    state: 'running',
  });
  assert.equal(afterRunning, done, 'ignored events must not allocate');
  const afterFailed = applyCommandLifecycle(done, {
    type: 'command_result',
    commandId: 'c1',
    status: 'failed',
    error: 'replayed failure',
  });
  assert.equal(afterFailed, done);
  assert.equal(done.c1.state, 'completed');
});

test('illegal transitions are ignored fail-closed', () => {
  const accepted: CommandTracker = { c1: { state: 'accepted' } };
  const back = applyCommandLifecycle(accepted, {
    type: 'command_progress',
    commandId: 'c1',
    state: 'accepted',
  });
  assert.equal(back, accepted, 'accepted → accepted is not a transition');
});

test('a terminal result with no prior progress boots the entry directly', () => {
  const map = applyCommandLifecycle({}, {
    type: 'command_result',
    commandId: 'c-late',
    status: 'unknown',
    error: 'host restarted before outcome',
  });
  assert.equal(map['c-late'].state, 'unknown');
  assert.equal(map['c-late'].error, 'host restarted before outcome');
});

test('malformed lifecycle events are ignored', () => {
  const base: CommandTracker = {};
  assert.equal(
    applyCommandLifecycle(base, { type: 'command_progress', commandId: 'c1', state: 'weird' }),
    base,
  );
  assert.equal(
    applyCommandLifecycle(base, { type: 'command_result', commandId: 'c1', status: 'weird' }),
    base,
  );
});

/* ------------------------------------------------------------------ */
/* Capabilities                                                        */
/* ------------------------------------------------------------------ */

test('capability validity requires detail for anything not available', () => {
  assert.ok(isValidCapability({ id: 'session.list', status: 'available' }));
  assert.ok(isValidCapability({ id: 'diff.read', status: 'unavailable', detail: 'not supported' }));
  assert.ok(!isValidCapability({ id: 'diff.read', status: 'unavailable' }));
  assert.ok(!isValidCapability({ id: '', status: 'available' }));
  assert.ok(isUsable({ id: 'a', status: 'available' }));
  assert.ok(!isUsable({ id: 'a', status: 'limited', detail: 'partial' }));
});
