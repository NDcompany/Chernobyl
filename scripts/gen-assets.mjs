/**
 * One-off generator for CHERNOBYL's monochrome app assets.
 * Draws the bracket [·] mark used in the reference design onto black /
 * transparent squares and writes valid PNGs using only node:zlib.
 *
 *   node scripts/gen-assets.mjs
 */

import { deflateSync } from 'node:zlib';
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const outDir = join(root, 'apps/mobile/assets');

/* ---------------- PNG encoding ---------------- */

const crcTable = new Int32Array(256).map((_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c;
});

function crc32(buf) {
  let c = -1;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function writePng(file, size, px) {
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: none
    px.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const png = Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
  writeFileSync(file, png);
  console.log('wrote', file);
}

/* ---------------- drawing ---------------- */

/** @param {'app'|'foreground'|'background'|'monochrome'|'splash'} style */
function draw(size, style) {
  const px = Buffer.alloc(size * size * 4);
  const set = (x, y, r, g, b, a) => {
    if (x < 0 || y < 0 || x >= size || y >= size) return;
    const i = (y * size + x) * 4;
    const na = a / 255;
    const oa = px[i + 3] / 255;
    const out = na + oa * (1 - na);
    if (out === 0) return;
    px[i] = Math.round((r * na + px[i] * oa * (1 - na)) / out);
    px[i + 1] = Math.round((g * na + px[i + 1] * oa * (1 - na)) / out);
    px[i + 2] = Math.round((b * na + px[i + 2] * oa * (1 - na)) / out);
    px[i + 3] = Math.round(out * 255);
  };
  const rect = (x0, y0, x1, y1, color) => {
    for (let y = Math.round(y0); y < Math.round(y1); y++)
      for (let x = Math.round(x0); x < Math.round(x1); x++) set(x, y, ...color);
  };

  const BLACK = [5, 5, 5, 255];
  const WHITE = [255, 255, 255, 255];

  if (style === 'app' || style === 'background') rect(0, 0, size, size, BLACK);
  if (style === 'app') {
    // full-bleed mark for the store/notification icon
    glyph(size, 0.62, rect, WHITE);
  } else if (style === 'foreground') {
    glyph(size, 0.44, rect, WHITE); // inside the adaptive safe zone
  } else if (style === 'monochrome') {
    glyph(size, 0.44, rect, WHITE);
  } else if (style === 'splash') {
    glyph(size, 0.3, rect, WHITE);
  }
  return px;
}

/** Bracket [·] mark: two brackets with a centre dot, box scaled to `span`. */
function glyph(size, span, rect, color) {
  const box = size * span;
  const x0 = (size - box) / 2;
  const y0 = (size - box) / 2;
  const u = box / 100; // unit
  const t = 9 * u; // stroke thickness
  const arm = 30 * u; // bracket arm length

  // left bracket
  rect(x0, y0, x0 + t, y0 + box, color);
  rect(x0, y0, x0 + arm, y0 + t, color);
  rect(x0, y0 + box - t, x0 + arm, y0 + box, color);
  // right bracket
  rect(x0 + box - t, y0, x0 + box, y0 + box, color);
  rect(x0 + box - arm, y0, x0 + box, y0 + t, color);
  rect(x0 + box - arm, y0 + box - t, x0 + box, y0 + box, color);
  // centre dot
  rect(x0 + 45 * u, y0 + 45 * u, x0 + 55 * u, y0 + 55 * u, color);
}

mkdirSync(outDir, { recursive: true });
writePng(join(outDir, 'icon.png'), 1024, draw(1024, 'app'));
writePng(join(outDir, 'android-icon-foreground.png'), 1024, draw(1024, 'foreground'));
writePng(join(outDir, 'android-icon-background.png'), 1024, draw(1024, 'background'));
writePng(join(outDir, 'android-icon-monochrome.png'), 1024, draw(1024, 'monochrome'));
writePng(join(outDir, 'splash-icon.png'), 1024, draw(1024, 'splash'));
writePng(join(outDir, 'favicon.png'), 64, draw(64, 'app'));
