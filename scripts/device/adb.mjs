// adb.mjs — minimal Android UI driver for device/emulator validation runs.
// Finds views through `uiautomator dump` (text or content-desc, which is where
// React Native puts accessibilityLabel), taps their centers, types text and
// captures screenshots. No test framework: the checklist runner composes these.
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';

const ADB = process.env.ADB ?? 'G:/Android/sdk/platform-tools/adb.exe';

export function adb(args, opts = {}) {
  return execFileSync(ADB, args, { encoding: opts.binary ? 'buffer' : 'utf8', maxBuffer: 64 * 1024 * 1024, timeout: opts.timeout ?? 60_000 });
}
export const shell = (cmd) => adb(['shell', cmd]);
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Every node of the current window: { text, desc, id, cls, clickable, x, y } (x,y = center). */
export function dump() {
  let xml = '';
  for (let i = 0; i < 3 && !xml.includes('<hierarchy'); i++) {
    try {
      shell('uiautomator dump /sdcard/ui.xml >/dev/null 2>&1');
      xml = shell('cat /sdcard/ui.xml');
    } catch {
      /* the dump fails while the screen animates; retry */
    }
  }
  const nodes = [];
  for (const m of xml.matchAll(/<node ([^>]*?)\/?>/g)) {
    const a = Object.fromEntries([...m[1].matchAll(/([\w-]+)="([^"]*)"/g)].map((x) => [x[1], unxml(x[2])]));
    const b = /\[(\d+),(\d+)\]\[(\d+),(\d+)\]/.exec(a.bounds ?? '');
    if (!b) continue;
    nodes.push({
      text: a.text ?? '',
      desc: a['content-desc'] ?? '',
      id: a['resource-id'] ?? '',
      cls: a.class ?? '',
      clickable: a.clickable === 'true',
      x: (Number(b[1]) + Number(b[3])) >> 1,
      y: (Number(b[2]) + Number(b[4])) >> 1,
    });
  }
  return nodes;
}
function unxml(s) {
  return s.replace(/&quot;/g, '"').replace(/&apos;/g, "'").replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&#10;/g, '\n').replace(/&amp;/g, '&');
}

/** First node whose text or content-desc matches (string = substring, or RegExp). */
export function find(match, nodes = dump()) {
  const ok = (s) => (match instanceof RegExp ? match.test(s) : s.includes(match));
  return nodes.find((n) => (n.text && ok(n.text)) || (n.desc && ok(n.desc))) ?? null;
}
export function screenText(nodes = dump()) {
  return nodes.flatMap((n) => [n.text, n.desc]).filter(Boolean).join(' | ');
}

export async function waitFor(match, ms = 15_000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const n = find(match);
    if (n) return n;
    if (Date.now() > deadline) return null;
    await sleep(500);
  }
}

export async function tap(match, ms = 15_000) {
  const n = typeof match === 'object' && match && 'x' in match ? match : await waitFor(match, ms);
  if (!n) throw new Error(`not on screen: ${match}\nscreen: ${screenText().slice(0, 600)}`);
  shell(`input tap ${n.x} ${n.y}`);
  await sleep(600);
  return n;
}

/**
 * Types into the focused field. Each chunk is single-quoted for the device
 * shell (backslash escapes truncated JSON at the first ':'); `input text`
 * still needs spaces as %s.
 */
export function type(text) {
  for (let i = 0; i < text.length; i += 120) {
    const chunk = text.slice(i, i + 120).replace(/ /g, '%s').replace(/'/g, `'\\''`);
    shell(`input text '${chunk}'`);
  }
}

export function key(code) {
  shell(`input keyevent ${code}`);
}

export function screenshot(path) {
  writeFileSync(path, adb(['exec-out', 'screencap', '-p'], { binary: true }));
}
