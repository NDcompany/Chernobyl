#!/usr/bin/env node
// package-release.mjs — ADR-007 packaging: unsigned binary + published SHA-256
// checksums + a documented verify step.
//
//   node scripts/package-release.mjs              build + emit SHA256SUMS.txt
//   node scripts/package-release.mjs --verify     re-hash artifacts, check sums
//
// ADR-007 ships the companion as an UNSIGNED binary (no code-signing cert yet)
// with published SHA-256 checksums. This script produces those checksums and
// the operator's verify step. It does NOT sign anything — that limitation is
// documented, not silently papered over.
//
// Artifacts (when present, each is hashed):
//   services/companion-rs/target/release/chernobyl-companion[.exe]
//   apps/mobile/dist-android/*.apk
//
// Checksums land in `release/SHA256SUMS.txt` in the standard `<hex>  <path>`
// format (verifiable with `sha256sum -c` on Linux/macOS or this script's
// --verify on any platform).

import { createHash } from 'node:crypto';
import { createReadStream, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const releaseDir = join(root, 'release');
const sumsPath = join(releaseDir, 'SHA256SUMS.txt');

/** Candidate artifacts, in the order they are hashed. Missing ones are skipped
 *  (and reported) rather than fabricated — packaging must never invent a sum. */
function candidates() {
  const exe = process.platform === 'win32' ? '.exe' : '';
  const list = [
    join(root, 'services/companion-rs/target/release', `chernobyl-companion${exe}`),
  ];
  const apkDir = join(root, 'apps/mobile/dist-android');
  if (existsSync(apkDir)) {
    for (const f of readdirSync(apkDir)) {
      if (f.endsWith('.apk')) list.push(join(apkDir, f));
    }
  }
  return list.filter(existsSync);
}

function sha256File(path) {
  return new Promise((res, rej) => {
    const h = createHash('sha256');
    createReadStream(path)
      .on('data', (c) => h.update(c))
      .on('end', () => res(h.digest('hex')))
      .on('error', rej);
  });
}

async function build() {
  const files = candidates();
  if (files.length === 0) {
    console.error('no artifacts found — run `cargo build --release` first');
    process.exit(1);
  }
  mkdirSync(releaseDir, { recursive: true });
  const lines = [];
  for (const f of files) {
    const hex = await sha256File(f);
    const rel = relative(root, f).split('\\').join('/');
    lines.push(`${hex}  ${rel}`);
    console.log(`${hex}  ${rel}`);
  }
  writeFileSync(sumsPath, lines.join('\n') + '\n');
  console.log(`\nwrote ${relative(root, sumsPath)} (${lines.length} artifact(s))`);
  console.log('NOTE (ADR-007): these are UNSIGNED binaries — checksums prove integrity, not authorship.');
}

async function verify() {
  if (!existsSync(sumsPath)) {
    console.error(`no ${relative(root, sumsPath)} — run this script without --verify first`);
    process.exit(1);
  }
  const lines = readFileSync(sumsPath, 'utf8').split('\n').filter(Boolean);
  let bad = 0;
  for (const line of lines) {
    const [want, rel] = line.split(/\s+/);
    const abs = resolve(root, rel);
    if (!existsSync(abs)) {
      console.error(`MISSING  ${rel}`);
      bad++;
      continue;
    }
    const got = await sha256File(abs);
    if (got === want) {
      console.log(`OK       ${rel}`);
    } else {
      console.error(`MISMATCH ${rel}\n  want ${want}\n  got  ${got}`);
      bad++;
    }
  }
  if (bad > 0) {
    console.error(`\n${bad} artifact(s) failed verification`);
    process.exit(1);
  }
  console.log(`\nall ${lines.length} artifact(s) match their published SHA-256`);
}

const mode = process.argv.includes('--verify') ? verify : build;
mode().catch((e) => {
  console.error(e);
  process.exit(1);
});
