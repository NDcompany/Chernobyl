// TS engine microbenchmark — the symmetric pair of the Rust
// `microbench_engine_stale_decide` ignored test, so engine-vs-engine can be
// compared without wire/persistence noise.
//
// Usage: node engine-bench.mjs [iterations]
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { MockEngine } = require('../../../packages/protocol/dist/engine.js');

const N = Number(process.argv[2] ?? 1_000_000);
const events = [];
const engine = new MockEngine((m) => events.push(m));
const permission = engine
  .snapshot()
  .permissions.find((p) => p.status === 'pending');
if (!permission) throw new Error('no seeded pending permission');

const stale = () => ({
  type: 'permission_decision',
  permissionId: permission.id,
  decision: 'reject',
  revision: permission.revision + 1,
});

const first = engine.handle({ ...stale(), commandId: 'mb-0' });
if (first?.ok !== false) throw new Error(`stale must refuse: ${JSON.stringify(first)}`);

const t0 = process.hrtime.bigint();
for (let i = 1; i < N; i++) {
  engine.handle({ ...stale(), commandId: `mb-${i}` });
}
const ms = Number(process.hrtime.bigint() - t0) / 1e6;
if (events.length !== 0) throw new Error(`stale refusals must not emit events: ${events.length}`);

console.log(
  `ts engine stale decide (validate+find+refuse+ack-cache, unique ids): ` +
    `${Math.round((ms * 1e6) / N)} ns/op (${N} ops in ${ms.toFixed(0)} ms)`,
);
