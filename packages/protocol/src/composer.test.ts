/**
 * ADR-017 composer: the validators read the same fixtures as the Rust
 * companion (`protocol.rs`, `transcript.rs`), so both stacks accept and
 * refuse the same set_model / rename_session / images messages.
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import {
  MockEngine,
  PROMPT_IMAGES_DATA_MAX,
  applyTranscript,
  emptyTranscriptView,
  isClientMessage,
  isModelName,
  isServerMessage,
  type LiveServerMessage,
  type ServerMessage,
} from './index';

const fixture = (name: string): unknown =>
  JSON.parse(readFileSync(join(__dirname, '..', 'fixtures', 'live', name), 'utf8'));

test('composer fixtures: valid messages pass, invalid ones fail', () => {
  for (const msg of fixture('composer-valid.json') as unknown[]) {
    assert.ok(isClientMessage(msg), JSON.stringify(msg).slice(0, 200));
  }
  for (const { why, msg } of fixture('composer-invalid.json') as Array<{ why: string; msg: unknown }>) {
    assert.equal(isClientMessage(msg), false, why);
  }
});

test('image data is capped across all images; titles count UTF-16 units', () => {
  const image = (len: number) => ({ mediaType: 'image/gif', data: `R0lGOD${'A'.repeat(len - 6)}` });
  const prompt = (...images: object[]) => ({ type: 'submit_prompt', commandId: 'c', sessionId: 's', text: 't', images });
  const max = PROMPT_IMAGES_DATA_MAX;
  assert.ok(isClientMessage(prompt(image(max))));
  assert.ok(!isClientMessage(prompt(image(max + 4))));
  assert.ok(isClientMessage(prompt(image(max / 2), image(max / 2))));
  assert.ok(!isClientMessage(prompt(image(max / 2), image(max / 2 + 4))));
  const rename = (title: string) => ({ type: 'rename_session', commandId: 'c', sessionId: 's', title });
  assert.ok(isClientMessage(rename('\u{1F600}'.repeat(40))));
  assert.ok(!isClientMessage(rename(`${'\u{1F600}'.repeat(40)}x`)));
  assert.ok(isClientMessage(rename('t'.repeat(80))));
  for (const good of ['default', 'sonnet', 'opus', 'haiku', 'fable', 'opusplan', 'claude-x']) assert.ok(isModelName(good), good);
  const long = `claude-${'a'.repeat(57)}`;
  assert.ok(isModelName(long) && !isModelName(`${long}a`));
  for (const bad of ['', 'claude-', 'claude-A', 'claude-a b', 'gpt_5', '-model', 'claude-a/b', 7]) {
    assert.ok(!isModelName(bad), String(bad));
  }
  // ADR-022: any runtime's model id on the wire
  for (const good of ['claude-sonnet-4-6', 'gemini-3.8-flash-low', 'gpt-oss-120b-medium', 'gpt-5']) {
    assert.ok(isModelName(good), good);
  }
  for (const bad of ['best', 'Opus', '--dangerously-skip-permissions', 'gemini..x', '9-model', 'gemini-3.8 flash', 'a/b-c']) {
    assert.ok(!isModelName(bad), bad);
  }
});

test('composer transcript fixture: image count and composer state reach the view', () => {
  const msg = fixture('composer-transcript.json') as LiveServerMessage;
  assert.ok(isServerMessage(msg));
  const { view } = applyTranscript({ ...emptyTranscriptView(), epoch: '9f86d081884c7d65' }, msg);
  const user = view.items[0];
  assert.equal(user.kind, 'user');
  assert.equal(user.kind === 'user' ? user.images : undefined, 2);
  assert.deepEqual(view.state.slashCommands, ['compact', 'context', 'code-review']);
  assert.deepEqual(view.state.models?.map((m) => m.value), ['default', 'haiku']);
  assert.deepEqual(view.state.context, { usedTokens: 22900, maxTokens: 1000000 });
});

test('the mock engine refuses the composer extras (parity with the Rust engine)', () => {
  const engine = new MockEngine(() => undefined);
  const refused = (reply: ServerMessage | undefined) =>
    reply?.type === 'ack' && !reply.ok && reply.error === 'not supported by this agent';
  const png = { mediaType: 'image/png' as const, data: 'iVBORw0KGgoAAAANSUhEUgAAAAE=' };
  assert.ok(refused(engine.handle({ type: 'set_model', commandId: 'm', sessionId: 's', model: 'opus' })));
  assert.ok(refused(engine.handle({ type: 'rename_session', commandId: 'r', sessionId: 's', title: 'x' })));
  assert.ok(refused(engine.handle({ type: 'submit_prompt', commandId: 'p', sessionId: 's', text: 't', images: [png] })));
  assert.ok(
    refused(engine.handle({ type: 'create_session', commandId: 'c', agentId: 'a', title: 't', prompt: 'p', model: 'opus' })),
  );
});
