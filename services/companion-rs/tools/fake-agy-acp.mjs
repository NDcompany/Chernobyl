/**
 * Fake `agy acp` (Agent Client Protocol over stdio) for the companion's Antigravity adapter tests
 * (ADR-022). Node >= 20, no dependencies. Driven through the real spawn path; shapes copied from
 * live agy 1.5.6 captures (docs/reverse-engineering/codex-and-antigravity.md B9).
 *
 * Wire: JSON-RPC 2.0, one object per line, with "jsonrpc":"2.0". Our own requests to the client
 * (session/request_permission) use string ids "1", "2", ... as agy does.
 *
 * Methods: initialize, session/new {cwd}, session/resume and session/load {sessionId, cwd}
 * (load replays the saved prompts as updates first), session/list {cwd}, session/set_model
 * {modelId: fake-*}, session/set_mode {modeId}, session/close, session/prompt, and the
 * session/cancel notification. With --state-dir <dir>, sessions are saved as <dir>/<id>.json, so
 * a later process can resume them; otherwise only this process knows them.
 *
 * Prompt scenarios (text contains): "run hostname" asks permission for `hostname` (allow → the
 * command runs and prints FAKE-HOST; deny or cancelled → failed); "slow" starts `sleep 60` and
 * waits for session/cancel; "what did I ask first" answers "You first said: <first prompt>";
 * anything else answers "Hi there". A prompt sent while another runs steers it: the running one
 * ends with end_turn at once.
 *
 * Strictness: stderr lines starting "FAKE-AGY-VIOLATION" are adapter bugs (an answer to an id
 * never issued or answered twice, a prompt before initialize, malformed JSON).
 */
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { randomUUID } from 'node:crypto';

const args = process.argv.slice(2);
const stateDir = args.includes('--state-dir') ? args[args.indexOf('--state-dir') + 1] : null;

const out = (msg) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...msg }) + '\n');
const violation = (why) => process.stderr.write(`FAKE-AGY-VIOLATION ${why}\n`);
const update = (sessionId, u) => out({ method: 'session/update', params: { sessionId, update: u } });
const MODELS = {
  availableModels: [
    { modelId: 'fake-model-high', name: 'Fake (High)' },
    { modelId: 'fake-model-low', name: 'Fake (Low)' },
  ],
  currentModelId: 'fake-model-high',
};
const MODES = { currentModeId: 'default', availableModes: [{ id: 'default', name: 'Default' }] };

const sessions = new Map(); // id -> { cwd, prompts: [], title }
const running = new Map(); // id -> { finish(stopReason), cancel() }
const asks = new Map(); // our request id -> resolve(outcome)
let nextAsk = 1;
let initialized = false;

function save(id) {
  if (!stateDir) return;
  fs.mkdirSync(stateDir, { recursive: true });
  fs.writeFileSync(path.join(stateDir, `${id}.json`), JSON.stringify(sessions.get(id)));
}

function restore(id) {
  if (sessions.has(id)) return sessions.get(id);
  if (!stateDir || !/^[0-9a-f-]+$/.test(id)) return null;
  try {
    const s = JSON.parse(fs.readFileSync(path.join(stateDir, `${id}.json`), 'utf8'));
    sessions.set(id, s);
    return s;
  } catch {
    return null;
  }
}

function listed(cwd) {
  const all = new Map(sessions);
  if (stateDir && fs.existsSync(stateDir)) {
    for (const f of fs.readdirSync(stateDir)) {
      const id = f.replace(/\.json$/, '');
      if (!all.has(id)) all.set(id, restore(id));
    }
  }
  const norm = (p) => p.replace(/\\/g, '/').replace(/^\/(?=[A-Za-z]:)/, '').toLowerCase();
  return [...all]
    .filter(([, s]) => s && norm(s.cwd) === norm(cwd))
    .map(([id, s]) => ({ sessionId: id, cwd: '/' + s.cwd.replace(/\\/g, '/'), title: s.title, updatedAt: new Date().toISOString() }));
}

function ask(sessionId, toolCall) {
  const id = String(nextAsk++);
  const options = [
    { optionId: 'allow', name: 'Allow', kind: 'allow_once' },
    { optionId: 'deny', name: 'Deny', kind: 'reject_once' },
  ];
  out({ method: 'session/request_permission', params: { sessionId, toolCall, options }, id });
  return new Promise((resolve) => asks.set(id, resolve));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function prompt(id, sessionId, blocks) {
  const s = sessions.get(sessionId);
  const text = (blocks.find((b) => b.type === 'text') || {}).text || '';
  s.prompts.push(text);
  save(sessionId);
  // a prompt while another runs steers it
  running.get(sessionId)?.finish('end_turn', false);
  let done = false;
  let cancelled = () => {};
  const finish = (stopReason, usage = true) => {
    if (done) return;
    done = true;
    running.delete(sessionId);
    const result = { stopReason };
    if (usage) result.usage = { inputTokens: 100, outputTokens: 10, cachedReadTokens: 0, thoughtTokens: 0, totalTokens: 110 };
    out({ result, id });
  };
  running.set(sessionId, { finish, cancel: () => cancelled() });
  update(sessionId, { sessionUpdate: 'user_message_chunk', content: { type: 'text', text } });
  const say = (t) => update(sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: t } });
  const call = (toolCallId, command, status) => ({
    sessionUpdate: 'tool_call',
    toolCallId,
    name: 'run_command',
    status,
    kind: 'execute',
    title: command,
    rawInput: { CommandLine: command },
  });
  if (/run hostname/i.test(text)) {
    const tc = call('call_host', 'hostname', 'in_progress');
    update(sessionId, tc);
    update(sessionId, { ...tc, sessionUpdate: 'tool_call_update', status: 'pending' });
    const outcome = await ask(sessionId, { ...tc, status: 'pending' });
    if (done) return;
    if (outcome.outcome === 'selected' && outcome.optionId === 'allow') {
      update(sessionId, { ...tc, sessionUpdate: 'tool_call_update', status: 'in_progress' });
      const output = 'Command exited with code 0.\n\nCommand output:\nFAKE-HOST\n';
      update(sessionId, {
        ...tc,
        sessionUpdate: 'tool_call_update',
        status: 'completed',
        rawOutput: { output, exit_code: 0 },
        content: [{ type: 'content', content: { type: 'text', text: output } }],
      });
      say('The host is FAKE-HOST.');
    } else {
      update(sessionId, { ...tc, sessionUpdate: 'tool_call_update', status: 'failed', rawOutput: { output: '' } });
      say('You denied that.');
    }
    return finish('end_turn');
  }
  if (/slow/i.test(text)) {
    const tc = call('call_slow', 'sleep 60', 'in_progress');
    update(sessionId, tc);
    cancelled = () => {
      update(sessionId, { ...tc, sessionUpdate: 'tool_call_update', status: 'failed' });
      finish('cancelled');
    };
    await sleep(60_000);
    return finish('end_turn');
  }
  if (/what did i ask first/i.test(text)) {
    say(`You first said: ${s.prompts[0]}`);
    return finish('end_turn');
  }
  say('Hi ');
  await sleep(5);
  say('there');
  finish('end_turn');
}

function open(id, msg, replay) {
  const { sessionId, cwd } = msg.params || {};
  const s = restore(sessionId);
  if (!s) return out({ error: { code: -32603, message: `conversation not found: ${sessionId}` }, id });
  if (cwd) s.cwd = cwd;
  if (replay) {
    for (const p of s.prompts) {
      update(sessionId, { sessionUpdate: 'user_message_chunk', content: { type: 'text', text: p } });
      update(sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Replayed.' } });
    }
  }
  out({ result: { sessionId, models: MODELS, modes: MODES }, id });
}

readline.createInterface({ input: process.stdin }).on('line', (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    violation(`malformed JSON: ${line.slice(0, 200)}`);
    return;
  }
  const { id, method } = msg;
  if (!method) {
    const resolve = asks.get(String(id));
    if (!resolve) return violation(`answer to an unknown or answered request ${id}`);
    asks.delete(String(id));
    return resolve((msg.result && msg.result.outcome) || { outcome: 'cancelled' });
  }
  if (method !== 'initialize' && !initialized) violation(`${method} before initialize`);
  const p = msg.params || {};
  switch (method) {
    case 'initialize':
      initialized = true;
      return out({
        result: { protocolVersion: 1, agentCapabilities: { loadSession: true, promptCapabilities: { image: true } }, agentInfo: { name: 'Fake agy', version: '0.0.0' } },
        id,
      });
    case 'session/new': {
      const sessionId = randomUUID();
      sessions.set(sessionId, { cwd: p.cwd, prompts: [], title: 'Fake session' });
      save(sessionId);
      out({ result: { sessionId, models: MODELS, modes: MODES }, id });
      return update(sessionId, { sessionUpdate: 'available_commands_update', availableCommands: [{ name: 'compact', description: 'Compact' }] });
    }
    case 'session/resume':
      return open(id, msg, false);
    case 'session/load':
      return open(id, msg, true);
    case 'session/list':
      return out({ result: { sessions: listed(p.cwd || '') }, id });
    case 'session/set_model':
      if (!/^fake-/.test(p.modelId || '')) return out({ error: { code: -32602, message: 'unknown model' }, id });
      return out({ result: {}, id });
    case 'session/set_mode':
      out({ result: {}, id });
      return update(p.sessionId, { sessionUpdate: 'current_mode_update', currentModeId: p.modeId });
    case 'session/close':
      running.get(p.sessionId)?.finish('cancelled');
      return out({ result: {}, id });
    case 'session/cancel':
      return running.get(p.sessionId)?.cancel();
    case 'session/prompt':
      if (!sessions.has(p.sessionId)) return out({ error: { code: -32602, message: 'unknown session' }, id });
      return void prompt(id, p.sessionId, p.prompt || []);
    default:
      if (id !== undefined) out({ error: { code: -32601, message: `Method not found: ${method}` }, id });
  }
}).on('close', () => process.exit(0));
