// adb.mjs — minimal Android UI driver for device/emulator validation runs.
// Finds views through `uiautomator dump` (text or content-desc, which is where
// React Native puts accessibilityLabel), taps their centers, types text and
// captures screenshots. No test framework: the checklist runner composes these.
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';

const ADB = process.env.ADB ?? 'G:/Android/sdk/platform-tools/adb.exe';
// with several devices attached, every call names one (ANDROID_SERIAL=emulator-5554)
const SERIAL = process.env.ANDROID_SERIAL ? ['-s', process.env.ANDROID_SERIAL] : [];

export function adb(args, opts = {}) {
  return execFileSync(ADB, [...SERIAL, ...args], { encoding: opts.binary ? 'buffer' : 'utf8', maxBuffer: 64 * 1024 * 1024, timeout: opts.timeout ?? 60_000 });
}
export const shell = (cmd) => adb(['shell', cmd]);
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Every node of the current window: { text, desc, id, cls, pkg, clickable, x, y } (x,y = center). */
export function dump() {
  let xml = '';
  for (let i = 0; i < 3 && !xml.includes('<hierarchy'); i++) {
    try {
      // a failed dump (no idle UI) leaves the previous file: never read a stale tree
      shell('rm -f /sdcard/ui.xml; uiautomator dump /sdcard/ui.xml >/dev/null 2>&1');
      xml = shell('cat /sdcard/ui.xml 2>/dev/null');
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
      pkg: a.package ?? '',
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
export function type(text, chunkSize = 120) {
  for (let i = 0; i < text.length; i += chunkSize) {
    const chunk = text.slice(i, i + chunkSize).replace(/ /g, '%s').replace(/'/g, `'\\''`);
    shell(`input text '${chunk}'`);
  }
}

/**
 * Types into the focused field and checks what landed: a busy device drops
 * `input text` characters, which once corrupted a pairing key mid-run. Retries
 * with smaller, paced chunks after clearing the field.
 */
export async function typeVerified(text, attempts = 3) {
  for (let n = 0; n < attempts; n++) {
    if (n > 0) {
      // select all + delete (two events; one DEL per char timed out adb)
      shell('input keycombination KEYCODE_CTRL_LEFT KEYCODE_A');
      key('KEYCODE_DEL');
      await sleep(300);
    }
    const size = n === 0 ? 120 : 24;
    for (let i = 0; i < text.length; i += size) {
      type(text.slice(i, i + size), size);
      if (n > 0) await sleep(250);
    }
    await sleep(500);
    const field = dump().find((x) => x.cls.includes('EditText') && x.text && text.startsWith(x.text.slice(0, 8)));
    if (field && field.text === text) return true;
  }
  throw new Error('typed text did not land intact after retries');
}

export function key(code) {
  shell(`input keyevent ${code}`);
}

export function screenshot(path) {
  writeFileSync(path, adb(['exec-out', 'screencap', '-p'], { binary: true }));
}
