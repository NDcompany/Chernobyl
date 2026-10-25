#!/usr/bin/env node
// validate.mjs — runs PHONE-VALIDATION-CHECKLIST rows against the real APK on a
// device or emulator and a real companion binary on this PC. Each step saves
// screenshots and companion log excerpts under docs/validation/evidence/<run>/
// and a results.json with one PASS/FAIL entry per row.
//
//   node scripts/device/validate.mjs            all rows, in order
//   node scripts/device/validate.mjs 1.1 6.1    selected rows (state carries)
//
// Emulator networking: the companion stays bound to loopback and the phone
// dials ws://10.0.2.2:<port> (the emulator's alias for the host loopback).
import { mkdirSync, rmSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import * as a from './adb.mjs';
import * as c from './companion.mjs';

const RUN = process.env.RUN ?? '2026-10-25-emulator';
const OUT = join(process.cwd(), 'docs/validation/evidence', RUN);
const HOME = join(process.env.TEMP ?? '.', 'chernobyl-device-run');
const PKG = 'com.chernobyl.app';
const TOKEN = 'device-run-token';
mkdirSync(OUT, { recursive: true });
const resultsPath = join(OUT, 'results.json');
const results = existsSync(resultsPath) ? JSON.parse(readFileSync(resultsPath, 'utf8')) : {};

let comp = null;
let deviceId = null;

const shot = (row, name) => a.screenshot(join(OUT, `${row}-${name}.png`));
const record = (row, pass, observed) => {
  results[row] = { pass, observed, at: new Date().toISOString() };
  writeFileSync(resultsPath, JSON.stringify(results, null, 2));
  console.log(`${pass ? 'PASS' : 'FAIL'} ${row} — ${observed}`);
};
const logTail = (n = 12) => c.logText(comp).split('\n').filter(Boolean).slice(-n).join('\n');
const saveLog = (row) => writeFileSync(join(OUT, `${row}-companion.txt`), logTail(30));

async function start(opts = {}) {
  if (comp) c.stopCompanion(comp);
  await a.sleep(800);
  comp = c.startCompanion({ home: HOME, log: join(OUT, 'companion-full.txt'), ...opts });
  await c.waitListening(comp);
}
function stop() {
  c.stopCompanion(comp);
}
function launch() {
  a.shell(`am start -n ${PKG}/.MainActivity`);
}
async function home() {
  // home connection pill: "N hosts online" | Reconnecting… | Authorizing… | Access denied … | Offline
  return a.screenText();
}
async function waitText(re, ms = 30_000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const t = a.screenText();
    if (re.test(t)) return t;
    if (Date.now() > deadline) return null;
    await a.sleep(1000);
  }
}
async function dismissSystemDialogs() {
  for (const re of [/^Wait$/, /^Allow$/]) {
    const n = a.find(re);
    if (n) await a.tap(n);
  }
}
async function freshPair(row) {
  a.shell(`pm clear ${PKG}`);
  launch();
  await a.waitFor(/Skip onboarding|^Allow$/, 30_000);
  if (a.find(/^Allow$/)) await a.tap(/^Allow$/);
  await a.tap('Skip onboarding');
  await a.tap('Pair with a host');
  const { payload } = c.pair(comp);
  deviceId = JSON.parse(payload).dev;
  await a.tap('Pairing payload from the host');
  a.type(payload);
  a.key('KEYCODE_BACK');
  await a.waitFor(/Device: /, 10_000);
  shot(row, 'confirm');
  await a.tap('Pair with this host');
  if (await a.waitFor(/^Allow$/, 8_000)) await a.tap(/^Allow$/);
}

const ROWS = {
  // 1.1 + 3.1 + 1.2(loopback, no token): pairing by pasted payload, mutual handshake, live snapshot
  async '1.1'() {
    await start();
    await freshPair('1.1');
    const t = await waitText(/hosts? online/, 45_000);
    shot('1.1', 'home');
    const h = await c.health(comp);
    const dev = c.devices(comp);
    saveLog('1.1');
    writeFileSync(join(OUT, '1.1-devices.txt'), dev);
    record('1.1', !!t && h.clients >= 1 && dev.includes(deviceId), `home: ${t ? /\d+ hosts? online/.exec(t)[0] : 'not connected'}; companion clients=${h.clients}; device ${deviceId} registered=${dev.includes(deviceId)}`);
  },
  // 1.4 / 6.2: force-stop + relaunch reconnects from the keystore, no re-pair
  async '6.2'() {
    a.shell(`am force-stop ${PKG}`);
    await a.sleep(1500);
    launch();
    const t0 = Date.now();
    const t = await waitText(/hosts? online/, 45_000);
    shot('6.2', 'relaunched');
    record('6.2', !!t, t ? `reconnected without re-pairing in ${Math.round((Date.now() - t0) / 1000)} s` : 'did not reconnect');
  },
  // 6.1: companion restart while the app stays open
  async '6.1'() {
    stop();
    const r = await waitText(/Reconnecting/, 30_000);
    shot('6.1', 'reconnecting');
    await start();
    const t0 = Date.now();
    const t = await waitText(/hosts? online/, 60_000);
    shot('6.1', 'back');
    saveLog('6.1');
    record('6.1', !!r && !!t, `showed Reconnecting=${!!r}; back online in ${Math.round((Date.now() - t0) / 1000)} s=${!!t}`);
  },
  // 5.1: phone network off ≥ 30 s, back on
  async '5.1'() {
    a.shell('svc wifi disable');
    a.shell('svc data disable');
    const r = await waitText(/Reconnecting/, 60_000);
    shot('5.1', 'offline');
    await a.sleep(30_000);
    a.shell('svc wifi enable');
    a.shell('svc data enable');
    const t0 = Date.now();
    const t = await waitText(/hosts? online/, 90_000);
    shot('5.1', 'online');
    record('5.1', !!r && !!t, `Reconnecting while offline=${!!r}; reconnected ${Math.round((Date.now() - t0) / 1000)} s after network returned=${!!t}`);
  },
  // 8.5 (part): background — foreground service + connection kept
  async '8.5'() {
    a.key('KEYCODE_HOME');
    await a.sleep(5000);
    const svc = a.shell(`dumpsys activity services ${PKG}`);
    const running = /ConnectionForegroundService/.test(svc);
    a.shell('cmd statusbar expand-notifications');
    await a.sleep(1500);
    shot('8.5', 'notification');
    const notif = a.screenText();
    a.shell('cmd statusbar collapse');
    const before = (await c.health(comp)).clients;
    await a.sleep(120_000);
    const after = (await c.health(comp)).clients;
    writeFileSync(join(OUT, '8.5-dumpsys.txt'), svc.slice(0, 4000));
    record('8.5', running && after >= 1, `foreground service running=${running}; notification="${/Background connection[^|]*\|[^|]*/.exec(notif)?.[0] ?? 'not seen'}"; companion clients before/after 2 min in background = ${before}/${after}`);
    launch();
    await waitText(/hosts? online/, 30_000);
  },
  // 7.1 / 2.3: revoke the live device -> 4003 -> Denied, service stopped
  async '7.1'() {
    const r = c.revoke(comp, deviceId);
    const t = await waitText(/Access denied/, 30_000);
    shot('7.1', 'denied');
    await a.sleep(2000);
    const svc = /ConnectionForegroundService/.test(a.shell(`dumpsys activity services ${PKG}`));
    saveLog('7.1');
    record('7.1', r.status === 0 && !!t && !svc, `revoke: ${r.out.trim()}; app: ${t ? 'Access denied — re-pair this phone' : 'no denied state'}; service still running=${svc}`);
  },
  // 7.2 / 8.6: relaunch with the revoked pairing still stored, 5 cold launches
  async '7.2'() {
    let ok = true;
    for (let i = 0; i < 5; i++) {
      a.shell(`am force-stop ${PKG}`);
      launch();
      const t = await waitText(/Access denied/, 30_000);
      if (!t) ok = false;
    }
    shot('7.2', 'denied-after-relaunch');
    const crash = a.shell('logcat -d -b crash');
    const fgsCrash = /did not then call Service\.startForeground|FATAL EXCEPTION/.test(crash);
    writeFileSync(join(OUT, '7.2-crash-buffer.txt'), crash.slice(-4000) || '(empty)');
    record('7.2', ok && !fgsCrash, `5 cold launches all show Access denied=${ok}; crash buffer has a fatal/startForeground crash=${fgsCrash}`);
  },
  // 8.1: companion not running at app start
  async '8.1'() {
    stop();
    await start();
    await freshPair('8.1');
    await waitText(/hosts? online/, 45_000);
    stop();
    a.shell(`am force-stop ${PKG}`);
    launch();
    const t = await waitText(/Reconnecting|Connecting/, 30_000);
    shot('8.1', 'no-companion');
    await a.tap('Sessions');
    await a.sleep(1500);
    shot('8.1', 'ui-usable');
    const usable = /Sessions/.test(a.screenText());
    record('8.1', !!t && usable, `status with no companion: ${t ? /Reconnecting…|Connecting…/.exec(t)?.[0] : 'none'}; UI navigable=${usable}`);
    await a.tap('Home');
  },
  // 1.3 / 7.3: token required, phone has none -> 4001 -> Denied (terminal, not a retry loop)
  async '1.3'() {
    await start({ token: TOKEN });
    const t = await waitText(/Access denied/, 45_000);
    shot('1.3', 'denied-401');
    await a.sleep(10_000);
    const attempts = (c.logText(comp).split('--- start').pop().match(/4001|unauthori[sz]ed/gi) ?? []).length;
    saveLog('1.3');
    record('1.3', !!t, `wrong/missing token -> app: ${t ? 'Access denied' : 'no denied state'}; companion log 4001 mentions over 10 s: ${attempts}`);
  },
};

const pick = process.argv.slice(2);
const order = pick.length ? pick : Object.keys(ROWS);
try {
  await dismissSystemDialogs();
  for (const row of order) {
    try {
      await ROWS[row]();
    } catch (e) {
      shot(row, 'error');
      record(row, false, `error: ${e.message.split('\n')[0]}`);
    }
  }
} finally {
  if (comp) stop();
}
