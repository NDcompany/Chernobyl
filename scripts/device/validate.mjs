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
import { mkdirSync, rmSync, writeFileSync, appendFileSync, readFileSync, existsSync, openSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
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
/** `pass: null` records NOT RUN (the observation says why). */
const record = (row, pass, observed) => {
  results[row] = { pass, observed, at: new Date().toISOString() };
  writeFileSync(resultsPath, JSON.stringify(results, null, 2));
  console.log(`${pass === null ? 'NOT RUN' : pass ? 'PASS' : 'FAIL'} ${row} — ${observed}`);
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
async function freshPair(row, { token, endpoints } = {}) {
  a.shell(`pm clear ${PKG}`);
  launch();
  await untilScreen(/Skip onboarding/);
  await a.tap('Skip onboarding');
  await a.waitFor('Pair with a host');
  // the pairing deep link opens the confirm step straight from the entry screen
  const { payload } = endpoints ? c.pair(comp, endpoints) : c.pair(comp);
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
  // T.1: away from home. The QR carries ONLY the tunnel URL (TUNNEL_URL, e.g.
  // wss://x.ngrok-free.dev forwarding to 127.0.0.1:8787), so a connection
  // proves the app reaches the companion through the tunnel, with requireE2e.
  async 'T.1'() {
    const url = process.env.TUNNEL_URL;
    if (!url) return record('T.1', null, 'TUNNEL_URL not set');
    await start({ port: 8787, diag: 19487, token: TOKEN, requireE2e: true });
    await freshPair('T.1', { token: TOKEN, endpoints: [url] });
    const t = await waitText(/hosts? online/, 60_000);
    shot('T.1', 'home');
    const h = await c.health(comp);
    saveLog('T.1');
    record('T.1', !!t && h.clients >= 1, `QR endpoints=[${url}] only; home: ${t ? /\d+ hosts? online/.exec(t)[0] : 'not connected'}; companion clients=${h.clients}`);
  },
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
      const cut = nodes.findIndex((n) => /^decided$/i.test(n.text));
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
  // 2.2: a pairing QR stops being a credential once the first phone has paired
  // (its device key is rotated at first use, hs_rekey)
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
    record('2.2', !!t, `the same QR re-used after the first phone paired and acked its rotated key -> ${t ? 'Access denied (proof verification failed)' : 'not denied'}`);
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
  // rows 1.3/7.3 leave a token-requiring companion behind; section 9 a live one
  if (!comp || comp.token || comp.live) {
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
async function newTask(title, message, { doubleTap = false, expectNavigate = true, afterTap, verified = false, agent } = {}) {
  await a.tap(await scrollTo('Start a new task'));
  await a.waitFor('Start session', 15_000);
  if (agent) await a.tap(agent);
  await a.tap('Fix flaky API tests');
  a.type(title);
  a.key('KEYCODE_BACK');
  await a.tap('Describe what the agent should do');
  if (verified) await a.typeVerified(message);
  else a.type(message);
  a.key('KEYCODE_BACK');
  const btn = await a.waitFor('Start a new agent session', 10_000);
  a.shell(`input tap ${btn.x} ${btn.y}`);
  if (doubleTap) a.shell(`input tap ${btn.x} ${btn.y}`);
  if (afterTap) afterTap();
  if (expectNavigate) await a.sleep(2500);
}

/* ---- section 9 (ADR-015 §7 step 11): live sessions on the real Claude Code CLI ---- */

// Its own CHERNOBYL_HOME: only the claude adapter (no demo agents), one
// allow-listed workspace that must not contain the data dir (§4.2.3).
const LIVE_HOME = join(process.env.TEMP ?? '.', 'chernobyl-device-run-live');
const LIVE_WS = process.env.LIVE_WS ?? join(process.env.TEMP ?? '.', 'chernobyl-live-ws');
// every child claude.exe runs this model: tiny prompts on Haiku cost cents per run
const LIVE_MODEL = process.env.LIVE_MODEL ?? 'haiku';
const FAKE_KEY = 'sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKEFAKE';
const live = { sid: null, sids: [], obs: null, costs: new Map(), model: null, echoTurns: null, queued: new Set() };

async function startLive(extraAdapters = []) {
  mkdirSync(LIVE_WS, { recursive: true });
  mkdirSync(LIVE_HOME, { recursive: true });
  const cfg = {
    version: 1,
    name: 'CHERNOBYL host',
    dataDir: LIVE_HOME,
    adapters: [{ id: 'claude-1', type: 'claude', enabled: true, workspaces: [LIVE_WS], maxSessions: 3 }, ...extraAdapters],
    retention: { events: 5000, commands: 5000, eventsMaxAgeMs: 604800000 },
    diagnostics: { port: 9487 },
    probeIntervalMs: 15000,
    server: { port: 8787, bind: '127.0.0.1', requireE2e: false },
  };
  writeFileSync(join(LIVE_HOME, 'companion.json'), JSON.stringify(cfg, null, 2));
  // the runner may itself run inside Claude Code: none of its session
  // variables may reach the child CLI, only the model choice
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^(CLAUDE|ANTHROPIC_)/i.test(k)));
  env.ANTHROPIC_MODEL = LIVE_MODEL;
  if (comp) c.stopCompanion(comp);
  live.obs?.close();
  live.obs = null;
  await a.sleep(800);
  comp = c.startCompanion({ home: LIVE_HOME, log: join(OUT, '9-companion-full.txt'), env });
  comp.live = true;
  await c.waitListening(comp);
}

/** A loopback client subscribed to the live sessions: the companion's own
 *  view of each transcript, to compare the phone against, plus turn costs. */
async function observer() {
  if (live.obs && live.obs.closed == null) return live.obs;
  const o = await hc.connect(`ws://127.0.0.1:${comp.port}`);
  o.send({ type: 'hello', clientId: 'device-run-observer', protocol: 2 });
  o.views = new Map();
  o.ws.on('message', (d) => {
    let m;
    try {
      m = JSON.parse(d.toString());
    } catch {
      return;
    }
    if (m.type !== 'transcript' && m.type !== 'transcript_snapshot') return;
    let v = o.views.get(m.sessionId);
    if (!v || m.type === 'transcript_snapshot') {
      v = { items: new Map(), turns: new Map(), state: v?.state ?? null, mcp: v?.mcp ?? [] };
      o.views.set(m.sessionId, v);
    }
    const turn = (t) => {
      v.turns.set(t.id, t);
      if (t.costUsd != null) live.costs.set(t.id, t.costUsd);
    };
    const state = (s) => {
      v.state = s;
      if (s?.model) live.model = s.model;
    };
    if (m.type === 'transcript_snapshot') {
      for (const it of m.items) v.items.set(it.id, it);
      m.turns.forEach(turn);
      state(m.state);
      v.mcp = m.mcp;
      return;
    }
    for (const op of m.ops) {
      if (op.op === 'item') {
        v.items.set(op.item.id, op.item);
        if (op.item.kind === 'user' && op.item.queued) live.queued.add(op.item.text);
      }
      else if (op.op === 'append') {
        const it = v.items.get(op.id);
        if (it && op.field === 'text') it.text += op.text;
        else if (it?.tool) it.tool[op.field] = (it.tool[op.field] ?? '') + op.text;
      } else if (op.op === 'turn') turn(op.turn);
      else if (op.op === 'state') state(op.state);
      else if (op.op === 'mcp') v.mcp = op.servers;
    }
  });
  for (const sid of live.sids) o.send({ type: 'session_subscribe', commandId: `obs-${Date.now()}-${sid}`, sessionId: sid });
  live.obs = o;
  return o;
}
const view = () => live.obs?.views.get(live.sid) ?? { items: new Map(), turns: new Map(), state: null, mcp: [] };
const endedTurns = () => [...view().turns.values()].filter((t) => t.status !== 'running');
const totalCost = () => [...live.costs.values()].reduce((x, y) => x + y, 0);
/** The companion snapshot (`state` replies carry no commandId: take the next one). */
async function snapshotNow() {
  const o = await observer();
  o.send({ type: 'list_state', commandId: `ls-${Date.now()}` });
  const st = await o.next((t) => t.includes('"type":"state"'), 5000, true);
  return st ? JSON.parse(st).snapshot : null;
}
async function until(f, ms, step = 500) {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) return v;
    if (Date.now() > deadline) return null;
    await a.sleep(step);
  }
}

/** One of the app's own views (never a system dialog: the inline "Allow" is the app's). */
const appNode = (re, nodes = a.dump()) => nodes.find((n) => n.pkg === PKG && (re.test(n.text) || re.test(n.desc))) ?? null;
const waitApp = (re, ms = 15_000) => until(() => appNode(re), ms);
async function tapApp(re, ms = 15_000) {
  const n = (await waitApp(re, ms)) ?? (await reveal(re));
  if (!n) throw new Error(`not on screen: ${re}\nscreen: ${a.screenText().slice(0, 600)}`);
  return a.tap(n);
}

async function ensureLive(row) {
  if (!comp?.live) {
    await startLive();
    deviceId = null;
  }
  if (!deviceId) await freshPair(row);
  await goHome();
  if (!(await waitText(/hosts? online/, 45_000))) throw new Error('phone not connected');
  await observer();
}
async function openSession(sid) {
  a.shell(`am start -a android.intent.action.VIEW -d 'chernobyl:///session/${sid}' ${PKG}`);
  await waitApp(/^Message Claude Code/, 20_000);
}
/** Starts a Claude session from the phone's new-task screen; resolves its id from the companion. */
async function newLiveSession(title, prompt) {
  await newTask(title, prompt, { verified: true });
  const sid = await until(async () => (await snapshotNow())?.sessions?.find((s) => s.title === title)?.id, 30_000, 1000);
  if (!sid) throw new Error(`no session titled ${title}`);
  live.sid = sid;
  live.sids.push(sid);
  live.obs.send({ type: 'session_subscribe', commandId: `obs-${Date.now()}`, sessionId: sid });
  return sid;
}
/** The shared conversation for rows 9.2-9.11 (one CLI process keeps the cost down). */
async function ensureLiveSession(row) {
  await ensureLive(row);
  if (!live.sid) {
    await newLiveSession('Live session', 'Reply with the word OK.');
    await waitTurnEnd(0, 120_000);
  } else await openSession(live.sid);
}
/** Taps "Jump to latest" until it goes: one tap often lands short of the end
 *  after a long text item (app issue); the extra taps are counted for the report. */
async function jumpLatest() {
  for (let i = 0; i < 5; i++) {
    const j = appNode(/^Jump to latest$/);
    if (!j) return;
    if (i > 0) live.extraJumps = (live.extraJumps ?? 0) + 1;
    await a.tap(j);
    await a.sleep(800);
  }
}
/** Scrolls the conversation down until `re` shows: an expanded card can run
 *  past the composer, and the list does not always follow new items (app
 *  issue: no "Jump to latest" pill either, 9.10-approval.png). */
async function reveal(re) {
  for (let i = 0; i < 4; i++) {
    const n = await waitApp(re, 2000);
    if (n) return n;
    a.shell('input swipe 360 1000 360 650 300');
    await a.sleep(700);
  }
  return null;
}
/** Types a prompt into the conversation composer; `sendTyped()` taps Send. */
async function typePrompt(text) {
  const field = await tapApp(/^Message .+…$/);
  await a.typeVerified(text);
  if (!live.kbShot) {
    // the conversation view does not move the composer above the keyboard
    live.kbShot = true;
    shot('9', 'keyboard-over-composer');
  }
  return field;
}
/** The soft keyboard covers the composer (the Send node stays in the tree
 *  under it, so a tap would hit a key): hide it first. */
async function hideKeyboard() {
  if (/mInputShown=true/.test(a.shell('dumpsys input_method'))) {
    a.key('KEYCODE_BACK');
    await a.sleep(600);
  }
}
async function sendTyped() {
  await hideKeyboard();
  return tapApp(/^Send prompt to agent$/, 5000);
}
async function sendPrompt(text) {
  await typePrompt(text);
  await sendTyped();
}
/** Waits until more turns have ended than `n0` and none runs. */
const waitTurnEnd = (n0, ms = 120_000) =>
  until(() => endedTurns().length > n0 && ![...view().turns.values()].some((t) => t.status === 'running'), ms, 1000);
/** The session's pending request (snapshot record), once Claude asked. */
const waitPerm = (ms = 120_000) =>
  until(async () => (await snapshotNow())?.permissions?.find((p) => p.sessionId === live.sid && p.status === 'pending'), ms, 1500);
/** Requests the runner may allow: inside the workspace (a bare name or a path under it). */
const inWorkspace = (p) => {
  const cmd = p.command ?? '';
  const norm = (s) => s.replace(/\\/g, '/').toLowerCase();
  return !cmd.includes('..') && (norm(cmd).includes(norm(LIVE_WS)) || !/[\\/]|[a-z]:/i.test(cmd));
};
/** The phone's text item that starts with `prefix` (number runs keep rows apart). */
const phoneText = (prefix, nodes = a.dump()) => nodes.find((n) => n.pkg === PKG && n.text.startsWith(prefix))?.text ?? null;
const obsText = (prefix) => [...view().items.values()].find((i) => i.kind === 'text' && i.text.trimStart().startsWith(prefix)) ?? null;
const contiguous = (s, from, to) => {
  const ns = s.trim().split(/\s+/).map(Number);
  return ns.length === to - from + 1 && ns.every((n, i) => n === from + i);
};

Object.assign(ROWS, {
  // 9.1: "Starting Claude Code…", then the text item grows in place
  async '9.1'() {
    await ensureLive('9.1');
    const title = 'Live 9.1';
    await newTask(title, 'Print the numbers 1 to 1500 separated by spaces.', { verified: true, expectNavigate: false });
    const sid = await until(async () => (await snapshotNow())?.sessions?.find((s) => s.title === title)?.id, 30_000, 300);
    if (!sid) throw new Error('no session');
    live.sid = sid;
    live.sids.push(sid);
    live.obs.send({ type: 'session_subscribe', commandId: `obs-${Date.now()}`, sessionId: sid });
    // the hint shows while the CLI initializes; a dump works then (nothing streams yet)
    const hint = await until(() => (obsText('1 2 3') ? 'late' : a.find(/Starting Claude Code/)), 30_000, 0);
    const starting = !!hint && hint !== 'late';
    if (starting) shot('9.1', 'starting');
    // while text streams, uiautomator never gets an idle UI (a dump blocks ~11 s and
    // returns the stale tree), so the phone is sampled by screencap, paired with
    // the companion's text length at that moment
    const frames = [];
    await until(() => {
      const len = obsText('1 2 3')?.text.length ?? 0;
      if (len && len !== frames.at(-1)?.len) frames.push({ len, png: a.adb(['exec-out', 'screencap', '-p'], { binary: true }) });
      return endedTurns().length > 0;
    }, 120_000, 0);
    const mid = frames.filter((f) => f.len < 6000);
    mid.slice(0, 2).forEach((f, i) => writeFileSync(join(OUT, `9.1-stream-${i + 1}.png`), f.png));
    const differ = mid.length >= 2 && !mid[0].png.equals(mid[1].png);
    const jumpShown = !!appNode(/^Jump to latest$/);
    await jumpLatest();
    const done = await waitApp(/^Done · /, 10_000);
    shot('9.1', 'done');
    const finalOk = contiguous(phoneText('1 2 3 4 5') ?? '', 1, 1500);
    record('9.1', starting && differ && finalOk && !!done && /haiku/i.test(live.model ?? ''), `"Starting Claude Code…" shown=${starting}; phone frames while streaming at companion text lengths ${mid.map((f) => f.len).join(' -> ')} (stream-1/stream-2 differ=${differ}); final phone text is 1..1500=${finalOk}; "Jump to latest" shown at turn end=${jumpShown}; footer "${done?.desc || done?.text || 'none'}"; model=${live.model}; cost so far $${totalCost().toFixed(4)}`);
  },
  // 9.3: inline Allow inside the card; the Approvals tab entry clears; the file exists
  async '9.3'() {
    await ensureLiveSession('9.3');
    const n0 = endedTurns().length;
    await sendPrompt('Create a.txt containing hi.');
    const perm = await waitPerm();
    if (!perm) return record('9.3', false, 'no permission request within 120 s');
    if (!inWorkspace(perm)) return record('9.3', false, `request outside the workspace: ${perm.title}`);
    await jumpLatest();
    const card = await waitApp(/^Tool Write: awaiting approval$/, 20_000);
    shot('9.3', 'inline-approval');
    await goHome();
    await a.tap('Approvals');
    await a.sleep(1500);
    const pendingRow = (nodes) => {
      const cut = nodes.findIndex((n) => /^decided$/i.test(n.text));
      return (cut >= 0 ? nodes.slice(0, cut) : nodes).find((n) => n.pkg === PKG && (n.desc.includes(perm.command) || n.text.includes(perm.command))) ?? null;
    };
    const row = pendingRow(a.dump());
    shot('9.3', 'approvals-pending');
    // the inbox row opens the conversation at the card
    if (row) await a.tap(row);
    else await openSession(live.sid);
    const atCard = await waitApp(/^Tool Write: awaiting approval$/, 20_000);
    await tapApp(/^Allow$/);
    const doneCard = await waitApp(/^Tool Write: completed$/, 60_000);
    shot('9.3', 'allowed');
    await waitTurnEnd(n0);
    await goHome();
    await a.tap('Approvals');
    await a.sleep(1500);
    const cleared = !pendingRow(a.dump());
    shot('9.3', 'approvals-after');
    const file = existsSync(join(LIVE_WS, 'a.txt')) ? readFileSync(join(LIVE_WS, 'a.txt'), 'utf8').trim() : null;
    record('9.3', !!card && !!row && !!atCard && !!doneCard && cleared && file !== null, `inline card "Tool Write: awaiting approval"=${!!card}; listed on the Approvals tab=${!!row}; the row opened the conversation at the card=${!!atCard}; after Allow "Tool Write: completed"=${!!doneCard}; gone from the pending list=${cleared}; a.txt in the workspace=${JSON.stringify(file)}`);
    await openSession(live.sid);
  },
  // 9.2: tool card input then output; the Edit card shows its diff
  async '9.2'() {
    await ensureLiveSession('9.2');
    const n0 = endedTurns().length;
    await sendPrompt('Create e.txt containing hi, then edit it to say hello.');
    const allowed = [];
    for (let i = 0; i < 4; i++) {
      const perm = await waitPerm(i === 0 ? 120_000 : 30_000);
      if (!perm) break;
      await jumpLatest();
      const n = await waitApp(/^Tool \w+: awaiting approval$/, 20_000);
      if (i === 0) shot('9.2', 'write-approval');
      if (/Edit/.test(n?.desc ?? '')) shot('9.2', 'edit-approval');
      if (!inWorkspace(perm)) {
        await tapApp(/^Deny$/);
        continue;
      }
      await tapApp(/^Allow$/);
      allowed.push(/Tool (\w+)/.exec(n?.desc ?? '')?.[1] ?? '?');
      await a.sleep(2000);
    }
    await waitTurnEnd(n0);
    await jumpLatest();
    // expand this turn's cards (the lowest match; earlier rows left Write cards
    // above): Write shows its input and output, then Edit its diff
    const last = (re) => a.dump().filter((n) => n.pkg === PKG && re.test(n.desc)).sort((p, q) => q.y - p.y)[0] ?? null;
    const write = last(/^Tool Write: completed$/);
    if (write) await a.tap(write);
    await a.sleep(800);
    const writeScreen = a.screenText();
    shot('9.2', 'write-expanded');
    const writeShown = /Input/.test(writeScreen) && /e\.txt/.test(writeScreen) && /File created successfully/.test(writeScreen);
    if (write) await a.tap(write); // collapse again; the header stays put
    await a.sleep(800);
    const edit = last(/^Tool Edit: completed$/);
    if (edit) await a.tap(edit);
    await a.sleep(800);
    // the diff follows the Input block, below the fold
    const diff = await reveal(/^Diff .*e\.txt/);
    shot('9.2', 'edit-diff');
    const obsWrite = [...view().items.values()].find((i) => i.tool?.name === 'Write' && /e\.txt/.test(i.tool.title ?? i.tool.input ?? ''));
    const content = existsSync(join(LIVE_WS, 'e.txt')) ? readFileSync(join(LIVE_WS, 'e.txt'), 'utf8').trim() : null;
    record('9.2', writeShown && !!diff, `allowed ${allowed.join(', ')}; Write card expanded shows its Input and the output "File created successfully…"=${writeShown} (companion output ${JSON.stringify(obsWrite?.tool.output?.slice(0, 30) ?? null)}); Edit card diff label: ${diff ? `"${diff.desc}"` : 'none'}; companion Write diff patch ${JSON.stringify(obsWrite?.tool.diffs?.[0]?.patch ?? null)}; e.txt=${JSON.stringify(content)}`);
  },
  // 9.4: "Deny and tell Claude why": the tool shows denied, Claude continues, the turn ends
  async '9.4'() {
    await ensureLiveSession('9.4');
    const n0 = endedTurns().length;
    await sendPrompt('Create b.txt containing hi.');
    const perm = await waitPerm();
    if (!perm) return record('9.4', false, 'no permission request within 120 s');
    await jumpLatest();
    await waitApp(/^Tool \w+: awaiting approval$/, 20_000);
    await tapApp(/^More options$/);
    const why = await reveal(/and tell Claude why$/);
    if (!why) {
      shot('9.4', 'more-options');
      await tapApp(/^Deny$/);
      return record('9.4', false, `no "… and tell Claude why" choice under More (9.4-more-options.png); the request's options: ${JSON.stringify((perm.options ?? []).map((o) => ({ label: o.label, decision: o.decision, scope: o.scope, takesMessage: o.takesMessage })))}`);
    }
    await a.tap(why);
    await tapApp(/^Reason for Claude$/);
    await a.typeVerified('Not now. Reply OK.');
    await hideKeyboard();
    shot('9.4', 'reason');
    await tapApp(/^Send reason$/);
    const denied = await waitApp(/^Tool \w+: denied$/, 30_000);
    // a retry by another route is refused too: the row is about the turn ending
    let extra = 0;
    const ended = await until(async () => {
      if (await waitTurnEnd(n0, 1)) return true;
      const again = (await snapshotNow())?.permissions?.some((x) => x.sessionId === live.sid && x.status === 'pending');
      const deny = again && (await reveal(/^Deny$/));
      if (deny && extra < 3) {
        extra++;
        await a.tap(deny);
      }
      return false;
    }, 120_000, 1500);
    shot('9.4', 'after');
    const toolItem = [...view().items.values()].find((i) => i.tool?.permissionId === perm.id);
    const after = toolItem ? [...view().items.values()].filter((i) => i.kind === 'text' && i.ts >= toolItem.ts && i.turnId === toolItem.turnId) : [];
    record('9.4', !!denied && !!ended && after.length > 0, `card "${denied?.desc ?? 'no denied mark'}"; turn ended=${!!ended}; Claude's text after the denial: ${JSON.stringify(after.map((i) => i.text).join(' ').slice(0, 120))}; extra requests denied plainly: ${extra}`);
  },
  // 9.5: a follow-up mid-turn shows "Queued" and is answered, in that turn or
  // the next (Claude merges only sometimes; the companion advertises `queue`).
  // Two cases: sent while text streams (no tool in the turn), and sent while
  // the turn is about to run a tool (Bash seq)
  async '9.5'() {
    await ensureLiveSession('9.5');
    const cases = [
      { tag: '9.5a', first: 'Without tools, print 2001 to 2800 separated by spaces.', word: 'PINEAPPLE', streamFirst: true },
      { tag: '9.5b', first: 'Use bash seq to print 5001 to 5800.', word: 'MANGO', streamFirst: false },
    ];
    const notes = [];
    let pass = true;
    for (const k of cases) {
      const n0 = endedTurns().length;
      const t0 = view().turns.size;
      const follow = `Then say ${k.word}.`;
      // no dumps once the turn runs (see 9.1): the follow-up goes in by the
      // field and button positions found while typing the first prompt
      const field = await typePrompt(k.first);
      const send = await sendTyped();
      await a.tap(field);
      a.type(follow);
      await hideKeyboard();
      await until(() => (k.streamFirst ? [...view().items.values()].some((i) => i.kind === 'text' && i.text.length > 300 && !i.done) : [...view().turns.values()].some((t) => t.status === 'running')), 60_000, 200);
      const runningAtSend = [...view().turns.values()].some((t) => t.status === 'running');
      await a.tap(send);
      await a.sleep(1500);
      shot(k.tag, 'joins');
      await waitTurnEnd(n0);
      await jumpLatest();
      shot(k.tag, 'answered');
      const newTurns = [...view().turns.values()].slice(t0);
      const said = [...view().items.values()].find((i) => i.kind === 'text' && i.text.includes(k.word));
      const merged = newTurns.length === 1 && said?.turnId === newTurns[0].id;
      const settled = newTurns.length > 0 && newTurns.every((t) => t.status !== 'running');
      pass &&= runningAtSend && live.queued.has(follow) && !!said && settled;
      notes.push(`${k.tag} (${k.streamFirst ? 'sent while text streamed, no tool' : 'sent before a Bash call'}): sent while the turn ran=${runningAtSend}; companion marked it queued=${live.queued.has(follow)}; turns: ${newTurns.length} (${newTurns.map((t) => t.status).join(', ')}); ${k.word} answered=${!!said} (merged into the first turn=${merged}); all turns settled=${settled}`);
    }
    record('9.5', pass, `${notes.join('. ')}. While sending, the phone shows "Queued" (${cases.map((k) => `${k.tag}-joins.png`).join(', ')}); ${cases.map((k) => `${k.tag}-answered.png`).join(', ')} show the word under its own turn footer`);
  },
  // 9.6: Stop mid-stream -> "Stopped"; the next prompt works in the same session
  async '9.6'() {
    await ensureLiveSession('9.6');
    const sid = live.sid;
    const n0 = endedTurns().length;
    // long and tool-free (Claude answers a short run with `seq` in a second)
    await typePrompt('Without tools, print 3001 to 6000 separated by spaces.');
    const send = await sendTyped();
    // dumps stall once text streams (see 9.1), so Stop is tapped at its place
    // left of Send in the composer row (measured on the 720 px emulator)
    const streaming = await until(() => (obsText('3001 3002')?.text.length ?? 0) > 400, 90_000, 200);
    shot('9.6', 'streaming');
    a.shell(`input tap ${send.x - 178} ${send.y}`);
    await waitTurnEnd(n0, 30_000);
    await jumpLatest();
    // the turn footer: an accessible view labelled "Stopped" (a tool card says "Tool …: cancelled")
    const stopped = (await reveal(/^Stopped$/)) && a.dump().find((n) => n.pkg === PKG && n.desc === 'Stopped');
    shot('9.6', 'stopped');
    const interrupted = endedTurns().at(-1)?.status === 'interrupted';
    const cut = obsText('3001 3002')?.text.trim().split(/\s+/).length ?? 0;
    const n1 = endedTurns().length;
    await sendPrompt('Reply with the word OK.');
    const ok = await waitTurnEnd(n1);
    await jumpLatest();
    const done = await waitApp(/^Done · /, 15_000);
    shot('9.6', 'next-prompt');
    const last = endedTurns().at(-1);
    record('9.6', !!streaming && !!stopped && interrupted && !!ok && last?.status === 'completed' && live.sid === sid, `stopped after ${cut} of 3000 numbers; footer "Stopped"=${!!stopped}; companion turn status=${interrupted ? 'interrupted' : endedTurns().at(-2)?.status}; next prompt in the same session -> ${last?.status} ("${done?.desc || 'no footer'}")`);
  },
  // 9.7: backgrounded; the approval arrives as a notification; the tap opens the card
  async '9.7'() {
    await ensureLiveSession('9.7');
    const n0 = endedTurns().length;
    await sendPrompt('Create d.txt containing hi.');
    a.key('KEYCODE_HOME');
    let notif = null;
    for (let i = 0; i < 40 && !notif; i++) {
      await a.sleep(2000);
      a.shell('cmd statusbar expand-notifications');
      await a.sleep(800);
      notif = a.dump().find((n) => /Approval needed/.test(n.text));
      if (!notif) a.shell('cmd statusbar collapse');
    }
    shot('9.7', 'notification');
    if (!notif) {
      launch();
      return record('9.7', false, 'no approval notification within 90 s');
    }
    await a.tap(notif);
    const card = await waitApp(/^Tool \w+: awaiting approval$/, 20_000);
    const allow = appNode(/^Allow$/);
    shot('9.7', 'opened-at-card');
    await tapApp(/^Deny$/);
    await waitTurnEnd(n0);
    record('9.7', !!card && !!allow, `notification "${notif.text}"; the tap opened the conversation with "${card?.desc ?? 'no card'}" and its Allow button on screen=${!!allow}; denied afterwards`);
  },
  // 9.8: network off/on mid-stream: the transcript resumes with no gap or duplicate
  async '9.8'() {
    await ensureLiveSession('9.8');
    const n0 = endedTurns().length;
    await sendPrompt('Print the numbers 4001 to 4800 separated by spaces.');
    const before = await until(() => obsText('4001 4002')?.text.length, 60_000, 200);
    shot('9.8', 'streaming');
    a.shell('svc wifi disable');
    a.shell('svc data disable');
    const offline = await waitText(/Not connected|Reconnecting/, 30_000);
    shot('9.8', 'offline');
    await waitTurnEnd(n0, 120_000);
    await a.sleep(5000);
    netOn();
    await until(() => !/Not connected/.test(a.screenText()), 90_000, 1000);
    await jumpLatest();
    const final = await until(() => {
      const t = phoneText('4001 4002');
      return t && /4800\s*$/.test(t) ? t : null;
    }, 60_000, 1000);
    shot('9.8', 'resumed');
    const copies = a.dump().filter((n) => n.pkg === PKG && n.text.startsWith('4001 4002')).length;
    const ref = obsText('4001 4002')?.text ?? '';
    const norm = (s) => (s ?? '').trim().replace(/\s+/g, ' ');
    const same = !!final && norm(final) === norm(ref);
    record('9.8', !!before && before < ref.length && !!offline && same && contiguous(final ?? '', 4001, 4800) && copies === 1, `network cut at ${before} of ${ref.length} chars of the companion's text item (9.8-streaming.png); offline banner=${!!offline}; after reconnect the phone text equals the companion's item=${same}, is 4001..4800 contiguous (no gap, no repeat)=${contiguous(final ?? '', 4001, 4800)}; copies on screen=${copies}`);
  },
  // 9.9: the MCP chip and sheet render the user's real servers (no sign-in)
  async '9.9'() {
    await ensureLiveSession('9.9');
    const chip = await waitApp(/^MCP servers: \d+ of \d+ connected/, 20_000);
    const mode = appNode(/^Permission mode: /);
    shot('9.9', 'chips');
    if (!chip) return record('9.9', false, 'no MCP chip in the header');
    await a.tap(chip);
    const sheet = await waitApp(/^MCP servers$/, 10_000);
    await a.sleep(800);
    shot('9.9', 'sheet');
    const statuses = view().mcp.reduce((m, s) => ({ ...m, [s.status]: (m[s.status] ?? 0) + 1 }), {});
    await tapApp(/^Close$/);
    record('9.9', !!sheet, `chip "${chip.desc}"; mode chip "${mode?.desc ?? 'none'}"; sheet "MCP servers" opened=${!!sheet}; companion statuses ${JSON.stringify(statuses)}. Sign-in part NOT RUN: it would start an OAuth flow on the user's real MCP servers and open the desktop browser`);
  },
  // 9.10: a persistent "always allow" suggestion confirms the rule and the file (cancelled here)
  async '9.10'() {
    await ensureLiveSession('9.10');
    writeFileSync(join(LIVE_WS, 'fake.txt'), `${FAKE_KEY}\n`);
    live.echoTurns = endedTurns().length;
    await sendPrompt('Run in bash: cp fake.txt k.txt; cat k.txt');
    const perm = await waitPerm();
    if (!perm) return record('9.10', null, 'NOT RUN: no permission request (the command ran without asking)');
    await jumpLatest();
    await waitApp(/^Tool \w+: awaiting approval$/, 20_000);
    shot('9.10', 'approval');
    const persistent = (perm.options ?? []).find((o) => o.scope === 'persistent' && o.decision !== 'reject');
    let res;
    if (!persistent) res = [null, `NOT RUN: no persistent suggestion offered (options: ${(perm.options ?? []).map((o) => o.label).join(' / ')})`];
    else {
      await tapApp(/^More options$/);
      await a.tap(await reveal(new RegExp(`^${persistent.label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`)));
      const modal = await waitApp(/^Rule: /, 5000);
      shot('9.10', 'confirm');
      await tapApp(/^Cancel$/);
      await a.sleep(1000);
      const written = existsSync(join(LIVE_WS, '.claude', 'settings.local.json'));
      const stillPending = (await snapshotNow())?.permissions?.some((p) => p.id === perm.id && p.status === 'pending');
      res = [!!modal && /settings\.local\.json/.test(modal.text) && !written && stillPending, `option "${persistent.label}"; confirm shows ${JSON.stringify(modal?.text ?? null)}; cancelled: settings.local.json written=${written}, request still pending=${stillPending}`];
    }
    record('9.10', ...res);
    // 9.11 continues with this request: allow it once
    await tapApp(inWorkspace(perm) ? /^Allow$/ : /^Deny$/);
  },
  // 9.11: a key in tool output shows as «redacted:…», never the key
  async '9.11'() {
    await ensureLiveSession('9.11');
    if (live.echoTurns == null) {
      writeFileSync(join(LIVE_WS, 'fake.txt'), `${FAKE_KEY}\n`);
      live.echoTurns = endedTurns().length;
      await sendPrompt('Run in bash: cp fake.txt k.txt; cat k.txt');
    }
    // allow whatever else the command needs (workspace only)
    const ended = await until(async () => {
      if (await waitTurnEnd(live.echoTurns, 1)) return true;
      const p = (await snapshotNow())?.permissions?.find((x) => x.sessionId === live.sid && x.status === 'pending');
      const btn = p && (await reveal(inWorkspace(p) ? /^Allow$/ : /^Deny$/));
      if (btn) await a.tap(btn);
      return false;
    }, 150_000, 1500);
    await jumpLatest();
    for (const n of a.dump().filter((x) => x.pkg === PKG && /^Tool Bash: completed$/.test(x.desc))) await a.tap(n);
    await a.sleep(800);
    const screen = a.screenText();
    shot('9.11', 'redacted');
    const all = JSON.stringify([...view().items.values()]);
    const masked = /«redacted:[^»]*»/.exec(screen)?.[0] ?? null;
    record('9.11', !!ended && !!masked && !screen.includes('FAKEFAKE') && !all.includes('FAKEFAKEFAKE'), `phone shows ${masked ?? 'no redaction marker'}; key text on screen=${screen.includes('FAKEFAKE')}; key anywhere in the companion's transcript=${all.includes('FAKEFAKEFAKE')}; turn ended=${!!ended}`);
  },
  // 9.12: OpenCode on local Ollama, attached to an ISOLATED `opencode serve`
  // (HOME, USERPROFILE and XDG_* under a temp dir, as tests/opencode_e2e.rs
  // spawn_server does): streaming, an inline bash approval, an abort as Stopped
  async '9.12'() {
    const exe = process.env.OPENCODE_BIN ?? join(process.env.APPDATA ?? '', 'npm/node_modules/opencode-ai/bin/opencode.exe');
    if (!existsSync(exe)) return record('9.12', null, `NOT RUN: no opencode CLI at ${exe}`);
    const ALIAS = 'chernobyl-qwen35-24k';
    const OLLAMA = 'http://127.0.0.1:11434';
    const made = await fetch(`${OLLAMA}/api/create`, { method: 'POST', body: JSON.stringify({ model: ALIAS, from: 'qwen3.5:4b', parameters: { num_ctx: 24576 }, stream: false }) }).then((r) => r.text()).catch((e) => String(e));
    if (!/success/.test(made)) return record('9.12', null, `NOT RUN: no local Ollama qwen3.5:4b (${made.slice(0, 120)})`);
    const base = join(process.env.TEMP ?? '.', `chernobyl-oc-device-${Date.now()}`);
    const dir = (n) => {
      mkdirSync(join(base, 'iso', n), { recursive: true });
      return join(base, 'iso', n);
    };
    const work = join(base, 'work');
    mkdirSync(join(base, 'iso', 'config', 'opencode'), { recursive: true });
    mkdirSync(work, { recursive: true });
    // a git root stops OpenCode's walk up from the cwd into the user's profile
    spawnSync('git', ['init', '-q'], { cwd: work });
    writeFileSync(join(base, 'iso', 'config', 'opencode', 'opencode.json'), JSON.stringify({
      $schema: 'https://opencode.ai/config.json', autoupdate: false, share: 'disabled', model: `ollama/${ALIAS}`,
      provider: { ollama: { npm: '@ai-sdk/openai-compatible', name: 'Ollama', options: { baseURL: `${OLLAMA}/v1` }, models: { [ALIAS]: { name: 'qwen3.5 4b 24k', limit: { context: 24576, output: 4096 } } } } },
      permission: { bash: 'ask', edit: 'ask' },
    }));
    const port = 14096;
    const log = openSync(join(base, 'serve.log'), 'a');
    const env = { ...process.env, HOME: dir('home'), USERPROFILE: dir('home'), XDG_DATA_HOME: dir('data'), XDG_CONFIG_HOME: join(base, 'iso', 'config'), XDG_CACHE_HOME: dir('cache'), XDG_STATE_HOME: dir('state') };
    const serve = spawn(exe, ['serve', '--port', String(port), '--hostname', '127.0.0.1'], { cwd: work, env, stdio: ['ignore', log, log] });
    try {
      const up = await until(() => fetch(`http://127.0.0.1:${port}/session`).then((r) => r.ok).catch(() => false), 60_000, 1000);
      if (!up) return record('9.12', false, 'isolated opencode serve never answered');
      await startLive([{ id: 'oc-1', type: 'opencode', enabled: true, endpoint: `http://127.0.0.1:${port}` }]);
      await ensureLive('9.12');
      const title = 'Live 9.12';
      // the wording of tests/opencode_e2e.rs: the 4B model otherwise just prints the command
      await newTask(title, 'Use the bash tool to run exactly this command: echo hi > note.txt', { verified: true, agent: / · OpenCode$/ });
      const sid = await until(async () => (await snapshotNow())?.sessions?.find((x) => x.title === title)?.id, 30_000, 1000);
      if (!sid) return record('9.12', false, 'no OpenCode session created');
      live.sid = sid;
      live.obs.send({ type: 'session_subscribe', commandId: `obs-${Date.now()}`, sessionId: sid });
      // a CPU-bound 4B model reads OpenCode's prompt slowly: minutes
      const perm = await waitPerm(600_000);
      if (!perm) return record('9.12', false, 'no bash approval within 10 min');
      await jumpLatest();
      const card = await waitApp(/^Tool bash: awaiting approval$/, 30_000);
      shot('9.12', 'approval');
      await tapApp(/^Allow once$/);
      const ran = await until(() => [...view().items.values()].find((i) => i.tool?.name === 'bash' && i.tool.status === 'completed'), 300_000, 2000);
      await waitTurnEnd(0, 600_000);
      await jumpLatest();
      shot('9.12', 'ran');
      // abort mid-turn
      const n1 = endedTurns().length;
      await typePrompt('Write a long poem about the sea.');
      const send = await sendTyped();
      const stop = await waitApp(/^Stop$/, 10_000);
      // abort once the poem streams
      const poem = await until(() => [...view().items.values()].find((i) => i.kind === 'text' && !i.done && i.text.length > 40), 300_000, 500);
      const atStop = poem?.text.length ?? 0;
      if (stop) await a.tap(stop);
      else a.shell(`input tap ${send.x - 178} ${send.y}`);
      await waitTurnEnd(n1, 120_000);
      const streamed = !!poem;
      const stopped = await waitApp(/^Stopped$/, 30_000);
      shot('9.12', 'stopped');
      record('9.12', !!card && !!ran && streamed && !!stopped && endedTurns().at(-1)?.status === 'interrupted', `isolated opencode serve 1.19 on Ollama qwen3.5:4b; inline card "${card?.desc ?? 'none'}" -> Allow once -> bash ${ran ? `completed, output ${JSON.stringify((ran.tool.output ?? '').trim().slice(0, 40))}` : 'not completed'}; the poem streamed (${atStop} chars when Stop was tapped, ${(poem && view().items.get(poem.id)?.text.length) ?? 0} at the end)=${streamed}; Stop -> footer "Stopped"=${!!stopped}, companion turn ${endedTurns().at(-1)?.status}`);
    } finally {
      spawnSync('taskkill', ['/F', '/T', '/PID', String(serve.pid)]);
      await fetch(`${OLLAMA}/api/delete`, { method: 'DELETE', body: JSON.stringify({ model: ALIAS }) }).catch(() => {});
      rmSync(base, { recursive: true, force: true, maxRetries: 3 });
    }
  },
  async '9.13'() {
    record('9.13', null, 'NOT RUN: no pre-ADR-015 APK artifact kept');
  },
});

/** Ends every live session (the CLI exits on stdin EOF) and reports the spend. */
async function endLive() {
  if (!live.obs) return;
  for (const sid of live.sids) live.obs.send({ type: 'session_end', commandId: `end-${Date.now()}-${sid}`, sessionId: sid });
  await a.sleep(8000);
  console.log(`live model ${live.model}; Claude cost this run $${totalCost().toFixed(4)} over ${live.costs.size} turns; extra "Jump to latest" taps needed: ${live.extraJumps ?? 0}`);
  appendFileSync(join(OUT, '9-cost.txt'), `${new Date().toISOString()} rows ${pick.join(' ')}: model ${live.model}, ${live.costs.size} turns, total_cost_usd ${totalCost().toFixed(4)}\n`);
  live.obs.close();
}

// rows 5.1/5.3 switch the phone's network off; a row that fails midway must
// not leave it off for every row after it
const netOn = () => {
  a.shell('svc wifi enable');
  a.shell('svc data enable');
};

const pick = process.argv.slice(2);
// section 9 drives the real Claude CLI (it costs money): named rows or LIVE=1 only
const order = pick.length ? pick : Object.keys(ROWS).filter((r) => (!r.startsWith('9.') || process.env.LIVE === '1') && !r.startsWith('T.'));
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
  await endLive();
  if (comp) stop();
}
