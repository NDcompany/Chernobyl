/**
 * ADR-017 composer helpers (pure, unit-tested by `npm test -w @chernobyl/mobile`):
 * the slash-command menu filter, the image and frame budgets, the camera
 * picture size and the context meter. Bounds come in as arguments (the
 * protocol constants), so this file imports types only.
 */
import type { LiveState } from '@chernobyl/protocol';

/** The companion reads one WebSocket frame of at most this many bytes (`ws::MAX_PAYLOAD_BYTES`). */
export const FRAME_MAX_BYTES = 256_000;

/**
 * `/com` while the user is still typing the command name: the partial name
 * (may be ''); null once there is a space or the text does not start with `/`.
 */
export function slashQuery(text: string): string | null {
  const m = /^\/(\S*)$/.exec(text);
  return m ? m[1] : null;
}

/** Commands whose name starts with `query` first, then those that contain it; at most `max`. */
export function filterSlashCommands(commands: readonly string[], query: string, max = 8): string[] {
  const q = query.toLowerCase();
  const starts = commands.filter((c) => c.toLowerCase().startsWith(q));
  const contains = q ? commands.filter((c) => !c.toLowerCase().startsWith(q) && c.toLowerCase().includes(q)) : [];
  return [...starts, ...contains].slice(0, max);
}

/** UTF-8 length of a JS string (lone surrogates count as U+FFFD, 3 bytes). */
export function utf8Bytes(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x80) n += 1;
    else if (c < 0x800) n += 2;
    else if (c >= 0xd800 && c < 0xdc00 && i + 1 < s.length && (s.charCodeAt(i + 1) & 0xfc00) === 0xdc00) {
      n += 4;
      i++;
    } else n += 3;
  }
  return n;
}

/**
 * The WebSocket frame a message becomes once sealed (ADR-011: JSON, +16 tag
 * bytes, base64, in `{"n":<counter>,"d":"…"}`), rounded up generously.
 */
export function sealedFrameBytes(msg: unknown): number {
  const plain = utf8Bytes(JSON.stringify(msg));
  return Math.ceil((plain + 16) / 3) * 4 + 64;
}

/** Base64 characters still free for another image. */
export function imageBudgetLeft(images: readonly { data: string }[], totalMax: number): number {
  return Math.max(0, totalMax - images.reduce((n, im) => n + im.data.length, 0));
}

/**
 * The camera size to shoot at: the largest whose long edge is at most
 * `maxEdge`, else the smallest offered. Sizes look like "1280x720".
 */
export function pickPictureSize(sizes: readonly string[], maxEdge: number): string | undefined {
  const parsed = sizes
    .map((s) => {
      const m = /^(\d+)x(\d+)$/.exec(s);
      return m ? { s, edge: Math.max(Number(m[1]), Number(m[2])), area: Number(m[1]) * Number(m[2]) } : null;
    })
    .filter((p): p is { s: string; edge: number; area: number } => p !== null);
  if (!parsed.length) return undefined;
  const fits = parsed.filter((p) => p.edge <= maxEdge).sort((a, b) => b.area - a.area);
  if (fits.length) return fits[0].s;
  return [...parsed].sort((a, b) => a.area - b.area)[0].s;
}

/** Base64 as the camera returns it, minus a `data:` prefix and line breaks. */
export function cleanBase64(data: string): string {
  return data.replace(/^data:[^,]*,/, '').replace(/\s+/g, '');
}

/** Context use as a whole percent (0-100), or undefined when unknown. */
export function contextPercent(context: LiveState['context']): number | undefined {
  if (!context || !(context.maxTokens > 0) || !(context.usedTokens >= 0)) return undefined;
  return Math.min(100, Math.round((context.usedTokens / context.maxTokens) * 100));
}

/** Aliases Claude Code accepts when the CLI listed no models. */
export const FALLBACK_MODELS: readonly { value: string; label: string }[] = [
  { value: 'default', label: 'Default' },
  { value: 'sonnet', label: 'Sonnet' },
  { value: 'opus', label: 'Opus' },
  { value: 'haiku', label: 'Haiku' },
];

/** The models to offer: the CLI's list when it sent one. */
export function modelChoices(state: Pick<LiveState, 'models'>): readonly { value: string; label: string }[] {
  return state.models?.length ? state.models : FALLBACK_MODELS;
}

/**
 * The chip label for the current model: the CLI reports a resolved id
 * (`claude-haiku-5-5`), shown as "Haiku 5.5"; an alias is capitalized.
 */
export function modelLabel(model: string | undefined, choices: readonly { value: string; label: string }[]): string {
  if (!model) return 'Model';
  const known = choices.find((c) => c.value === model);
  if (known) return known.label;
  const m = /^claude-([a-z]+)-(\d+)(?:-(\d{1,2}))?(?:-\d{8})?$/.exec(model);
  if (m) return `${m[1][0].toUpperCase()}${m[1].slice(1)} ${m[2]}${m[3] ? `.${m[3]}` : ''}`;
  return model;
}
