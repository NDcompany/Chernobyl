/**
 * Pure helpers behind the ADR-015 conversation view (`src/ui/transcript.tsx`):
 * a streaming-safe markdown subset parser, unified-diff classification, turn
 * footer formatting, the PermissionRequest → approval-button mapping and the
 * row model of the transcript list. No React Native imports and type-only
 * protocol imports, so `node --test` runs this file directly (transcript.test.ts).
 */
import type {
  AgentKind,
  McpServerState,
  PermissionDecision,
  PermissionDetail,
  PermissionRequest,
  TranscriptItem,
  TurnState,
} from '@chernobyl/protocol';

/* ------------------------------------------------------------------ */
/* Markdown subset (ADR-015 §5, §6 T3)                                 */
/*                                                                     */
/* Paragraphs, headings, bold, italic, inline code, fenced code, lists */
/* and rules. No HTML, no images, no remote loads: everything else is  */
/* text. Links are http(s) only and always show their full URL.        */
/* ------------------------------------------------------------------ */

export interface MdSpan {
  text: string;
  bold?: true;
  italic?: true;
  code?: true;
  /** http(s) only; `text` already contains the full URL. */
  url?: string;
}

export interface MdListItem {
  /** "•" or the item number ("3.") */
  marker: string;
  depth: number;
  spans: MdSpan[];
}

/** `src` is the block's raw source, a cheap memo key for rendering. */
export type MdBlock =
  | { kind: 'paragraph'; src: string; spans: MdSpan[] }
  | { kind: 'heading'; src: string; level: number; spans: MdSpan[] }
  | { kind: 'code'; src: string; lang: string; text: string; open: boolean }
  | { kind: 'list'; src: string; items: MdListItem[] }
  | { kind: 'rule'; src: string };

const FENCE_RE = /^(\s*)(`{3,}|~{3,})(.*)$/;
const HEADING_RE = /^ {0,3}(#{1,6})[ \t]+(.+?)(?:[ \t]+#+)?[ \t]*$/;
const RULE_RE = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const ITEM_RE = /^(\s*)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;
/** While streaming, a last line that is only the start of a block marker waits for more text. */
const PARTIAL_TAIL_RE = /^\s*(`{1,2}|~{1,2}|[-*+_]{1,2}|\d{1,9}[.)]?|#{1,6}[ \t]*)$/;

function stripIndent(line: string, n: number): string {
  let k = 0;
  while (k < n && line[k] === ' ') k++;
  return line.slice(k);
}

/**
 * Parses the markdown subset. `streaming` = the text is still arriving: an
 * unclosed fence or emphasis renders as if closed and a dangling marker is
 * held back, so the output only grows instead of flipping style. Never throws.
 */
export function parseMarkdown(src: string, streaming = false): MdBlock[] {
  const lines = src.split(/\r?\n/);
  if (streaming && PARTIAL_TAIL_RE.test(lines[lines.length - 1])) lines.pop();
  const blocks: MdBlock[] = [];
  let paraStart = -1;
  let list: {
    start: number;
    items: { marker: string; depth: number; text: string }[];
    indents: number[];
    counters: (number | undefined)[];
  } | null = null;

  const raw = (from: number, to: number) => lines.slice(from, to).join('\n');
  const endPara = (at: number) => {
    if (paraStart < 0) return;
    const text = lines.slice(paraStart, at).map((l) => l.trim()).join('\n');
    blocks.push({ kind: 'paragraph', src: raw(paraStart, at), spans: parseInline(text, streaming) });
    paraStart = -1;
  };
  const endList = (at: number) => {
    if (!list) return;
    blocks.push({
      kind: 'list',
      src: raw(list.start, at),
      items: list.items.map((it) => ({ marker: it.marker, depth: it.depth, spans: parseInline(it.text, streaming) })),
    });
    list = null;
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = FENCE_RE.exec(line);
    if (fence && !(fence[2][0] === '`' && fence[3].includes('`'))) {
      endPara(i);
      endList(i);
      const start = i;
      const mark = fence[2];
      const body: string[] = [];
      let open = true;
      for (i++; i < lines.length; i++) {
        const close = FENCE_RE.exec(lines[i]);
        if (close && close[2][0] === mark[0] && close[2].length >= mark.length && close[3].trim() === '') {
          open = false;
          break;
        }
        body.push(stripIndent(lines[i], fence[1].length));
      }
      // a blank line still being typed would make the box grow, then shrink at the closing fence
      while (open && streaming && body.length && !body[body.length - 1].trim()) body.pop();
      blocks.push({
        kind: 'code',
        src: raw(start, i + 1),
        lang: fence[3].trim().split(/\s+/)[0] ?? '',
        text: body.join('\n'),
        open,
      });
      continue;
    }
    if (line.trim() === '') {
      endPara(i);
      endList(i);
      continue;
    }
    const heading = HEADING_RE.exec(line);
    if (heading) {
      endPara(i);
      endList(i);
      blocks.push({ kind: 'heading', src: line, level: heading[1].length, spans: parseInline(heading[2], streaming) });
      continue;
    }
    if (RULE_RE.test(line)) {
      endPara(i);
      endList(i);
      blocks.push({ kind: 'rule', src: line });
      continue;
    }
    const item = ITEM_RE.exec(line);
    if (item) {
      endPara(i);
      if (!list) list = { start: i, items: [], indents: [], counters: [] };
      const indent = item[1].replace(/\t/g, '    ').length;
      while (list.indents.length && indent < list.indents[list.indents.length - 1]) list.indents.pop();
      if (!list.indents.length || indent > list.indents[list.indents.length - 1]) list.indents.push(indent);
      const depth = Math.min(list.indents.length - 1, 6);
      list.counters.length = depth + 1; // a new nested list restarts its numbering
      let marker = '•';
      if (item[2][0] >= '0' && item[2][0] <= '9') {
        const prev = list.counters[depth];
        const n = prev === undefined ? parseInt(item[2], 10) : prev + 1;
        list.counters[depth] = n;
        marker = `${n}.`;
      } else {
        list.counters[depth] = undefined;
      }
      list.items.push({ marker, depth, text: item[3] });
      continue;
    }
    if (list) {
      // lazy continuation of the last item
      list.items[list.items.length - 1].text += `\n${line.trim()}`;
      continue;
    }
    if (paraStart < 0) paraStart = i;
  }
  endPara(lines.length);
  endList(lines.length);
  return blocks;
}

const ESCAPABLE = '\\`*_{}[]()#+-.!|~<>"\'';
const SPACE_RE = /\s/;
const ALNUM_RE = /[A-Za-z0-9]/;
// invisible and bidi controls end a URL: a link carrying one is never tappable,
// so what the confirm shows is what opens
const URL_RE = /^https?:\/\/[^\s<>`"'()[\]{}​-‏‪-‮⁠-⁩﻿]+/;

function canOpen(s: string, from: number, to: number, ch: string): boolean {
  if (to >= s.length || SPACE_RE.test(s[to])) return false;
  return !(ch === '_' && from > 0 && ALNUM_RE.test(s[from - 1]));
}

function canClose(s: string, from: number, to: number, ch: string): boolean {
  if (from === 0 || SPACE_RE.test(s[from - 1])) return false;
  return !(ch === '_' && to < s.length && ALNUM_RE.test(s[to]));
}

/** Code span content; `open` = its closing backticks have not streamed in yet. */
function codeText(raw: string, open = false): string {
  let t = raw.replace(/\n/g, ' ');
  if (open) {
    // drop a half-arrived closer and the padding space a closed span would strip
    t = t.replace(/`+$/, '');
    if (t[0] === ' ') t = t.slice(1);
    return t.endsWith(' ') ? t.slice(0, -1) : t;
  }
  return t.length > 2 && t[0] === ' ' && t[t.length - 1] === ' ' && t.trim() ? t.slice(1, -1) : t;
}

/**
 * Inline spans of one block. Linear time on hostile input: emphasis openers
 * check a precomputed last-closer position, code spans a per-length index
 * of backtick runs, links a cached next-bracket position.
 */
export function parseInline(s: string, streaming = false): MdSpan[] {
  const out: MdSpan[] = [];
  let buf = '';
  let bold = ''; // delimiter char that opened it, '' = closed
  let italic = '';

  const emit = (text: string, extra?: { code?: true; url?: string }) => {
    if (!text) return;
    const span: MdSpan = { text };
    if (bold) span.bold = true;
    if (italic) span.italic = true;
    if (extra?.code) span.code = true;
    if (extra?.url) span.url = extra.url;
    const prev = out[out.length - 1];
    if (prev && !prev.code && !prev.url && !extra && prev.bold === span.bold && prev.italic === span.italic) {
      prev.text += text;
    } else {
      out.push(span);
    }
  };
  const flush = () => {
    emit(buf);
    buf = '';
  };

  // last position where a `*`/`_` run can close italic (i) or bold (b)
  const lastClose: Record<string, number> = {};
  // backtick runs by length, for matching code spans
  const ticks = new Map<number, number[]>();
  for (let p = 0; p < s.length; ) {
    const ch = s[p];
    if (ch !== '*' && ch !== '_' && ch !== '`') {
      p++;
      continue;
    }
    let q = p;
    while (s[q] === ch) q++;
    if (ch === '`') {
      const at = ticks.get(q - p);
      if (at) at.push(p);
      else ticks.set(q - p, [p]);
    } else if (canClose(s, p, q, ch)) {
      if (q - p !== 2) lastClose[`${ch}i`] = p;
      if (q - p >= 2) lastClose[`${ch}b`] = p;
    }
    p = q;
  }
  const tickAt = new Map<number, number>();
  const nextTick = (len: number, after: number): number => {
    const at = ticks.get(len) ?? [];
    let k = tickAt.get(len) ?? 0;
    while (k < at.length && at[k] <= after) k++;
    tickAt.set(len, k);
    return k < at.length ? at[k] : -1;
  };
  const found: Record<string, number> = {};
  const nextOf = (ch: string, from: number): number => {
    const hit = found[ch];
    if (hit !== undefined && (hit === -1 || hit >= from)) return hit;
    return (found[ch] = s.indexOf(ch, from));
  };

  for (let i = 0; i < s.length; ) {
    const c = s[i];
    if (c === '\\' && streaming && i + 1 === s.length) break; // half of an escape
    if (c === '\\' && i + 1 < s.length && ESCAPABLE.includes(s[i + 1])) {
      buf += s[i + 1];
      i += 2;
      continue;
    }
    if (c === '`') {
      let q = i;
      while (s[q] === '`') q++;
      const close = nextTick(q - i, i);
      if (close >= 0 || streaming) {
        flush();
        emit(close >= 0 ? codeText(s.slice(q, close)) : codeText(s.slice(q), true), { code: true });
        i = close >= 0 ? close + (q - i) : s.length;
      } else {
        buf += s.slice(i, q);
        i = q;
      }
      continue;
    }
    if (c === '*' || c === '_') {
      let q = i;
      while (s[q] === c) q++;
      // after a non-word character a marker may open before its closer has streamed in
      const guess = streaming && (i === 0 || !ALNUM_RE.test(s[i - 1]));
      if (streaming && q === s.length && (guess || bold === c || italic === c)) {
        // a trailing marker waits: it may open the next text or be half of a closer
        i = q;
        continue;
      }
      const opens = canOpen(s, i, q, c);
      const closes = canClose(s, i, q, c);
      let rem = q - i > 3 ? 0 : q - i;
      while (rem > 0) {
        if (rem >= 2 && bold === c && closes) {
          flush();
          bold = '';
          rem -= 2;
        } else if (italic === c && closes) {
          flush();
          italic = '';
          rem -= 1;
        } else if (rem >= 2 && !bold && opens && (guess || (lastClose[`${c}b`] ?? -1) > i)) {
          flush();
          bold = c;
          rem -= 2;
        } else if (!italic && opens && (guess || (lastClose[`${c}i`] ?? -1) > i)) {
          flush();
          italic = c;
          rem -= 1;
        } else {
          break;
        }
      }
      buf += c.repeat(q - i > 3 ? q - i : rem);
      i = q;
      continue;
    }
    if (c === '[') {
      const close = nextOf(']', i + 1);
      const end = close > i && s[close + 1] === '(' ? nextOf(')', close + 2) : -1;
      if (end > close && close - i <= 1000 && end - close <= 2100) {
        const label = s.slice(i + 1, close).trim();
        const target = s.slice(close + 2, end).trim().split(/\s+/)[0] ?? '';
        if (label && target) {
          // the full target is always visible; only a whole http(s) URL is tappable
          const shown = label === target ? target : `${label} (${target})`;
          if (URL_RE.exec(target)?.[0] === target) {
            flush();
            emit(shown, { url: target });
          } else {
            buf += shown;
          }
          i = end + 1;
          continue;
        }
      }
      // while streaming, a link still arriving at the end waits instead of showing its syntax
      if (
        streaming &&
        s.length - i < 2100 &&
        (close < 0
          ? !s.includes('\n', i)
          : close === s.length - 1 || (s[close + 1] === '(' && end < 0 && !/\s/.test(s.slice(close + 2))))
      ) {
        break;
      }
    }
    if (c === 'h' && (i === 0 || !ALNUM_RE.test(s[i - 1])) && (s.startsWith('http://', i) || s.startsWith('https://', i))) {
      const m = URL_RE.exec(s.slice(i, i + 2100));
      const url = m ? m[0].replace(/[.,;:!?*_~]+$/, '') : '';
      if (/^https?:\/\/[^/]/.test(url)) {
        flush();
        emit(url, { url });
        i += url.length;
        continue;
      }
    }
    buf += c;
    i++;
  }
  flush();
  return out;
}

/* ------------------------------------------------------------------ */
/* Unified diffs                                                       */
/* ------------------------------------------------------------------ */

export type DiffLineKind = 'add' | 'del' | 'context' | 'hunk' | 'meta';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

/** Lines past this many render behind "Show more". */
export const DIFF_PREVIEW_LINES = 200;

/**
 * Classifies each line by its first character. File headers (`---`/`+++`,
 * `diff --git`, `index …`) count only before the first hunk, so a removed
 * line that starts with "--" is still a deletion.
 */
export function parseDiff(patch: string): DiffLine[] {
  const lines = patch.split(/\r?\n/);
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  let inHunk = false;
  return lines.map((text): DiffLine => {
    if (text.startsWith('@@')) {
      inHunk = true;
      return { kind: 'hunk', text };
    }
    if (text.startsWith('diff --git ')) {
      inHunk = false;
      return { kind: 'meta', text };
    }
    const c = text[0];
    if (!inHunk && (text.startsWith('+++') || text.startsWith('---') || (c !== '+' && c !== '-' && c !== ' '))) {
      return { kind: 'meta', text };
    }
    return { kind: c === '+' ? 'add' : c === '-' ? 'del' : c === '\\' ? 'meta' : 'context', text };
  });
}

export function diffCounts(lines: DiffLine[]): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  for (const l of lines) {
    if (l.kind === 'add') additions++;
    else if (l.kind === 'del') deletions++;
  }
  return { additions, deletions };
}

/* ------------------------------------------------------------------ */
/* Turn footer                                                         */
/* ------------------------------------------------------------------ */

export function formatDuration(ms: number): string {
  if (ms < 60_000) return `${(Math.floor(Math.max(0, ms) / 100) / 10).toFixed(1)} s`;
  const total = Math.floor(ms / 1000);
  return `${Math.floor(total / 60)}m ${String(total % 60).padStart(2, '0')}s`;
}

export function formatTokens(n: number): string {
  const one = (x: number) => x.toFixed(1).replace(/\.0$/, '');
  if (n < 1000) return `${n} tokens`;
  if (n < 1_000_000) return `${one(n / 1000)}k tokens`;
  return `${one(n / 1_000_000)}M tokens`;
}

export function formatCost(usd: number): string {
  return usd > 0 && usd < 0.01 ? '<$0.01' : `$${Math.max(0, usd).toFixed(2)}`;
}

export type FooterTone = 'done' | 'stopped' | 'error';

/**
 * "Done · 12.4 s · 4.2k tokens · $0.01" (tokens = input + output, cache reads
 * excluded; cost only when the runtime reports it), "Stopped", or the error.
 * Null while the turn runs.
 */
export function turnFooter(turn: TurnState): { text: string; tone: FooterTone } | null {
  switch (turn.status) {
    case 'interrupted':
      return { text: 'Stopped', tone: 'stopped' };
    case 'failed':
      return { text: turn.error || 'Failed', tone: 'error' };
    case 'completed': {
      const parts = ['Done'];
      if (turn.endedAt != null) parts.push(formatDuration(turn.endedAt - turn.startedAt));
      if (turn.usage) parts.push(formatTokens(turn.usage.inputTokens + turn.usage.outputTokens));
      if (turn.costUsd != null) parts.push(formatCost(turn.costUsd));
      return { text: parts.join(' · '), tone: 'done' };
    }
    default:
      return null;
  }
}

/* ------------------------------------------------------------------ */
/* Approval buttons (ADR-015 §4.5, T3/T4)                              */
/* ------------------------------------------------------------------ */

/** What the approval UI hands to `onDecide`; the data layer sends it as `permission_decision`. */
export interface ApprovalDecision {
  permissionId: string;
  decision: PermissionDecision;
  optionId?: string;
  message?: string;
  /** keyed by PermissionDetail.question.questions[].id */
  answers?: Record<string, string[]>;
  /** the record's revision the user decided against (G2) */
  revision: number;
}

export interface ApprovalChoice {
  /** stable React key */
  key: string;
  /** shown verbatim */
  label: string;
  decision: PermissionDecision;
  optionId?: string;
  scope: 'once' | 'session' | 'persistent';
  rule?: string;
  destination?: string;
  /** the "…and tell <agent> why" variant: asks for a message first */
  withMessage?: true;
  stopsTurn?: true;
}

export interface ApprovalButtons {
  primary?: ApprovalChoice;
  secondary?: ApprovalChoice;
  /** behind "More…": session and persistent grants, other rejects, the tell-why variants */
  more: ApprovalChoice[];
}

/**
 * Buttons come only from the companion's PermissionRequest record: its
 * `options` (labels verbatim) or, when absent, the legacy
 * `supportedDecisions`. Never from transcript text. The primary is the first
 * once-scoped allow (a persistent grant is never primary); the secondary is
 * the first reject. A record whose first option is a reject (the companion's
 * `default_to_no` ordering) swaps them, so Deny becomes primary.
 */
export function approvalButtons(
  p: Pick<PermissionRequest, 'options' | 'supportedDecisions'>,
  agentName = 'the agent',
): ApprovalButtons {
  if (!p.options?.length) {
    const has = (d: PermissionDecision) => p.supportedDecisions.includes(d);
    const choice = (decision: PermissionDecision, label: string, scope: 'once' | 'session'): ApprovalChoice => ({
      key: decision,
      label,
      decision,
      scope,
    });
    return {
      primary: has('approve-once') ? choice('approve-once', 'Allow', 'once') : undefined,
      secondary: has('reject') ? choice('reject', 'Deny', 'once') : undefined,
      more: has('approve-session') ? [choice('approve-session', 'Allow for this session', 'session')] : [],
    };
  }
  const choices = p.options.map((o): ApprovalChoice => {
    const c: ApprovalChoice = { key: o.id, label: o.label, decision: o.decision, optionId: o.id, scope: o.scope };
    if (o.rule) c.rule = o.rule;
    if (o.destination) c.destination = o.destination;
    if (o.stopsTurn) c.stopsTurn = true;
    return c;
  });
  const allow = choices.find((c) => c.decision !== 'reject' && c.scope === 'once');
  const deny =
    choices.find((c) => c.decision === 'reject' && !c.stopsTurn) ?? choices.find((c) => c.decision === 'reject');
  const denyFirst = choices[0].decision === 'reject';
  const primary = denyFirst ? deny : allow;
  const secondary = denyFirst ? allow : deny;
  const more = choices.filter((c) => c !== primary && c !== secondary);
  p.options.forEach((o, i) => {
    if (o.decision === 'reject' && o.takesMessage) {
      more.push({ ...choices[i], key: `${o.id}:why`, label: `${o.label} and tell ${agentName} why`, withMessage: true });
    }
  });
  return { primary, secondary, more };
}

type Questions = Extract<PermissionDetail, { kind: 'question' }>['questions'];

/** Answers keyed by question id: the chosen option labels plus any free text. */
export function collectAnswers(
  questions: Questions,
  chosen: Record<string, string[]>,
  free: Record<string, string>,
): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  for (const q of questions) {
    const text = free[q.id]?.trim();
    out[q.id] = [...(chosen[q.id] ?? []), ...(text ? [text] : [])];
  }
  return out;
}

export function agentShortName(kind?: AgentKind): string {
  switch (kind) {
    case 'claude-code':
      return 'Claude';
    case 'codex':
      return 'Codex';
    case 'opencode':
      return 'OpenCode';
    default:
      return 'the agent';
  }
}

/* ------------------------------------------------------------------ */
/* Permission mode + MCP                                               */
/* ------------------------------------------------------------------ */

/** The only modes the phone may set (ADR-015 §3.4); never bypass modes. */
export const PERMISSION_MODE_CHOICES = ['default', 'acceptEdits', 'plan'] as const;
export type PermissionModeChoice = (typeof PERMISSION_MODE_CHOICES)[number];

export function isModeChoice(mode: string): mode is PermissionModeChoice {
  return (PERMISSION_MODE_CHOICES as readonly string[]).includes(mode);
}

/** Claude's mode names as the VS Code extension labels them; unknown modes verbatim. */
export function modeLabel(mode?: string): string {
  switch (mode) {
    case 'default':
      return 'Ask before edits';
    case 'acceptEdits':
      return 'Accept edits';
    case 'plan':
      return 'Plan';
    default:
      return mode || 'Permission mode';
  }
}

/** "MCP 4/5": connected of the enabled servers; attention = one needs sign-in or failed. */
export function mcpSummary(servers: McpServerState[]): { connected: number; total: number; attention: boolean } {
  const enabled = servers.filter((s) => s.status !== 'disabled');
  return {
    connected: enabled.filter((s) => s.status === 'connected').length,
    total: enabled.length,
    attention: enabled.some((s) => s.status === 'needs-auth' || s.status === 'failed'),
  };
}

/** Client-side precheck of a pasted OAuth callback; the companion does the full check (§4.7). */
export function isCallbackUrl(url: string, prefix?: string): boolean {
  const u = url.trim();
  return u.length <= 4096 && u.startsWith('http://') && (!prefix || u.startsWith(prefix));
}

/* ------------------------------------------------------------------ */
/* Transcript rows                                                     */
/* ------------------------------------------------------------------ */

export type TranscriptRow =
  | { key: string; type: 'item'; item: TranscriptItem }
  | { key: string; type: 'footer'; turn: TurnState }
  | { key: string; type: 'approval'; permission: PermissionRequest };

export interface TranscriptModel {
  rows: TranscriptRow[];
  /** child items by parent tool item id (subagent work under a Task tool) */
  children: Map<string, TranscriptItem[]>;
  /** the pending request rendered inside each tool card, by tool item id */
  approvals: Map<string, PermissionRequest>;
  /** tool items with a pending approval on themselves or a descendant: kept open */
  attention: Set<string>;
}

const KINDS: ReadonlySet<string> = new Set(['user', 'text', 'thinking', 'tool', 'plan', 'notice']);

/**
 * Rows in item order, a footer after each ended turn's last item, and a
 * standalone approval at the end for every pending request whose tool item
 * is not in view, so a request is never invisible. Unknown item kinds are
 * skipped (the reducer keeps them).
 */
export function buildTranscript(
  items: TranscriptItem[],
  turns: TurnState[],
  permissions: PermissionRequest[],
): TranscriptModel {
  const turnById = new Map(turns.map((t) => [t.id, t]));
  // a notice that repeats its ended turn's footer ("Stopped" after an interrupt) shows once, as the
  // footer; kept when it is the turn's only item, since the footer hangs off the turn's last item
  const echo = (it: TranscriptItem) => {
    const turn = it.kind === 'notice' && it.turnId ? turnById.get(it.turnId) : undefined;
    return turn !== undefined && it.kind === 'notice' && it.text === turnFooter(turn)?.text;
  };
  const known = items.filter(
    (it) => KINDS.has(it.kind) && !(echo(it) && items.some((o) => o.turnId === it.turnId && KINDS.has(o.kind) && !echo(o))),
  );
  const byId = new Map(known.map((it) => [it.id, it]));
  const toolParent = (it: TranscriptItem) =>
    it.parentId && byId.get(it.parentId)?.kind === 'tool' ? it.parentId : undefined;
  // nested only under a tool item, and only when the parent chain has no cycle
  const parentOf = new Map<string, string>();
  for (const it of known) {
    const seen = new Set([it.id]);
    let cur: TranscriptItem | undefined = it;
    while (cur && toolParent(cur) && !seen.has(toolParent(cur)!)) {
      seen.add(toolParent(cur)!);
      cur = byId.get(toolParent(cur)!);
    }
    if (cur && !toolParent(cur) && toolParent(it)) parentOf.set(it.id, toolParent(it)!);
  }

  const children = new Map<string, TranscriptItem[]>();
  const top: TranscriptItem[] = [];
  for (const it of known) {
    const parent = parentOf.get(it.id);
    const siblings = parent ? children.get(parent) : undefined;
    if (!parent) top.push(it);
    else if (siblings) siblings.push(it);
    else children.set(parent, [it]);
  }

  const approvals = new Map<string, PermissionRequest>();
  const pending = permissions.filter((p) => p.status === 'pending');
  for (const p of pending) {
    if (p.toolItemId && byId.get(p.toolItemId)?.kind === 'tool' && !approvals.has(p.toolItemId)) {
      approvals.set(p.toolItemId, p);
    }
  }
  const attention = new Set<string>();
  for (const id of approvals.keys()) {
    for (let cur: string | undefined = id; cur && !attention.has(cur); cur = parentOf.get(cur)) attention.add(cur);
  }

  const lastOfTurn = new Map<string, string>();
  for (const it of top) if (it.turnId) lastOfTurn.set(it.turnId, it.id);
  const rows: TranscriptRow[] = [];
  for (const it of top) {
    rows.push({ key: it.id, type: 'item', item: it });
    const turn = it.turnId && lastOfTurn.get(it.turnId) === it.id ? turnById.get(it.turnId) : undefined;
    if (turn && turn.status !== 'running') rows.push({ key: `turn:${turn.id}`, type: 'footer', turn });
  }
  for (const p of pending) {
    if (approvals.get(p.toolItemId ?? '') !== p) rows.push({ key: `perm:${p.id}`, type: 'approval', permission: p });
  }
  return { rows, children, approvals, attention };
}

/**
 * The row that shows an item (deep links): its own, its top-level card when
 * it is nested under a Task tool, or the standalone approval of a request
 * whose card is not in view; -1 when nothing shows it yet.
 */
export function rowIndexFor(model: TranscriptModel, itemId: string): number {
  const parentOf = new Map<string, string>();
  for (const [parent, kids] of model.children) for (const kid of kids) parentOf.set(kid.id, parent);
  let top = itemId;
  for (let hops = 0; parentOf.has(top) && hops < 64; hops++) top = parentOf.get(top)!;
  return model.rows.findIndex(
    (r) => (r.type === 'item' && r.item.id === top) || (r.type === 'approval' && r.permission.toolItemId === itemId),
  );
}
