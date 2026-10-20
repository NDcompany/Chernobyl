/**
 * Companion configuration (PRD §25 Phase 2): load, validate, migrate.
 *
 * File format is JSON, versioned. A config written by a NEWER companion is
 * refused (fail closed — never guess at unknown fields). A missing config
 * is created with defaults on first run. `CHERNOBYL_HOME` overrides the
 * default location (tests, portable installs).
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { homedir } from 'node:os';

export const CONFIG_VERSION = 1;

export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ConfigError';
  }
}

export interface AdapterEntryConfig {
  /** Stable instance id, e.g. 'mock-1'. */
  id: string;
  /** Adapter type as resolved by discovery, e.g. 'mock'. */
  type: string;
  /** Adapter-specific options (passed through to the factory). */
  options?: Record<string, unknown>;
  /** Disabled entries are skipped, not deleted. */
  enabled?: boolean;
}

export interface CompanionConfig {
  version: number;
  /** Display name of this host. */
  name: string;
  /** Working data directory — companion.db lives here. */
  dataDir: string;
  adapters: AdapterEntryConfig[];
  retention: {
    /** Hard cap on retained event rows. */
    events: number;
    /** Events older than this are pruned at startup. */
    eventsMaxAgeMs: number;
  };
  /** Loopback diagnostics port (PRD §15.2). 0 = ephemeral. */
  diagnostics: { port: number };
  /** Degraded-adapter health probe cadence. */
  probeIntervalMs: number;
}

export function configDir(): string {
  const home = process.env.CHERNOBYL_HOME;
  if (home) return home;
  if (process.platform === 'win32') {
    return join(process.env.APPDATA ?? join(homedir(), 'AppData', 'Roaming'), 'CHERNOBYL');
  }
  return join(homedir(), '.config', 'chernobyl');
}

export function defaultConfigPath(): string {
  return join(configDir(), 'companion.json');
}

export function defaultConfig(dataDir?: string): CompanionConfig {
  return {
    version: CONFIG_VERSION,
    name: 'CHERNOBYL host',
    dataDir: dataDir ?? configDir(),
    adapters: [{ id: 'mock-1', type: 'mock', enabled: true }],
    retention: { events: 5_000, eventsMaxAgeMs: 7 * 24 * 60 * 60 * 1000 },
    diagnostics: { port: 9487 },
    probeIntervalMs: 15_000,
  };
}

function bad(msg: string): never {
  throw new ConfigError(msg);
}

function asRecord(v: unknown): Record<string, unknown> {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) bad('config root must be an object');
  return v as Record<string, unknown>;
}

function asStr(v: unknown, field: string): string {
  if (typeof v !== 'string' || v.length === 0) bad(`'${field}' must be a non-empty string`);
  return v;
}

function asInt(v: unknown, field: string, min: number, max: number): number {
  if (typeof v !== 'number' || !Number.isFinite(v) || !Number.isInteger(v) || v < min || v > max) {
    bad(`'${field}' must be an integer between ${min} and ${max}`);
  }
  return v;
}

/** Validates untrusted config JSON into a full CompanionConfig. */
export function validateConfig(raw: unknown): CompanionConfig {
  const v = asRecord(raw);
  // a config from a FUTURE companion is refused before any range checks
  if (typeof v.version === 'number' && v.version > CONFIG_VERSION) {
    bad(`config was written by a newer companion (v${v.version} > v${CONFIG_VERSION}) — upgrade the companion, not the config`);
  }
  const version = asInt(v.version ?? 1, 'version', 1, CONFIG_VERSION);
  if (version < CONFIG_VERSION) {
    bad(`config version ${version} is older than supported ${CONFIG_VERSION} — migration not defined`);
  }

  const name = asStr(v.name, 'name');
  const dataDir = asStr(v.dataDir, 'dataDir');

  if (!Array.isArray(v.adapters)) bad(`'adapters' must be an array`);
  const adapters: AdapterEntryConfig[] = v.adapters.map((entry, i) => {
    const e = asRecord(entry);
    const id = asStr(e.id, `adapters[${i}].id`);
    const type = asStr(e.type, `adapters[${i}].type`);
    const out: AdapterEntryConfig = { id, type };
    if (e.options !== undefined) {
      if (typeof e.options !== 'object' || e.options === null || Array.isArray(e.options)) {
        bad(`'adapters[${i}].options' must be an object`);
      }
      out.options = e.options as Record<string, unknown>;
    }
    if (e.enabled !== undefined) {
      if (typeof e.enabled !== 'boolean') bad(`'adapters[${i}].enabled' must be a boolean`);
      out.enabled = e.enabled;
    }
    return out;
  });
  const ids = adapters.filter((a) => a.enabled !== false).map((a) => a.id);
  if (new Set(ids).size !== ids.length) bad(`adapter ids must be unique`);

  const ret = asRecord(v.retention);
  const retention = {
    events: asInt(ret.events, 'retention.events', 100, 1_000_000),
    eventsMaxAgeMs: asInt(ret.eventsMaxAgeMs, 'retention.eventsMaxAgeMs', 60_000, 365 * 24 * 60 * 60 * 1000),
  };

  const diag = asRecord(v.diagnostics);
  const diagnostics = { port: asInt(diag.port, 'diagnostics.port', 0, 65_535) };
  const probeIntervalMs = asInt(v.probeIntervalMs, 'probeIntervalMs', 1_000, 600_000);

  return { version: CONFIG_VERSION, name, dataDir, adapters, retention, diagnostics, probeIntervalMs };
}

/**
 * Loads the config at `path` (default location when omitted). Missing file
 * → defaults written to disk; invalid file → ConfigError (fail closed).
 */
export function loadConfig(path?: string): { config: CompanionConfig; path: string; created: boolean } {
  const file = path ?? defaultConfigPath();
  if (!existsSync(file)) {
    const config = defaultConfig();
    mkdirSync(dirname(file), { recursive: true });
    writeFileSync(file, `${JSON.stringify(config, null, 2)}\n`, 'utf8');
    return { config, path: file, created: true };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(readFileSync(file, 'utf8'));
  } catch (err) {
    throw new ConfigError(`config ${file} is not valid JSON: ${(err as Error).message}`);
  }
  return { config: validateConfig(parsed), path: file, created: false };
}
