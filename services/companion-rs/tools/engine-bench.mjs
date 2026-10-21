// TS engine microbenchmark — the symmetric pair of the Rust in-crate
// `#[ignore]` microbenches, so engine/engine and full-path/full-path can be
// compared without wire/persistence noise.
//
// Usage: node engine-bench.mjs [engine|full] [iterations]
//   engine — engine.handle only (Rust: microbench_engine_stale_decide)
//   full   — JSON.parse + validate + handle + JSON.stringify(ack),
//            the whole in-process path minus sockets and storage
//            (Rust: microbench_parse_validate_handle_encode)
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { MockEngine } = require('../../../packages/protocol/dist/engine.js');
const { parseClientMessage } = require('../../../packages/protocol/dist/index.js');

const mode = process.argv[2] === 'full' ? 'full' : 'engine';
const N = Number(process.argv[3] ?? 1_000_000);
const events = [];
const engine = new MockEngine((m) => events.push(m));
const permission = engine
  .snapshot()
  .permissions.find((p) => p.status === 'pending');
if (!permission) throw new Error('no seeded pending permission');

const staleDecision = (id) => ({
  type: 'permission_decision',
  permissionId: permission.id,
  decision: 'reject',
  revision: permission.revision + 1, // stale → refused every time
});

const first = engine.handle({ ...staleDecision('mb-0'), commandId: 'mb-0' });
if (first?.ok !== false) throw new Error(`stale must refuse: ${JSON.stringify(first)}`);

let t0;
if (mode === 'engine') {
  t0 = process.hrtime.bigint();
  for (let i = 1; i < N; i++) {
    engine.handle({ ...staleDecision(`mb-${i}`), commandId: `mb-${i}` });
  }
} else {
  // prebuild raw frames so the timed loop measures the server path only
  const raws = new Array(N);
  for (let i = 0; i < N; i++) {
    raws[i] = JSON.stringify({ ...staleDecision(`mb-${i}`), commandId: `mb-${i}` });
  }
  t0 = process.hrtime.bigint();
  for (const raw of raws) {
    const msg = parseClientMessage(raw);
    if (!msg) throw new Error('frame must parse+validate');
    const reply = engine.handle(msg);
    if (!reply) throw new Error('stale decision must ack');
    JSON.stringify(reply);
  }
}
const ms = Number(process.hrtime.bigint() - t0) / 1e6;
if (events.length !== 0) throw new Error(`stale refusals must not emit events: ${events.length}`);

const label =
  mode === 'engine'
    ? 'ts engine stale decide (validate+find+refuse+ack-cache, unique ids)'
    : 'ts in-process full path (parse+validate+handle+encode, no socket/storage)';
console.log(`${label}: ${Math.round((ms * 1e6) / N)} ns/op (${N} ops in ${ms.toFixed(0)} ms)`);
