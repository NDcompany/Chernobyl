#!/usr/bin/env node
// clean-test-tmp.mjs — removes the temp dirs that test runs leave behind (`chb-*`, `chernobyl-*`
// in the OS temp dir). Runs at the start of the verify gates.
//
//   node scripts/clean-test-tmp.mjs          entries older than 1 hour (a concurrent run keeps its own)
//   node scripts/clean-test-tmp.mjs --all    everything, including the device-run fixtures
//
// ponytail: tests still leak per run; this bounds it to the last hour instead of fixing each helper.
// A symlink or junction is unlinked, never followed.
import { lstatSync, readdirSync, rmdirSync, unlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const all = process.argv.includes('--all');
// device-run fixtures (scripts/device/*) are reused across runs and may be in use for hours
const KEEP = /^chernobyl-(live-|device-run|kitkat-run)/;
const cutoff = Date.now() - 60 * 60 * 1000;

function rm(p) {
  const st = lstatSync(p);
  if (st.isSymbolicLink()) {
    try { rmdirSync(p); } catch { unlinkSync(p); } // a junction is a directory link: rmdir removes the link only
  } else if (st.isDirectory()) {
    for (const n of readdirSync(p)) rm(join(p, n));
    rmdirSync(p);
  } else unlinkSync(p);
}

const dir = tmpdir();
let removed = 0, failed = 0;
for (const name of readdirSync(dir)) {
  if (!/^(chb-|chernobyl-)/.test(name) || (!all && KEEP.test(name))) continue;
  const p = join(dir, name);
  try {
    if (!all && lstatSync(p).mtimeMs > cutoff) continue;
    rm(p);
    removed++;
  } catch {
    failed++; // in use or already gone: the next run gets it
  }
}
console.log(`clean-test-tmp: removed ${removed} entr${removed === 1 ? 'y' : 'ies'} from ${dir}${failed ? `, ${failed} skipped (in use)` : ''}`);
