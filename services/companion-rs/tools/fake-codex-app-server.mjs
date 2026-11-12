/**
 * Fake `codex app-server` (stdio) for the companion's Codex adapter tests (ADR-015 step 7/10).
 * Node >= 20, no dependencies. Codex is not installed here, so the adapter is driven against
 * this process through the real spawn path.
 *
 * Written independently from the generated protocol schema of openai/codex @ 970111f
 * (codex-rs/app-server-protocol/schema/typescript: ClientRequest.ts, ServerRequest.ts,
 * ServerNotificationEnvelope.ts, v2/*.ts); ordering and error texts cross-checked against
 * codex-rs/app-server/src at the same commit. Method names, field names and enum spellings are
 * the schema's ("on-request", "workspace-write", "acceptForSession", "authenticationRequired").
 *
 * Wire: one JSON object per line, JSON-RPC 2.0 shapes WITHOUT the "jsonrpc" field
 * (app-server-protocol/src/rpc.rs). Notifications carry a top-level "emittedAtMs". Server->client
 * request ids are integers from 0, as in codex, so they overlap client ids: route on "method".
 * Handshake: initialize {clientInfo} -> result, then the `initialized` notification. Requests
 * before initialize get -32600 "Not initialized".
 *
 * Scenarios: turn/start input text is searched (case-insensitive, whole words) for
 *   hello think run edit plan ask perm elicit unknown-request withdraw error slow
 * Each keyword found runs, in prompt order, inside one turn; none found means `hello`.
 * `amend` makes `run` propose an execpolicy amendment. A thread whose approvalPolicy is "never"
 * runs `run`/`edit` without asking. Input sent by turn/steer (or by turn/start while a turn is
 * active, which codex treats as a steer) is answered with "Following up on: <text>" at the end of
 * the same turn.
 *
 * Resume: thread/resume {threadId, cwd?, approvalPolicy?, sandbox?} returns a loaded thread as is,
 * or one a previous process started when --state-dir <dir> is given (thread/start and every turn end
 * save <dir>/<threadId>.json, as codex keeps rollouts on disk); otherwise -32600 "no rollout found for
 * thread id <id>". The resumed thread comes back with `turns: []` (history is not replayed).
 *
 * Verified live on codex-cli 0.167.0-alpha.4.1 (2026-11-12) and mirrored here: model/list (three
 * visible models), thread/list {cwd: string | string[], limit, sortKey} (newest first, also threads
 * no process has loaded), thread/items/list {threadId, limit, sortDirection} ({turnId, item} entries;
 * works on unloaded threads), thread/name/set (then thread/name/updated {threadId, threadName}),
 * thread/fork {threadId, excludeTurns} (a new thread with the old one's items, forkedFromId set),
 * turn/start {model} (kept by the thread) and image inputs ({type: "image", url: "data:..."}).
 * With approvalPolicy "untrusted" a command approval carries availableDecisions ["accept",
 * {acceptWithExecpolicyAmendment} when proposed, "cancel"] and any other decision is a violation; on
 * Windows the command is wrapped as codex does ("...powershell.exe" -Command '<cmd>') while
 * commandActions[].command keeps the plain command. Agent messages carry phase "final_answer".
 *
 * MCP: fake-docs (stdio, connected) and fake-linear (HTTP, authenticationRequired). One
 * mcpServer/startupStatus/updated per server follows the initialize response. A
 * mcpServer/oauth/login for fake-linear completes after --oauth-delay-ms (default 500, negative =
 * never): mcpServer/oauthLogin/completed is sent and the server becomes connected.
 *
 * Strictness: stderr lines starting "FAKE-CODEX-VIOLATION" are adapter bugs: a server request
 * answered twice, after its serverRequest/resolved, or never (unknown-request waits 5 s; anything
 * still open when a turn completes normally), an answer to an id never issued, a malformed
 * answer or an execpolicy amendment other than the proposed one, a non-empty permissions grant,
 * a request before `initialized`, malformed JSON (also answered with -32700; real codex only logs
 * it). At stdin EOF the exit code is 3 if any violation happened, else 0.
 *
 * Simplifications: experimental fields are never sent; an interrupted turn leaves in-flight items
 * without item/completed; methods other than the ones above get -32600, as codex answers unknown
 * methods.
 *
 * Usage: node fake-codex-app-server.mjs [--transcript <file>] [--oauth-delay-ms <ms>] [--state-dir <dir>]
 *        node fake-codex-app-server.mjs --self-test
 * --transcript appends "<ISO time> <- line" (in), "-> line" (out) and "!! text" (violation).
 */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { appendFileSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

const TICK_MS = 10;
const UNKNOWN_METHOD = 'fake/unknownServerRequest';
const FAKE_SECRET = 'sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKE';
const word = (w) => new RegExp(`(?<![\\w-])${w}(?![\\w-])`, 'i');
const KEYWORDS = ['hello', 'think', 'run', 'edit', 'plan', 'ask', 'perm', 'elicit', 'unknown-request', 'withdraw', 'error', 'slow'].map((k) => [k, word(k)]);
const AMEND = word('amend');
const APPROVAL_POLICIES = ['untrusted', 'on-request', 'never'];
const SANDBOX_MODES = ['read-only', 'workspace-write', 'danger-full-access'];
const SANDBOX_POLICY_TYPES = ['dangerFullAccess', 'readOnly', 'externalSandbox', 'workspaceWrite'];
const USER_INPUT_TYPES = ['text', 'image', 'localImage', 'audio', 'localAudio', 'skill', 'mention'];
const MODELS = [
  ['gpt-6-luna', 'GPT-6-Luna', true],
  ['gpt-5.6-terra', 'GPT-5.6-Terra', false],
  ['gpt-5.6-luna', 'GPT-5.6-Luna', false],
];
const POWERSHELL = 'C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe';
const shellCommand = (cmd) => (process.platform === 'win32' ? `"${POWERSHELL}" -Command '${cmd}'` : cmd);
const FILE_DECISIONS = new Map([['accept', 'apply'], ['acceptForSession', 'apply'], ['decline', 'declined'], ['cancel', 'cancel']]);
const STATUS_FLAGS = {
  'item/commandExecution/requestApproval': 'waitingOnApproval',
  'item/fileChange/requestApproval': 'waitingOnApproval',
  'item/permissions/requestApproval': 'waitingOnApproval',
  'mcpServer/elicitation/request': 'waitingOnApproval',
  'item/tool/requestUserInput': 'waitingOnUserInput',
};
const MCP_TOOLS = {
  'fake-docs': { search: { name: 'search', description: 'Search the fake docs', inputSchema: { type: 'object', properties: { query: { type: 'string' } } } } },
  'fake-linear': { create_issue: { name: 'create_issue', description: 'Create a fake issue', inputSchema: { type: 'object', properties: { title: { type: 'string' } } } } },
};
const TURN_DIFF = `diff --git a/src/greeting.txt b/src/greeting.txt
index 3b18e51..a1f6c2d 100644
--- a/src/greeting.txt
+++ b/src/greeting.txt
@@ -1,3 +1,3 @@
 line one
-hello world
+hello codex
 line three
diff --git a/src/notes.md b/src/notes.md
new file mode 100644
index 0000000..5c4b2f1
--- /dev/null
+++ b/src/notes.md
@@ -0,0 +1,3 @@
+# Notes
+
+Created by the fake app-server.
`;

class RpcError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}
const invalid = (message) => new RpcError(-32600, message);
class TurnFailed extends Error {
  constructor(turnError) {
    super(turnError.message);
    this.turnError = turnError;
  }
}
const ABORT = Symbol('turn aborted');

const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const isStr = (v) => typeof v === 'string';
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

// serde-style "missing field" / "invalid type" / "unknown variant" errors, as codex returns them.
function field(obj, name, ok, expected) {
  if (obj[name] === undefined) throw invalid(`Invalid request: missing field \`${name}\``);
  if (!ok(obj[name])) throw invalid(`Invalid request: invalid type for \`${name}\`, expected ${expected}`);
  return obj[name];
}
function variant(value, accepted, listed = accepted) {
  if (isStr(value) && accepted.includes(value)) return value;
  const shown = isStr(value) ? value : JSON.stringify(value);
  throw invalid(`Invalid request: unknown variant \`${shown}\`, expected one of ${listed.map((v) => `\`${v}\``).join(', ')}`);
}
function approvalPolicy(value, experimentalApi) {
  if (isObj(value) && isObj(value.granular)) {
    if (!experimentalApi) throw invalid('askForApproval.granular requires experimentalApi capability');
    return value;
  }
  return variant(value, APPROVAL_POLICIES, ['untrusted', 'on-request', 'granular', 'never']);
}
function sandboxPolicy(mode) {
  if (mode === 'danger-full-access') return { type: 'dangerFullAccess' };
  if (mode === 'workspace-write') return { type: 'workspaceWrite', writableRoots: [], networkAccess: false, excludeTmpdirEnvVar: false, excludeSlashTmp: false };
  return { type: 'readOnly', networkAccess: false };
}
function userInput(list) {
  return list.map((item) => {
    if (!isObj(item)) throw invalid('Invalid request: invalid type for `input` item, expected struct UserInput');
    variant(field(item, 'type', isStr, 'a string'), USER_INPUT_TYPES);
    if (item.type === 'image' && !(isStr(item.url) || isStr(item.fileId))) throw invalid('Invalid request: image input needs `url` or `fileId`');
    if (item.type !== 'text') return item;
    field(item, 'text', isStr, 'a string');
    // text_elements is required in the TS type but #[serde(default)] in Rust.
    return { type: 'text', text: item.text, text_elements: Array.isArray(item.text_elements) ? item.text_elements : [] };
  });
}
const textOf = (input) => input.filter((i) => i.type === 'text').map((i) => i.text).join('\n');
function scenariosFor(text) {
  const found = KEYWORDS.map(([k, re]) => [re.exec(text)?.index, k]).filter(([at]) => at !== undefined);
  return found.length ? found.sort((a, b) => a[0] - b[0]).map(([, k]) => k) : ['hello'];
}
function uuidv7() {
  const b = randomBytes(16);
  b.writeUIntBE(Date.now(), 0, 6);
  b[6] = (b[6] & 0x0f) | 0x70;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = b.toString('hex');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

function createServer({ write, log, transcript = null, oauthDelayMs = 500, answerTimeoutMs = 5000, stateDir = null }) {
  const threads = new Map();
  // ponytail: requests are never pruned and status scans are O(n); fine for test-length sessions.
  const requests = new Map(); // JSON.stringify(id) -> server->client request (keys keep 0 and "0" apart)
  const timers = new Set();
  const logins = new Map(); // MCP server name -> pending login timer
  const violations = [];
  const mcp = {
    'fake-docs': { http: false, runtimeStatus: 'connected', authStatus: 'unsupported' },
    'fake-linear': { http: true, runtimeStatus: 'authenticationRequired', authStatus: 'notLoggedIn' },
  };
  let initialized = false; // initialize answered
  let notified = false; // `initialized` notification received
  let experimentalApi = false;
  let optOut = new Set();
  let clientName = null;
  let closed = false;
  let nextRequestId = 0;
  let nextItem = 0;

  const nowMs = () => Date.now();
  const nowSecs = () => Math.floor(Date.now() / 1000);
  const newItemId = (prefix) => `${prefix}_${++nextItem}`;

  function out(message) {
    if (closed) return;
    const line = JSON.stringify(message);
    transcript?.('->', line);
    write(line);
  }
  const respond = (id, result) => out({ id, result });
  function notify(method, params) {
    if (!optOut.has(method)) out({ method, params, emittedAtMs: nowMs() });
  }
  function violation(text) {
    if (closed) return;
    violations.push(text);
    transcript?.('!!', text);
    log(`FAKE-CODEX-VIOLATION ${text}`);
  }
  function sleep(ms) {
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        timers.delete(timer);
        resolve();
      }, ms);
      timers.add(timer);
    });
  }

  class Turn {
    constructor(thread, prompt, rootTurnId) {
      this.thread = thread;
      this.id = uuidv7();
      this.rootTurnId = isStr(rootTurnId) ? rootTurnId : this.id;
      this.prompt = prompt;
      this.ids = { threadId: thread.id, turnId: this.id };
      this.startedAt = nowSecs();
      this.startedMs = nowMs();
      this.steers = [];
      this.interrupts = []; // turn/interrupt request ids, answered when the turn ends
      this.lastAgentMessage = null;
      this.reasoned = false;
      this.aborted = false;
      this.onAbort = new Set();
    }
    wait(promise) {
      if (this.aborted) return Promise.reject(ABORT);
      return new Promise((resolve, reject) => {
        const abort = () => reject(ABORT);
        this.onAbort.add(abort);
        promise.then((value) => {
          this.onAbort.delete(abort);
          resolve(value);
        });
      });
    }
    sleep(ms) {
      return this.wait(sleep(ms));
    }
    abort() {
      this.aborted = true;
      for (const abort of this.onAbort) abort();
      this.onAbort.clear();
    }
    shape(status, extra = {}) {
      return { id: this.id, rootTurnId: this.rootTurnId, items: [], itemsView: 'notLoaded', status, error: null, startedAt: null, completedAt: null, durationMs: null, ...extra };
    }
  }

  function refreshStatus(thread) {
    let status = { type: 'idle' };
    if (thread.turn) {
      const flags = new Set();
      for (const r of requests.values()) if (r.state === 'pending' && r.threadId === thread.id && r.flag) flags.add(r.flag);
      status = { type: 'active', activeFlags: ['waitingOnApproval', 'waitingOnUserInput'].filter((f) => flags.has(f)) };
    }
    const key = JSON.stringify(status);
    if (key === thread.statusKey) return;
    thread.statusKey = key;
    notify('thread/status/changed', { threadId: thread.id, status });
  }

  // ---- server -> client requests ------------------------------------------------------------
  function sendRequest(t, method, params) {
    const req = { id: nextRequestId++, method, threadId: t.thread.id, turn: t, state: 'pending', flag: STATUS_FLAGS[method] };
    req.answer = new Promise((resolve) => {
      req.resolve = resolve;
    });
    requests.set(JSON.stringify(req.id), req);
    refreshStatus(t.thread);
    out({ id: req.id, method, params });
    return req;
  }
  function settle(req, state) {
    req.state = state;
    notify('serverRequest/resolved', { threadId: req.threadId, requestId: req.id });
    refreshStatus(threads.get(req.threadId));
  }
  async function request(t, method, params, timeoutMs = 0) {
    const req = sendRequest(t, method, params);
    const answer = await t.wait(timeoutMs ? Promise.race([req.answer, sleep(timeoutMs).then(() => null)]) : req.answer);
    if (answer === null) {
      violation(`no answer to server request ${req.id} (${method}) within ${timeoutMs} ms`);
      settle(req, 'withdrawn');
    }
    return answer;
  }
  function onResponse(msg) {
    const req = requests.get(JSON.stringify(msg.id));
    if (!req) return violation(`answer to unknown server request id ${JSON.stringify(msg.id)}`);
    const label = `${req.id} (${req.method})`;
    if (req.state === 'answered') return violation(`duplicate answer to server request ${label}`);
    if (req.state === 'withdrawn') return violation(`answer to withdrawn server request ${label}; serverRequest/resolved was already sent`);
    if ('result' in msg === 'error' in msg) violation(`malformed answer to server request ${label}: needs exactly one of result and error`);
    settle(req, 'answered');
    req.resolve(msg);
  }

  // ---- turn building blocks -----------------------------------------------------------------
  const itemStarted = (t, item) => notify('item/started', { item, ...t.ids, startedAtMs: nowMs() });
  function itemCompleted(t, item) {
    const at = nowMs();
    if (item.type !== 'reasoning') t.thread.items.push({ turnId: t.id, item, startedAtMs: at, completedAtMs: at });
    notify('item/completed', { item, ...t.ids, completedAtMs: at });
  }
  function userMessage(t, input, clientId) {
    if (!input.length) return;
    const item = { type: 'userMessage', id: newItemId('user'), clientId: isStr(clientId) ? clientId : null, content: input };
    itemStarted(t, item);
    itemCompleted(t, item);
  }
  async function say(t, text, delayMs = TICK_MS) {
    const chunks = Array.isArray(text) ? text : text.match(/\S+\s*/g);
    const item = { type: 'agentMessage', id: newItemId('msg'), text: '', phase: 'final_answer', memoryCitation: null, delivery: null, questions: null };
    itemStarted(t, item);
    for (const delta of chunks) {
      await t.sleep(delayMs);
      notify('item/agentMessage/delta', { ...t.ids, itemId: item.id, delta });
    }
    t.lastAgentMessage = { ...item, text: chunks.join('') };
    itemCompleted(t, t.lastAgentMessage);
  }
  function steer(t, input, clientId) {
    if (!input.length) return;
    t.steers.push(textOf(input));
    userMessage(t, input, clientId);
  }

  function commandOutcome(answer, params) {
    if (answer.error) return 'failed'; // codex: "approval request failed", item failed, turn goes on
    const d = answer.result?.decision;
    const name = isObj(d) ? Object.keys(d)[0] : d;
    if (params.availableDecisions && !params.availableDecisions.some((a) => (isObj(a) ? Object.keys(a)[0] : a) === name)) {
      violation(`decision ${JSON.stringify(d)} for request ${answer.id} is not one of availableDecisions ${JSON.stringify(params.availableDecisions)}`);
    }
    if (d === 'accept' || d === 'acceptForSession') return 'run';
    if (d === 'decline') return 'declined';
    if (d === 'cancel') return 'cancel';
    if (isObj(d) && isObj(d.acceptWithExecpolicyAmendment)) {
      const amendment = d.acceptWithExecpolicyAmendment.execpolicy_amendment;
      if (!params.proposedExecpolicyAmendment) violation(`acceptWithExecpolicyAmendment answered request ${answer.id}, which proposed no amendment`);
      else if (!same(amendment, params.proposedExecpolicyAmendment)) violation(`execpolicy_amendment ${JSON.stringify(amendment)} for request ${answer.id} differs from the proposed ${JSON.stringify(params.proposedExecpolicyAmendment)}`);
      return 'run';
    }
    if (isObj(d) && isObj(d.applyNetworkPolicyAmendment)) return d.applyNetworkPolicyAmendment.network_policy_amendment?.action === 'deny' ? 'declined' : 'run';
    violation(`malformed answer to item/commandExecution/requestApproval ${answer.id}: ${JSON.stringify(answer.result)}`);
    return 'failed';
  }

  async function command(t, cmd, { amend = false, withdraw = false } = {}) {
    const { cwd } = t.thread;
    const item = { type: 'commandExecution', id: newItemId('call'), pluginId: null, scriptPath: null, command: shellCommand(cmd), cwd, processId: null, source: 'agent', status: 'inProgress', commandActions: [{ type: 'unknown', command: cmd }], aggregatedOutput: null, exitCode: null, durationMs: null };
    itemStarted(t, item); // codex starts the item, then asks
    let outcome = 'run';
    if (withdraw || t.thread.approvalPolicy !== 'never') {
      const params = { kind: 'command', ...t.ids, itemId: item.id, startedAtMs: nowMs(), environmentId: null, reason: `Run \`${cmd}\` in the workspace`, command: item.command, cwd, commandActions: item.commandActions };
      if (amend) params.proposedExecpolicyAmendment = cmd.split(' ');
      if (t.thread.approvalPolicy === 'untrusted') {
        params.availableDecisions = ['accept', ...(amend ? [{ acceptWithExecpolicyAmendment: { execpolicy_amendment: params.proposedExecpolicyAmendment } }] : []), 'cancel'];
      }
      // withdraw: another client answers (accept) before this one can.
      if (withdraw) settle(sendRequest(t, 'item/commandExecution/requestApproval', params), 'withdrawn');
      else outcome = commandOutcome(await request(t, 'item/commandExecution/requestApproval', params), params);
    }
    if (outcome !== 'run') {
      itemCompleted(t, { ...item, status: outcome === 'failed' ? 'failed' : 'declined' });
      if (outcome === 'cancel') throw ABORT; // codex aborts the turn: turn/completed "interrupted"
      return say(t, outcome === 'declined' ? `Okay, I did not run \`${cmd}\`.` : `The approval request failed, so \`${cmd}\` did not run.`);
    }
    // One chunk carries the whole fake secret; lines also straddle chunks (line-buffering).
    const chunks = [`> ${cmd}\n`, 'PASS test/greeting.test.js\nPASS te', 'st/notes.test.js\n', `loaded env: token ${FAKE_SECRET}`, ' (from .env)\n', 'Tests: 2 passed, 2 total\n'];
    for (const delta of chunks) {
      await t.sleep(TICK_MS);
      notify('item/commandExecution/outputDelta', { ...t.ids, itemId: item.id, delta });
    }
    itemCompleted(t, { ...item, status: 'completed', aggregatedOutput: chunks.join(''), exitCode: 0, durationMs: 1234 });
    return say(t, withdraw ? `Another client approved \`${cmd}\`; it ran here.` : `\`${cmd}\` finished: 2 tests passed.`);
  }

  const SCENARIOS = {
    hello: (t) => say(t, 'Hello from the fake Codex app-server. This reply streams in small deltas.'),

    async think(t) {
      const item = { type: 'reasoning', id: newItemId('rs'), summary: [], content: [] };
      const parts = [['**Reading the request**', '\n\nThe user wants a short answer.'], ['**Planning**', '\n\nReply in one sentence.']];
      const raw = 'Raw reasoning text; clients should prefer the summary.';
      itemStarted(t, item);
      for (const [index, part] of parts.entries()) {
        if (index) notify('item/reasoning/summaryPartAdded', { ...t.ids, itemId: item.id, summaryIndex: index });
        for (const delta of part) {
          await t.sleep(TICK_MS);
          notify('item/reasoning/summaryTextDelta', { ...t.ids, itemId: item.id, delta, summaryIndex: index });
        }
      }
      notify('item/reasoning/textDelta', { ...t.ids, itemId: item.id, delta: raw, contentIndex: 0 });
      itemCompleted(t, { ...item, summary: parts.map((p) => p.join('')), content: [raw] });
      t.reasoned = true;
      await say(t, 'After thinking it over: the answer is 42.');
    },

    run: (t) => command(t, 'npm test', { amend: AMEND.test(t.prompt) }),
    withdraw: (t) => command(t, 'npm run lint', { withdraw: true }),

    async edit(t) {
      const { cwd } = t.thread;
      const item = {
        type: 'fileChange',
        id: newItemId('call'),
        changes: [
          { path: join(cwd, 'src', 'greeting.txt'), kind: { type: 'update', move_path: null }, diff: '@@ -1,3 +1,3 @@\n line one\n-hello world\n+hello codex\n line three\n' },
          { path: join(cwd, 'src', 'notes.md'), kind: { type: 'add' }, diff: '# Notes\n\nCreated by the fake app-server.\n' }, // add: whole content
        ],
        status: 'inProgress',
      };
      itemStarted(t, item);
      let outcome = 'apply';
      if (t.thread.approvalPolicy !== 'never') {
        const answer = await request(t, 'item/fileChange/requestApproval', { ...t.ids, itemId: item.id, startedAtMs: nowMs(), reason: 'Edit src/greeting.txt and create src/notes.md' });
        outcome = answer.error ? 'declined' : FILE_DECISIONS.get(answer.result?.decision);
        if (!outcome) {
          violation(`malformed answer to item/fileChange/requestApproval ${answer.id}: ${JSON.stringify(answer.result)}`);
          outcome = 'declined';
        }
      }
      if (outcome !== 'apply') {
        itemCompleted(t, { ...item, status: 'declined' });
        if (outcome === 'cancel') throw ABORT;
        return say(t, 'Okay, I left the files unchanged.');
      }
      itemCompleted(t, { ...item, status: 'completed' });
      notify('turn/diff/updated', { ...t.ids, diff: TURN_DIFF });
      return say(t, 'Updated src/greeting.txt and created src/notes.md.');
    },

    async plan(t) {
      const steps = ['Read the failing test', 'Fix the bug', 'Run the test suite'];
      const update = (statuses) => notify('turn/plan/updated', { ...t.ids, explanation: 'Fixing the failing test', plan: steps.map((step, i) => ({ step, status: statuses[i] })) });
      update(['inProgress', 'pending', 'pending']);
      await t.sleep(TICK_MS);
      update(['completed', 'inProgress', 'pending']);
      await say(t, 'Plan: step 1 done, step 2 in progress, step 3 pending.');
    },

    async ask(t) {
      const questions = [
        { id: 'confirm_path', header: 'Confirm', question: 'Proceed with the plan?', isOther: false, isSecret: false, options: [{ label: 'Yes (Recommended)', description: 'Continue the current plan.' }, { label: 'No', description: 'Stop and revisit the approach.' }] },
        { id: 'favorite_color', header: 'Color', question: 'Which color should the button be?', isOther: true, isSecret: false, options: null },
      ];
      const answer = await request(t, 'item/tool/requestUserInput', { ...t.ids, itemId: newItemId('call'), questions, isBlocking: true, autoResolutionMs: null });
      let answers = {}; // codex submits empty answers when the client errors
      if (answer.result) {
        const a = answer.result.answers;
        if (isObj(a) && Object.values(a).every((v) => isObj(v) && Array.isArray(v.answers) && v.answers.every(isStr))) answers = a;
        else violation(`malformed answer to item/tool/requestUserInput ${answer.id}: ${JSON.stringify(answer.result)}`);
      }
      await say(t, `You answered: ${questions.map((q) => `${q.id}=${answers[q.id]?.answers.join('|') || '(none)'}`).join('; ')}`);
    },

    async perm(t) {
      const { cwd } = t.thread;
      const answer = await request(t, 'item/permissions/requestApproval', { ...t.ids, itemId: newItemId('call'), environmentId: null, startedAtMs: nowMs(), cwd, reason: 'Needs network and write access to install dependencies', permissions: { network: { enabled: true }, fileSystem: { read: null, write: [cwd] } } });
      let granted = [];
      if (answer.result) {
        const { permissions, scope } = answer.result;
        if (!isObj(permissions) || !['turn', 'session'].includes(scope)) violation(`malformed answer to item/permissions/requestApproval ${answer.id}: ${JSON.stringify(answer.result)}`);
        else granted = Object.keys(permissions).filter((k) => permissions[k] != null);
        if (granted.length) violation(`item/permissions/requestApproval ${answer.id} granted ${JSON.stringify(permissions)}; expected an empty grant`);
      }
      await say(t, `Permissions granted: ${granted.join(', ') || 'nothing'}. Continuing inside the sandbox.`);
    },

    async elicit(t) {
      const item = { type: 'mcpToolCall', id: newItemId('call'), server: 'fake-docs', tool: 'search', status: 'inProgress', arguments: { query: 'release notes' }, appContext: null, mcpAppUi: null, pluginId: null, readOnlyHint: true, result: null, error: null, durationMs: null };
      itemStarted(t, item);
      const answer = await request(t, 'mcpServer/elicitation/request', { ...t.ids, serverName: 'fake-docs', mode: 'form', _meta: null, message: 'fake-docs needs a workspace name to search.', requestedSchema: { type: 'object', properties: { workspace: { type: 'string', title: 'Workspace' } }, required: ['workspace'] } });
      let action = 'decline'; // codex turns a client error into decline
      if (answer.result) {
        if (['accept', 'decline', 'cancel'].includes(answer.result.action)) action = answer.result.action;
        else violation(`malformed answer to mcpServer/elicitation/request ${answer.id}: ${JSON.stringify(answer.result)}`);
      }
      const verb = { accept: 'accepted', decline: 'declined', cancel: 'cancelled' }[action];
      itemCompleted(t, action === 'accept'
        ? { ...item, status: 'completed', result: { content: [{ type: 'text', text: '3 results' }], structuredContent: null, _meta: null }, durationMs: 87 }
        : { ...item, status: 'failed', error: { message: `elicitation ${verb} by the client` }, durationMs: 87 });
      await say(t, `The MCP elicitation was ${verb}.`);
    },

    async 'unknown-request'(t) {
      const answer = await request(t, UNKNOWN_METHOD, { ...t.ids, reason: 'a method no client knows; answer with a JSON-RPC error' }, answerTimeoutMs);
      if (answer && 'result' in answer) violation(`${UNKNOWN_METHOD} ${answer.id} was answered with a result; expected a JSON-RPC error`);
      await say(t, !answer ? 'The client never answered the unknown request.' : answer.error ? `The client answered the unknown request with error ${answer.error.code}: ${answer.error.message}` : 'The client answered the unknown request with a result instead of an error.');
    },

    async error(t) {
      notify('error', { error: { message: 'Reconnecting... 1/5', codexErrorInfo: { responseStreamDisconnected: { httpStatusCode: 502 } }, additionalDetails: 'stream disconnected before completion', misalignment: null }, willRetry: true, ...t.ids });
      await t.sleep(TICK_MS * 2);
      const error = { message: 'The model is overloaded. Try again later.', codexErrorInfo: 'serverOverloaded', additionalDetails: null, misalignment: null };
      notify('error', { error, willRetry: false, ...t.ids });
      throw new TurnFailed(error);
    },

    slow: (t) => say(t, Array.from({ length: 25 }, (_, i) => `chunk ${i + 1}/25 `), 200),
  };

  async function runTurn(t, keywords) {
    let status = 'completed';
    let error = null;
    try {
      for (const k of keywords) await SCENARIOS[k](t);
      for (let i = 0; i < t.steers.length; i++) await say(t, `Following up on: ${t.steers[i]}`);
      const last = { totalTokens: 0, inputTokens: 1200 + t.prompt.length, cachedInputTokens: 1024, cacheWriteInputTokens: 0, outputTokens: 48, reasoningOutputTokens: t.reasoned ? 64 : 0 };
      last.totalTokens = last.inputTokens + last.outputTokens;
      for (const k of Object.keys(last)) t.thread.usage[k] += last[k];
      notify('thread/tokenUsage/updated', { ...t.ids, tokenUsage: { total: { ...t.thread.usage }, last, modelContextWindow: 272000 } });
    } catch (e) {
      if (e === ABORT) status = 'interrupted';
      else if (e instanceof TurnFailed) [status, error] = ['failed', e.turnError];
      else {
        log(`FAKE-CODEX-ERROR ${e?.stack ?? e}`);
        [status, error] = ['failed', { message: `fake app-server bug: ${e?.message ?? e}`, codexErrorInfo: 'other', additionalDetails: null, misalignment: null }];
      }
    }
    endTurn(t, status, error);
  }

  function endTurn(t, status, error) {
    const { thread } = t;
    if (thread.turn === t) thread.turn = null;
    thread.updatedAt = nowSecs();
    save(thread);
    for (const req of requests.values()) {
      if (req.turn !== t || req.state !== 'pending') continue;
      // An interrupt resolves open requests server-side; on a normal end nothing may be open.
      if (status !== 'interrupted') violation(`missing answer to server request ${req.id} (${req.method}) at turn end`);
      settle(req, 'withdrawn');
    }
    for (const id of t.interrupts) respond(id, {});
    refreshStatus(thread); // codex reports idle before turn/completed
    const summary = status === 'completed' && t.lastAgentMessage;
    notify('turn/completed', {
      threadId: thread.id,
      turn: t.shape(status, { items: summary ? [t.lastAgentMessage] : [], itemsView: summary ? 'summary' : 'notLoaded', error, startedAt: t.startedAt, completedAt: nowSecs(), durationMs: nowMs() - t.startedMs }),
    });
  }

  // ---- client -> server requests ------------------------------------------------------------
  function threadOf(p) {
    const id = field(p, 'threadId', isStr, 'a string');
    const thread = threads.get(id);
    if (!thread) throw invalid(`thread not found: ${id}`);
    return thread;
  }

  function initialize(id, p) {
    if (initialized) throw invalid('Already initialized');
    const info = field(p, 'clientInfo', isObj, 'struct ClientInfo');
    field(info, 'name', isStr, 'a string');
    field(info, 'version', isStr, 'a string'); // title is optional on the wire (serde Option)
    const caps = isObj(p.capabilities) ? p.capabilities : {};
    experimentalApi = caps.experimentalApi === true;
    optOut = new Set(Array.isArray(caps.optOutNotificationMethods) ? caps.optOutNotificationMethods : []);
    clientName = info.name;
    initialized = true;
    const win = process.platform === 'win32';
    respond(id, { userAgent: `codex_app_server_fake/0.0.0 (${info.name}; ${info.version})`, codexHome: join(tmpdir(), 'fake-codex-home'), platformFamily: win ? 'windows' : 'unix', platformOs: win ? 'windows' : process.platform === 'darwin' ? 'macos' : 'linux' });
    for (const name of Object.keys(mcp)) notify('mcpServer/startupStatus/updated', startupStatus(name));
  }

  function threadStart(id, p) {
    const approval = p.approvalPolicy == null ? 'on-request' : approvalPolicy(p.approvalPolicy, experimentalApi);
    const sandbox = sandboxPolicy(p.sandbox == null ? 'read-only' : variant(p.sandbox, SANDBOX_MODES));
    if (p.cwd != null && !isStr(p.cwd)) throw invalid('Invalid request: invalid type for `cwd`, expected a string');
    const cwd = p.cwd ?? process.cwd();
    const model = isStr(p.model) ? p.model : MODELS[0][0];
    const t = newThread(uuidv7(), cwd, model, approval, sandbox);
    t.ephemeral = p.ephemeral === true;
    save(t);
    const thread = threadShape(t, p.ephemeral === true, isStr(p.threadSource) ? p.threadSource : null);
    respond(id, threadResult(t, thread));
    notify('thread/started', { thread });
  }

  function threadResume(id, p) {
    const tid = field(p, 'threadId', isStr, 'a string');
    const approval = p.approvalPolicy == null ? null : approvalPolicy(p.approvalPolicy, experimentalApi);
    const sandbox = p.sandbox == null ? null : sandboxPolicy(variant(p.sandbox, SANDBOX_MODES));
    if (p.cwd != null && !isStr(p.cwd)) throw invalid('Invalid request: invalid type for `cwd`, expected a string');
    let t = threads.get(tid);
    if (!t) {
      t = loadSaved(tid);
      if (!t) throw invalid(`no rollout found for thread id ${tid}`);
    }
    if (isStr(p.model)) t.model = p.model;
    if (approval) t.approvalPolicy = approval;
    if (sandbox) t.sandbox = sandbox;
    if (isStr(p.cwd)) t.cwd = p.cwd;
    respond(id, threadResult(t, threadShape(t, false, null)));
  }

  function newThread(tid, cwd, model, approval, sandbox) {
    const t = {
      id: tid, cwd, model, approvalPolicy: approval, sandbox, turn: null, statusKey: '{"type":"idle"}', createdAt: nowSecs(),
      usage: { totalTokens: 0, inputTokens: 0, cachedInputTokens: 0, cacheWriteInputTokens: 0, outputTokens: 0, reasoningOutputTokens: 0 },
      items: [], name: null, forkedFromId: null, ephemeral: false,
    };
    t.updatedAt = t.createdAt;
    threads.set(tid, t);
    return t;
  }
  // codex keeps rollouts on disk: --state-dir stands in for ~/.codex/sessions
  function save(t) {
    if (!stateDir || t.ephemeral) return;
    mkdirSync(stateDir, { recursive: true });
    const { id, cwd, model, name, items, createdAt, updatedAt, forkedFromId } = t;
    writeFileSync(join(stateDir, `${id}.json`), JSON.stringify({ id, cwd, model, name, items, createdAt, updatedAt, forkedFromId }));
  }
  function loadSaved(tid) {
    let saved = null;
    try {
      if (stateDir && /^[\w-]+$/.test(tid)) saved = JSON.parse(readFileSync(join(stateDir, `${tid}.json`), 'utf8'));
    } catch {}
    if (!saved) return null;
    const t = newThread(tid, saved.cwd, saved.model, 'on-request', sandboxPolicy('read-only'));
    Object.assign(t, { items: saved.items ?? [], name: saved.name ?? null, createdAt: saved.createdAt ?? t.createdAt, updatedAt: saved.updatedAt ?? t.updatedAt, forkedFromId: saved.forkedFromId ?? null });
    return t;
  }
  // reading a thread does not load it
  function known(tid) {
    if (threads.has(tid)) return threads.get(tid);
    const t = loadSaved(tid);
    if (t) threads.delete(tid);
    return t;
  }
  function preview(t) {
    const first = t.items.find((e) => e.item.type === 'userMessage');
    return first ? textOf(first.item.content) : '';
  }
  function threadShape(t, ephemeral, threadSource) {
    return {
      id: t.id, sessionId: t.id, forkedFromId: t.forkedFromId, parentThreadId: null, preview: preview(t), ephemeral, section: null, sectionEnteredAt: null,
      projectId: null, historyMode: 'paginated', modelProvider: 'openai', model: t.model, reasoningEffort: null, createdAt: t.createdAt, updatedAt: t.updatedAt, recencyAt: t.updatedAt,
      status: t.turn ? { type: 'active', activeFlags: [] } : threads.has(t.id) ? { type: 'idle' } : { type: 'notLoaded' }, path: null, cwd: t.cwd, cliVersion: '0.0.0-fake', originator: clientName, source: 'vscode',
      threadSource, agentNickname: null, agentRole: null, gitInfo: null, name: t.name, turns: [],
    };
  }
  const threadResult = (t, thread) => ({ thread, model: t.model, modelProvider: 'openai', serviceTier: null, disabledPluginIds: [], cwd: t.cwd, instructionSources: [], approvalPolicy: t.approvalPolicy, approvalsReviewer: 'user', sandbox: t.sandbox, reasoningEffort: null });

  function turnStart(id, p) {
    const thread = threadOf(p);
    const input = userInput(field(p, 'input', Array.isArray, 'a sequence'));
    const approval = p.approvalPolicy == null ? thread.approvalPolicy : approvalPolicy(p.approvalPolicy, experimentalApi);
    if (p.sandboxPolicy != null && !(isObj(p.sandboxPolicy) && variant(p.sandboxPolicy.type, SANDBOX_POLICY_TYPES))) throw invalid('Invalid request: invalid type for `sandboxPolicy`, expected struct SandboxPolicy');
    thread.approvalPolicy = approval;
    if (p.sandboxPolicy != null) thread.sandbox = p.sandboxPolicy;
    if (isStr(p.cwd)) thread.cwd = p.cwd;
    if (p.model != null) thread.model = variant(p.model, MODELS.map((m) => m[0])); // codex: "model not found"
    if (thread.turn) {
      // codex start_or_steer_turn: input joins the active turn, whose id is returned.
      respond(id, { turn: thread.turn.shape('inProgress') });
      return steer(thread.turn, input, p.clientUserMessageId);
    }
    const t = new Turn(thread, textOf(input), p.rootTurnId);
    thread.turn = t;
    thread.lastInput = input;
    respond(id, { turn: t.shape('inProgress') });
    refreshStatus(thread);
    notify('turn/started', { threadId: thread.id, turn: t.shape('inProgress', { startedAt: t.startedAt }) });
    userMessage(t, input, p.clientUserMessageId);
    runTurn(t, scenariosFor(t.prompt));
  }

  function turnSteer(id, p) {
    const thread = threadOf(p);
    const input = userInput(field(p, 'input', Array.isArray, 'a sequence'));
    const expected = field(p, 'expectedTurnId', isStr, 'a string');
    if (!expected) throw invalid('expectedTurnId must not be empty');
    if (!thread.turn) throw invalid('no active turn to steer');
    if (thread.turn.id !== expected) throw invalid(`expected active turn id \`${expected}\` but found \`${thread.turn.id}\``);
    if (!input.length) throw invalid('input must not be empty');
    respond(id, { turnId: thread.turn.id });
    steer(thread.turn, input, p.clientUserMessageId);
  }

  function turnInterrupt(id, p) {
    const thread = threadOf(p);
    const turnId = field(p, 'turnId', isStr, 'a string');
    if (!turnId) return respond(id, {}); // startup interrupt: acknowledged at once
    const t = thread.turn;
    if (!t) throw invalid('no active turn to interrupt');
    if (t.id !== turnId) throw invalid(`expected active turn id ${turnId} but found ${t.id}`);
    t.interrupts.push(id); // answered just before turn/completed, as codex does
    t.abort();
  }

  function mcpShape(name) {
    const s = mcp[name];
    const up = s.runtimeStatus === 'connected';
    return {
      name, runtimeStatus: s.runtimeStatus, pluginId: null, httpOrigin: s.http ? 'https://mcp.fake-linear.example' : null,
      serverInfo: up ? { name, title: null, version: '1.0.0', description: null, icons: null, websiteUrl: null } : null,
      serverCapabilities: up ? { tools: {} } : null, toolListMeta: null, tools: up ? MCP_TOOLS[name] : {}, toolsError: null,
      resources: [], resourceTemplates: [], authStatus: s.authStatus,
    };
  }
  function startupStatus(name) {
    const up = mcp[name].runtimeStatus === 'connected';
    return { threadId: null, name, status: up ? 'ready' : 'failed', error: up ? null : 'OAuth token expired and could not be refreshed', failureReason: up ? null : 'reauthenticationRequired' };
  }
  function mcpList(id, p) {
    if (p.detail != null) variant(p.detail, ['full', 'toolsAndAuthOnly']);
    if (p.threadId != null) threadOf(p);
    const names = Object.keys(mcp).filter((n) => p.serverName == null || n === p.serverName); // unknown name: empty page
    respond(id, { data: names.map(mcpShape), nextCursor: null });
  }
  function mcpLogin(id, p) {
    const name = field(p, 'name', isStr, 'a string');
    if (p.threadId != null) threadOf(p);
    if (p.clientRegistration != null) variant(p.clientRegistration, ['auto', 'cimd', 'dcr']);
    if (!Object.hasOwn(mcp, name)) throw invalid(`No MCP server named '${name}' found.`);
    if (!mcp[name].http) throw invalid('OAuth login is only supported for streamable HTTP servers.');
    const loginId = uuidv7();
    const redirect = encodeURIComponent('http://127.0.0.1:1455/callback');
    respond(id, { authorizationUrl: `https://auth.fake-linear.example/oauth/authorize?response_type=code&client_id=fake-codex&redirect_uri=${redirect}&state=${randomBytes(8).toString('hex')}`, loginId });
    const previous = logins.get(name); // a new login replaces the pending one
    clearTimeout(previous);
    timers.delete(previous);
    if (oauthDelayMs < 0) return;
    const timer = setTimeout(() => {
      timers.delete(timer);
      logins.delete(name);
      Object.assign(mcp[name], { runtimeStatus: 'connected', authStatus: 'oAuth' });
      notify('mcpServer/oauthLogin/completed', { name, threadId: isStr(p.threadId) ? p.threadId : null, loginId, success: true });
    }, oauthDelayMs);
    timers.add(timer);
    logins.set(name, timer);
  }
  function modelList(id) {
    respond(id, {
      data: MODELS.map(([model, displayName, isDefault]) => ({ id: model, model, upgrade: null, upgradeInfo: null, availabilityNux: null, displayName, description: `${displayName} (fake)`, modelSpecialty: null, hidden: false, supportedReasoningEfforts: [{ reasoningEffort: 'low', description: 'Fast' }], defaultReasoningEffort: 'medium', inputModalities: ['text', 'image'], supportsPersonality: false, multiAgentVersion: null, additionalSpeedTiers: [], serviceTiers: [], defaultServiceTier: null, availableAccessPrograms: null, isDefault })),
      nextCursor: null,
    });
  }
  function threadList(id, p) {
    const cwds = p.cwd == null ? null : Array.isArray(p.cwd) ? p.cwd : [p.cwd];
    if (cwds && !cwds.every(isStr)) throw invalid('Invalid request: invalid type for `cwd`, expected a string or a sequence of strings');
    if (p.sortKey != null) variant(p.sortKey, ['created_at', 'updated_at', 'recency_at', 'section_position']);
    const all = new Map(threads);
    if (stateDir) {
      let names = [];
      try {
        names = readdirSync(stateDir).filter((n) => n.endsWith('.json'));
      } catch {}
      for (const n of names) if (!all.has(n.slice(0, -5))) {
        const t = loadSaved(n.slice(0, -5));
        threads.delete(t.id); // listed, not loaded
        all.set(t.id, t);
      }
    }
    const list = [...all.values()].filter((t) => !t.ephemeral && (!cwds || cwds.includes(t.cwd))).sort((a, b) => b.updatedAt - a.updatedAt);
    respond(id, { data: list.slice(0, p.limit ?? 25).map((t) => threadShape(t, false, null)), nextCursor: null, backwardsCursor: null });
  }
  function itemsList(id, p) {
    const tid = field(p, 'threadId', isStr, 'a string');
    const t = known(tid);
    if (!t) throw invalid(`no rollout found for thread id ${tid}`);
    if (p.sortDirection != null) variant(p.sortDirection, ['asc', 'desc']);
    const ordered = p.sortDirection === 'desc' ? [...t.items].reverse() : t.items;
    respond(id, { data: ordered.slice(0, p.limit ?? 100), nextCursor: null, backwardsCursor: null });
  }
  function setName(id, p) {
    const t = known(field(p, 'threadId', isStr, 'a string'));
    const name = field(p, 'name', isStr, 'a string');
    if (!t) throw invalid('thread not found');
    t.name = name;
    save(t);
    respond(id, {});
    notify('thread/name/updated', { threadId: t.id, threadName: name });
  }
  function threadFork(id, p) {
    const from = known(field(p, 'threadId', isStr, 'a string'));
    if (!from) throw invalid(`no rollout found for thread id ${p.threadId}`);
    const approval = p.approvalPolicy == null ? 'on-request' : approvalPolicy(p.approvalPolicy, experimentalApi);
    const sandbox = sandboxPolicy(p.sandbox == null ? 'read-only' : variant(p.sandbox, SANDBOX_MODES));
    const t = newThread(uuidv7(), isStr(p.cwd) ? p.cwd : from.cwd, from.model, approval, sandbox);
    Object.assign(t, { items: [...from.items], forkedFromId: from.id });
    save(t);
    const thread = threadShape(t, false, null);
    respond(id, threadResult(t, thread));
    notify('thread/started', { thread });
  }
  function mcpReload(id) {
    respond(id, {});
    for (const name of Object.keys(mcp)) notify('mcpServer/startupStatus/updated', startupStatus(name));
  }

  const HANDLERS = {
    initialize,
    'thread/start': threadStart,
    'thread/resume': threadResume,
    'turn/start': turnStart,
    'model/list': modelList,
    'thread/list': threadList,
    'thread/items/list': itemsList,
    'thread/name/set': setName,
    'thread/fork': threadFork,
    'turn/steer': turnSteer,
    'turn/interrupt': turnInterrupt,
    'mcpServerStatus/list': mcpList,
    'mcpServer/oauth/login': mcpLogin,
    'config/mcpServer/reload': mcpReload, // its params are Option<()>: anything goes
  };

  function onRequest({ id, method, params }) {
    try {
      if (method !== 'initialize') {
        if (!initialized) throw invalid('Not initialized');
        if (!notified) violation(`request ${method} (id ${JSON.stringify(id)}) sent before the initialized notification`);
      }
      const handler = Object.hasOwn(HANDLERS, method) && HANDLERS[method];
      if (!handler) throw invalid(`Invalid request: unknown variant \`${method}\` (not implemented by fake-codex-app-server)`);
      if (method !== 'config/mcpServer/reload') {
        if (params === undefined) throw invalid('Invalid request: missing field `params`');
        if (!isObj(params)) throw invalid(`Invalid request: invalid type: ${params === null ? 'null' : typeof params}, expected struct params`);
      }
      handler(id, params);
    } catch (e) {
      if (!(e instanceof RpcError)) log(`FAKE-CODEX-ERROR ${e?.stack ?? e}`);
      out({ error: { code: e instanceof RpcError ? e.code : -32603, message: e.message }, id });
    }
  }

  function handleLine(line) {
    if (closed || !line.trim()) return;
    transcript?.('<-', line);
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      violation(`malformed JSON from client: ${e.message}`);
      return out({ error: { code: -32700, message: `Parse error: ${e.message}` }, id: null });
    }
    const idOk = isObj(msg) && (isStr(msg.id) || Number.isInteger(msg.id));
    if (isObj(msg) && isStr(msg.method) && msg.id === undefined) {
      if (msg.method !== 'initialized') return; // codex logs and ignores other client notifications
      if (!initialized) violation('initialized notification before the initialize response');
      notified = true;
      return;
    }
    if (idOk && isStr(msg.method)) return onRequest(msg);
    if (idOk && ('result' in msg || 'error' in msg)) return onResponse(msg);
    violation(`not a JSON-RPC request, notification or response: ${line.slice(0, 200)}`);
    out({ error: { code: -32600, message: 'Invalid request: not a request, notification or response' }, id: idOk ? msg.id : null });
  }

  function end() {
    closed = true;
    for (const timer of timers) clearTimeout(timer);
    timers.clear();
    return violations.length ? 3 : 0;
  }

  return { handleLine, end };
}

// ---- self-test: an in-process client through every scenario, plus two real child processes --
function mailbox() {
  const msgs = [];
  const waiters = new Set();
  return {
    msgs,
    push(m) {
      msgs.push(m);
      for (const w of [...waiters]) w();
    },
    waitFor(pred, what = 'a message', ms = 8000) {
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          waiters.delete(check);
          reject(new Error(`timed out waiting for ${what}`));
        }, ms);
        const check = () => {
          const m = msgs.find(pred);
          if (!m) return;
          clearTimeout(timer);
          waiters.delete(check);
          resolve(m);
        };
        waiters.add(check);
        check();
      });
    },
  };
}

const servers = [];
function connect(options = {}) {
  const box = mailbox();
  const c = { msgs: box.msgs, logs: [], transcript: [], responder: null, nextId: 1, waitFor: box.waitFor };
  c.server = createServer({
    write(line) {
      const m = JSON.parse(line);
      box.push(m);
      // Answer server requests like a real client: later, never inside the server's call stack.
      if (isStr(m.method) && m.id !== undefined && c.responder) {
        const responder = c.responder;
        setImmediate(() => Promise.resolve().then(() => responder(m)).then(
          (answer) => answer && c.send({ id: m.id, ...answer }),
          (e) => c.logs.push(`RESPONDER-ERROR ${e?.stack ?? e}`),
        ));
      }
    },
    log: (line) => c.logs.push(line),
    transcript: (dir, line) => c.transcript.push(`${dir} ${line}`),
    oauthDelayMs: 30,
    ...options,
  });
  servers.push(c.server);
  c.send = (obj) => c.server.handleLine(JSON.stringify(obj));
  c.call = (method, params) => {
    const id = c.nextId++;
    c.send(params === undefined ? { id, method } : { id, method, params });
    return c.waitFor((m) => m.id === id && m.method === undefined, `the response to ${method}`);
  };
  return c;
}
async function ready(options) {
  const c = connect(options);
  assert.ok((await c.call('initialize', { clientInfo: { name: 'self-test', title: null, version: '0.0.0' }, capabilities: { experimentalApi: false } })).result);
  c.send({ method: 'initialized' });
  const started = await c.call('thread/start', { cwd: process.cwd(), approvalPolicy: 'on-request', sandbox: 'workspace-write' });
  return { c, threadId: started.result.thread.id };
}
async function turn(c, threadId, text, responder = null, extra = {}) {
  c.responder = responder;
  const res = await c.call('turn/start', { threadId, input: [{ type: 'text', text }], ...extra });
  assert.ok(res.result, JSON.stringify(res));
  const turnId = res.result.turn.id;
  const done = await c.waitFor((m) => m.method === 'turn/completed' && m.params.turn.id === turnId, `turn/completed for "${text}"`);
  const own = new Set(c.msgs.filter((m) => m.params?.turnId === turnId || m.params?.turn?.id === turnId));
  const reqIds = new Set([...own].filter((m) => m.id !== undefined).map((m) => m.id));
  const seq = c.msgs.filter((m) => own.has(m) || (m.method === 'serverRequest/resolved' && reqIds.has(m.params.requestId)));
  const items = seq.filter((m) => m.method === 'item/completed').map((m) => m.params.item);
  return { turnId, done, seq, items, methods: seq.map((m) => m.method), text: items.filter((i) => i.type === 'agentMessage').map((i) => i.text).join('\n') };
}
const deltas = (seq, method) => seq.filter((m) => m.method === method).map((m) => m.params.delta).join('');
const commandItem = (t) => t.items.find((i) => i.type === 'commandExecution');
const accept = () => ({ result: { decision: 'accept' } });

function child(script) {
  return new Promise((resolve, reject) => {
    const box = mailbox();
    const proc = spawn(process.execPath, [fileURLToPath(import.meta.url)], { stdio: ['pipe', 'pipe', 'pipe'] });
    let stderr = '';
    let buffered = '';
    proc.stderr.on('data', (d) => (stderr += d));
    proc.stdout.on('data', (d) => {
      buffered += d;
      for (let i = buffered.indexOf('\n'); i >= 0; i = buffered.indexOf('\n')) {
        box.push(JSON.parse(buffered.slice(0, i)));
        buffered = buffered.slice(i + 1);
      }
    });
    const timer = setTimeout(() => proc.kill(), 20000);
    proc.on('close', (code) => {
      clearTimeout(timer);
      resolve({ code, stderr, msgs: box.msgs, rest: buffered });
    });
    const send = (obj) => proc.stdin.write(`${JSON.stringify(obj)}\n`);
    script({ send, next: box.waitFor }).then(() => proc.stdin.end(), (e) => {
      proc.kill();
      reject(e);
    });
  });
}
async function childHandshake(send, next, text) {
  send({ id: 0, method: 'initialize', params: { clientInfo: { name: 'self-test', title: 'Self test', version: '1' }, capabilities: null } });
  await next((m) => m.id === 0 && m.method === undefined);
  send({ method: 'initialized' });
  send({ id: 1, method: 'thread/start', params: {} });
  const started = await next((m) => m.id === 1 && m.method === undefined);
  send({ id: 2, method: 'turn/start', params: { threadId: started.result.thread.id, input: [{ type: 'text', text }] } });
}

async function selfTest() {
  let passed = 0;
  let failed = 0;
  async function check(name, fn) {
    try {
      await fn();
      passed++;
      console.log(`ok - ${name}`);
    } catch (e) {
      failed++;
      console.log(`not ok - ${name}\n  ${String(e?.stack ?? e).split('\n').join('\n  ')}`);
    }
  }

  await check('requests before initialize get "Not initialized"', async () => {
    const c = connect();
    assert.deepEqual((await c.call('thread/start', {})).error, { code: -32600, message: 'Not initialized' });
    assert.equal(c.server.end(), 0);
  });

  await check('malformed JSON gets a -32700 reply and is a violation', async () => {
    const c = connect();
    c.server.handleLine('{"id": 1, "method": ');
    const reply = await c.waitFor((m) => m.error?.code === -32700, 'a parse error');
    assert.equal(reply.id, null);
    assert.match(c.logs[0], /^FAKE-CODEX-VIOLATION malformed JSON/);
    assert.equal(c.server.end(), 3);
  });

  await check('initialize: clientInfo required, answered once, MCP startup status follows', async () => {
    const c = connect();
    assert.equal((await c.call('initialize', {})).error.message, 'Invalid request: missing field `clientInfo`');
    const init = await c.call('initialize', { clientInfo: { name: 'self-test', version: '1' } });
    assert.deepEqual(Object.keys(init.result).sort(), ['codexHome', 'platformFamily', 'platformOs', 'userAgent']);
    assert.equal((await c.call('initialize', { clientInfo: { name: 'x', version: '1' } })).error.message, 'Already initialized');
    const startup = c.msgs.filter((m) => m.method === 'mcpServer/startupStatus/updated');
    assert.deepEqual(startup.map((m) => [m.params.name, m.params.status, m.params.failureReason]), [['fake-docs', 'ready', null], ['fake-linear', 'failed', 'reauthenticationRequired']]);
    assert.ok(startup.every((m) => Number.isInteger(m.emittedAtMs)));
    assert.ok(c.msgs.every((m) => !('jsonrpc' in m)));
    c.send({ method: 'initialized' });
    assert.equal(c.server.end(), 0);
  });

  await check('a request before the initialized notification is served but is a violation', async () => {
    const c = connect();
    await c.call('initialize', { clientInfo: { name: 'self-test', version: '1' } });
    assert.ok((await c.call('thread/start', {})).result);
    assert.match(c.logs.join('\n'), /thread\/start \(id 2\) sent before the initialized notification/);
    assert.equal(c.server.end(), 3);
  });

  await check('thread/start: schema enum spelling enforced, thread returned, then thread/started', async () => {
    const { c } = await ready();
    for (const [params, message] of [
      [{ sandbox: 'workspaceWrite' }, /unknown variant `workspaceWrite`, expected one of `read-only`/],
      [{ approvalPolicy: 'onRequest' }, /unknown variant `onRequest`/],
      [{ approvalPolicy: 'unlessTrusted' }, /unknown variant `unlessTrusted`/],
      [{ approvalPolicy: 'granular' }, /unknown variant `granular`/],
      [{ approvalPolicy: { granular: { sandbox_approval: true, rules: true, mcp_elicitations: true } } }, /^askForApproval.granular requires experimentalApi capability$/],
    ]) assert.match((await c.call('thread/start', params)).error.message, message);
    assert.match((await c.call('thread/start')).error.message, /missing field `params`/);
    const res = await c.call('thread/start', { cwd: process.cwd(), approvalPolicy: 'on-request', sandbox: 'workspace-write' });
    assert.equal(res.result.approvalPolicy, 'on-request');
    assert.equal(res.result.sandbox.type, 'workspaceWrite');
    assert.equal(res.result.thread.status.type, 'idle');
    const started = await c.waitFor((m) => m.method === 'thread/started' && m.params.thread.id === res.result.thread.id);
    assert.ok(c.msgs.indexOf(res) < c.msgs.indexOf(started));
    assert.equal((await c.call('turn/start', { threadId: 'nope', input: [] })).error.message, 'thread not found: nope');
    assert.match((await c.call('turn/start', { threadId: res.result.thread.id })).error.message, /missing field `input`/);
    assert.match((await c.call('turn/start', { threadId: res.result.thread.id, input: [{ text: 'x' }] })).error.message, /missing field `type`/);
    assert.match((await c.call('thread/read', { threadId: res.result.thread.id })).error.message, /unknown variant `thread\/read`/);
    assert.equal(c.server.end(), 0);
  });

  await check('thread/resume: a loaded thread, a saved one in a new process, else no rollout', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'fake-codex-state-'));
    try {
      const { c: first, threadId } = await ready({ stateDir: dir });
      const loaded = await first.call('thread/resume', { threadId, cwd: process.cwd(), approvalPolicy: 'on-request', sandbox: 'workspace-write' });
      assert.equal(loaded.result.thread.id, threadId);
      assert.equal(loaded.result.sandbox.type, 'workspaceWrite');
      assert.equal(first.server.end(), 0);
      const { c: second } = await ready({ stateDir: dir });
      assert.match((await second.call('thread/resume', {})).error.message, /missing field `threadId`/);
      assert.equal((await second.call('thread/resume', { threadId: 'nope' })).error.message, 'no rollout found for thread id nope');
      const resumed = await second.call('thread/resume', { threadId, approvalPolicy: 'on-request', sandbox: 'workspace-write' });
      assert.deepEqual([resumed.result.thread.id, resumed.result.thread.turns], [threadId, []]);
      assert.match((await turn(second, threadId, 'hello')).text, /\S/);
      assert.equal(second.server.end(), 0);
      const { c: bare } = await ready();
      assert.match((await bare.call('thread/resume', { threadId })).error.message, /no rollout found/);
      assert.equal(bare.server.end(), 0);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  const { c, threadId } = await ready();
  let expectedViolations = 0;

  await check('hello: user item, streamed agent message, token usage, completed turn', async () => {
    const t = await turn(c, threadId, 'hello there');
    assert.deepEqual(t.methods.filter((m) => m !== 'item/agentMessage/delta'), ['turn/started', 'item/started', 'item/completed', 'item/started', 'item/completed', 'thread/tokenUsage/updated', 'turn/completed']);
    const [user, message] = t.items;
    assert.deepEqual(user, { type: 'userMessage', id: user.id, clientId: null, content: [{ type: 'text', text: 'hello there', text_elements: [] }] });
    assert.ok(t.methods.filter((m) => m === 'item/agentMessage/delta').length >= 3);
    assert.equal(deltas(t.seq, 'item/agentMessage/delta'), message.text);
    assert.equal(t.done.params.turn.status, 'completed');
    assert.deepEqual(t.done.params.turn.items, [message]);
    const usage = t.seq.find((m) => m.method === 'thread/tokenUsage/updated').params.tokenUsage;
    assert.ok(usage.last.totalTokens > 0 && usage.total.totalTokens >= usage.last.totalTokens);
    const statuses = c.msgs.filter((m) => m.method === 'thread/status/changed').slice(-2).map((m) => m.params.status.type);
    assert.deepEqual(statuses, ['active', 'idle']);
  });

  await check('think: reasoning summary deltas, then a message', async () => {
    const t = await turn(c, threadId, 'think about it');
    const reasoning = t.items.find((i) => i.type === 'reasoning');
    assert.equal(deltas(t.seq, 'item/reasoning/summaryTextDelta'), reasoning.summary.join(''));
    assert.ok(t.methods.includes('item/reasoning/summaryPartAdded'));
    assert.equal(t.items.at(-1).type, 'agentMessage');
  });

  await check('run: item started, approval, resolved, output with the fake secret, completed', async () => {
    const t = await turn(c, threadId, 'Run the tests', accept);
    const req = t.seq.find((m) => m.method === 'item/commandExecution/requestApproval');
    assert.equal(req.params.kind, 'command');
    assert.equal(req.params.commandActions[0].command, 'npm test');
    assert.equal(req.params.command, process.platform === 'win32' ? `"${POWERSHELL}" -Command 'npm test'` : 'npm test');
    assert.ok(!('availableDecisions' in req.params), 'on-request lists no decisions');
    assert.equal(req.params.environmentId, null);
    assert.ok(!('proposedExecpolicyAmendment' in req.params));
    const at = t.seq.indexOf(req);
    assert.ok(t.seq.findIndex((m) => m.method === 'item/started' && m.params.item.type === 'commandExecution') < at);
    assert.equal(t.seq[at + 1].method, 'serverRequest/resolved');
    assert.equal(t.seq[at + 1].params.requestId, req.id);
    assert.ok(t.seq.some((m) => m.method === 'item/commandExecution/outputDelta' && m.params.delta.includes(FAKE_SECRET)));
    const cmd = commandItem(t);
    assert.deepEqual([cmd.status, cmd.exitCode, cmd.aggregatedOutput], ['completed', 0, deltas(t.seq, 'item/commandExecution/outputDelta')]);
    assert.equal(t.done.params.turn.status, 'completed');
  });

  await check('run amend: proposedExecpolicyAmendment, accepted with that amendment', async () => {
    const t = await turn(c, threadId, 'run amend', (req) => ({ result: { decision: { acceptWithExecpolicyAmendment: { execpolicy_amendment: req.params.proposedExecpolicyAmendment } } } }));
    assert.deepEqual(t.seq.find((m) => m.method === 'item/commandExecution/requestApproval').params.proposedExecpolicyAmendment, ['npm', 'test']);
    assert.equal(commandItem(t).status, 'completed');
  });

  await check('run: acceptForSession runs; decline completes declined and the turn goes on', async () => {
    assert.equal(commandItem(await turn(c, threadId, 'run', () => ({ result: { decision: 'acceptForSession' } }))).status, 'completed');
    const t = await turn(c, threadId, 'run', () => ({ result: { decision: 'decline' } }));
    assert.deepEqual([commandItem(t).status, commandItem(t).exitCode, commandItem(t).aggregatedOutput], ['declined', null, null]);
    assert.ok(!t.methods.includes('item/commandExecution/outputDelta'));
    assert.equal(t.done.params.turn.status, 'completed');
    assert.equal(t.items.at(-1).type, 'agentMessage');
  });

  await check('run: cancel interrupts the turn; an error answer fails the command; a bad decision is a violation', async () => {
    let t = await turn(c, threadId, 'run', () => ({ result: { decision: 'cancel' } }));
    assert.deepEqual([commandItem(t).status, t.done.params.turn.status], ['declined', 'interrupted']);
    t = await turn(c, threadId, 'run', () => ({ error: { code: -32603, message: 'boom' } }));
    assert.deepEqual([commandItem(t).status, t.done.params.turn.status], ['failed', 'completed']);
    t = await turn(c, threadId, 'run', () => ({ result: { decision: 'approve' } }));
    assert.equal(commandItem(t).status, 'failed');
    assert.match(c.logs.at(-1), /malformed answer to item\/commandExecution\/requestApproval/);
    expectedViolations++;
  });

  await check('edit: fileChange item with a unified diff, approval, multi-file turn diff', async () => {
    let t = await turn(c, threadId, 'edit the greeting', accept);
    const started = t.seq.find((m) => m.method === 'item/started' && m.params.item.type === 'fileChange');
    const req = t.seq.find((m) => m.method === 'item/fileChange/requestApproval');
    assert.ok(t.seq.indexOf(started) < t.seq.indexOf(req));
    assert.equal(req.params.itemId, started.params.item.id);
    assert.deepEqual(started.params.item.changes.map((ch) => ch.kind), [{ type: 'update', move_path: null }, { type: 'add' }]);
    assert.match(started.params.item.changes[0].diff, /^@@ -1,3 \+1,3 @@\n line one\n-hello world\n\+hello codex\n/);
    assert.equal(t.items.find((i) => i.type === 'fileChange').status, 'completed');
    assert.equal(t.seq.find((m) => m.method === 'turn/diff/updated').params.diff.match(/^diff --git /gm).length, 2);
    t = await turn(c, threadId, 'edit', () => ({ result: { decision: 'decline' } }));
    assert.equal(t.items.find((i) => i.type === 'fileChange').status, 'declined');
    assert.ok(!t.methods.includes('turn/diff/updated'));
  });

  await check('plan: turn/plan/updated reaches every step status', async () => {
    const plans = (await turn(c, threadId, 'plan it')).seq.filter((m) => m.method === 'turn/plan/updated');
    assert.equal(plans.length, 2);
    assert.deepEqual(plans[1].params.plan.map((s) => s.status), ['completed', 'inProgress', 'pending']);
  });

  await check('ask: requestUserInput answers are echoed into the final message', async () => {
    const t = await turn(c, threadId, 'ask me', (req) => ({ result: { answers: { confirm_path: { answers: [req.params.questions[0].options[0].label] }, favorite_color: { answers: ['teal'] } } } }));
    assert.equal(t.seq.find((m) => m.method === 'item/tool/requestUserInput').params.questions.length, 2);
    assert.match(t.text, /You answered: confirm_path=Yes \(Recommended\); favorite_color=teal/);
  });

  await check('perm: an empty grant passes, a real grant is a violation', async () => {
    const before = c.logs.length;
    const t = await turn(c, threadId, 'perm', () => ({ result: { permissions: {}, scope: 'turn' } }));
    assert.match(t.text, /Permissions granted: nothing/);
    assert.equal(c.logs.length, before);
    await turn(c, threadId, 'perm', () => ({ result: { permissions: { network: { enabled: true } }, scope: 'turn' } }));
    assert.match(c.logs.at(-1), /expected an empty grant/);
    expectedViolations++;
  });

  await check('elicit: elicitation inside an MCP tool call; decline fails the call', async () => {
    const t = await turn(c, threadId, 'elicit', () => ({ result: { action: 'decline', content: null, _meta: null } }));
    const req = t.seq.find((m) => m.method === 'mcpServer/elicitation/request');
    assert.deepEqual([req.params.mode, req.params.serverName, req.params.turnId], ['form', 'fake-docs', t.turnId]);
    assert.equal(t.items.find((i) => i.type === 'mcpToolCall').status, 'failed');
    assert.match(t.text, /declined/);
  });

  await check('unknown-request: a made-up method is answered with an error', async () => {
    const t = await turn(c, threadId, 'unknown-request', () => ({ error: { code: -32601, message: 'unsupported by host' } }));
    assert.ok(t.seq.some((m) => m.method === UNKNOWN_METHOD && m.id !== undefined));
    assert.match(t.text, /error -32601: unsupported by host/);
  });

  await check('withdraw: serverRequest/resolved comes straight after the request; a late answer is a violation', async () => {
    const t = await turn(c, threadId, 'withdraw');
    const req = t.seq.find((m) => m.method === 'item/commandExecution/requestApproval');
    const next = c.msgs[c.msgs.indexOf(req) + 1];
    assert.deepEqual([next.method, next.params.requestId], ['serverRequest/resolved', req.id]);
    assert.equal(commandItem(t).status, 'completed');
    c.send({ id: req.id, result: { decision: 'accept' } });
    assert.match(c.logs.at(-1), /answer to withdrawn server request/);
    expectedViolations++;
  });

  await check('error: retrying error, final error, failed turn', async () => {
    const t = await turn(c, threadId, 'error');
    const errors = t.seq.filter((m) => m.method === 'error');
    assert.deepEqual(errors.map((e) => e.params.willRetry), [true, false]);
    assert.equal(t.done.params.turn.status, 'failed');
    assert.deepEqual(t.done.params.turn.error, errors[1].params.error);
  });

  await check('slow: steer (also via turn/start) mid-turn, wrong ids refused, interrupt ends it', async () => {
    c.responder = null;
    const turnId = (await c.call('turn/start', { threadId, input: [{ type: 'text', text: 'slow please' }] })).result.turn.id;
    await c.waitFor((m) => m.method === 'item/agentMessage/delta' && m.params.turnId === turnId);
    const input = [{ type: 'text', text: 'focus on the failing test' }];
    assert.equal((await c.call('turn/steer', { threadId, input, expectedTurnId: 'nope' })).error.message, `expected active turn id \`nope\` but found \`${turnId}\``);
    assert.equal((await c.call('turn/steer', { threadId, input: [], expectedTurnId: turnId })).error.message, 'input must not be empty');
    assert.equal((await c.call('turn/steer', { threadId, input, expectedTurnId: turnId })).result.turnId, turnId);
    assert.equal((await c.call('turn/start', { threadId, input: [{ type: 'text', text: 'and the docs' }] })).result.turn.id, turnId);
    assert.equal((await c.call('turn/interrupt', { threadId, turnId: 'nope' })).error.message, `expected active turn id nope but found ${turnId}`);
    const interrupted = await c.call('turn/interrupt', { threadId, turnId });
    assert.deepEqual(interrupted.result, {});
    const done = await c.waitFor((m) => m.method === 'turn/completed' && m.params.turn.id === turnId);
    assert.equal(done.params.turn.status, 'interrupted');
    assert.ok(c.msgs.indexOf(interrupted) < c.msgs.indexOf(done));
    assert.equal(c.msgs.filter((m) => m.method === 'item/completed' && m.params.turnId === turnId && m.params.item.type === 'userMessage').length, 3);
    assert.equal((await c.call('turn/steer', { threadId, input, expectedTurnId: turnId })).error.message, 'no active turn to steer');
    assert.equal((await c.call('turn/interrupt', { threadId, turnId })).error.message, 'no active turn to interrupt');
  });

  await check('a steer is answered in the same turn; interrupting withdraws an open approval', async () => {
    let t = await turn(c, threadId, 'run', async (req) => {
      assert.ok((await c.call('turn/steer', { threadId, input: [{ type: 'text', text: 'print coverage too' }], expectedTurnId: req.params.turnId })).result);
      return accept();
    });
    assert.match(t.text, /Following up on: print coverage too/);
    t = await turn(c, threadId, 'run', async (req) => {
      await c.call('turn/interrupt', { threadId, turnId: req.params.turnId }); // and never answer
    });
    const req = t.seq.find((m) => m.method === 'item/commandExecution/requestApproval');
    assert.ok(t.seq.some((m) => m.method === 'serverRequest/resolved' && m.params.requestId === req.id));
    assert.equal(t.done.params.turn.status, 'interrupted');
  });

  await check('approvalPolicy "never" (a turn/start override that sticks) runs commands without asking', async () => {
    const t = await turn(c, threadId, 'run', null, { approvalPolicy: 'never' });
    assert.equal(commandItem(t).status, 'completed');
    assert.ok(!t.methods.includes('item/commandExecution/requestApproval'));
    assert.match((await c.call('turn/start', { threadId, input: [], approvalPolicy: 'on-request', sandboxPolicy: 'workspace-write' })).error.message, /invalid type for `sandboxPolicy`/);
    await turn(c, threadId, 'hello', null, { approvalPolicy: 'on-request', sandboxPolicy: { type: 'workspaceWrite', writableRoots: [], networkAccess: false, excludeTmpdirEnvVar: false, excludeSlashTmp: false } });
  });

  await check('untrusted (the companion policy): availableDecisions, an unlisted decision is a violation', async () => {
    const { c: u } = await ready();
    const tid = (await u.call('thread/start', { cwd: process.cwd(), approvalPolicy: 'untrusted', sandbox: 'danger-full-access' })).result.thread.id;
    let t = await turn(u, tid, 'run amend', (req) => ({ result: { decision: req.params.availableDecisions[1] } }));
    const req = t.seq.find((m) => m.method === 'item/commandExecution/requestApproval');
    assert.deepEqual(req.params.availableDecisions, ['accept', { acceptWithExecpolicyAmendment: { execpolicy_amendment: ['npm', 'test'] } }, 'cancel']);
    assert.equal(commandItem(t).status, 'completed');
    assert.equal(u.logs.length, 0);
    t = await turn(u, tid, 'run', () => ({ result: { decision: 'decline' } }));
    assert.match(u.logs.at(-1), /is not one of availableDecisions/);
    assert.equal(u.server.end(), 3);
  });

  await check('models, thread list (cwd filter), items, name, fork, model override, image input', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'fake-codex-state-'));
    try {
      const { c: a, threadId: tid } = await ready({ stateDir: dir });
      const models = (await a.call('model/list', {})).result.data;
      assert.deepEqual(models.map((m) => [m.model, m.isDefault]), [['gpt-6-luna', true], ['gpt-5.6-terra', false], ['gpt-5.6-luna', false]]);
      const image = { type: 'image', url: 'data:image/png;base64,iVBORw0KGgo=' };
      const res = await a.call('turn/start', { threadId: tid, input: [image, { type: 'text', text: 'hello' }], model: 'gpt-5.6-luna' });
      await a.waitFor((m) => m.method === 'turn/completed' && m.params.turn.id === res.result.turn.id);
      assert.match((await a.call('turn/start', { threadId: tid, input: [], model: 'gpt-9' })).error.message, /unknown variant `gpt-9`/);
      assert.match((await a.call('turn/start', { threadId: tid, input: [{ type: 'image' }] })).error.message, /needs `url`/);
      assert.deepEqual((await a.call('thread/name/set', { threadId: tid, name: 'Greeting' })).result, {});
      assert.equal((await a.waitFor((m) => m.method === 'thread/name/updated')).params.threadName, 'Greeting');
      assert.equal(a.server.end(), 0);
      // a new process: listed and readable without loading it
      const { c: b } = await ready({ stateDir: dir });
      const listed = (await b.call('thread/list', { limit: 50, sortKey: 'updated_at', cwd: [process.cwd(), 'C:\\nope'] })).result.data;
      const mine = listed.find((t) => t.id === tid);
      assert.deepEqual([mine.name, mine.preview, mine.status.type, mine.model], ['Greeting', 'hello', 'notLoaded', 'gpt-5.6-luna']);
      assert.deepEqual((await b.call('thread/list', { cwd: 'C:\\nope' })).result.data, []);
      const items = (await b.call('thread/items/list', { threadId: tid, limit: 300, sortDirection: 'desc' })).result.data;
      assert.deepEqual(items.map((e) => e.item.type), ['agentMessage', 'userMessage']);
      assert.deepEqual(items[1].item.content[0], image);
      const fork = await b.call('thread/fork', { threadId: tid, cwd: process.cwd(), approvalPolicy: 'untrusted', sandbox: 'danger-full-access', excludeTurns: true });
      assert.equal(fork.result.thread.forkedFromId, tid);
      assert.notEqual(fork.result.thread.id, tid);
      assert.equal((await b.call('thread/items/list', { threadId: fork.result.thread.id })).result.data.length, 2);
      assert.equal(b.server.end(), 0);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  await check('mcp: status list, oauth login, completion marks the server connected, reload', async () => {
    const list = await c.call('mcpServerStatus/list', {});
    assert.deepEqual(list.result.data.map((s) => [s.name, s.runtimeStatus, s.authStatus]), [['fake-docs', 'connected', 'unsupported'], ['fake-linear', 'authenticationRequired', 'notLoggedIn']]);
    assert.equal(list.result.nextCursor, null);
    assert.match((await c.call('mcpServerStatus/list')).error.message, /missing field `params`/);
    assert.equal((await c.call('mcpServer/oauth/login', { name: 'nope' })).error.message, "No MCP server named 'nope' found.");
    assert.equal((await c.call('mcpServer/oauth/login', { name: 'fake-docs' })).error.message, 'OAuth login is only supported for streamable HTTP servers.');
    const login = await c.call('mcpServer/oauth/login', { name: 'fake-linear' });
    assert.match(login.result.authorizationUrl, /^https:\/\/auth\.fake-linear\.example\//);
    const done = await c.waitFor((m) => m.method === 'mcpServer/oauthLogin/completed' && m.params.loginId === login.result.loginId);
    assert.deepEqual([done.params.name, done.params.success, 'error' in done.params], ['fake-linear', true, false]);
    const after = await c.call('mcpServerStatus/list', { serverName: 'fake-linear' });
    assert.deepEqual(after.result.data.map((s) => [s.runtimeStatus, s.authStatus]), [['connected', 'oAuth']]);
    const n = c.msgs.length;
    assert.deepEqual((await c.call('config/mcpServer/reload')).result, {});
    assert.deepEqual(c.msgs.slice(n).filter((m) => m.method === 'mcpServer/startupStatus/updated').map((m) => m.params.status), ['ready', 'ready']);
  });

  await check('violations: duplicate answer, unknown id, wrong id type; exit code 3', async () => {
    const t = await turn(c, threadId, 'ask', () => ({ result: { answers: {} } }));
    const req = t.seq.find((m) => m.method === 'item/tool/requestUserInput');
    c.send({ id: req.id, result: { answers: {} } });
    c.send({ id: 999999, result: {} });
    c.send({ id: String(req.id), result: {} });
    assert.match(c.logs.at(-3), /duplicate answer to server request/);
    assert.match(c.logs.at(-2), /unknown server request id 999999/);
    assert.match(c.logs.at(-1), /unknown server request id "\d+"/);
    expectedViolations += 3;
    assert.deepEqual(c.logs.filter((l) => !l.startsWith('FAKE-CODEX-VIOLATION ')), []);
    assert.equal(c.logs.length, expectedViolations);
    assert.ok(['<- ', '-> ', '!! '].every((dir) => c.transcript.some((l) => l.startsWith(dir))));
    assert.equal(c.server.end(), 3);
  });

  await check('an unanswered unknown request is a violation after the answer timeout', async () => {
    const { c: quick, threadId: tid } = await ready({ answerTimeoutMs: 50 });
    const t = await turn(quick, tid, 'unknown-request');
    assert.match(t.text, /never answered/);
    assert.match(quick.logs.join('\n'), /no answer to server request \d+ \(fake\/unknownServerRequest\) within 50 ms/);
    assert.equal(quick.server.end(), 3);
  });

  await check('stdio child: newline framing, no "jsonrpc" field, exit 0 at EOF', async () => {
    const r = await child(async ({ send, next }) => {
      await childHandshake(send, next, 'hello');
      await next((m) => m.method === 'turn/completed');
    });
    assert.equal(r.code, 0, r.stderr);
    assert.equal(r.rest, '');
    assert.ok(r.msgs.length > 5 && r.msgs.every((m) => !('jsonrpc' in m)));
  });

  await check('stdio child: a duplicate answer means exit 3 and a violation on stderr', async () => {
    const r = await child(async ({ send, next }) => {
      await childHandshake(send, next, 'ask');
      const req = await next((m) => m.method === 'item/tool/requestUserInput');
      send({ id: req.id, result: { answers: {} } });
      send({ id: req.id, result: { answers: {} } });
      await next((m) => m.method === 'turn/completed');
    });
    assert.equal(r.code, 3);
    assert.match(r.stderr, /^FAKE-CODEX-VIOLATION duplicate answer to server request 0 \(item\/tool\/requestUserInput\)/m);
  });

  for (const server of servers) server.end();
  console.log(`${passed}/${passed + failed} self-test checks passed`);
  return failed === 0;
}

const args = process.argv.slice(2);
const option = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
if (args.includes('--self-test')) {
  selfTest().then((ok) => {
    process.exitCode = ok ? 0 : 1;
  }, (e) => {
    console.error(e);
    process.exitCode = 1;
  });
} else {
  const transcriptFile = option('--transcript');
  const server = createServer({
    write: (line) => process.stdout.write(`${line}\n`),
    log: (line) => process.stderr.write(`${line}\n`),
    transcript: transcriptFile ? (dir, line) => appendFileSync(transcriptFile, `${new Date().toISOString()} ${dir} ${line}\n`) : null,
    oauthDelayMs: Number(option('--oauth-delay-ms') ?? 500),
    stateDir: option('--state-dir') ?? null,
  });
  process.stdout.on('error', () => {}); // the client went away; stdin EOF follows
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  lines.on('line', (line) => server.handleLine(line));
  lines.on('close', () => {
    process.exitCode = server.end(); // timers cleared, so the process exits once stdout drains
  });
}
