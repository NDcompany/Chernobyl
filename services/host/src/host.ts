/**
 * CHERNOBYL host service core — an instantiable server so the exact
 * production wiring is covered by integration tests.
 *
 * HTTP  /health   — liveness + counters
 * WS    /         — protocol v2 (see @chernobyl/protocol)
 *
 * Hardening:
 * - optional shared-secret auth (HOST_TOKEN): `?token=` query or
 *   `Authorization: Bearer` header, closed with 4001 otherwise
 * - protocol version gate on `hello`, closed with 4002 on mismatch
 * - server-side WS pings; browsers auto-pong, so idle clients stay alive
 *   and dead peers are terminated after HEARTBEAT_MISS_LIMIT misses
 * - max payload cap so a hostile frame cannot balloon memory
 */
import { createServer, type IncomingMessage } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { RawData, WebSocket } from 'ws';
import { WebSocketServer } from 'ws';
import {
  HEARTBEAT_MISS_LIMIT,
  HEARTBEAT_MS,
  MockEngine,
  PROTOCOL_VERSION,
  nextId,
  parseClientMessage,
  type ServerMessage,
} from '@chernobyl/protocol';

const MAX_PAYLOAD_BYTES = 256_000;
const CLOSE_UNAUTHORIZED = 4001;
const CLOSE_PROTOCOL_MISMATCH = 4002;

export interface HostOptions {
  /** 0 picks an ephemeral port (tests). Default 8787. */
  port?: number;
  /** Bind address. Default 0.0.0.0 (LAN access). */
  host?: string;
  /** Shared secret; when set, connections must present it. */
  token?: string;
}

export interface Host {
  port: number;
  /** ws://127.0.0.1:<port> — always local-correct, whatever the bind address. */
  url: string;
  close(): Promise<void>;
}

interface Client {
  id: string;
  lastSeen: number;
}

function authorized(req: IncomingMessage, token: string): boolean {
  try {
    const url = new URL(req.url ?? '/', 'http://localhost');
    if (url.searchParams.get('token') === token) return true;
  } catch {
    /* fall through to header check */
  }
  return req.headers.authorization === `Bearer ${token}`;
}

function handleFrame(
  engine: MockEngine,
  client: Client,
  ws: WebSocket,
  raw: RawData,
): void {
  const msg = parseClientMessage(raw.toString());
  if (!msg) {
    ws.send(
      JSON.stringify({
        type: 'ack',
        commandId: 'invalid',
        ok: false,
        error: 'malformed message',
      } satisfies ServerMessage),
    );
    return;
  }

  if (msg.type === 'hello') {
    if (msg.protocol !== PROTOCOL_VERSION) {
      ws.send(
        JSON.stringify({
          type: 'ack',
          commandId: 'hello',
          ok: false,
          error: `protocol mismatch: client v${msg.protocol}, server v${PROTOCOL_VERSION}`,
        } satisfies ServerMessage),
      );
      ws.close(CLOSE_PROTOCOL_MISMATCH, 'protocol mismatch');
      return;
    }
    client.id = msg.clientId;
    ws.send(
      JSON.stringify({
        type: 'hello_ack',
        protocol: PROTOCOL_VERSION,
        serverTime: Date.now(),
        snapshot: engine.snapshot(),
        lastSeq: engine.lastSeq(),
      } satisfies ServerMessage),
    );
    return;
  }

  const reply = engine.handle(msg);
  if (reply && ws.readyState === ws.OPEN) ws.send(JSON.stringify(reply));
}

export function createHost(opts: HostOptions = {}): Promise<Host> {
  const clients = new Map<WebSocket, Client>();
  const engine = new MockEngine((msg) => {
    const payload = JSON.stringify(msg);
    for (const ws of clients.keys()) {
      if (ws.readyState === ws.OPEN) ws.send(payload);
    }
  });

  const httpServer = createServer((req, res) => {
    if (req.url === '/health') {
      const s = engine.snapshot();
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(
        JSON.stringify({
          ok: true,
          protocol: PROTOCOL_VERSION,
          clients: clients.size,
          hosts: s.hosts.length,
          sessions: s.sessions.length,
          pending: s.permissions.filter((p) => p.status === 'pending').length,
        }),
      );
      return;
    }
    res.writeHead(404);
    res.end();
  });

  const wss = new WebSocketServer({ server: httpServer, maxPayload: MAX_PAYLOAD_BYTES });

  wss.on('connection', (ws, req) => {
    if (opts.token && !authorized(req, opts.token)) {
      ws.close(CLOSE_UNAUTHORIZED, 'unauthorized');
      return;
    }
    const client: Client = { id: nextId('client'), lastSeen: Date.now() };
    clients.set(ws, client);
    // browsers answer pings automatically — this keeps idle clients alive
    ws.on('pong', () => {
      client.lastSeen = Date.now();
    });
    ws.on('message', (raw) => {
      client.lastSeen = Date.now();
      handleFrame(engine, client, ws, raw);
    });
    ws.on('close', () => clients.delete(ws));
    ws.on('error', () => clients.delete(ws));
  });

  const heartbeat = setInterval(() => {
    engine.tick();
    const now = Date.now();
    const limit = HEARTBEAT_MS * HEARTBEAT_MISS_LIMIT;
    for (const [ws, client] of clients) {
      if (now - client.lastSeen > limit) {
        ws.terminate();
        clients.delete(ws);
        continue;
      }
      ws.ping();
    }
  }, HEARTBEAT_MS);

  return new Promise<Host>((resolve, reject) => {
    httpServer.once('error', reject);
    httpServer.listen(opts.port ?? 8787, opts.host ?? '0.0.0.0', () => {
      const { port } = httpServer.address() as AddressInfo;
      resolve({
        port,
        url: `ws://127.0.0.1:${port}`,
        close: () =>
          new Promise<void>((done) => {
            clearInterval(heartbeat);
            engine.dispose();
            for (const ws of clients.keys()) ws.terminate();
            clients.clear();
            wss.close(() => httpServer.close(() => done()));
          }),
      });
    });
  });
}
