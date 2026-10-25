/**
 * Pure selectors over the snapshot — shared so counts, filters and pending
 * logic stay consistent between the dashboard, badges and detail screens.
 */
import { isStalePermission } from '@chernobyl/protocol';
import type {
  ActivityEvent,
  Agent,
  Host,
  PermissionRequest,
  Session,
  Snapshot,
} from '@chernobyl/protocol';

export const EMPTY_SNAPSHOT: Snapshot = {
  hosts: [],
  agents: [],
  sessions: [],
  permissions: [],
  activity: [],
};

export function isPending(p: PermissionRequest, now: number): boolean {
  return p.status === 'pending' && !isStalePermission(p, now);
}

export function pendingPermissions(s: Snapshot, now: number): PermissionRequest[] {
  return s.permissions.filter((p) => isPending(p, now));
}

export function decidedPermissions(s: Snapshot): PermissionRequest[] {
  return s.permissions
    .filter((p) => p.status === 'approved' || p.status === 'rejected')
    .sort((a, b) => (b.decidedAt ?? b.requestedAt) - (a.decidedAt ?? a.requestedAt));
}

/** ADR-009: the companion is serving demo data (its mock adapter) somewhere in this snapshot. */
export function hasSimulatedData(s: Snapshot): boolean {
  return s.hosts.some((h) => h.simulated) || s.agents.some((a) => a.simulated);
}

export function hostById(s: Snapshot, id: string): Host | undefined {
  return s.hosts.find((h) => h.id === id);
}

export function agentById(s: Snapshot, id: string): Agent | undefined {
  return s.agents.find((a) => a.id === id);
}

export function sessionById(s: Snapshot, id: string): Session | undefined {
  return s.sessions.find((x) => x.id === id);
}

export function permissionById(s: Snapshot, id: string): PermissionRequest | undefined {
  return s.permissions.find((p) => p.id === id);
}

export function sessionsForAgent(s: Snapshot, agentId: string): Session[] {
  return s.sessions
    .filter((x) => x.agentId === agentId)
    .sort((a, b) => b.lastActivityAt - a.lastActivityAt);
}

/** sessions that are currently busy (running or waiting for a decision) */
export function activeSessions(s: Snapshot, agentId?: string): Session[] {
  return s.sessions.filter(
    (x) =>
      (x.status === 'running' || x.status === 'waiting') &&
      (agentId == null || x.agentId === agentId),
  );
}

export interface SnapshotCounts {
  hosts: number;
  agents: number;
  running: number;
  pending: number;
}

export function counts(s: Snapshot, now: number): SnapshotCounts {
  return {
    hosts: s.hosts.filter((h) => h.status === 'connected').length,
    agents: s.agents.filter((a) => a.state === 'online' || a.state === 'busy').length,
    running: activeSessions(s).length,
    pending: pendingPermissions(s, now).length,
  };
}

export function recentActivity(s: Snapshot, limit = 6): ActivityEvent[] {
  return [...s.activity].sort((a, b) => b.ts - a.ts).slice(0, limit);
}

export function activityFor(s: Snapshot, ids: Set<string>): ActivityEvent[] {
  return s.activity
    .filter((e) => e.refId != null && ids.has(e.refId))
    .sort((a, b) => b.ts - a.ts)
    .slice(0, 8);
}

export function searchAgents(s: Snapshot, query: string): Agent[] {
  const q = query.trim().toLowerCase();
  if (!q) return s.agents;
  return s.agents.filter((a) => {
    const host = hostById(s, a.hostId);
    return (
      a.name.toLowerCase().includes(q) ||
      a.kind.includes(q) ||
      a.capabilities.some((c) => c.toLowerCase().includes(q)) ||
      (host?.name.toLowerCase().includes(q) ?? false)
    );
  });
}

export function searchSessions(s: Snapshot, query: string, status: string): Session[] {
  const q = query.trim().toLowerCase();
  return s.sessions.filter((x) => {
    const okStatus = status === 'all' || x.status === status;
    if (!okStatus) return false;
    if (!q) return true;
    const agent = agentById(s, x.agentId);
    return (
      x.title.toLowerCase().includes(q) ||
      x.project.toLowerCase().includes(q) ||
      (agent?.name.toLowerCase().includes(q) ?? false)
    );
  });
}
