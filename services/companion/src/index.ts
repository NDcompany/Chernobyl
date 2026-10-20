/**
 * @chernobyl/companion — desktop companion foundation: configuration,
 * SQLite persistence (built-in node:sqlite, zero deps), adapter lifecycle
 * with failure isolation, loopback diagnostics, graceful entry point.
 */
export {
  ConfigError,
  CONFIG_VERSION,
  defaultConfig,
  defaultConfigPath,
  loadConfig,
  validateConfig,
  type AdapterEntryConfig,
  type CompanionConfig,
} from './config';
export {
  AdapterManager,
  discoverAdapters,
  type AdapterManagerHooks,
  type AdapterReport,
  type AdapterStatus,
  type DiscoveredAdapter,
} from './adapters';
export {
  COMPANION_VERSION,
  createCompanion,
  type Companion,
  type CompanionHealth,
  type CompanionOptions,
  type SubmitResult,
} from './companion';
export { startDiagnostics, type DiagnosticsHandle } from './diagnostics';
export {
  CompanionStorage,
  type AuditRow,
  type Retention,
  type StoredCommand,
  type StoredEvent,
} from './storage';
