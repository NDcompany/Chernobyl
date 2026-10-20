/**
 * Applies ServerMessages to the snapshot immutably. This is the single
 * merge path — both the mock transport and the WebSocket feed it, so every
 * screen sees identical state transitions.
 */
import type { ServerMessage, Snapshot } from '@chernobyl/protocol';

function upsert<T extends { id: string }>(list: T[], item: T): T[] {
  const i = list.findIndex((x) => x.id === item.id);
  if (i === -1) return [item, ...list];
  const next = list.slice();
  next[i] = item;
  return next;
}

export function applyServerMessage(prev: Snapshot, msg: ServerMessage): Snapshot {
  switch (msg.type) {
    case 'hello_ack':
    case 'state':
      return msg.snapshot;

    case 'host_updated':
      return { ...prev, hosts: upsert(prev.hosts, msg.host) };

    case 'agent_updated':
      return { ...prev, agents: upsert(prev.agents, msg.agent) };

    case 'session_created':
    case 'session_updated':
      return { ...prev, sessions: upsert(prev.sessions, msg.session) };

    case 'session_output': {
      const sessions = prev.sessions.map((s) =>
        s.id === msg.sessionId
          ? {
              ...s,
              output: [...s.output, ...msg.lines].slice(-300),
              progress: msg.progress,
              status: msg.status,
              lastActivityAt: Date.now(),
            }
          : s,
      );
      return { ...prev, sessions };
    }

    case 'permission_requested':
      return { ...prev, permissions: upsert(prev.permissions, msg.permission) };

    case 'permission_resolved':
      return { ...prev, permissions: upsert(prev.permissions, msg.permission) };

    case 'activity':
      return { ...prev, activity: [msg.event, ...prev.activity].slice(0, 80) };

    default:
      // ack / pong are correlated by the transport, not merged into state
      return prev;
  }
}
