/**
 * CHERNOBYL host service entry point.
 *
 * PORT       — HTTP/WS port (default 8787)
 * HOST_TOKEN — optional shared secret; when set, WS clients must present it
 *              as `?token=` query param or `Authorization: Bearer` header
 *
 * All behavior lives in host.ts so integration tests run the same wiring.
 */
import { PROTOCOL_VERSION } from '@chernobyl/protocol';
import { createHost } from './host';

const PORT = Number(process.env.PORT ?? 8787);
const TOKEN = process.env.HOST_TOKEN || undefined;

void createHost({ port: PORT, token: TOKEN }).then((host) => {
  console.log(
    `[chernobyl-host] listening on :${host.port} (protocol v${PROTOCOL_VERSION}${TOKEN ? ', token auth on' : ''})`,
  );

  const shutdown = (): void => {
    console.log('[chernobyl-host] shutting down');
    void host.close().then(() => process.exit(0));
    // hard-exit if sockets keep the loop alive
    setTimeout(() => process.exit(0), 1_500).unref();
  };
  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
});
