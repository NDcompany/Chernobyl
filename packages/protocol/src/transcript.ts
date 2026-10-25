/**
 * ADR-015 §3.2: the pure transcript reducer the phone renders from.
 *
 * Keep one view per subscribed session and feed it every `transcript` and
 * `transcript_snapshot` frame for that session. `resubscribe: true` means the
 * frame cannot continue the view (a gap or a new epoch): re-send
 * `session_subscribe` with the view's `epoch` and `seq`, and the companion
 * answers with the missing ops or a snapshot. Views are memory-only by design
 * (threat model T2): never persist one.
 */
import type {
  LiveServerMessage,
  LiveState,
  McpServerState,
  TranscriptItem,
  TurnState,
} from './index';

/**
 * An item this build cannot render: a kind from a newer companion, or a known
 * kind without its body. Kept so order, ids and parents stay intact; never
 * rendered.
 */
export interface UnknownTranscriptItem {
  id: string;
  ts: number;
  turnId?: string;
  parentId?: string;
  kind: 'unknown';
  /** The kind the companion sent. */
  originalKind: string;
}

export type TranscriptViewItem = TranscriptItem | UnknownTranscriptItem;

export interface TranscriptView {
  /** '' until the first snapshot arrives: render a loading state. */
  epoch: string;
  /** Last applied op seq: the resume cursor (`session_subscribe.afterSeq`). */
  seq: number;
  /** Render order, oldest first. */
  items: TranscriptViewItem[];
  /** Item id → position in `items`. */
  index: ReadonlyMap<string, number>;
  /** Turn id → state (turn footers: duration, usage, cost, error). */
  turns: ReadonlyMap<string, TurnState>;
  state: LiveState;
  mcp: McpServerState[];
  /** Older items exist only on the desktop. */
  truncatedBefore: boolean;
}

export function emptyTranscriptView(): TranscriptView {
  return {
    epoch: '',
    seq: 0,
    items: [],
    index: new Map(),
    turns: new Map(),
    state: { status: 'idle' },
    mcp: [],
    truncatedBefore: false,
  };
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Known, well-formed kinds pass through as-is; anything else is kept flagged. */
function toViewItem(raw: unknown): TranscriptViewItem | null {
  if (!isObj(raw) || typeof raw.id !== 'string' || raw.id === '') return null;
  const renderable =
    raw.kind === 'user' || raw.kind === 'text' || raw.kind === 'thinking' || raw.kind === 'notice'
      ? typeof raw.text === 'string'
      : raw.kind === 'tool'
        ? isObj(raw.tool) && typeof raw.tool.name === 'string'
        : raw.kind === 'plan' && Array.isArray(raw.steps);
  if (renderable) return raw as unknown as TranscriptItem;
  return {
    ...raw,
    id: raw.id,
    ts: typeof raw.ts === 'number' ? raw.ts : 0,
    kind: 'unknown',
    originalKind: String(raw.kind),
  };
}

/** Insert (appended in arrival order) or replace in place by id. */
function put(items: TranscriptViewItem[], index: Map<string, number>, item: TranscriptViewItem) {
  const at = index.get(item.id);
  if (at === undefined) {
    index.set(item.id, items.length);
    items.push(item);
  } else {
    items[at] = item;
  }
}

/** A streamed delta grows `text` (user/text/thinking/notice) or a tool's input/output. */
function grow(item: TranscriptViewItem, field: string, text: string): TranscriptViewItem | null {
  switch (item.kind) {
    case 'user':
    case 'text':
    case 'thinking':
    case 'notice':
      return field === 'text' ? { ...item, text: item.text + text } : null;
    case 'tool':
      if (field === 'input') return { ...item, tool: { ...item.tool, input: (item.tool.input ?? '') + text } };
      if (field === 'output') return { ...item, tool: { ...item.tool, output: (item.tool.output ?? '') + text } };
      return null;
    default:
      return null; // plans and unknown items take no deltas
  }
}

/**
 * Rules (ADR-015 §3.2): a snapshot replaces the view; a batch from another
 * epoch is not applied and asks to resubscribe; ops at or below `view.seq` are
 * duplicates and skipped; the first remaining op must be `view.seq + 1`, else
 * resubscribe; `append` to an unknown item id is ignored (trimmed out of a
 * snapshot); unknown item kinds are kept but flagged. Pure: never mutates
 * `view` or `msg`.
 */
export function applyTranscript(
  view: TranscriptView,
  msg: LiveServerMessage,
): { view: TranscriptView; resubscribe: boolean } {
  if (msg.type === 'transcript_snapshot') {
    const items: TranscriptViewItem[] = [];
    const index = new Map<string, number>();
    for (const raw of msg.items) {
      const item = toViewItem(raw);
      if (item) put(items, index, item);
    }
    const turns = new Map<string, TurnState>();
    for (const t of msg.turns) if (isObj(t) && typeof t.id === 'string') turns.set(t.id, t);
    return {
      view: {
        epoch: msg.epoch,
        seq: msg.seq,
        items,
        index,
        turns,
        state: msg.state,
        mcp: msg.mcp,
        truncatedBefore: msg.truncatedBefore,
      },
      resubscribe: false,
    };
  }

  if (msg.epoch !== view.epoch) return { view, resubscribe: true };
  let seq = view.seq;
  const fresh = msg.ops.filter((op) => op.seq > seq);
  if (fresh.length === 0) return { view, resubscribe: false };
  if (fresh[0].seq !== seq + 1) return { view, resubscribe: true };

  const items = view.items.slice();
  const index = new Map(view.index);
  const turns = new Map(view.turns);
  let { state, mcp } = view;
  let resubscribe = false;
  for (const op of fresh) {
    if (op.seq <= seq) continue; // duplicate inside the batch
    if (op.seq !== seq + 1) {
      // gap inside the batch: keep what applied, fetch the rest
      resubscribe = true;
      break;
    }
    seq = op.seq;
    switch (op.op) {
      case 'item': {
        const item = toViewItem(op.item);
        if (item) put(items, index, item);
        break;
      }
      case 'append': {
        const at = index.get(op.id);
        if (at !== undefined && typeof op.text === 'string') {
          const next = grow(items[at], op.field, op.text);
          if (next) items[at] = next;
        }
        break;
      }
      case 'turn':
        if (isObj(op.turn) && typeof op.turn.id === 'string') turns.set(op.turn.id, op.turn);
        break;
      case 'state':
        if (isObj(op.state)) state = op.state;
        break;
      case 'mcp':
        if (Array.isArray(op.servers)) mcp = op.servers;
        break;
      default:
        break; // an op kind from a newer companion: its seq still counts
    }
  }
  return { view: { ...view, seq, items, index, turns, state, mcp }, resubscribe };
}
