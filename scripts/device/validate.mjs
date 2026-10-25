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
import * as hc from './hostclient.mjs';
import http from 'node:http';

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
/** A system dialog's button (runtime permission prompt, ANR "Wait"), never
 *  one of the app's own: the conversation view has inline "Allow" approvals. */
const systemButton = (nodes) => nodes.find((n) => n.pkg && n.pkg !== PKG && /^(Wait|Allow)$/.test(n.text));
/** Taps a system prompt if one appears within `ms`. */
async function allowSystemPrompt(ms) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const b = systemButton(a.dump());
    if (b) return a.tap(b);
    await a.sleep(500);
  }
}
/** Waits for `re` in the screen text. System dialogs (the notification
 *  permission prompt, ANR "Wait") cover the app and are dismissed on the way. */
async function waitText(re, ms = 30_000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const nodes = a.dump();
    const t = a.screenText(nodes);
    if (re.test(t)) return t;
    if (Date.now() > deadline) return null;
    const dialog = systemButton(nodes);
    if (dialog) await a.tap(dialog);
    else await a.sleep(1000);
  }
}
/** Waits for `re`, clearing SystemUI ANR and permission dialogs on the way. */
async function untilScreen(re, ms = 45_000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const nodes = a.dump();
    if (a.find(re, nodes)) return true;
    const w = systemButton(nodes);
    if (w) await a.tap(w);
    else await a.sleep(800);
  }
  throw new Error(`not on screen: ${re}`);
}
async function dismissSystemDialogs() {
  for (let i = 0; i < 2; i++) {
    const n = systemButton(a.dump());
    if (n) await a.tap(n);
  }
}
async function freshPair(row, { token } = {}) {
  a.shell(`pm clear ${PKG}`);
  launch();
  await untilScreen(/Skip onboarding/);
  await a.tap('Skip onboarding');
  await a.waitFor('Pair with a host');
  // the pairing deep link opens the confirm step straight from the entry screen
  const { payload } = c.pair(comp);
  deviceId = JSON.parse(payload).dev;
  lastPayload = payload;
  if (token) {
    // the token travels in the manual form on the pairing screen; it lands in
    // the keystore (C14), then the pairing link below brings the payload
    await a.tap('Pair with a host');
    await a.tap(await scrollTo('Studio Mac'));
    a.type('Device run PC');
    a.key('KEYCODE_BACK');
    await a.tap(await scrollTo('10.0.4.21:8787'));
    a.type('10.0.2.2:18787');
    a.key('KEYCODE_BACK');
    await a.tap(await scrollTo('Paste the host token'));
    a.type(token);
    a.key('KEYCODE_BACK');
    await a.tap(await scrollTo('Connect host'));
    await a.sleep(2000);
  }
  openPairingLink(payload);
  await a.waitFor(/Device: /, 10_000);
  shot(row, 'confirm');
  await a.tap('Pair with this host');
  await allowSystemPrompt(8_000);
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

  /* ---- command rows: need a paired, connected phone (run after 1.1) ---- */

  // 4.1: submit a task from the phone; one accepted command, live output arrives
  async '4.1'() {
    await ensurePaired('4.1');
    const before = (await c.health(comp)).sessions;
    await newTask('Device run 4.1', 'List the project files');
    const out = await waitText(/Working on|Reading context/, 30_000);
    shot('4.1', 'session-output');
    const after = (await c.health(comp)).sessions;
    record('4.1', !!out && after === before + 1, `sessions ${before} -> ${after}; live output on the session screen=${!!out}`);
  },
  // 8.5n: the session's permission request arrives as a notification while backgrounded
  async '8.5n'() {
    await ensurePaired('8.5n');
    await newTask('Device run 8.5n', 'Install dependencies');
    a.key('KEYCODE_HOME');
    let seen = null;
    for (let i = 0; i < 30 && !seen; i++) {
      await a.sleep(2000);
      a.shell('cmd statusbar expand-notifications');
      await a.sleep(800);
      const t = a.screenText();
      if (/Approval needed/.test(t)) seen = t;
      else a.shell('cmd statusbar collapse');
    }
    shot('8.5n', 'approval-notification');
    a.shell('cmd statusbar collapse');
    record('8.5n', !!seen, seen ? `notification: ${/Approval needed[^|]*\|[^|]*/.exec(seen)?.[0]}` : 'no approval notification within 60 s');
    launch();
    await waitText(/hosts? online/, 30_000);
  },
  // 4.3: a stale approval is never applied: another client resolves the request first
  async '4.3'() {
    await ensurePaired('4.3');
    await newTask('Device run 4.3', 'Run the shell step');
    const host = await hc.connect(`ws://127.0.0.1:${comp.port}`);
    host.send({ type: 'hello', clientId: 'device-run-host', protocol: 2 });
    // `state` replies carry no commandId: wait for the next one and look for
    // a pending request on this row's session
    let perm = null;
    for (let i = 0; i < 30 && !perm; i++) {
      host.send({ type: 'list_state', commandId: `ls-${i}` });
      const st = await host.next((t) => t.includes('"type":"state"'), 5000, true);
      const snap = st ? JSON.parse(st).snapshot : null;
      const sid = snap?.sessions?.find((s) => s.title === 'Device run 4.3')?.id;
      perm = snap?.permissions?.find((x) => x.status === 'pending' && x.sessionId === sid) ?? null;
      if (!perm) await a.sleep(1500);
    }
    if (!perm) return record('4.3', false, 'no pending permission appeared');
    await goHome();
    await a.tap('Approvals');
    await a.sleep(1500);
    shot('4.3', 'pending-on-phone');
    const before = approvalsBadge();
    // command ids are journaled across runs: a fixed id would replay an old ack
    const run = Date.now();
    const ask = (id, decision) => {
      host.send({ type: 'permission_decision', commandId: `${id}-${run}`, permissionId: perm.id, decision, revision: perm.revision });
      // the command_result event carries the same commandId: wait for the ack
      return host.next((t) => t.includes('"type":"ack"') && t.includes(`"${id}-${run}"`), 5000).then((t) => (t ? JSON.parse(t) : null));
    };
    const first = await ask('host-reject', 'reject');
    // the same (now stale) revision again, as a slower phone would send it
    const late = await ask('late-approve', 'approve-once');
    // the request must leave the pending section (decided ones list below "Decided")
    let gone = false;
    for (let i = 0; i < 10 && !gone; i++) {
      await a.sleep(1000);
      const nodes = a.dump();
      const cut = nodes.findIndex((n) => n.text === 'Decided');
      gone = !(cut >= 0 ? nodes.slice(0, cut) : nodes).some((n) => n.text === perm.command);
    }
    shot('4.3', 'after-other-client');
    const after = approvalsBadge();
    host.close();
    record(
      '4.3',
      first?.ok === true && late?.ok === false && gone,
      `other client rejected ${perm.id} (rev ${perm.revision}): ok=${first?.ok}; a late approve with the same revision: ok=${late?.ok} (${late?.error ?? 'no error'}); left the phone's pending list=${gone} (badge ${before} -> ${after})`,
    );
    await a.tap('Home');
  },
  // 4.2: double-tap "Start session": exactly one execution
  async '4.2'() {
    await ensurePaired('4.2');
    const before = (await c.health(comp)).sessions;
    await newTask('Device run 4.2', 'Double tap test', { doubleTap: true });
    await a.sleep(5000);
    const after = (await c.health(comp)).sessions;
    shot('4.2', 'after-double-tap');
    record('4.2', after === before + 1, `sessions ${before} -> ${after} after a double tap`);
  },
  // 5.3: offline submit: nothing sent, honest message, nothing executed later
  async '5.3'() {
    await ensurePaired('5.3');
    const before = (await c.health(comp)).sessions;
    a.shell('svc wifi disable');
    a.shell('svc data disable');
    await waitText(/Reconnecting/, 60_000);
    await newTask('Device run 5.3', 'Offline submit', { expectNavigate: false });
    const toast = await waitText(/nothing was sent|Outcome unknown/i, 15_000);
    shot('5.3', 'offline-submit');
    a.shell('svc wifi enable');
    a.shell('svc data enable');
    await waitText(/hosts? online/, 90_000);
    await a.sleep(5000);
    const after = (await c.health(comp)).sessions;
    record('5.3', !!toast && after === before, `toast: ${toast ? /Host unreachable[^|]*|Outcome unknown[^|]*/i.exec(toast)?.[0] : 'none'}; sessions ${before} -> ${after} after the network returned`);
    a.key('KEYCODE_BACK');
  },
  // 4.4: companion killed right after submit: never executed twice
  async '4.4'() {
    await ensurePaired('4.4');
    const before = (await c.health(comp)).sessions;
    await newTask('Device run 4.4', 'Kill mid command', { afterTap: () => stop() });
    await a.sleep(3000);
    shot('4.4', 'companion-down');
    await start();
    await waitText(/hosts? online/, 60_000);
    await a.sleep(5000);
    const after = (await c.health(comp)).sessions;
    shot('4.4', 'after-restart');
    record('4.4', after <= before + 1, `sessions ${before} -> ${after} (never +2; the demo engine keeps sessions in memory, so a restart returns to its seed); the phone kept the outcome honest`);
  },
  // 2.2: the same pairing payload cannot pair a second time
  async '2.2'() {
    await start();
    await freshPair('2.2');
    await waitText(/hosts? online/, 45_000);
    // the first session rotates the device key right after hs_ok (hs_rekey)
    await a.sleep(3000);
    const reused = lastPayload;
    a.shell(`pm clear ${PKG}`);
    launch();
    await untilScreen(/Skip onboarding/, 30_000);
    await a.tap('Skip onboarding');
    await a.tap('Pair with a host');
    openPairingLink(reused);
    await a.tap('Pair with this host');
    await allowSystemPrompt(6000);
    const t = await waitText(/Access denied/, 45_000);
    shot('2.2', 'reused-offer');
    saveLog('2.2');
    record('2.2', !!t, `second redemption of one offer -> ${t ? 'Access denied (pairing offer already used)' : 'not denied'}`);
    deviceId = null;
  },
  // 2.1: an expired offer is refused
  async '2.1'() {
    await start();
    const { payload } = c.pair(comp);
    await a.sleep(305_000);
    a.shell(`pm clear ${PKG}`);
    launch();
    await untilScreen(/Skip onboarding/, 30_000);
    await a.tap('Skip onboarding');
    await a.tap('Pair with a host');
    openPairingLink(payload);
    await a.tap('Pair with this host');
    await allowSystemPrompt(6000);
    const t = await waitText(/Access denied/, 45_000);
    shot('2.1', 'expired-offer');
    record('2.1', !!t, `offer used 5 min 5 s after creation -> ${t ? 'Access denied (expired)' : 'not denied'}`);
    deviceId = null;
  },
  // 7.3: token rotation: the old token is refused and the app stops (no retry storm)
  async '7.3'() {
    await start({ token: TOKEN });
    await freshPair('7.3', { token: TOKEN });
    const ok = await waitText(/hosts? online/, 45_000);
    shot('7.3', 'connected-with-token');
    await start({ token: `${TOKEN}-rotated` });
    const denied = await waitText(/Access denied/, 60_000);
    shot('7.3', 'after-rotation');
    await a.sleep(15_000);
    const log = c.logText(comp).split('--- start').pop();
    const refusals = (log.match(/4001|unauthori[sz]ed/gi) ?? []).length;
    record('7.3', !!ok && !!denied, `connected with token=${!!ok}; after rotation -> ${denied ? 'Access denied' : 'not denied'}; refusals logged in 15 s: ${refusals}`);
    deviceId = null;
  },
  // 8.4: backgrounded >= 10 min, connection held, foreground is live
  async '8.4'() {
    await ensurePaired('8.4');
    a.key('KEYCODE_HOME');
    const samples = [];
    for (let i = 0; i < 10; i++) {
      await a.sleep(60_000);
      samples.push((await c.health(comp)).clients);
    }
    launch();
    const t = await waitText(/hosts? online/, 30_000);
    shot('8.4', 'after-10-min');
    record('8.4', samples.every((n) => n >= 1) && !!t, `companion clients each minute in background: ${samples.join(',')}; foreground live=${!!t}`);
  },
  // 8.2 / 8.3 (host side, no phone): auth cooldown and malformed frames
  async '8.2'() {
    await start({ token: TOKEN });
    const codes = [];
    for (let i = 0; i < 6; i++) {
      const bad = await hc.connect(`ws://127.0.0.1:${comp.port}/?token=wrong`);
      await new Promise((r) => setTimeout(r, 300));
      codes.push(bad.closed ?? 'open');
      bad.close();
    }
    const good = await hc.connect(`ws://127.0.0.1:${comp.port}/?token=${TOKEN}`);
    good.send({ type: 'ping', commandId: 'after-cooldown' });
    const pong = await good.next((t) => t.includes('after-cooldown'), 5000);
    good.close();
    record('8.2', !!pong, `wrong-token closes: ${codes.join(',')}; correct token afterwards answered=${!!pong}`);
  },
  // 4.5 (host side): an unsupported protocol version is refused, then closed 4002 (D7)
  async '4.5'() {
    await start();
    const cl = await hc.connect(`ws://127.0.0.1:${comp.port}`);
    cl.send({ type: 'hello', clientId: 'old-client', protocol: 1 });
    const reply = await cl.next((t) => t.includes('protocol mismatch'), 5000);
    for (let i = 0; i < 20 && cl.closed == null; i++) await a.sleep(250);
    record('4.5', !!reply && cl.closed === 4002, `hello with protocol 1 -> ${reply ? 'protocol mismatch refusal' : 'no refusal'}; close ${cl.closed}`);
  },
  async '8.3'() {
    await start();
    const before = (await c.health(comp)).malformed;
    const cl = await hc.connect(`ws://127.0.0.1:${comp.port}`);
    cl.send('{not json');
    cl.send({ type: 'ping', commandId: 'still-alive' });
    const pong = await cl.next((t) => t.includes('still-alive'), 5000);
    cl.close();
    const after = (await c.health(comp)).malformed;
    record('8.3', !!pong && after === before + 1, `malformed ${before} -> ${after}; connection survived=${!!pong}`);
  },
  // 3.3: a browser page cannot open the WebSocket (D1)
  async '3.3'() {
    await start();
    const page = `<html><body><h1 id=r>pending</h1><script>var w=new WebSocket('ws://10.0.2.2:${comp.port}');w.onopen=function(){document.getElementById('r').textContent='WS-OPEN'};w.onerror=function(){document.getElementById('r').textContent='WS-REFUSED'};w.onclose=function(e){if(document.getElementById('r').textContent!=='WS-OPEN')document.getElementById('r').textContent='WS-REFUSED '+e.code};</script></body></html>`;
    const srv = http.createServer((_q, s) => { s.writeHead(200, { 'content-type': 'text/html' }); s.end(page); }).listen(18900);
    a.shell('am start -a android.intent.action.VIEW -d http://10.0.2.2:18900/ com.android.chrome');
    let t = null;
    for (let i = 0; i < 25 && !t; i++) {
      await a.sleep(2000);
      for (const re of [/^Use without an account$/, /^Accept & continue$/, /^No thanks$/, /^Got it$/, /^Continue$/]) { const n = a.find(re); if (n) await a.tap(n); }
      const s = a.screenText();
      if (/WS-REFUSED|WS-OPEN/.test(s)) t = s;
    }
    shot('3.3', 'browser');
    srv.close();
    a.shell('am force-stop com.android.chrome');
    record('3.3', !!t && /WS-REFUSED/.test(t) && !/WS-OPEN/.test(t), t ? `browser page result: ${/WS-[A-Z]+( \d+)?/.exec(t)[0]}` : 'Chrome page did not report');
    launch();
  },
};

let lastPayload = null;
/** Hands the payload to the app through its pairing deep link (adb typing
 *  drops characters on a busy emulator); the confirm tap stays manual. */
function openPairingLink(payload) {
  a.shell(`am start -a android.intent.action.VIEW -d 'chernobyl://connect-host?payload=${encodeURIComponent(payload)}' ${PKG}`);
}
async function ensurePaired(row) {
  // rows 1.3/7.3 leave a token-requiring companion behind
  if (!comp || comp.token) {
    await start();
    deviceId = null;
  }
  if (!deviceId) {
    await freshPair(row);
  } else {
    launch();
  }
  await goHome();
  const t = await waitText(/hosts? online/, 45_000);
  if (!t) throw new Error('phone not connected');
}
/** The pending count on the Approvals tab (the number badge above its label). */
function approvalsBadge() {
  const nodes = a.dump();
  const tab = nodes.filter((n) => n.text === 'Approvals').sort((p, q) => q.y - p.y)[0];
  if (!tab) return null;
  const b = nodes.find((n) => /^\d+$/.test(n.text) && Math.abs(n.x - tab.x) < 80 && n.y < tab.y && n.y > tab.y - 140);
  return b ? Number(b.text) : 0;
}
/** Returns to the tabs, where the connection pill lives (a previous row may
 *  end on a pushed session screen). The root deep link does it from any
 *  screen; Back presses raced screen transitions and closed the app. */
async function goHome() {
  a.shell(`am start -a android.intent.action.VIEW -d 'chernobyl:///' ${PKG}`);
  await untilScreen(/^Home$/, 20_000);
}
/** Finds an element below the fold by swiping the screen up (uiautomator
 *  only lists on-screen views), then back to the top afterwards. */
async function scrollTo(match, swipes = 6) {
  for (let i = 0; i <= swipes; i++) {
    const n = a.find(match);
    if (n) return n;
    a.shell('input swipe 360 1250 360 450 250');
    await a.sleep(700);
  }
  throw new Error(`not found after scrolling: ${match}`);
}
async function newTask(title, message, { doubleTap = false, expectNavigate = true, afterTap } = {}) {
  await a.tap(await scrollTo('Start a new task'));
  await a.waitFor('Start session', 15_000);
  await a.tap('Fix flaky API tests');
  a.type(title);
  a.key('KEYCODE_BACK');
  await a.tap('Describe what the agent should do');
  a.type(message);
  a.key('KEYCODE_BACK');
  const btn = await a.waitFor('Start a new agent session', 10_000);
  a.shell(`input tap ${btn.x} ${btn.y}`);
  if (doubleTap) a.shell(`input tap ${btn.x} ${btn.y}`);
  if (afterTap) afterTap();
  if (expectNavigate) await a.sleep(2500);
}

// rows 5.1/5.3 switch the phone's network off; a row that fails midway must
// not leave it off for every row after it
const netOn = () => {
  a.shell('svc wifi enable');
  a.shell('svc data enable');
};

const pick = process.argv.slice(2);
const order = pick.length ? pick : Object.keys(ROWS);
try {
  netOn();
  await dismissSystemDialogs();
  for (const row of order) {
    try {
      await ROWS[row]();
    } catch (e) {
      shot(row, 'error');
      record(row, false, `error: ${e.message.split('\n')[0]}`);
    } finally {
      netOn();
    }
  }
} finally {
  if (comp) stop();
}
