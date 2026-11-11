/**
 * Unit tests for the ADR-017 composer helpers. Run with `npm test -w @chernobyl/mobile`.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  FRAME_MAX_BYTES,
  cleanBase64,
  contextPercent,
  filterSlashCommands,
  imageBudgetLeft,
  modelChoices,
  modelLabel,
  pickPictureSize,
  sealedFrameBytes,
  slashQuery,
  utf8Bytes,
} from './composer.ts';

test('slash menu: opens on a leading slash, closes at the first space', () => {
  assert.equal(slashQuery('/'), '');
  assert.equal(slashQuery('/comp'), 'comp');
  assert.equal(slashQuery('/compact now'), null);
  assert.equal(slashQuery('say /compact'), null);
  assert.equal(slashQuery(''), null);
  const cmds = ['compact', 'context', 'code-review', 'cost', 'review', 'init'];
  assert.deepEqual(filterSlashCommands(cmds, 'co'), ['compact', 'context', 'code-review', 'cost']);
  assert.deepEqual(filterSlashCommands(cmds, 'review'), ['review', 'code-review']);
  assert.deepEqual(filterSlashCommands(cmds, 'CON'), ['context']);
  assert.deepEqual(filterSlashCommands(cmds, '', 3), ['compact', 'context', 'code-review']);
  assert.deepEqual(filterSlashCommands(cmds, 'zzz'), []);
});

test('frame budget: UTF-8 bytes and the sealed size', () => {
  assert.equal(utf8Bytes('abc'), 3);
  assert.equal(utf8Bytes('é'), 2);
  assert.equal(utf8Bytes('✓'), 3);
  assert.equal(utf8Bytes('\u{1F600}'), 4);
  assert.equal(utf8Bytes('\ud800'), 3);
  const msg = { type: 'submit_prompt', text: 'x'.repeat(1000) };
  const plain = JSON.stringify(msg).length;
  const sealed = sealedFrameBytes(msg);
  assert.ok(sealed >= Math.ceil(((plain + 16) * 4) / 3) && sealed < plain * 1.4 + 100, String(sealed));
  // the protocol's image cap leaves room for a long ASCII prompt in one frame
  const full = { type: 'submit_prompt', commandId: 'c'.repeat(40), sessionId: 's'.repeat(36), text: 't'.repeat(8000), images: [{ mediaType: 'image/jpeg', data: 'A'.repeat(180_000) }] };
  assert.ok(sealedFrameBytes(full) <= FRAME_MAX_BYTES, String(sealedFrameBytes(full)));
  // …but not for 32,000 CJK characters on top: the composer must check
  const cjk = { ...full, text: '字'.repeat(32_000) };
  assert.ok(sealedFrameBytes(cjk) > FRAME_MAX_BYTES);
});

test('image budget, picture size and base64 cleanup', () => {
  assert.equal(imageBudgetLeft([], 100), 100);
  assert.equal(imageBudgetLeft([{ data: 'a'.repeat(60) }, { data: 'b'.repeat(30) }], 100), 10);
  assert.equal(imageBudgetLeft([{ data: 'a'.repeat(120) }], 100), 0);
  const sizes = ['640x480', '1280x720', '1920x1080', '4032x3024', 'bogus'];
  assert.equal(pickPictureSize(sizes, 1280), '1280x720');
  assert.equal(pickPictureSize(sizes, 1000), '640x480');
  assert.equal(pickPictureSize(['1920x1080', '4032x3024'], 1280), '1920x1080');
  assert.equal(pickPictureSize([], 1280), undefined);
  assert.equal(cleanBase64('data:image/jpeg;base64,/9j/AA\nBB\r\n=='), '/9j/AABB==');
});

test('context meter and model labels', () => {
  assert.equal(contextPercent(undefined), undefined);
  assert.equal(contextPercent({ usedTokens: 22_900, maxTokens: 1_000_000 }), 2);
  assert.equal(contextPercent({ usedTokens: 150_000, maxTokens: 200_000 }), 75);
  assert.equal(contextPercent({ usedTokens: 300_000, maxTokens: 200_000 }), 100);
  assert.equal(contextPercent({ usedTokens: 5, maxTokens: 0 }), undefined);
  const listed = [{ value: 'haiku', label: 'Haiku 5.5' }];
  assert.deepEqual(modelChoices({ models: listed }), listed);
  assert.deepEqual(modelChoices({}).map((m) => m.value), ['default', 'sonnet', 'opus', 'haiku']);
  assert.equal(modelLabel('claude-haiku-5-5', listed), 'Haiku 5.5');
  assert.equal(modelLabel('claude-haiku-4-5-20251001', listed), 'Haiku 4.5');
  assert.equal(modelLabel('claude-opus-5', listed), 'Opus 5');
  assert.equal(modelLabel('haiku', listed), 'Haiku 5.5');
  assert.equal(modelLabel(undefined, listed), 'Model');
  assert.equal(modelLabel('something-else', listed), 'something-else');
});
