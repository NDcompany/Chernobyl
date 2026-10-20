/**
 * Loopback-only diagnostics API (PRD §15.2): health, version, adapters.
 * Bound to 127.0.0.1 explicitly — never a public interface. Read-only by
 * design; configuration changes go through the config file + restart.
 */
import { createServer, type Server } from 'node:http';
import type { Companion } from './companion';

export interface DiagnosticsHandle {
  /** The actual port (useful when config port is 0). */
  port: number;
  close(): Promise<void>;
}

export function startDiagnostics(companion: Companion, port: number): Promise<DiagnosticsHandle> {
  const server: Server = createServer((req, res) => {
    const path = (req.url ?? '/').split('?')[0];
    const json = (body: unknown) => {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(body));
    };
    if (path === '/health') {
      json(companion.health());
      return;
    }
    if (path === '/version') {
      const h = companion.health();
      json({ version: h.version, protocol: h.protocol });
      return;
    }
    if (path === '/adapters') {
      json(companion.health().adapters);
      return;
    }
    res.writeHead(404, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: 'not found' }));
  });

  return new Promise((resolve, reject) => {
    server.once('error', reject);
    // loopback by default; never 0.0.0.0 (PRD §15.2)
    server.listen(port, '127.0.0.1', () => {
      const addr = server.address();
      const actual = typeof addr === 'object' && addr ? addr.port : port;
      resolve({
        port: actual,
        close: () =>
          new Promise<void>((done) => {
            server.close(() => done());
          }),
      });
    });
  });
}
