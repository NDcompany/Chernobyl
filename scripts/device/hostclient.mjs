// hostclient.mjs — a second client on the PC side of a device run, used to
// race the phone (stale approvals) and to probe server-side rules (auth
// cooldown, malformed frames). Loopback plaintext, like the dev tools.
import { WebSocket } from 'ws';

export function connect(url, { headers } = {}) {
  return new Promise((resolve) => {
    const ws = new WebSocket(url, { headers });
    const inbox = [];
    const waiters = [];
    let closed = null;
    ws.on('message', (d) => {
      const t = d.toString();
      inbox.push(t);
      for (const w of [...waiters]) if (w.pred(t)) { waiters.splice(waiters.indexOf(w), 1); w.res(t); }
    });
    ws.on('close', (code) => { closed = code; for (const w of waiters.splice(0)) w.res(null); resolve(api); });
    ws.on('error', () => {});
    ws.on('unexpected-response', (_req, res) => { closed = res.statusCode; resolve(api); });
    ws.on('open', () => resolve(api));
    const api = {
      ws,
      get closed() { return closed; },
      send: (o) => ws.send(typeof o === 'string' ? o : JSON.stringify(o)),
      /** First message matching `pred`, already received or arriving within
       *  `ms`; `fresh` skips what already arrived (polling replies like `state`
       *  carry no commandId). */
      next(pred, ms = 10_000, fresh = false) {
        const hit = fresh ? null : inbox.find(pred);
        if (hit) return Promise.resolve(hit);
        return new Promise((res) => {
          const w = { pred, res };
          waiters.push(w);
          setTimeout(() => { const i = waiters.indexOf(w); if (i >= 0) { waiters.splice(i, 1); res(null); } }, ms);
        });
      },
      close: () => ws.close(),
    };
  });
}
