/**
 * Local persistence backed by expo-sqlite (with an in-memory fallback for
 * web / open failures). Used for the Zustand persisted slice and the audit
 * trail. No secrets are ever stored here — see secure.ts.
 */
import * as SQLite from 'expo-sqlite';
import { Platform } from 'react-native';
import type { StateStorage } from 'zustand/middleware';

const memory = new Map<string, string>();
let db: SQLite.SQLiteDatabase | null = null;
let failed = false;

function getDb(): SQLite.SQLiteDatabase | null {
  if (Platform.OS === 'web' || failed) return null;
  if (db) return db;
  try {
    db = SQLite.openDatabaseSync('chernobyl.db');
    db.execSync(
      'CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL)',
    );
    return db;
  } catch {
    failed = true;
    return null;
  }
}

export const sqliteStorage: StateStorage = {
  getItem: (name) => {
    const d = getDb();
    if (!d) return memory.get(name) ?? null;
    try {
      const row = d.getFirstSync<{ value: string }>('SELECT value FROM kv WHERE key = ?', name);
      return row?.value ?? null;
    } catch {
      return null;
    }
  },
  setItem: (name, value) => {
    const d = getDb();
    if (!d) {
      memory.set(name, value);
      return;
    }
    try {
      d.runSync(
        'INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value',
        name,
        value,
      );
    } catch {
      memory.set(name, value);
    }
  },
  removeItem: (name) => {
    const d = getDb();
    if (!d) {
      memory.delete(name);
      return;
    }
    try {
      d.runSync('DELETE FROM kv WHERE key = ?', name);
    } catch {
      memory.delete(name);
    }
  },
};

/* ------------------------------------------------------------------ */
/* Audit trail                                                         */
/* ------------------------------------------------------------------ */

export interface AuditRow {
  id: number;
  permissionId: string;
  agent: string;
  command: string;
  decision: string;
  risk: string;
  ts: number;
}

const memoryAudit: AuditRow[] = [];
// newest rows kept on the device; older decisions are dropped (retention)
const AUDIT_KEEP = 1000;

function rememberInMemory(entry: Omit<AuditRow, 'id'>): void {
  memoryAudit.unshift({ ...entry, id: memoryAudit.length + 1 });
  memoryAudit.length = Math.min(memoryAudit.length, AUDIT_KEEP);
}

function ensureAuditTable(d: SQLite.SQLiteDatabase): void {
  d.execSync(
    'CREATE TABLE IF NOT EXISTS audit (id INTEGER PRIMARY KEY AUTOINCREMENT, permission_id TEXT NOT NULL, agent TEXT NOT NULL, command TEXT NOT NULL, decision TEXT NOT NULL, risk TEXT NOT NULL, ts INTEGER NOT NULL)',
  );
}

export function recordAudit(entry: Omit<AuditRow, 'id'>): void {
  const d = getDb();
  if (!d) {
    rememberInMemory(entry);
    return;
  }
  try {
    ensureAuditTable(d);
    d.runSync(
      'INSERT INTO audit (permission_id, agent, command, decision, risk, ts) VALUES (?, ?, ?, ?, ?, ?)',
      entry.permissionId,
      entry.agent,
      entry.command,
      entry.decision,
      entry.risk,
      entry.ts,
    );
    d.runSync('DELETE FROM audit WHERE id <= (SELECT MAX(id) FROM audit) - ?', AUDIT_KEEP);
  } catch {
    rememberInMemory(entry);
  }
}

export function readAudit(limit = 50): AuditRow[] {
  const d = getDb();
  if (!d) return memoryAudit.slice(0, limit);
  try {
    ensureAuditTable(d);
    return d.getAllSync<AuditRow>(
      'SELECT id, permission_id AS permissionId, agent, command, decision, risk, ts FROM audit ORDER BY id DESC LIMIT ?',
      limit,
    );
  } catch {
    return memoryAudit.slice(0, limit);
  }
}
