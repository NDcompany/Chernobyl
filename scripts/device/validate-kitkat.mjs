#!/usr/bin/env node
// validate-kitkat.mjs — rows K.1-K.4 of PHONE-VALIDATION-CHECKLIST on the Android 4.4 client
// (android-kitkat/, ADR-019) in the `kitkat` AVD against a real companion with four live runtimes:
// Claude Code (Haiku), OpenCode (an isolated `opencode serve` on local Ollama), Codex and Antigravity.
//
//   ANDROID_SERIAL=emulator-5556 LIVE_WS=... LIVE_AGENT_WS=... node scripts/device/validate-kitkat.mjs [K.1 ...]
//
// Companion on 127.0.0.1:18797, the tablet dials ws://10.0.2.2:18797. Evidence: docs/validation/evidence/kitkat/.
import { mkdirSync, rmSync, writeFileSync, readFileSync, existsSync, openSync, appendFileSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
import { join } from 'node:path';
import * as a from './adb.mjs';
import * as c from './companion.mjs';
import * as hc from './hostclient.mjs';

const OUT = join(process.cwd(), 'docs/validation/evidence/kitkat');
const PKG = 'in.ndcompany.chernobyl.kitkat';
const TMP = process.env.TEMP ?? '.';
const HOME = join(TMP, 'chernobyl-kitkat-run');
const CLAUDE_WS = process.env.LIVE_WS ?? join(TMP, 'chernobyl-live-ws');
const AGENT_WS = process.env.LIVE_AGENT_WS ?? join(TMP, 'chernobyl-live-agent-ws');
const PORT = 18797;
const DIAG = 19497;
mkdirSync(OUT, { recursive: true });
const resultsPath = join(OUT, 'K-results.json');
const results = existsSync(resultsPath) ? JSON.parse(readFileSync(resultsPath, 'utf8')) : {};
const record = (row, pass, observed) => {
  results[row] = { pass, observed, at: new Date().toISOString() };
  writeFileSync(resultsPath, JSON.stringify(results, null, 2));
  console.log(`${pass === null ? 'PARTIAL/NOT RUN' : pass ? 'PASS' : 'FAIL'} ${row} — ${observed}`);
};
const shot = (name) => a.screenshot(join(OUT, `K-${name}.png`));
const until = async (f, ms, step = 700) => {
  const end = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) return v;
    if (Date.now() > end) return null;
    await a.sleep(step);
  }
};
const app = (re, nodes = a.dump()) => nodes.find((n) => n.pkg === PKG && (re.test(n.text) || re.test(n.desc))) ?? null;
const waitApp = (re, ms = 15_000) => until(() => app(re), ms);
const tapApp = async (re, ms = 15_000) => {
  const n = await waitApp(re, ms);
  if (!n) throw new Error(`not on screen: ${re}\n${a.screenText().slice(0, 500)}`);
  return a.tap(n);
};
const tapRail = (label) => tapApp(new RegExp(`^${label}`));

/* ---- companion with the four runtimes ---- */
const git = (args, cwd) => spawnSync('git', args, { cwd, encoding: 'utf8' });
function repo(dir) {
  if (existsSync(join(dir, '.git'))) return dir;
  mkdirSync(join(dir, 'src'), { recursive: true });
  writeFileSync(join(dir, 'README.md'), '# KitKat device run workspace\n');
  writeFileSync(join(dir, 'src', 'hello.txt'), 'hello from the KitKat device run\n');
  for (const args of [['init', '-q', '-b', 'main'], ['config', 'user.name', 'CHERNOBYL device run'], ['config', 'user.email', 'device-run@localhost'], ['add', '-A'], ['commit', '-q', '-m', 'Initial commit']]) git(args, dir);
  return dir;
}
/** The isolated `opencode serve` of row 9.12 (HOME, USERPROFILE and XDG_* under a temp dir). */
async function openCode() {
  const exe = process.env.OPENCODE_BIN ?? join(process.env.APPDATA ?? '', 'npm/node_modules/opencode-ai/bin/opencode.exe');
  if (!existsSync(exe)) return null;
  const ALIAS = 'chernobyl-qwen35-24k';
  const OLLAMA = 'http://127.0.0.1:11434';
  const made = await fetch(`${OLLAMA}/api/create`, { method: 'POST', body: JSON.stringify({ model: ALIAS, from: 'qwen3.5:4b', parameters: { num_ctx: 24576 }, stream: false }) }).then((r) => r.text()).catch((e) => String(e));
  if (!/success/.test(made)) return null;
  const base = join(TMP, `chernobyl-oc-kitkat-${Date.now()}`);
  const dir = (n) => {
    mkdirSync(join(base, 'iso', n), { recursive: true });
    return join(base, 'iso', n);
  };
  const work = join(base, 'work');
  mkdirSync(join(base, 'iso', 'config', 'opencode'), { recursive: true });
  mkdirSync(work, { recursive: true });
  spawnSync('git', ['init', '-q'], { cwd: work });
  writeFileSync(join(base, 'iso', 'config', 'opencode', 'opencode.json'), JSON.stringify({
    $schema: 'https://opencode.ai/config.json', autoupdate: false, share: 'disabled', model: `ollama/${ALIAS}`,
    provider: { ollama: { npm: '@ai-sdk/openai-compatible', name: 'Ollama', options: { baseURL: `${OLLAMA}/v1` }, models: { [ALIAS]: { name: 'qwen3.5 4b 24k', limit: { context: 24576, output: 4096 } } } } },
    permission: { bash: 'ask', edit: 'ask' },
  }));
  const port = 14097;
  const log = openSync(join(base, 'serve.log'), 'a');
  const env = { ...process.env, HOME: dir('home'), USERPROFILE: dir('home'), XDG_DATA_HOME: dir('data'), XDG_CONFIG_HOME: join(base, 'iso', 'config'), XDG_CACHE_HOME: dir('cache'), XDG_STATE_HOME: dir('state') };
  const serve = spawn(exe, ['serve', '--port', String(port), '--hostname', '127.0.0.1'], { cwd: work, env, stdio: ['ignore', log, log] });
  const up = await until(() => fetch(`http://127.0.0.1:${port}/session`).then((r) => r.ok).catch(() => false), 60_000, 1000);
  const stop = async () => {
    spawnSync('taskkill', ['/F', '/T', '/PID', String(serve.pid)]);
    await fetch(`${OLLAMA}/api/delete`, { method: 'DELETE', body: JSON.stringify({ model: ALIAS }) }).catch(() => {});
    rmSync(base, { recursive: true, force: true, maxRetries: 3 });
  };
  if (!up) {
    await stop();
    return null;
  }
  return { port, stop };
}

let comp = null;
let oc = null;
async function startCompanion() {
  mkdirSync(HOME, { recursive: true });
  const adapters = [
    { id: 'claude-1', type: 'claude', enabled: true, workspaces: [repo(CLAUDE_WS)], maxSessions: 3 },
    { id: 'codex-1', type: 'codex', enabled: true, workspaces: [repo(`${AGENT_WS}-codex`)] },
    { id: 'antigravity-1', type: 'antigravity', enabled: true, workspaces: [repo(`${AGENT_WS}-antigravity`)] },
  ];
  if (oc) adapters.push({ id: 'oc-1', type: 'opencode', enabled: true, endpoint: `http://127.0.0.1:${oc.port}` });
  writeFileSync(join(HOME, 'companion.json'), JSON.stringify({
    version: 1, name: 'CHERNOBYL host', dataDir: HOME, adapters,
    retention: { events: 5000, commands: 5000, eventsMaxAgeMs: 604800000 },
    diagnostics: { port: DIAG }, probeIntervalMs: 3000,
    server: { port: PORT, bind: '127.0.0.1', requireE2e: false },
  }, null, 2));
  // none of the runner's own Claude session variables may reach the child CLI, only the model choice
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^(CLAUDE|ANTHROPIC_)/i.test(k)));
  env.ANTHROPIC_MODEL = process.env.LIVE_MODEL ?? 'claude-haiku-5-5';
  if (comp) c.stopCompanion(comp);
  await a.sleep(800);
  comp = c.startCompanion({ home: HOME, port: PORT, diag: DIAG, log: join(OUT, 'K-companion-full.txt'), env });
  await c.waitListening(comp);
}

/** A loopback observer: the companion's own view of every session (what the tablet is compared against). */
let obs = null;
const views = new Map();
async function observer() {
  if (obs && obs.closed == null) return obs;
  obs = await hc.connect(`ws://127.0.0.1:${PORT}`);
  obs.send({ type: 'hello', clientId: 'kitkat-run-observer', protocol: 2 });
  obs.ws.on('message', (d) => {
    let m;
    try {
      m = JSON.parse(d.toString());
    } catch {
      return;
    }
    if (m.type !== 'transcript' && m.type !== 'transcript_snapshot') return;
    let v = views.get(m.sessionId);
    if (!v || m.type === 'transcript_snapshot') {
      v = { items: new Map(), turns: new Map() };
      views.set(m.sessionId, v);
    }
    if (m.type === 'transcript_snapshot') {
      for (const it of m.items) v.items.set(it.id, it);
      m.turns.forEach((t) => v.turns.set(t.id, t));
      return;
    }
    for (const op of m.ops) {
      if (op.op === 'item') v.items.set(op.item.id, op.item);
      else if (op.op === 'append') {
        const it = v.items.get(op.id);
        if (it && op.field === 'text') it.text += op.text;
        else if (it?.tool) it.tool[op.field] = (it.tool[op.field] ?? '') + op.text;
      } else if (op.op === 'turn') v.turns.set(op.turn.id, op.turn);
    }
  });
  return obs;
}
async function snapshot() {
  const o = await observer();
  o.send({ type: 'list_state', commandId: `ls-${Date.now()}` });
  const st = await o.next((t) => t.includes('"type":"state"'), 5000, true);
  return st ? JSON.parse(st).snapshot : null;
}

/* ---- the tablet ---- */
/** `adb input text` on 4.4 expands braces with commas inside one word: short chunks keep `{` and `}` apart. */
function typeSafe(text) {
  for (let i = 0; i < text.length; i += 24) a.shell(`input text '${text.slice(i, i + 24).replace(/ /g, '%s').replace(/'/g, `'\\''`)}'`);
}
async function typeInto(re, text) {
  const field = await until(() => a.dump().find((n) => n.pkg === PKG && n.cls.includes('EditText') && (re.test(n.desc) || re.test(n.text))), 10_000);
  if (!field) throw new Error(`no field ${re}`);
  await a.tap(field);
  // keys typed while the 4.4 keyboard is still opening are dropped
  await a.sleep(1500);
  typeSafe(text);
  await a.sleep(500);
  // the keyboard covers the lower fields of the dialog (a tap there hits a key)
  await hideKeyboard();
  return field;
}
const hideKeyboard = async () => {
  if (/mInputShown=true/.test(a.shell('dumpsys input_method'))) {
    a.key('KEYCODE_BACK');
    await a.sleep(600);
  }
};
async function ensureApp() {
  // always the APK in release/ (build-release.ps1): a stale install once hid a fix
  a.adb(['install', '-r', join(process.cwd(), 'release/chernobyl-kitkat-0.2.0.apk')], { timeout: 180_000 });
  a.shell(`am force-stop ${PKG}`);
  a.shell(`pm clear ${PKG}`);
  // 4.4 uiautomator only dumps an idle screen: animated agent marks would block every dump (restored in finally)
  a.shell('settings put global animator_duration_scale 0');
  a.shell(`am start -n ${PKG}/com.chernobyl.kitkat.MainActivity`);
  await waitApp(/^Home$/, 60_000);
}
async function pair() {
  const { payload } = c.pair(comp, [`ws://10.0.2.2:${PORT}`]);
  await tapRail('Pair');
  await waitApp(/^Pair with your PC$/, 15_000);
  const field = await a.dump().find((n) => n.pkg === PKG && n.cls.includes('EditText'));
  await a.tap(field);
  typeSafe(payload);
  await a.sleep(800);
  // (4.4 uiautomator does not report the field's text: a bad payload shows as a refused pairing below)
  await hideKeyboard();
  await tapApp(/^Pair and connect$/);
  await tapRail('Home');
}
const sessionsOf = async () => (await snapshot())?.sessions ?? [];
const agentsOf = async () => (await snapshot())?.agents ?? [];

/** Starts a session through the New dialog: agent in the spinner, title, first prompt. */
async function newSession(agentName, title, prompt) {
  await tapRail('Sessions');
  await tapApp(/^New$/);
  await waitApp(/^New session$/, 10_000);
  // the agent spinner: the first Spinner in the dialog
  const spin = a.dump().find((n) => n.pkg === PKG && n.cls.includes('Spinner'));
  await a.tap(spin);
  await a.sleep(800);
  const choice = await waitApp(new RegExp(`^${agentName}( \\(simulated\\))?$`), 10_000);
  if (!choice) throw new Error(`agent ${agentName} not in the spinner: ${a.screenText().slice(0, 300)}`);
  await a.tap(choice);
  await a.sleep(500);
  await typeInto(/^Task title/, title);
  await typeInto(/^First prompt/, prompt);
  await hideKeyboard();
  shot(`new-session-${agentName.replace(/\W/g, '')}`);
  await tapApp(/^Start$/);
  const sid = await until(async () => (await sessionsOf()).find((s) => s.title === title)?.id, 60_000, 1000);
  if (!sid) throw new Error(`no session titled ${title}`);
  const o = await observer();
  o.send({ type: 'session_subscribe', commandId: `obs-${Date.now()}`, sessionId: sid });
  return sid;
}
const view = (sid) => views.get(sid) ?? { items: new Map(), turns: new Map() };
const turnsEnded = (sid) => [...view(sid).turns.values()].filter((t) => t.status !== 'running').length;
const running = (sid) => [...view(sid).turns.values()].some((t) => t.status === 'running');

/** One agent: a shell approval (Allow on the tablet), the reply, then a long prompt stopped mid-stream. */
async function agentRun(S) {
  const notes = { agent: S.name };
  const title = `K ${S.kind} ${Date.now() % 10000}`;
  const sid = await newSession(S.name, title, S.prompt);
  notes.sid = sid;
  const limitNotice = () => [...view(sid).items.values()].find((i) => i.kind === 'notice' && i.level === 'error' && /usage limit|quota|exhausted|rate limit/i.test(i.text));
  const ask = () => until(async () => (limitNotice() ? 'limit' : ((await snapshot())?.permissions?.find((p) => p.sessionId === sid && p.status === 'pending') ?? (turnsEnded(sid) > 0 && !running(sid) ? 'ended' : null))), S.wait ?? 240_000, 1500);
  let first = await ask();
  for (let nudge = 0; first === 'ended' && nudge < 2; nudge++) {
    // a small local model sometimes answers with the command as text: one nudge, recorded
    notes.nudged = nudge + 1;
    await typeInto(/^Message /, 'Call the bash tool now to run that command.');
    await hideKeyboard();
    await tapApp(/^Send$/);
    await until(() => running(sid), 30_000, 1000);
    first = await ask();
  }
  if (first === 'limit') {
    notes.limit = limitNotice().text;
    shot(`${S.kind}-limit`);
    notes.limitOnTablet = !!(await waitApp(/usage limit/i, 20_000));
    return notes;
  }
  if (!first || first === 'ended') {
    shot(`${S.kind}-no-approval`);
    notes.error = `no approval request (${a.screenText().slice(0, 200)})`;
    return notes;
  }
  notes.approval = `${first.title}: ${first.command}`;
  notes.options = (first.options ?? []).map((o) => o.label);
  // the conversation is open on the tablet: Allow is the card's first button
  const allow = await waitApp(/^Allow( once)?$/, 120_000);
  shot(`${S.kind}-approval`);
  notes.cardShown = !!allow;
  // Stop sits at a fixed spot in the composer; 4.4 uiautomator cannot dump while text streams, so keep where it is now
  const stopAt = allow ? a.dump().find((n) => n.pkg === PKG && n.text === 'Stop') : null;
  // the slow 4.4 emulator can drop a tap while the list re-lays out: re-dump and tap again, counted in the evidence
  const pending = async () => (await snapshot())?.permissions?.some((p) => p.id === first.id && p.status === 'pending');
  notes.allowTaps = 0;
  for (let b = allow; b && notes.allowTaps < 3; b = (await pending()) ? await waitApp(/^Allow( once)?$/, 5_000) : null) {
    await a.tap(b);
    notes.allowTaps++;
    await until(async () => !(await pending()), 10_000, 1000);
  }
  // the Stop button is in the composer for as long as a turn runs
  let stop = S.combined ? ((await waitApp(/^Stop$/, 15_000)) ?? stopAt) : null;
  notes.toolDone = !!(await until(() => [...view(sid).items.values()].find((i) => i.kind === 'tool' && i.tool.status === 'completed'), 240_000, 1500));
  const n1 = S.combined ? 0 : 1;
  if (!S.combined) {
    // two turns: this one ends with its footer, a long second prompt is then stopped
    await until(() => turnsEnded(sid) > 0 && !running(sid), 240_000, 1500);
    notes.replyOnTablet = !!app(/Done · /);
    shot(`${S.kind}-done`);
    const text = [...view(sid).items.values()].find((i) => i.kind === 'text');
    notes.reply = (text?.text ?? '').slice(0, 60);
    await typeInto(/^Message /, S.long ?? 'Write a 16-line poem about the sea.');
    await hideKeyboard();
    await tapApp(/^Send$/);
    stop = (await waitApp(/^Stop$/, 30_000)) ?? stopAt;
  }
  const streaming = await until(() => [...view(sid).items.values()].find((i) => i.kind === 'text' && !i.done && i.text.length > 80), 240_000, 400);
  notes.streamedChars = streaming?.text.length ?? 0;
  if (stop) await a.tap(stop);
  await until(() => turnsEnded(sid) > n1, 120_000, 1000);
  notes.stopped = !!(await waitApp(/^Stopped( · |$)/, 30_000));
  notes.turn = [...view(sid).turns.values()].at(-1)?.status;
  shot(`${S.kind}-stopped`);
  if (S.combined) {
    // one turn did it all: the reply streamed (it was on screen) and was cut by Stop
    const text = [...view(sid).items.values()].find((i) => i.kind === 'text');
    notes.reply = (text?.text ?? '').slice(0, 60);
    notes.replyOnTablet = notes.streamedChars > 0;
  }
  return notes;
}

const ROWS = {
  async 'K.1'() {
    await startCompanion();
    await ensureApp();
    await pair();
    // OpenCode lists its own agents (build, explore, general, plan); the Home list scrolls
    const want = ['Claude Code', 'Codex', 'Antigravity', 'build'];
    const seen = new Set();
    const home = await until(() => {
      for (const x of a.dump()) if (x.pkg === PKG) seen.add(x.text);
      if (want.every((n) => seen.has(n))) return true;
      a.shell('input swipe 1000 900 1000 300 300');
      return false;
    }, 90_000, 1500);
    for (let i = 0; i < 4; i++) a.shell('input swipe 1000 300 1000 900 300');
    shot('home-four-agents');
    const online = a.screenText();
    const agents = (await agentsOf()).map((x) => `${x.name} (${x.kind}, ${x.state})`);
    record('K.1', !!home, `paired by paste (${/Online/.test(online) ? 'rail "Online"' : 'rail not online'}); Home lists ${want.map((n) => `${n}=${seen.has(n)}`).join(', ')}; companion agents: ${agents.join('; ')}; screen: ${online.slice(0, 260)}`);
  },
};
const AGENTS = [
  { kind: 'claude', name: 'Claude Code', prompt: 'Use the shell tool to run exactly: echo hi > note.txt', long: 'Write the integers from 1 to 700 separated by spaces, nothing else.' },
  { kind: 'opencode', name: 'build', prompt: 'Use the bash tool to run exactly this command: echo hi > note.txt', wait: 600_000 },
  // one turn each (the account's quota): the approval, then a long reply that Stop cuts
  { kind: 'codex', name: 'Codex', combined: true, prompt: 'Use the shell tool to run exactly this command: echo hi > note.txt. When it has finished, write the integers from 1 to 900 separated by spaces, nothing else.' },
  { kind: 'antigravity', name: 'Antigravity', combined: true, prompt: 'Use the shell tool to run exactly this command: echo hi > note.txt. When it has finished, write the integers from 1 to 900 separated by spaces, nothing else.' },
];
const runs = {};
async function ensureRuns() {
  for (const S of AGENTS) if (!runs[S.kind]) runs[S.kind] = await agentRun(S).catch((e) => ({ agent: S.name, error: e.message.split('\n')[0] }));
}
Object.assign(ROWS, {
  // K.2 is every agent's streamed reply, K.3 its approval, K.4 its Stop: one conversation per agent serves all three
  async 'K.2'() {
    await ensureRuns();
    record('K.2', AGENTS.every((S) => runs[S.kind].replyOnTablet), AGENTS.map((S) => `${S.name}: ${runs[S.kind].replyOnTablet ? `reply ${JSON.stringify(runs[S.kind].reply)}, footer "Done · …" on the tablet` : runs[S.kind].limit ? `refused by the account: "${runs[S.kind].limit.slice(0, 90)}"` : runs[S.kind].error ?? 'no reply'}`).join('; '));
  },
  async 'K.3'() {
    await ensureRuns();
    record('K.3', AGENTS.every((S) => runs[S.kind].toolDone), AGENTS.map((S) => `${S.name}: ${runs[S.kind].cardShown ? `card "${runs[S.kind].approval}" options [${runs[S.kind].options.join(' | ')}] -> Allow -> tool ${runs[S.kind].toolDone ? 'completed' : 'not completed'}` : runs[S.kind].limit ? 'no approval: the account refused the turn' : runs[S.kind].error ?? 'no card'}`).join('; '));
  },
  async 'K.4'() {
    await ensureRuns();
    record('K.4', AGENTS.every((S) => runs[S.kind].stopped), AGENTS.map((S) => `${S.name}: ${runs[S.kind].stopped ? `Stop at ${runs[S.kind].streamedChars} chars -> "Stopped", companion turn ${runs[S.kind].turn}` : runs[S.kind].limit ? 'not run: the account refused the turn' : runs[S.kind].error ?? 'not stopped'}`).join('; '));
  },
});

const pick = process.argv.slice(2);
try {
  oc = await openCode();
  for (const row of pick.length ? pick : Object.keys(ROWS)) {
    try {
      await ROWS[row]();
    } catch (e) {
      shot(`${row}-error`);
      record(row, false, `error: ${e.message.split('\n')[0]}`);
    }
  }
} finally {
  appendFileSync(join(OUT, 'K-runs.json'), `${JSON.stringify({ at: new Date().toISOString(), runs })}\n`);
  obs?.close();
  a.shell('settings put global animator_duration_scale 1');
  if (comp) c.stopCompanion(comp);
  await oc?.stop();
}
