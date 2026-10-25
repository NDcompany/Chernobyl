#!/usr/bin/env node
// make-logo-assets.mjs — every logo file in the repo, regenerated from the one
// master the owner designed: design/Chernobyl Logo.png (a white rounded tile
// carrying the black mark, on transparency).
//
//   node scripts/brand/make-logo-assets.mjs
//
// The mark and the tile are separated by alpha and luminance, so each output
// can recolour them (white tile on the black splash, a black tile for dark
// pages, a pure-alpha silhouette for Android's themed and status-bar icons).
// Bitmaps are box-filtered straight from the master (exact area averages, so
// small sizes stay crisp); the vector mark is traced once (marching squares on
// the antialiased mask, then Douglas-Peucker) and reused by the app's
// LogoMark, the Android status-bar glyph and the SVG files.
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';

const require = createRequire(join(process.cwd(), 'package.json'));
const { PNG } = require('pngjs');

const SRC = 'design/Chernobyl Logo.png';
const BLACK = [5, 5, 5]; // colors.black in the app (#050505)
const WHITE = [255, 255, 255];

const src = PNG.sync.read(readFileSync(SRC));
const W = src.width;
const H = src.height;

/* masks in [0, 1]: the tile is the alpha; the mark is the dark ink on it.
   The master's flat areas carry a few levels of noise; clamping the ends
   keeps the antialiased edges and makes the outputs flat (and small). */
const clean = (m) => Math.min(Math.max((m - 0.04) / 0.92, 0), 1);
const tile = new Float64Array(W * H);
const mark = new Float64Array(W * H);
for (let i = 0; i < W * H; i++) {
  const [r, g, b, a] = src.data.subarray(i * 4, i * 4 + 4);
  const lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
  tile[i] = clean(a / 255);
  mark[i] = clean((a / 255) * (1 - lum));
}

function bbox(mask) {
  let x0 = W, y0 = H, x1 = -1, y1 = -1;
  for (let y = 0; y < H; y++) {
    for (let x = 0; x < W; x++) {
      if (mask[y * W + x] < 0.5) continue;
      if (x < x0) x0 = x;
      if (x > x1) x1 = x;
      if (y < y0) y0 = y;
      if (y > y1) y1 = y;
    }
  }
  const size = Math.max(x1 - x0 + 1, y1 - y0 + 1);
  return { cx: (x0 + x1 + 1) / 2, cy: (y0 + y1 + 1) / 2, size };
}
const markBox = bbox(mark);
const tileBox = bbox(tile);
/** the designer's proportion: mark width over tile width */
const MARK_IN_TILE = markBox.size / tileBox.size;

/* ---------------- exact box-filter resampling via summed-area tables ---------------- */

function sat(mask) {
  const S = new Float64Array((W + 1) * (H + 1));
  for (let y = 0; y < H; y++) {
    let row = 0;
    for (let x = 0; x < W; x++) {
      row += mask[y * W + x];
      S[(y + 1) * (W + 1) + x + 1] = S[y * (W + 1) + x + 1] + row;
    }
  }
  return S;
}
const SAT = { tile: sat(tile), mark: sat(mark) };

/** S at a fractional point: the SAT of a piecewise-constant image is
 *  piecewise bilinear, so this is the exact area integral. */
function satAt(S, x, y) {
  x = Math.min(Math.max(x, 0), W);
  y = Math.min(Math.max(y, 0), H);
  const xi = Math.min(Math.floor(x), W - 1);
  const yi = Math.min(Math.floor(y), H - 1);
  const fx = x - xi;
  const fy = y - yi;
  const at = (a, b) => S[b * (W + 1) + a];
  return (
    at(xi, yi) * (1 - fx) * (1 - fy) +
    at(xi + 1, yi) * fx * (1 - fy) +
    at(xi, yi + 1) * (1 - fx) * fy +
    at(xi + 1, yi + 1) * fx * fy
  );
}

/**
 * Renders `layers` onto a square canvas of `size` px. Each layer is a mask
 * ('tile' | 'mark') in a colour; `box` is the master's bounding box of the
 * layer group, drawn `scale` of the canvas wide and centred.
 */
function render(size, { background = null, layers, box, scale }) {
  const out = new PNG({ width: size, height: size });
  const k = box.size / (size * scale); // master px per output px
  const ox = box.cx - (size / 2) * k;
  const oy = box.cy - (size / 2) * k;
  for (let v = 0; v < size; v++) {
    for (let u = 0; u < size; u++) {
      let [r, g, b, a] = background ? [...background, 1] : [0, 0, 0, 0];
      const x0 = ox + u * k, y0 = oy + v * k, x1 = x0 + k, y1 = y0 + k;
      for (const { mask, color } of layers) {
        const S = SAT[mask];
        const cover = (satAt(S, x1, y1) - satAt(S, x0, y1) - satAt(S, x1, y0) + satAt(S, x0, y0)) / (k * k);
        const la = Math.min(Math.max(cover, 0), 1);
        // "over" in straight alpha
        const na = la + a * (1 - la);
        if (na > 0) {
          r = (color[0] * la + r * a * (1 - la)) / na;
          g = (color[1] * la + g * a * (1 - la)) / na;
          b = (color[2] * la + b * a * (1 - la)) / na;
        }
        a = na;
      }
      const i = (v * size + u) * 4;
      const alpha = Math.round(a * 255);
      // fully transparent pixels get zero colour: stray colour under alpha 0
      // costs compression and nothing else
      out.data[i] = alpha ? Math.round(r) : 0;
      out.data[i + 1] = alpha ? Math.round(g) : 0;
      out.data[i + 2] = alpha ? Math.round(b) : 0;
      out.data[i + 3] = alpha;
    }
  }
  return PNG.sync.write(out, { colorType: 6 });
}

const tileLayers = (tileColor, markColor) => [
  { mask: 'tile', color: tileColor },
  { mask: 'mark', color: markColor },
];
const markLayer = (color) => [{ mask: 'mark', color }];

/* ---------------- vector trace of the mark ---------------- */

function traceMark() {
  const T = 0.5;
  const v = (x, y) => (x < 0 || y < 0 || x >= W || y >= H ? 0 : mark[y * W + x]);
  // marching squares on pixel centres; segment endpoints sit on cell edges,
  // interpolated where the antialiased mask crosses T
  const hEdge = (x, y) => [x + (T - v(x, y)) / (v(x + 1, y) - v(x, y)), y];
  const vEdge = (x, y) => [x, y + (T - v(x, y)) / (v(x, y + 1) - v(x, y))];
  const keyH = (x, y) => `h${x},${y}`;
  const keyV = (x, y) => `v${x},${y}`;
  const next = new Map(); // edge key -> [endpoint key, point]
  const pts = new Map();
  const seg = (ka, pa, kb, pb) => {
    pts.set(ka, pa);
    pts.set(kb, pb);
    next.set(ka, kb);
  };
  for (let y = -1; y < H; y++) {
    for (let x = -1; x < W; x++) {
      const a = v(x, y) >= T, b = v(x + 1, y) >= T, c = v(x + 1, y + 1) >= T, d = v(x, y + 1) >= T;
      const code = (a ? 8 : 0) | (b ? 4 : 0) | (c ? 2 : 0) | (d ? 1 : 0);
      if (code === 0 || code === 15) continue;
      const top = () => [keyH(x, y), hEdge(x, y)];
      const right = () => [keyV(x + 1, y), vEdge(x + 1, y)];
      const bottom = () => [keyH(x, y + 1), hEdge(x, y + 1)];
      const left = () => [keyV(x, y), vEdge(x, y)];
      // oriented so ink is on the left of each segment (outer loops CCW in
      // screen space); even-odd fill makes orientation irrelevant anyway
      const S = (p, q) => seg(p[0], p[1], q[0], q[1]);
      const centre = (v(x, y) + v(x + 1, y) + v(x + 1, y + 1) + v(x, y + 1)) / 4 >= T;
      switch (code) {
        case 1: S(left(), bottom()); break;
        case 2: S(bottom(), right()); break;
        case 3: S(left(), right()); break;
        case 4: S(right(), top()); break;
        case 5: if (centre) { S(left(), top()); S(right(), bottom()); } else { S(left(), bottom()); S(right(), top()); } break;
        case 6: S(bottom(), top()); break;
        case 7: S(left(), top()); break;
        case 8: S(top(), left()); break;
        case 9: S(top(), bottom()); break;
        case 10: if (centre) { S(top(), right()); S(bottom(), left()); } else { S(top(), left()); S(bottom(), right()); } break;
        case 11: S(top(), right()); break;
        case 12: S(right(), left()); break;
        case 13: S(right(), bottom()); break;
        case 14: S(bottom(), left()); break;
      }
    }
  }
  const loops = [];
  const seen = new Set();
  for (const start of next.keys()) {
    if (seen.has(start)) continue;
    const loop = [];
    let k = start;
    while (k !== undefined && !seen.has(k)) {
      seen.add(k);
      loop.push(pts.get(k));
      k = next.get(k);
    }
    if (loop.length > 8) loops.push(loop);
  }
  return loops;
}

/** Douglas-Peucker on a closed loop (split at the two farthest points). */
function simplify(loop, eps) {
  const d2 = (p, a, b) => {
    const dx = b[0] - a[0], dy = b[1] - a[1];
    const l = dx * dx + dy * dy;
    const t = l ? Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / l)) : 0;
    const x = a[0] + t * dx - p[0], y = a[1] + t * dy - p[1];
    return x * x + y * y;
  };
  const dp = (pts) => {
    const keep = new Uint8Array(pts.length);
    keep[0] = keep[pts.length - 1] = 1;
    const stack = [[0, pts.length - 1]];
    while (stack.length) {
      const [i, j] = stack.pop();
      let best = -1, idx = -1;
      for (let m = i + 1; m < j; m++) {
        const dd = d2(pts[m], pts[i], pts[j]);
        if (dd > best) { best = dd; idx = m; }
      }
      if (best > eps * eps) {
        keep[idx] = 1;
        stack.push([i, idx], [idx, j]);
      }
    }
    return pts.filter((_, m) => keep[m]);
  };
  let far = 0, fd = -1;
  for (let m = 1; m < loop.length; m++) {
    const dd = (loop[m][0] - loop[0][0]) ** 2 + (loop[m][1] - loop[0][1]) ** 2;
    if (dd > fd) { fd = dd; far = m; }
  }
  const a = dp(loop.slice(0, far + 1));
  const b = dp([...loop.slice(far), loop[0]]);
  return [...a.slice(0, -1), ...b.slice(0, -1)];
}

/** Path data for the traced mark: its bounding square mapped onto
 *  [pad, view - pad] in a `view`-unit square. */
function markPath(loops, view, pad) {
  const k = (view - 2 * pad) / markBox.size;
  const ox = markBox.cx - (view / 2) / k;
  const oy = markBox.cy - (view / 2) / k;
  const f = (n) => (Math.round(n * 100) / 100).toString();
  return loops
    .map((loop) => 'M' + loop.map(([x, y]) => `${f((x + 0.5 - ox) * k)} ${f((y + 0.5 - oy) * k)}`).join('L') + 'Z')
    .join('');
}

/* ---------------- ICO (PNG-compressed entries, Vista+) ---------------- */

function ico(images) {
  const head = Buffer.alloc(6 + 16 * images.length);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(images.length, 4);
  let offset = head.length;
  images.forEach(({ size, png }, n) => {
    const e = 6 + n * 16;
    head.writeUInt8(size >= 256 ? 0 : size, e);
    head.writeUInt8(size >= 256 ? 0 : size, e + 1);
    head.writeUInt16LE(1, e + 4); // planes
    head.writeUInt16LE(32, e + 6); // bpp
    head.writeUInt32LE(png.length, e + 8);
    head.writeUInt32LE(offset, e + 12);
    offset += png.length;
  });
  return Buffer.concat([head, ...images.map((i) => i.png)]);
}

/* ---------------- outputs ---------------- */

const write = (path, data) => {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
  console.log(`  ${path} (${(data.length / 1024).toFixed(1)} KB)`);
};

console.log(`master ${W}x${H}: mark ${markBox.size}px in a ${tileBox.size}px tile (${(MARK_IN_TILE * 100).toFixed(1)}%)`);

// Expo app (apps/mobile/app.json)
const A = 'apps/mobile/assets';
// iOS masks a full-bleed square itself: white canvas, mark at the tile's proportion
write(`${A}/icon.png`, render(1024, { background: WHITE, layers: markLayer(BLACK), box: markBox, scale: MARK_IN_TILE }));
// adaptive icon: 108dp canvas, 72dp visible; the mark keeps its proportion
// of the visible area and stays inside the 66dp safe circle
const adaptive = MARK_IN_TILE * (72 / 108);
write(`${A}/android-icon-foreground.png`, render(1024, { layers: markLayer(BLACK), box: markBox, scale: adaptive }));
write(`${A}/android-icon-monochrome.png`, render(1024, { layers: markLayer(WHITE), box: markBox, scale: adaptive }));
write(`${A}/android-icon-background.png`, render(1024, { background: WHITE, layers: [], box: markBox, scale: 1 }));
// splash: the tile as designed, on the near-black splash; Android 12+ shows
// the splash icon inside a circle of 2/3 the canvas, so the tile takes half
write(`${A}/splash-icon.png`, render(1024, { layers: tileLayers(WHITE, BLACK), box: tileBox, scale: 0.5 }));
write(`${A}/favicon.png`, render(64, { layers: tileLayers(WHITE, BLACK), box: tileBox, scale: 60 / 64 }));

// design kit (README, docs, store listings)
const D = 'design/logo';
for (const size of [1024, 256]) {
  write(`${D}/chernobyl-tile-${size}.png`, render(size, { layers: tileLayers(WHITE, BLACK), box: tileBox, scale: 0.96 }));
  write(`${D}/chernobyl-tile-dark-${size}.png`, render(size, { layers: tileLayers(BLACK, WHITE), box: tileBox, scale: 0.96 }));
  write(`${D}/chernobyl-mark-${size}.png`, render(size, { layers: markLayer(BLACK), box: markBox, scale: 0.96 }));
  write(`${D}/chernobyl-mark-white-${size}.png`, render(size, { layers: markLayer(WHITE), box: markBox, scale: 0.96 }));
}

// desktop companion: Explorer, taskbar and shortcuts (embedded by build.rs)
const icoSizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];
write(
  'services/companion-rs/assets/chernobyl.ico',
  ico(icoSizes.map((size) => ({ size, png: render(size, { layers: tileLayers(WHITE, BLACK), box: tileBox, scale: size <= 32 ? 1 : 0.96 }) }))),
);

// vector mark
const loops = traceMark().map((l) => simplify(l, 0.75));
const d100 = markPath(loops, 100, 0);
const points = loops.reduce((n, l) => n + l.length, 0);
console.log(`traced ${loops.length} contours, ${points} points`);
write(
  'apps/mobile/src/ui/logoPath.ts',
  Buffer.from(
    `// Generated by scripts/brand/make-logo-assets.mjs from design/Chernobyl Logo.png.\n` +
      `// Do not edit by hand: rerun the script after changing the master.\n\n` +
      `/** The CHERNOBYL mark traced to one path in a 100 x 100 box (even-odd fill). */\n` +
      `export const LOGO_MARK_PATH =\n  '${d100}';\n`,
  ),
);
// Android status-bar glyph: alpha only, mark in the central 20 of 24dp
write(
  'apps/mobile/modules/connection-service/android/src/main/res/drawable/ic_stat_chernobyl.xml',
  Buffer.from(
    `<!-- Status-bar icon for every CHERNOBYL notification: the app mark, traced from\n` +
      `     design/Chernobyl Logo.png by scripts/brand/make-logo-assets.mjs (do not edit by hand).\n` +
      `     Android draws only the alpha channel of a small icon. 24dp with the mark in the central 20dp. -->\n` +
      `<vector xmlns:android="http://schemas.android.com/apk/res/android"\n` +
      `  android:width="24dp"\n  android:height="24dp"\n  android:viewportWidth="120"\n  android:viewportHeight="120">\n` +
      `  <path\n    android:fillColor="#FFFFFFFF"\n    android:fillType="evenOdd"\n    android:pathData="${markPath(loops, 120, 10)}" />\n</vector>\n`,
  ),
);
const tilePath = (() => {
  // the tile outline: a rounded square traced the same way would be overkill;
  // its master is a superellipse-like squircle, approximated by a 22% radius
  const r = 22;
  return `M${r} 0H${100 - r}C${100 - r * 0.45} 0 100 ${r * 0.45} 100 ${r}V${100 - r}C100 ${100 - r * 0.45} ${100 - r * 0.45} 100 ${100 - r} 100H${r}C${r * 0.45} 100 0 ${100 - r * 0.45} 0 ${100 - r}V${r}C0 ${r * 0.45} ${r * 0.45} 0 ${r} 0Z`;
})();
write(
  `${D}/chernobyl-mark.svg`,
  Buffer.from(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill="#050505" fill-rule="evenodd" d="${d100}"/></svg>\n`),
);
const inner = markPath(loops, 100, 50 * (1 - MARK_IN_TILE));
write(
  `${D}/chernobyl-tile.svg`,
  Buffer.from(
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill="#fff" d="${tilePath}"/><path fill="#050505" fill-rule="evenodd" d="${inner}"/></svg>\n`,
  ),
);
write(
  `${D}/chernobyl-tile-dark.svg`,
  Buffer.from(
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill="#050505" d="${tilePath}"/><path fill="#fff" fill-rule="evenodd" d="${inner}"/></svg>\n`,
  ),
);
