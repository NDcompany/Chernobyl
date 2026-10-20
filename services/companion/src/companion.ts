/**
 * Companion core (PRD §25 Phase 2 exit criteria):
 *  - runs without the mobile app,
 *  - adapter failure does not crash it,
 *  - commands and event cursors survive restarts,
 *  - resource usage is measured (health carries RSS/heap/CPU/uptime),
 *  - fail-closed when durable state cannot be written (PRD §19.4).
 */
import { join } from 'node:path';
import { PROTOCOL_VERSION, type ClientMessage, type PermissionRequest, type ServerMessage } from '@chernobyl/protocol';
import type { AgentAdapter } from '@chernobyl/adapter-sdk';
import {
  AdapterManager,
  discoverAdapters,
  type AdapterReport,
  type DiscoveredAdapter,
  type Factories,
} from './adapters';
import type { CompanionConfig } from './config';
import { CompanionStorage, type StoredCommand } from './storage';

export const COMPANION_VERSION = '0.1.0';

const MUTATING = new Set<ClientMessage['type']>([
  'create_session',
  'submit_prompt',
  'interrupt',
  'permission_decision',
]);

export interface SubmitResult {
  /** Acceptance only — never an execution outcome (gate G1). */
  ok: boolean;
  duplicate?: boolean;
  error?: string;
  /** Persisted command record (present once durable). */
  record?: StoredCommand;
}

export interface CompanionHealth {
  ok: boolean;
  version: string;
  protocol: number;
  name: string;
  uptimeMs: number;
  storage: {
    ok: boolean;
    file: string;
    events: number;
    seq: number;
    commands: number;
    approvals: number;
    audit: number;
    detail?: string;
  };
  adapters: AdapterReport[];
  resources: {
    rssBytes: number;
    heapUsedBytes: number;
    heapTotalBytes: number;
    cpuUserUs: number;
    cpuSystemUs: number;
  };
}

export interface Companion {
  health(): CompanionHealth;
  getCommand(commandId: string): StoredCommand | undefined;
  submitCommand(msg: ClientMessage, opts?: { adapterId?: string }): Promise<SubmitResult>;
  /** Event replay from a cursor (the storage side of resync, PRD §14.4). */
  readEventsAfter(cursor: number, limit?: number): ReturnType<CompanionStorage['readEventsAfter']>;
  close(): Promise<void>;
}

export interface CompanionOptions {
  config: CompanionConfig;
  /** Test injection: pre-built adapters (skips discovery). */
  adapters?: DiscoveredAdapter[];
  /** Test injection: discovery factories. */
  factories?: Factories;
  /** Test injection: pre-opened storage (fail-closed latching, inspection). */
  storage?: CompanionStorage;
}

export async function createCompanion(opts: CompanionOptions): Promise<Companion> {
  const { config } = opts;
  const storage =
    opts.storage ?? CompanionStorage.open(join(config.dataDir, 'companion.db'), config.retention);
  const startedAt = Date.now();

  // boot reconciliation (PRD §19.1/§19.2) — before any new work is accepted
  const unknownCommands = storage.reconcileUnresolvedCommands();
  const staleApprovals = storage.reconcileExpiredApprovals();
  if (unknownCommands > 0) {
    storage.audit({
      category: 'recovery',
      text: `reconciled ${unknownCommands} unresolved command(s) to unknown after restart`,
    });
  }
  if (staleApprovals > 0) {
    storage.audit({
      category: 'recovery',
      text: `reconciled ${staleApprovals} expired pending approval(s) to stale after restart`,
    });
  }

  const discovered =
    opts.adapters ?? discoverAdapters(config.adapters, opts.factories);

  const manager = new AdapterManager(
    discovered,
    {
      onEvent: (adapterId, msg) => persistEvent(adapterId, msg),
      onStatus: (report) => {
        storage.setAdapterStatus(report.id, report.type, report.status, report.detail);
        storage.audit({
          category: 'adapter',
          text: `${report.id} → ${report.status}${report.detail ? ` (${report.detail})` : ''}`,
        });
      },
    },
    config.probeIntervalMs,
  );

  function persistEvent(adapterId: string, msg: ServerMessage): void {
    const payload: Record<string, unknown> = { ...msg };
    delete payload.seq; // the companion is the sequence authority (PRD §14.3)
    const occurredAt =
      'occurredAt' in msg && typeof msg.occurredAt === 'number' ? msg.occurredAt : Date.now();
    // storage guards + latches fail-closed internally; errors propagate to
    // the manager's per-adapter isolation wrapper
    storage.appendEvents([{ type: msg.type, payload, occurredAt }]);

    if (msg.type === 'command_progress') {
      storage.setCommandState(msg.commandId, msg.state);
    } else if (msg.type === 'command_result') {
      storage.setCommandState(msg.commandId, msg.status, msg.error);
    } else if (msg.type === 'permission_requested' || msg.type === 'permission_resolved') {
      storage.upsertApproval(msg.permission.id, msg.permission as PermissionRequest, adapterId);
    }
  }

  await manager.start();
  storage.audit({
    category: 'lifecycle',
    text: `companion started (v${COMPANION_VERSION}, protocol v${PROTOCOL_VERSION})`,
  });

  let closed = false;

  return {
    health(): CompanionHealth {
      const adapters = manager.reportsSnapshot();
      const storageOk = storage.canWrite();
      return {
        ok: storageOk && adapters.every((a) => a.status !== 'degraded'),
        version: COMPANION_VERSION,
        protocol: PROTOCOL_VERSION,
        name: config.name,
        uptimeMs: Date.now() - startedAt,
        storage: {
          ok: storageOk,
          file: join(config.dataDir, 'companion.db'),
          events: storage.eventCount(),
          seq: storage.currentSeq(),
          commands: storage.commandCount(),
          approvals: storage.approvalCount(),
          audit: storage.auditCount(),
          detail: storage.writeFailure() ?? undefined,
        },
        adapters,
        resources: {
          rssBytes: process.memoryUsage.rss(),
          heapUsedBytes: process.memoryUsage().heapUsed,
          heapTotalBytes: process.memoryUsage().heapTotal,
          cpuUserUs: process.cpuUsage().user,
          cpuSystemUs: process.cpuUsage().system,
        },
      };
    },

    getCommand: (commandId) => storage.getCommand(commandId),

    async submitCommand(msg, submitOpts): Promise<SubmitResult> {
      if (closed) return { ok: false, error: 'companion is shutting down' };
      if (!MUTATING.has(msg.type) || !('commandId' in msg)) {
        return { ok: false, error: `unsupported command type '${msg.type}'` };
      }
      // fail closed BEFORE routing or executing (PRD §19.4)
      if (!storage.canWrite()) {
        return { ok: false, error: `durable state unavailable — refusing command (${storage.writeFailure()})` };
      }

      const adapterId = manager.routeAdapter(submitOpts?.adapterId);
      if (!adapterId) {
        return { ok: false, error: 'no healthy adapter available' };
      }

      // persist BEFORE execution (PRD §10.4); duplicates return stored state
      let inserted: 'inserted' | 'duplicate';
      try {
        inserted = storage.insertCommand(msg.commandId, msg.type, adapterId, msg as unknown as Record<string, unknown>);
      } catch (err) {
        return { ok: false, error: `durable state unavailable — refusing command (${(err as Error).message})` };
      }
      if (inserted === 'duplicate') {
        const record = storage.getCommand(msg.commandId);
        return { ok: true, duplicate: true, record };
      }

      const { reply, error } = await manager.submit(adapterId, msg);
      if (error) {
        storage.setCommandState(msg.commandId, 'failed', error);
        return { ok: false, error, record: storage.getCommand(msg.commandId) };
      }
      if (reply?.type === 'ack' && reply.ok === false) {
        // refused at acceptance → no lifecycle follows (G1); record closes as failed
        storage.setCommandState(msg.commandId, 'failed', reply.error);
        return { ok: false, error: reply.error, record: storage.getCommand(msg.commandId) };
      }
      return { ok: true, duplicate: false, record: storage.getCommand(msg.commandId) };
    },

    readEventsAfter: (cursor, limit) => storage.readEventsAfter(cursor, limit),

    async close(): Promise<void> {
      if (closed) return;
      closed = true;
      await manager.stop();
      storage.audit({ category: 'lifecycle', text: 'companion stopped' });
      storage.close();
    },
  };
}

/** Re-exported for callers building adapters explicitly (tests, future hosts). */
export type { AgentAdapter };
