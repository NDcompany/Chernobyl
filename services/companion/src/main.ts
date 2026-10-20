#!/usr/bin/env node
/**
 * Companion background process entry point (PRD §25 Phase 2).
 *
 * Usage:
 *   node dist/main.js [--config <file>] [--data-dir <dir>] [--port <n>]
 *
 * Runs without the mobile app; SIGINT/SIGTERM trigger graceful shutdown
 * (stop adapters → audit → close storage → exit 0).
 */
import { mkdirSync } from 'node:fs';
import { ConfigError, defaultConfigPath, loadConfig, type CompanionConfig } from './config';
import { createCompanion } from './companion';
import { startDiagnostics } from './diagnostics';

interface Args {
  config?: string;
  dataDir?: string;
  port?: number;
  /** Smoke-test hook: run the graceful shutdown routine after N ms. */
  exitAfter?: number;
}

function parseArgs(argv: string[]): Args {
  const args: Args = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--config') args.config = argv[++i];
    else if (a === '--data-dir') args.dataDir = argv[++i];
    else if (a === '--port') args.port = Number(argv[++i]);
    else if (a === '--exit-after') args.exitAfter = Number(argv[++i]);
    else if (a === '--help' || a === '-h') {
      process.stdout.write(
        'chernobyl-companion [--config <file>] [--data-dir <dir>] [--port <n>] [--exit-after <ms>]\n',
      );
      process.exit(0);
    } else {
      process.stderr.write(`unknown argument: ${a}\n`);
      process.exit(2);
    }
  }
  return args;
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  let config: CompanionConfig;
  let configPath: string;
  try {
    ({ config, path: configPath } = loadConfig(args.config ?? defaultConfigPath()));
  } catch (err) {
    if (err instanceof ConfigError) {
      process.stderr.write(`configuration error: ${err.message}\n`);
      process.exit(1);
    }
    throw err;
  }

  if (args.dataDir) config.dataDir = args.dataDir;
  if (typeof args.port === 'number' && Number.isInteger(args.port) && args.port >= 0) {
    config.diagnostics.port = args.port;
  }
  mkdirSync(config.dataDir, { recursive: true });

  const companion = await createCompanion({ config });
  const diagnostics = await startDiagnostics(companion, config.diagnostics.port);

  process.stdout.write(
    `CHERNOBYL companion v${companion.health().version} started (config: ${configPath})\n` +
      `diagnostics: http://127.0.0.1:${diagnostics.port}/health\n`,
  );

  let shuttingDown = false;
  const shutdown = (signal: string) => {
    if (shuttingDown) return;
    shuttingDown = true;
    process.stdout.write(`received ${signal} — shutting down gracefully\n`);
    void (async () => {
      try {
        await diagnostics.close();
        await companion.close();
        process.exit(0);
      } catch (err) {
        process.stderr.write(`shutdown error: ${(err as Error).message}\n`);
        process.exit(1);
      }
    })();
  };
  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));
  if (typeof args.exitAfter === 'number' && Number.isFinite(args.exitAfter)) {
    setTimeout(() => shutdown('exit-after'), args.exitAfter);
  }
}

void main().catch((err) => {
  process.stderr.write(`fatal: ${(err as Error).stack ?? String(err)}\n`);
  process.exit(1);
});
