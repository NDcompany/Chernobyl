// companion.mjs — drive a real companion binary for device validation runs:
// start/stop with an isolated CHERNOBYL_HOME, pair, revoke, read /health.
// The emulator reaches the host's loopback as 10.0.2.2, so a loopback-bound
// companion is enough (and keeps the run off the LAN).
import { spawn, spawnSync } from 'node:child_process';
import { appendFileSync, mkdirSync, openSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

export const BIN = process.env.COMPANION_BIN ?? join(process.cwd(), 'services/companion-rs/target/release/chernobyl-companion.exe');

/**
 * Starts the companion writing straight to `log` (a file, not a pipe: the
 * companion's eprintln! panics on a closed pipe, and a file outlives this
 * script so later steps can keep reading it).
 */
export function startCompanion({ home, port = 18787, diag = 19487, token, requireE2e = false, bind = '127.0.0.1', log }) {
  mkdirSync(home, { recursive: true });
  const args = ['--bind', bind, '--port', String(port), '--diagnostics-port', String(diag)];
  if (token) args.push('--token', token);
  if (requireE2e) args.push('--require-e2e');
  const env = { ...process.env, CHERNOBYL_HOME: home };
  delete env.HOST_TOKEN;
  appendFileSync(log, `\n--- start ${new Date().toISOString()} ${args.join(' ')}\n`);
  const fd = openSync(log, 'a');
  const child = spawn(BIN, args, { env, stdio: ['ignore', fd, fd], detached: true });
  child.unref();
  return { child, home, port, diag, log, token };
}

export const logText = (c) => readFileSync(c.log, 'utf8');

export async function waitListening(c, ms = 15_000) {
  const deadline = Date.now() + ms;
  const up = () => logText(c).split('--- start').pop().includes('listening on');
  while (!up() && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
  if (!up()) throw new Error(`companion did not start:\n${logText(c).slice(-800)}`);
}

export function stopCompanion(c) {
  if (c?.child && c.child.exitCode === null) c.child.kill();
}

function cli(home, args) {
  const env = { ...process.env, CHERNOBYL_HOME: home };
  return spawnSync(BIN, args, { env, encoding: 'utf8', timeout: 30_000 });
}

/** Pairing offer for the emulator; returns { payload, out }. */
export function pair(c, endpoints = [`ws://10.0.2.2:${c.port}`]) {
  const r = cli(c.home, ['--diagnostics-port', String(c.diag), '--pair', ...endpoints.flatMap((e) => ['--endpoint', e])]);
  const m = /^payload: (.+)$/m.exec(r.stdout);
  if (!m) throw new Error(`no payload:\n${r.stdout}\n${r.stderr}`);
  return { payload: m[1], out: `${r.stdout}\n${r.stderr}` };
}

export function devices(c) {
  const r = cli(c.home, ['--devices']);
  return `${r.stdout}${r.stderr}`;
}

export function revoke(c, deviceId) {
  const r = cli(c.home, ['--diagnostics-port', String(c.diag), '--revoke', deviceId]);
  return { status: r.status, out: `${r.stdout}${r.stderr}` };
}

export async function health(c) {
  const res = await fetch(`http://127.0.0.1:${c.diag}/health`);
  return res.json();
}
