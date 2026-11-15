import assert from 'node:assert/strict';
import test from 'node:test';
import { isClaudeModelName, isModelName } from './index.ts';

interface ModelTestCase {
  name: string;
  isClaude: boolean;
  isModel: boolean;
}

const cases: ModelTestCase[] = [
  // 64 / 65 char boundary
  { name: 'a'.repeat(64), isClaude: false, isModel: true },
  { name: 'a'.repeat(65), isClaude: false, isModel: false },
  { name: `claude-${'a'.repeat(57)}`, isClaude: true, isModel: true },
  { name: `claude-${'a'.repeat(58)}`, isClaude: false, isModel: false },
  { name: `provider/${'a'.repeat(55)}`, isClaude: false, isModel: true },
  { name: `provider/${'a'.repeat(56)}`, isClaude: false, isModel: false },

  // Leading / trailing punctuation
  { name: '.dot', isClaude: false, isModel: false },
  { name: 'dot.', isClaude: false, isModel: false },
  { name: '-dash', isClaude: false, isModel: false },
  { name: 'dash-', isClaude: false, isModel: false },
  { name: '_under', isClaude: false, isModel: false },
  { name: 'under_', isClaude: false, isModel: false },
  { name: '/slash', isClaude: false, isModel: false },
  { name: 'slash/', isClaude: false, isModel: false },
  { name: ':colon', isClaude: false, isModel: false },
  { name: 'colon:', isClaude: false, isModel: false },
  { name: '@at', isClaude: false, isModel: false },
  { name: 'at@', isClaude: false, isModel: false },

  // Casing and charset
  { name: 'GPT-4', isClaude: false, isModel: false },
  { name: 'Opus', isClaude: false, isModel: false },
  { name: 'gemini-🦀', isClaude: false, isModel: false },
  { name: '', isClaude: false, isModel: false },
  { name: '--flag', isClaude: false, isModel: false },
  { name: 'a;b', isClaude: false, isModel: false },
  { name: 'a b', isClaude: false, isModel: false },

  // Specific prompt requirements
  { name: 'claude-', isClaude: false, isModel: false },
  { name: 'best', isClaude: false, isModel: true },
  { name: 'ollama/qwen3.5:4b', isClaude: false, isModel: true },
  { name: 'gpt-6-luna', isClaude: false, isModel: true },
  { name: 'gemini-3.8-flash-low', isClaude: false, isModel: true },
  { name: 'codex_mini', isClaude: false, isModel: true },

  // Aliases and standard Claude / Provider models
  { name: 'default', isClaude: true, isModel: true },
  { name: 'sonnet', isClaude: true, isModel: true },
  { name: 'claude-3-5-sonnet', isClaude: true, isModel: true },
  { name: 'cloudflare/@cf/meta/llama-3', isClaude: false, isModel: true },
  { name: 'provider/-model', isClaude: false, isModel: false },
  { name: '-provider/model', isClaude: false, isModel: false },
  { name: 'openrouter/anthropic/claude-3.5-sonnet', isClaude: false, isModel: true },
];

test('model name validation table matches protocol specification', () => {
  for (const tc of cases) {
    assert.equal(
      isClaudeModelName(tc.name),
      tc.isClaude,
      `isClaudeModelName mismatch for "${tc.name}"`,
    );
    assert.equal(
      isModelName(tc.name),
      tc.isModel,
      `isModelName mismatch for "${tc.name}"`,
    );
  }
});
