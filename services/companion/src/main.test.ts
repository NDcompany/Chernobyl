/**
 * The background process entry point (PRD §25 Phase 2): starts without the
 * mobile app, serves loopback diagnostics, and shuts down gracefully —
 * after which its durable state (commands, events, audit) is intact.
 *
 * `--exit-after` drives the same shutdown routine as SIGINT/SIGTERM; on
 * Windows a forced child kill would bypass the signal handlers, so the
 * smoke test uses the portable path.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { CompanionStorage } from './storage';
import { forceRemove } from './testutil';

test('entry point: runs standalone, serves /health, shuts down gracefully, state intact', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'chernobyl-main-'));
  try {
    const child = spawn(
      process.execPath,
      [join(__dirname, 'main.js'), '--port', '0', '--exit-after', '2500'],
      { env: { ...process.env, CHERNOBYL_HOME: dir }, stdio: ['ignore', 'pipe', 'pipe'] },
    );

    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (b: Buffer) => (stdout += b.toString()));
    child.stderr.on('data', (b: Buffer) => (stderr += b.toString()));

    // wait for the diagnostics line, then prove it actually serves
    const listening = await new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`never started. stdout=${stdout} stderr=${stderr}`)), 20_000);
      const check = setInterval(() => {
        const m = stdout.match(/diagnostics: http:\/\/127\.0\.0\.1:(\d+)\/health/);
        if (m) {
          clearTimeout(timer);
          clearInterval(check);
          resolve(m[1]);
        } else if (child.exitCode !== null) {
          clearTimeout(timer);
          clearInterval(check);
          reject(new Error(`exited early (${child.exitCode}). stderr=${stderr}`));
        }
      }, 50);
    });

    const health = await fetch(`http://127.0.0.1:${listening}/health`);
    assert.equal(health.status, 200);
    const body = (await health.json()) as { ok: boolean; storage: { ok: boolean } };
    assert.equal(body.storage.ok, true);

    // graceful shutdown runs the real routine and exits 0
    const code = await new Promise<number>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`did not exit. stderr=${stderr}`)), 20_000);
      child.on('exit', (c) => {
        clearTimeout(timer);
        resolve(c ?? -1);
      });
    });
    assert.equal(code, 0, `expected graceful exit, stderr=${stderr}`);
    assert.match(stdout, /shutting down gracefully/);

    // durable state survived the shutdown: audit records start AND stop
    const storage = CompanionStorage.open(join(dir, 'companion.db'), {
      events: 100,
      eventsMaxAgeMs: 3600_000,
    });
    const texts = storage.listAudit().map((a) => a.text);
    assert.ok(texts.some((x) => /companion started/.test(x)), `audit=${JSON.stringify(texts)}`);
    assert.ok(texts.some((x) => /companion stopped/.test(x)), `audit=${JSON.stringify(texts)}`);
    storage.close();
  } finally {
    forceRemove(dir);
  }
});
