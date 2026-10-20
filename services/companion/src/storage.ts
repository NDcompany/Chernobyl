/**
 * Companion SQLite persistence (PRD §16.2, §19.1, §19.4).
 *
 * Uses Node's built-in `node:sqlite` — zero dependencies. Invariants:
 *  - The event sequence and its rows commit in ONE transaction, so a
 *    restart never silently resets or desyncs the per-host sequence
 *    (PRD §14.3, conflict C2 remainder).
 *  - Commands are persisted BEFORE execution and idempotent by commandId
 *    (PRD §10.4). Unresolved commands are reconciled to `unknown` at boot —
 *    never blindly repeated (PRD §19.1, gate G1).
 *  - Pending approvals whose native deadline passed are reconciled to
 *    `stale` at boot — never auto-applied (PRD §19.2).
 *  - Retention is enforced (PRD §16.3): hard row cap on append, age sweep
 *    at open.
 *
 * Write failures latch: `canWrite()` turns false so consequential
 * operations fail closed instead of running without durable records
 * (PRD §19.4).
 */
import { DatabaseSync } from 'node:sqlite';
import { mkdirSync } from 'node:fs';
import { dirname } from 'node:path';
import type { CommandState } from '@chernobyl/domain';
import type { PermissionRequest } from '@chernobyl/protocol';

export interface StoredEvent {
  seq: number;
  type: string;
  /** Event payload WITHOUT envelope seq (seq is the column). */
  payload: Record<string, unknown>;
  occurredAt: number;
}

export interface StoredCommand {
  commandId: string;
  type: string;
  adapterId: string | null;
  status: CommandState;
  payload: Record<string, unknown>;
  error: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface AuditRow {
  category: string;
  text: string;
  refId?: string;
}

export interface Retention {
  events: number;
  eventsMaxAgeMs: number;
}

/** PRD §16.2 initial tables. */
const MIGRATION_V1 = [
  `CREATE TABLE IF NOT EXISTS schema_migrations (
     version INTEGER PRIMARY KEY,
     applied_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS companion_config (
     key TEXT PRIMARY KEY,
     value TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS adapter_instances (
     id TEXT PRIMARY KEY,
     type TEXT NOT NULL,
     status TEXT NOT NULL,
     detail TEXT,
     updated_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS native_entity_mappings (
     adapter_id TEXT NOT NULL,
     entity_kind TEXT NOT NULL,
     native_id TEXT NOT NULL,
     local_id TEXT NOT NULL,
     updated_at INTEGER NOT NULL,
     PRIMARY KEY (adapter_id, entity_kind, native_id)
   )`,
  `CREATE TABLE IF NOT EXISTS sessions (
     id TEXT PRIMARY KEY,
     adapter_id TEXT NOT NULL,
     native_session_id TEXT,
     title TEXT,
     status TEXT,
     payload TEXT NOT NULL,
     updated_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS commands (
     command_id TEXT PRIMARY KEY,
     type TEXT NOT NULL,
     adapter_id TEXT,
     status TEXT NOT NULL,
     payload TEXT NOT NULL,
     error TEXT,
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS approvals (
     id TEXT PRIMARY KEY,
     native_request_id TEXT NOT NULL,
     adapter_id TEXT,
     session_id TEXT,
     status TEXT NOT NULL,
     revision INTEGER NOT NULL,
     expires_at INTEGER NOT NULL,
     payload TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS events (
     seq INTEGER PRIMARY KEY,
     type TEXT NOT NULL,
     payload TEXT NOT NULL,
     occurred_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS event_cursors (
     key TEXT PRIMARY KEY,
     value INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS audit_entries (
     id INTEGER PRIMARY KEY AUTOINCREMENT,
     category TEXT NOT NULL,
     text TEXT NOT NULL,
     ref_id TEXT,
     created_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS workspace_registry (
     path TEXT PRIMARY KEY,
     label TEXT,
     last_seen_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS notification_outbox (
     id INTEGER PRIMARY KEY AUTOINCREMENT,
     kind TEXT NOT NULL,
     payload TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     delivered_at INTEGER
   )`,
];

const SEQ_KEY = 'host_seq';

export class CompanionStorage {
  private db: DatabaseSync;
  private writeFailed: string | null = null;

  private constructor(
    file: string,
    private retention: Retention,
  ) {
    if (file !== ':memory:') mkdirSync(dirname(file), { recursive: true });
    this.db = new DatabaseSync(file);
    this.db.exec('PRAGMA journal_mode = WAL');
    this.db.exec('PRAGMA synchronous = NORMAL');
    this.migrate();
    this.pruneByAge();
  }

  static open(file: string, retention: Retention): CompanionStorage {
    return new CompanionStorage(file, retention);
  }

  private migrate(): void {
    this.db.exec('BEGIN');
    try {
      for (const stmt of MIGRATION_V1) this.db.exec(stmt);
      const has = this.db
        .prepare('SELECT version FROM schema_migrations WHERE version = 1')
        .get();
      if (!has) {
        this.db
          .prepare('INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?)')
          .run(Date.now());
      }
      // the sequence counter lives in event_cursors — created with v1
      this.db
        .prepare(
          'INSERT OR IGNORE INTO event_cursors (key, value) VALUES (?, 0)',
        )
        .run(SEQ_KEY);
      this.db.exec('COMMIT');
    } catch (err) {
      this.db.exec('ROLLBACK');
      throw err;
    }
  }

  /* ---------------------------------------------------------------- */
  /* Fail-closed write guard (PRD §19.4)                               */
  /* ---------------------------------------------------------------- */

  canWrite(): boolean {
    return this.writeFailed === null;
  }

  writeFailure(): string | null {
    return this.writeFailed;
  }

  /** Latches the fail-closed state after a write error. */
  latchWriteFailure(err: unknown): void {
    this.writeFailed ??= (err as Error)?.message ?? String(err);
  }

  private guard<T>(fn: () => T): T {
    try {
      return fn();
    } catch (err) {
      this.latchWriteFailure(err);
      throw err;
    }
  }

  /* ---------------------------------------------------------------- */
  /* Event log + persisted sequence (PRD §14.3)                        */
  /* ---------------------------------------------------------------- */

  currentSeq(): number {
    const row = this.db
      .prepare('SELECT value FROM event_cursors WHERE key = ?')
      .get(SEQ_KEY) as { value: number } | undefined;
    return row?.value ?? 0;
  }

  /**
   * Appends events and advances the persisted sequence in one transaction.
   * Returns the assigned seqs. Payloads are stored envelope-free; the seq
   * column is the companion's authority stamp.
   */
  appendEvents(
    rows: Array<{ type: string; payload: Record<string, unknown>; occurredAt: number }>,
  ): number[] {
    if (rows.length === 0) return [];
    return this.guard(() => {
      this.db.exec('BEGIN IMMEDIATE');
      try {
        const next = this.currentSeq();
        const stmt = this.db.prepare(
          'INSERT INTO events (seq, type, payload, occurred_at) VALUES (?, ?, ?, ?)',
        );
        const seqs: number[] = [];
        rows.forEach((r, i) => {
          const seq = next + i + 1;
          stmt.run(seq, r.type, JSON.stringify(r.payload), r.occurredAt);
          seqs.push(seq);
        });
        this.db
          .prepare('UPDATE event_cursors SET value = ? WHERE key = ?')
          .run(next + rows.length, SEQ_KEY);
        this.db.exec('COMMIT');
        this.pruneByCount();
        return seqs;
      } catch (err) {
        this.db.exec('ROLLBACK');
        throw err;
      }
    });
  }

  readEventsAfter(cursor: number, limit = 500): StoredEvent[] {
    const rows = this.db
      .prepare(
        'SELECT seq, type, payload, occurred_at FROM events WHERE seq > ? ORDER BY seq LIMIT ?',
      )
      .all(cursor, limit) as Array<{
      seq: number;
      type: string;
      payload: string;
      occurred_at: number;
    }>;
    return rows.map((r) => ({
      seq: r.seq,
      type: r.type,
      payload: JSON.parse(r.payload) as Record<string, unknown>,
      occurredAt: r.occurred_at,
    }));
  }

  private pruneByCount(): void {
    const cap = this.retention.events;
    this.db
      .prepare(
        `DELETE FROM events WHERE seq <= COALESCE(
           (SELECT seq FROM events ORDER BY seq DESC LIMIT 1 OFFSET ?), 0
         )`,
      )
      .run(cap);
  }

  private pruneByAge(): void {
    const cutoff = Date.now() - this.retention.eventsMaxAgeMs;
    this.db.prepare('DELETE FROM events WHERE occurred_at < ?').run(cutoff);
  }

  eventCount(): number {
    const row = this.db.prepare('SELECT COUNT(*) AS n FROM events').get() as { n: number };
    return row.n;
  }

  /* ---------------------------------------------------------------- */
  /* Commands (PRD §10.4, §19.1)                                       */
  /* ---------------------------------------------------------------- */

  getCommand(commandId: string): StoredCommand | undefined {
    const r = this.db
      .prepare(
        'SELECT command_id, type, adapter_id, status, payload, error, created_at, updated_at FROM commands WHERE command_id = ?',
      )
      .get(commandId) as
      | {
          command_id: string;
          type: string;
          adapter_id: string | null;
          status: string;
          payload: string;
          error: string | null;
          created_at: number;
          updated_at: number;
        }
      | undefined;
    if (!r) return undefined;
    return {
      commandId: r.command_id,
      type: r.type,
      adapterId: r.adapter_id,
      status: r.status as CommandState,
      payload: JSON.parse(r.payload) as Record<string, unknown>,
      error: r.error,
      createdAt: r.created_at,
      updatedAt: r.updated_at,
    };
  }

  /** Returns 'duplicate' when the commandId already exists (no overwrite). */
  insertCommand(
    commandId: string,
    type: string,
    adapterId: string,
    payload: Record<string, unknown>,
  ): 'inserted' | 'duplicate' {
    return this.guard(() => {
      const now = Date.now();
      const res = this.db
        .prepare(
          `INSERT OR IGNORE INTO commands (command_id, type, adapter_id, status, payload, error, created_at, updated_at)
           VALUES (?, ?, ?, 'accepted', ?, NULL, ?, ?)`,
        )
        .run(commandId, type, adapterId, JSON.stringify(payload), now, now);
      return Number(res.changes) === 0 ? 'duplicate' : 'inserted';
    });
  }

  setCommandState(commandId: string, status: CommandState, error?: string): void {
    this.guard(() => {
      this.db
        .prepare('UPDATE commands SET status = ?, error = ?, updated_at = ? WHERE command_id = ?')
        .run(status, error ?? null, Date.now(), commandId);
    });
  }

  /**
   * Boot reconciliation (PRD §19.1): commands still accepted/running become
   * `unknown` — the outcome was lost with the previous process and must
   * never be guessed or re-executed. Returns how many were reconciled.
   */
  reconcileUnresolvedCommands(): number {
    return this.guard(() => {
      const now = Date.now();
      const res = this.db
        .prepare(
          `UPDATE commands
             SET status = 'unknown',
                 error = 'companion restarted before an outcome was recorded',
                 updated_at = ?
           WHERE status IN ('created', 'queued', 'accepted', 'running')`,
        )
        .run(now);
      return Number(res.changes);
    });
  }

  commandCount(): number {
    const row = this.db.prepare('SELECT COUNT(*) AS n FROM commands').get() as { n: number };
    return row.n;
  }

  /* ---------------------------------------------------------------- */
  /* Approvals (PRD §13, §19.2)                                        */
  /* ---------------------------------------------------------------- */

  upsertApproval(permissionId: string, p: PermissionRequest, adapterId: string): void {
    this.guard(() => {
      this.db
        .prepare(
          `INSERT INTO approvals (id, native_request_id, adapter_id, session_id, status, revision, expires_at, payload, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET
             status = excluded.status,
             revision = excluded.revision,
             payload = excluded.payload,
             updated_at = excluded.updated_at`,
        )
        .run(
          permissionId,
          p.nativeRequestId,
          adapterId,
          p.sessionId,
          p.status,
          p.revision,
          p.expiresAt,
          JSON.stringify(p),
          p.requestedAt,
          Date.now(),
        );
    });
  }

  /**
   * Boot reconciliation (PRD §19.2): pending approvals past their native
   * deadline become `stale` — expired approvals are never applied
   * automatically. Returns how many were reconciled.
   */
  reconcileExpiredApprovals(now: number = Date.now()): number {
    return this.guard(() => {
      const res = this.db
        .prepare(
          `UPDATE approvals SET status = 'stale', updated_at = ?
           WHERE status = 'pending' AND expires_at <= ?`,
        )
        .run(now, now);
      return Number(res.changes);
    });
  }

  approvalCount(): number {
    const row = this.db.prepare('SELECT COUNT(*) AS n FROM approvals').get() as { n: number };
    return row.n;
  }

  /* ---------------------------------------------------------------- */
  /* Adapter instances + audit + config KV                             */
  /* ---------------------------------------------------------------- */

  setAdapterStatus(id: string, type: string, status: string, detail?: string): void {
    this.guard(() => {
      this.db
        .prepare(
          `INSERT INTO adapter_instances (id, type, status, detail, updated_at) VALUES (?, ?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET status = excluded.status, detail = excluded.detail, updated_at = excluded.updated_at`,
        )
        .run(id, type, status, detail ?? null, Date.now());
    });
  }

  audit(row: AuditRow): void {
    this.guard(() => {
      this.db
        .prepare('INSERT INTO audit_entries (category, text, ref_id, created_at) VALUES (?, ?, ?, ?)')
        .run(row.category, row.text, row.refId ?? null, Date.now());
    });
  }

  auditCount(): number {
    const row = this.db.prepare('SELECT COUNT(*) AS n FROM audit_entries').get() as { n: number };
    return row.n;
  }

  listAudit(limit = 50): Array<{ id: number; category: string; text: string; createdAt: number }> {
    const rows = this.db
      .prepare(
        'SELECT id, category, text, created_at FROM audit_entries ORDER BY id DESC LIMIT ?',
      )
      .all(limit) as Array<{ id: number; category: string; text: string; created_at: number }>;
    return rows.map((r) => ({ id: r.id, category: r.category, text: r.text, createdAt: r.created_at }));
  }

  setConfigKV(key: string, value: string): void {
    this.guard(() => {
      this.db
        .prepare(
          'INSERT INTO companion_config (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value',
        )
        .run(key, value);
    });
  }

  getConfigKV(key: string): string | undefined {
    const row = this.db
      .prepare('SELECT value FROM companion_config WHERE key = ?')
      .get(key) as { value: string } | undefined;
    return row?.value;
  }

  /** Table list — used by tests to prove all PRD §16.2 tables exist. */
  tableNames(): string[] {
    const rows = this.db
      .prepare(`SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name`)
      .all() as Array<{ name: string }>;
    return rows.map((r) => r.name);
  }

  close(): void {
    try {
      this.db.close();
    } catch {
      /* already closed */
    }
  }
}
