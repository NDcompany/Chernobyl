import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  ConfigError,
  CONFIG_VERSION,
  defaultConfig,
  loadConfig,
  validateConfig,
} from './config';
import { forceRemove } from './testutil';

function tempDir(): string {
  return mkdtempSync(join(tmpdir(), 'chernobyl-config-'));
}

test('a missing config is created with defaults and a version', () => {
  const dir = tempDir();
  try {
    const file = join(dir, 'companion.json');
    const { config, created } = loadConfig(file);
    assert.equal(created, true);
    assert.equal(config.version, CONFIG_VERSION);
    assert.ok(config.adapters.length >= 1);
    // and it is actually on disk, valid on second load
    const reread = loadConfig(file);
    assert.equal(reread.created, false);
    assert.deepEqual(reread.config, config);
    assert.ok(JSON.parse(readFileSync(file, 'utf8')).version >= 1);
  } finally {
    forceRemove(dir);
  }
});

test('invalid configs fail closed with actionable messages', () => {
  const dir = tempDir();
  const file = join(dir, 'bad.json');
  try {
    writeFileSync(file, '{ not json', 'utf8');
    assert.throws(() => loadConfig(file), ConfigError);

    assert.throws(() => validateConfig({ ...defaultConfig(dir), diagnostics: { port: 99999 } }), /diagnostics\.port/);
    assert.throws(() => validateConfig({ ...defaultConfig(dir), adapters: [{ id: 'a', type: '' }] }), /adapters\[0\]\.type/);
    assert.throws(
      () =>
        validateConfig({
          ...defaultConfig(dir),
          adapters: [
            { id: 'dup', type: 'mock' },
            { id: 'dup', type: 'mock' },
          ],
        }),
      /unique/,
    );
  } finally {
    forceRemove(dir);
  }
});

test('a config written by a newer companion is refused, never guessed at', () => {
  assert.throws(
    () => validateConfig({ ...defaultConfig('/tmp'), version: CONFIG_VERSION + 5 }),
    /newer companion/,
  );
});

test('CHERNOBYL_HOME relocates the default config location', () => {
  const dir = tempDir();
  const prev = process.env.CHERNOBYL_HOME;
  process.env.CHERNOBYL_HOME = dir;
  try {
    const { config, path, created } = loadConfig();
    assert.equal(created, true);
    assert.equal(path, join(dir, 'companion.json'));
    assert.equal(config.dataDir, dir); // data dir defaults to the same home
  } finally {
    if (prev === undefined) delete process.env.CHERNOBYL_HOME;
    else process.env.CHERNOBYL_HOME = prev;
    forceRemove(dir);
  }
});
