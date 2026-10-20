/**
 * Test-only helpers (no production imports).
 */
import { rmSync } from 'node:fs';

/**
 * Windows temp dirs occasionally hit transient EPERM during recursive
 * deletion (indexing/AV holding a handle) — retry briefly, then give up
 * with a warning. Never throws: a cleanup failure must not mask the real
 * test assertion that preceded it.
 */
export function forceRemove(path: string): void {
  for (let i = 0; i < 5; i++) {
    try {
      rmSync(path, { recursive: true, force: true });
      return;
    } catch {
      // busy wait a touch — Atomics.wait keeps it synchronous and cheap
      const sab = new SharedArrayBuffer(4);
      Atomics.wait(new Int32Array(sab), 0, 0, 50);
    }
  }
  process.stderr.write(`warning: could not remove temp dir ${path}\n`);
}
