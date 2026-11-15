/**
 * Unit tests for the conversation-view helpers. Run with `npm test -w @chernobyl/mobile`
 * (`node --test` with Node's built-in TypeScript type stripping, no extra dependencies).
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { PermissionOption, PermissionRequest, TranscriptItem, TurnState } from '@chernobyl/protocol';
import {
  approvalButtons,
  buildTranscript,
  collectAnswers,
  diffCounts,
  formatCost,
  formatDuration,
  formatTokens,
  isCallbackUrl,
  mcpSummary,
  modeLabel,
  parseDiff,
  parseInline,
  parseMarkdown,
  replyStarts,
  rowIndexFor,
  turnFooter,
  type MdBlock,
} from './transcript.ts';

const visible = (blocks: MdBlock[]) =>
  blocks
    .map((b) =>
      b.kind === 'code'
        ? b.text
        : b.kind === 'list'
          ? b.items.map((it) => it.spans.map((s) => s.text).join('')).join('\n')
          : b.kind === 'rule'
            ? '---'
            : b.spans.map((s) => s.text).join(''),
    )
    .join('\n');

test('markdown: blocks, inline styles and list numbering', () => {
  const blocks = parseMarkdown(
    '# Title\n\nSome **bold**, *italic* and `code`.\nSecond line\n\n- one\n  - nested\n- two\n\n1. a\n1. b\n\n---\n\n```ts\nconst x = 1;\n```',
  );
  assert.deepEqual(
    blocks.map((b) => b.kind),
    ['heading', 'paragraph', 'list', 'list', 'rule', 'code'],
  );
  const [heading, para, bullets, numbers, , code] = blocks;
  assert.ok(heading.kind === 'heading' && heading.level === 1);
  assert.ok(para.kind === 'paragraph');
  assert.deepEqual(para.spans, [
    { text: 'Some ' },
    { text: 'bold', bold: true },
    { text: ', ' },
    { text: 'italic', italic: true },
    { text: ' and ' },
    { text: 'code', code: true },
    { text: '.\nSecond line' },
  ]);
  assert.ok(bullets.kind === 'list');
  assert.deepEqual(
    bullets.items.map((it) => [it.marker, it.depth]),
    [['•', 0], ['•', 1], ['•', 0]],
  );
  assert.ok(numbers.kind === 'list');
  assert.deepEqual(
    numbers.items.map((it) => it.marker),
    ['1.', '2.'],
  );
  assert.ok(code.kind === 'code');
  assert.deepEqual([code.lang, code.text, code.open], ['ts', 'const x = 1;', false]);
});

test('markdown: an unclosed fence is an open code block to the end', () => {
  for (const streaming of [true, false]) {
    const blocks = parseMarkdown('Intro\n```js\nlet a = 1;\nlet b', streaming);
    const last = blocks[blocks.length - 1];
    assert.ok(last.kind === 'code');
    assert.deepEqual([last.lang, last.text, last.open], ['js', 'let a = 1;\nlet b', true]);
  }
  // `**` inside a fence is code, not emphasis
  const fenced = parseMarkdown('```\n**not bold**\n```');
  assert.ok(fenced[0].kind === 'code' && fenced[0].text === '**not bold**');
});

test('markdown: streaming fragments render as if closed, settled text stays literal', () => {
  assert.deepEqual(parseInline('Hello **wor', true), [{ text: 'Hello ' }, { text: 'wor', bold: true }]);
  assert.deepEqual(parseInline('Hello **wor', false), [{ text: 'Hello **wor' }]);
  assert.deepEqual(parseInline('Hello **', true), [{ text: 'Hello ' }]);
  assert.deepEqual(parseInline('Use `npm', true), [{ text: 'Use ' }, { text: 'npm', code: true }]);
  assert.deepEqual(parseInline('Use `npm', false), [{ text: 'Use `npm' }]);
  // a half-typed fence or list marker on the last line waits for more text
  assert.deepEqual(parseMarkdown('Para\n``', true).map((b) => b.kind), ['paragraph']);
  assert.equal(visible(parseMarkdown('Para\n-', true)), 'Para');

  // no flicker: while `**bold** done` streams in, the bold text only grows
  const doc = '**bold** done';
  const boldText = [...doc].map((_, n) =>
    parseInline(doc.slice(0, n + 1), true)
      .filter((s) => s.bold)
      .map((s) => s.text)
      .join(''),
  );
  for (let n = 1; n < boldText.length; n++) assert.ok(boldText[n].startsWith(boldText[n - 1]), `prefix ${n}`);
  assert.equal(boldText[boldText.length - 1], 'bold');
});

test('markdown: streamed in char by char, visible text only grows and never throws', () => {
  const doc =
    '## Plan\n\nI will **edit `src/a.ts`** and _run_ the [tests](https://example.com/t).\n\n' +
    '1. First *step*\n2. Second\n   ```bash\n   npm test -- --watch\n   ```\n\n---\n\n' +
    '\\*escaped\\* __init__ snake_case ***both*** `` a`b `` see https://x.dev/p. Done.';
  let prev = '';
  for (let n = 0; n <= doc.length; n++) {
    const now = visible(parseMarkdown(doc.slice(0, n), true));
    assert.ok(now.startsWith(prev), `prefix ${n}: ${JSON.stringify(prev)} -> ${JSON.stringify(now)}`);
    prev = now;
    parseMarkdown(doc.slice(0, n), false);
  }
  assert.equal(visible(parseMarkdown(doc, true)), visible(parseMarkdown(doc)));
  assert.match(visible(parseMarkdown(doc)), /\*escaped\* init snake_case both a`b see https:\/\/x\.dev\/p\. Done\./);
});

test('markdown: underscores inside words and arithmetic stay literal, also mid-stream', () => {
  for (const streaming of [false, true]) {
    assert.deepEqual(parseInline('call my_var_name now', streaming), [{ text: 'call my_var_name now' }]);
    assert.deepEqual(parseInline('2 * 3 = 6 and 2*3', streaming), [{ text: '2 * 3 = 6 and 2*3' }]);
  }
  assert.deepEqual(parseInline('snake_case_', true), [{ text: 'snake_case_' }]);
});

test('markdown: links show the full URL; only http(s) is tappable; no HTML', () => {
  assert.deepEqual(parseInline('[docs](https://x.dev/a)'), [{ text: 'docs (https://x.dev/a)', url: 'https://x.dev/a' }]);
  assert.deepEqual(parseInline('see https://x.dev/b.'), [
    { text: 'see ' },
    { text: 'https://x.dev/b', url: 'https://x.dev/b' },
    { text: '.' },
  ]);
  for (const hostile of ['[click](javascript:alert(1))', '[a](file:///etc/passwd)', '<img src=x onerror=alert(1)>', '![i](https://t.co/p.png)']) {
    const spans = parseInline(hostile);
    const urls = spans.filter((s) => s.url).map((s) => s.url);
    assert.ok(urls.every((u) => u === 'https://t.co/p.png'), hostile);
  }
  // an image is never an image: its URL shows as text
  assert.match(parseInline('![i](https://t.co/p.png)').map((s) => s.text).join(''), /i \(https:\/\/t\.co\/p\.png\)/);
  assert.equal(parseInline('<b>x</b>')[0].text, '<b>x</b>');
  // a bidi override or zero-width char inside the target: not tappable
  for (const ch of ['‮', '​', '⁦']) {
    const spans = parseInline(`[bank](https://bank.example${ch}moc.live/x)`);
    assert.ok(spans.every((s) => !s.url), JSON.stringify(spans));
  }
});

test('markdown: hostile input parses in linear time', () => {
  const started = Date.now();
  for (const s of ['*a '.repeat(40_000), '['.repeat(100_000), '`` `'.repeat(30_000), '_a '.repeat(40_000)]) {
    parseMarkdown(s, true);
    parseMarkdown(s, false);
  }
  assert.ok(Date.now() - started < 3000, `took ${Date.now() - started} ms`);
});

test('diff: lines classify by first character; headers only before the first hunk', () => {
  const lines = parseDiff(
    'diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,4 +1,3 @@\n keep\n-old\n+new\n--- sql comment removed\n\\ No newline at end of file\n',
  );
  assert.deepEqual(
    lines.map((l) => l.kind),
    ['meta', 'meta', 'meta', 'meta', 'hunk', 'context', 'del', 'add', 'del', 'meta'],
  );
  assert.deepEqual(diffCounts(lines), { additions: 1, deletions: 2 });
  assert.deepEqual(
    parseDiff('+a\r\n-b\r\n c').map((l) => [l.kind, l.text]),
    [['add', '+a'], ['del', '-b'], ['context', ' c']],
  );
});

test('diff: a one-line Write (hunk count omitted, no trailing newline) keeps its added line', () => {
  const lines = parseDiff('@@ -0,0 +1 @@\n+hi');
  assert.deepEqual(lines.map((l) => [l.kind, l.text]), [['hunk', '@@ -0,0 +1 @@'], ['add', '+hi']]);
  assert.deepEqual(diffCounts(lines), { additions: 1, deletions: 0 });
});

test('footer: done with duration, tokens and optional cost; stopped; error', () => {
  const turn: TurnState = {
    id: 't1',
    status: 'completed',
    startedAt: 1_000,
    endedAt: 13_400,
    usage: { inputTokens: 3_000, outputTokens: 1_200, cacheReadTokens: 90_000 },
    costUsd: 0.0123,
  };
  assert.deepEqual(turnFooter(turn), { text: 'Done · 12.4 s · 4.2k tokens · $0.01', tone: 'done' });
  assert.equal(turnFooter({ ...turn, costUsd: undefined })?.text, 'Done · 12.4 s · 4.2k tokens');
  // a mirrored PC turn: endedAt = startedAt, zero usage, no cost → just "Done"
  const mirrored = { ...turn, endedAt: turn.startedAt, usage: { inputTokens: 0, outputTokens: 0 }, costUsd: undefined };
  assert.equal(turnFooter(mirrored)?.text, 'Done');
  assert.equal(turnFooter({ ...mirrored, endedAt: undefined, usage: undefined, costUsd: 0 })?.text, 'Done');
  assert.deepEqual(turnFooter({ ...turn, status: 'interrupted' }), { text: 'Stopped', tone: 'stopped' });
  assert.deepEqual(turnFooter({ ...turn, status: 'failed', error: 'Claude Code exited with code 1' }), {
    text: 'Claude Code exited with code 1',
    tone: 'error',
  });
  assert.equal(turnFooter({ ...turn, status: 'running' }), null);
  assert.equal(formatDuration(95_000), '1m 35s');
  assert.equal(formatDuration(59_960), '59.9 s');
  assert.equal(formatTokens(950), '950 tokens');
  assert.equal(formatTokens(2_000_000), '2M tokens');
  assert.equal(formatCost(0.004), '<$0.01');
  assert.equal(formatCost(1.5), '$1.50');
});

const opt = (o: Partial<PermissionOption> & Pick<PermissionOption, 'id' | 'decision'>): PermissionOption => ({
  label: o.id,
  scope: 'once',
  ...o,
});

test('approval: legacy supportedDecisions when options are absent or empty', () => {
  for (const options of [undefined, []]) {
    const b = approvalButtons({ options, supportedDecisions: ['approve-once', 'approve-session', 'reject'] });
    assert.deepEqual(b.primary, { key: 'approve-once', label: 'Allow', decision: 'approve-once', scope: 'once' });
    assert.deepEqual(b.secondary, { key: 'reject', label: 'Deny', decision: 'reject', scope: 'once' });
    assert.deepEqual(
      b.more.map((c) => [c.label, c.decision, c.optionId]),
      [['Allow for this session', 'approve-session', undefined]],
    );
  }
  const rejectOnly = approvalButtons({ supportedDecisions: ['reject'] });
  assert.equal(rejectOnly.primary, undefined);
  assert.equal(rejectOnly.secondary?.decision, 'reject');
});

test('approval: options map to Allow / Deny / More with verbatim labels', () => {
  const b = approvalButtons(
    {
      supportedDecisions: ['approve-once', 'reject'],
      options: [
        opt({ id: 'allow', decision: 'approve-once', label: 'Allow' }),
        opt({ id: 'session', decision: 'approve-session', label: 'Allow for this session', scope: 'session' }),
        opt({
          id: 'persist',
          decision: 'approve-session',
          label: 'Always allow `npm test` in this project',
          scope: 'persistent',
          rule: 'Bash(npm test)',
          destination: '.claude/settings.local.json',
        }),
        opt({ id: 'deny', decision: 'reject', label: 'Deny', takesMessage: true }),
      ],
    },
    'Claude',
  );
  assert.equal(b.primary?.optionId, 'allow');
  assert.equal(b.secondary?.optionId, 'deny');
  assert.deepEqual(
    b.more.map((c) => c.key),
    ['session', 'persist', 'deny:why'],
  );
  const persist = b.more[1];
  assert.deepEqual([persist.rule, persist.destination], ['Bash(npm test)', '.claude/settings.local.json']);
  const why = b.more[2];
  assert.deepEqual([why.label, why.decision, why.optionId, why.withMessage], [
    'Deny and tell Claude why',
    'reject',
    'deny',
    true,
  ]);
});

test('approval: a persistent option is never primary', () => {
  const b = approvalButtons({
    supportedDecisions: ['reject'],
    options: [
      opt({ id: 'always', decision: 'approve-session', scope: 'persistent', rule: 'Read(**)', destination: '~/.claude/settings.json' }),
      opt({ id: 'deny', decision: 'reject' }),
    ],
  });
  assert.equal(b.primary, undefined);
  assert.equal(b.secondary?.optionId, 'deny');
  assert.deepEqual(b.more.map((c) => c.optionId), ['always']);
});

test('approval: reject listed first (default_to_no) makes Deny primary; Codex enum maps', () => {
  const no = approvalButtons({
    supportedDecisions: ['approve-once', 'reject'],
    options: [opt({ id: 'deny', decision: 'reject' }), opt({ id: 'allow', decision: 'approve-once' })],
  });
  assert.deepEqual([no.primary?.optionId, no.secondary?.optionId], ['deny', 'allow']);

  const codex = approvalButtons({
    supportedDecisions: ['approve-once', 'approve-session', 'reject'],
    options: [
      opt({ id: 'accept', decision: 'approve-once', label: 'Allow' }),
      opt({ id: 'acceptForSession', decision: 'approve-session', scope: 'session' }),
      opt({ id: 'decline', decision: 'reject', label: 'Deny' }),
      opt({ id: 'cancel', decision: 'reject', label: 'Deny and stop', stopsTurn: true }),
    ],
  });
  assert.deepEqual([codex.primary?.optionId, codex.secondary?.optionId], ['accept', 'decline']);
  assert.deepEqual(codex.more.map((c) => c.optionId), ['acceptForSession', 'cancel']);
  assert.equal(codex.more[1].stopsTurn, true);
});

test('approval: question answers combine chosen chips and free text', () => {
  const questions = [
    { id: 'q1', text: 'Which?', options: [{ label: 'A' }, { label: 'B' }], multiSelect: true, allowFreeText: true },
    { id: 'q2', text: 'Name?', options: [], multiSelect: false, allowFreeText: true },
  ];
  assert.deepEqual(collectAnswers(questions, { q1: ['A', 'B'] }, { q1: ' other ', q2: '' }), {
    q1: ['A', 'B', 'other'],
    q2: [],
  });
});

const perm = (id: string, toolItemId: string | undefined, status: PermissionRequest['status'] = 'pending'): PermissionRequest => ({
  id,
  nativeRequestId: `n-${id}`,
  sessionId: 's1',
  agentId: 'claude-1',
  hostId: 'h1',
  action: 'shell',
  title: 'Run shell command',
  command: 'npm test',
  args: [],
  risk: 'high',
  status,
  supportedDecisions: ['approve-once', 'reject'],
  revision: 1,
  requestedAt: 0,
  expiresAt: 1,
  context: '',
  toolItemId,
});

test('rows: item order, turn footers, nesting, inline vs standalone approvals', () => {
  const tool = (id: string, parentId?: string): TranscriptItem => ({
    id,
    ts: 0,
    turnId: 't1',
    parentId,
    kind: 'tool',
    tool: { name: id === 'T' ? 'Task' : 'Bash', status: 'running' },
  });
  const items: TranscriptItem[] = [
    { id: 'u1', ts: 0, turnId: 't1', kind: 'user', text: 'go' },
    { id: 'x1', ts: 0, turnId: 't1', kind: 'text', text: 'ok', done: true },
    tool('T'),
    tool('c1', 'T'),
    { id: 'x2', ts: 0, turnId: 't1', kind: 'text', text: 'done', done: true },
    { id: 'u2', ts: 0, turnId: 't2', kind: 'user', text: 'more' },
    { id: 'odd', ts: 0, kind: 'video', text: '' } as unknown as TranscriptItem,
    { id: 'loopA', ts: 0, kind: 'tool', parentId: 'loopB', tool: { name: 'A', status: 'completed' } },
    { id: 'loopB', ts: 0, kind: 'tool', parentId: 'loopA', tool: { name: 'B', status: 'completed' } },
  ];
  const turns: TurnState[] = [
    { id: 't1', status: 'completed', startedAt: 0, endedAt: 1 },
    { id: 't2', status: 'running', startedAt: 2 },
  ];
  const m = buildTranscript(items, turns, [perm('p1', 'c1'), perm('p2', 'gone'), perm('p3', 'T', 'approved'), perm('p4', undefined)]);
  assert.deepEqual(
    m.rows.map((r) => r.key),
    ['u1', 'x1', 'T', 'x2', 'turn:t1', 'u2', 'loopA', 'loopB', 'perm:p2', 'perm:p4'],
  );
  assert.deepEqual(m.children.get('T')?.map((c) => c.id), ['c1']);
  assert.equal(m.approvals.get('c1')?.id, 'p1');
  assert.deepEqual([...m.attention].sort(), ['T', 'c1']);
});

test('rows: a "Stopped" notice in an interrupted turn shows once, as the footer', () => {
  const stopped = (id: string, turnId: string): TranscriptItem => ({ id, ts: 0, turnId, kind: 'notice', level: 'info', text: 'Stopped' });
  const items: TranscriptItem[] = [
    { id: 'u1', ts: 0, turnId: 't1', kind: 'user', text: 'count' },
    { id: 'x1', ts: 0, turnId: 't1', kind: 'text', text: '1 2 3', done: true },
    stopped('n1', 't1'),
    { id: 'u2', ts: 0, turnId: 't2', kind: 'user', text: 'go' },
    stopped('n2', 't2'),
    stopped('n3', 't3'),
  ];
  const turns: TurnState[] = [
    { id: 't1', status: 'interrupted', startedAt: 0, endedAt: 1 },
    { id: 't2', status: 'completed', startedAt: 2, endedAt: 3 },
    { id: 't3', status: 'interrupted', startedAt: 4, endedAt: 5 },
  ];
  assert.deepEqual(
    buildTranscript(items, turns, []).rows.map((r) => r.key),
    ['u1', 'x1', 'turn:t1', 'u2', 'n2', 'turn:t2', 'n3', 'turn:t3'],
    'dropped only where the footer says the same; kept when it is all the turn has',
  );
});

test('rows: the row that shows an item (Approvals and notification deep links)', () => {
  const tool = (id: string, parentId?: string): TranscriptItem => ({
    id,
    ts: 0,
    turnId: 't1',
    parentId,
    kind: 'tool',
    tool: { name: 'Bash', status: 'running' },
  });
  const items: TranscriptItem[] = [{ id: 'u1', ts: 0, turnId: 't1', kind: 'user', text: 'go' }, tool('T'), tool('c1', 'T'), tool('g1', 'c1')];
  const m = buildTranscript(items, [], [perm('p1', 'gone')]);
  assert.deepEqual(m.rows.map((r) => r.key), ['u1', 'T', 'perm:p1']);
  assert.equal(rowIndexFor(m, 'u1'), 0);
  assert.equal(rowIndexFor(m, 'g1'), 1, 'nested work scrolls to its top-level card');
  assert.equal(rowIndexFor(m, 'gone'), 2, 'a request whose card is not in view: its standalone card');
  assert.equal(rowIndexFor(m, 'missing'), -1);
});

test('rows: the first agent item of each reply carries the mark', () => {
  const t = (id: string, turnId: string | undefined, kind: 'user' | 'text' | 'notice'): TranscriptItem =>
    kind === 'user'
      ? { id, ts: 0, turnId, kind, text: 'go' }
      : kind === 'notice'
        ? { id, ts: 0, turnId, kind, level: 'info', text: 'note' }
        : { id, ts: 0, turnId, kind, text: 'ok', done: true };
  const items: TranscriptItem[] = [
    t('x0', undefined, 'text'),
    t('u1', 't1', 'user'),
    t('x1', 't1', 'text'),
    t('n1', 't1', 'notice'),
    t('x2', 't1', 'text'),
    { id: 'T', ts: 0, turnId: 't1', kind: 'tool', tool: { name: 'Task', status: 'completed' } },
    { id: 'c1', ts: 0, turnId: 't1', parentId: 'T', kind: 'text', text: 'sub', done: true },
    t('x3', 't2', 'text'),
    t('u3', 't3', 'user'),
    t('u4', 't3', 'user'),
    t('x4', 't3', 'text'),
  ];
  const m = buildTranscript(items, [], []);
  assert.deepEqual([...replyStarts(m.rows)], ['x0', 'x1', 'x3', 'x4'], 'notices and nested work never start one');
});

test('mode labels, MCP summary and callback precheck', () => {
  assert.deepEqual(['default', 'acceptEdits', 'plan', 'bypassPermissions', undefined].map((m) => modeLabel(m)), [
    'Ask before edits',
    'Accept edits',
    'Plan',
    'bypassPermissions',
    'Permission mode',
  ]);
  // ADR-021: OpenCode's modes are its build and plan agents
  assert.deepEqual(['default', 'plan'].map((m) => modeLabel(m, 'opencode')), ['Build', 'Plan']);
  assert.deepEqual(
    mcpSummary([
      { name: 'a', status: 'connected', authFlows: [] },
      { name: 'b', status: 'needs-auth', authFlows: ['desktop'] },
      { name: 'c', status: 'disabled', authFlows: [] },
    ]),
    { connected: 1, total: 2, attention: true },
  );
  assert.equal(isCallbackUrl('http://localhost:44818/callback?code=x&state=y', 'http://localhost:44818/callback'), true);
  assert.equal(isCallbackUrl('https://evil.example/callback', 'http://localhost:44818/callback'), false);
});
