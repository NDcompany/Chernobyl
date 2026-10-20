/**
 * MockAdapter — the Adapter interface implemented over the protocol's
 * MockEngine (single mock source of truth, ADR-009), plus the G4 fault
 * knobs the conformance suite drives. No shell is ever spawned here
 * either; this proves the contract is implementable before any real
 * adapter exists (Phase 3).
 */
import type { Capability } from '@chernobyl/domain';
import {
  MockEngine,
  nextId,
  type Agent,
  type AgentKind,
  type ClientMessage,
  type ServerMessage,
  type Session,
} from '@chernobyl/protocol';
import {
  AdapterError,
  type AdapterFaultOptions,
  type AdapterHealth,
  type AgentAdapter,
} from './adapter';
import { validateJsonResponse } from './response';

interface Envelope {
  seq: number;
  eventId: string;
  occurredAt: number;
}
/** Distributive omit: an event message with its envelope fields removed. */
type Unenveloped<T> = T extends Envelope ? Omit<T, keyof Envelope> : T;
type EmitPayload = Unenveloped<Extract<ServerMessage, Envelope>>;

/** Commands an adapter owns; hello/ping/list_state are host-level (PRD §14). */
const ADAPTER_COMMANDS: ReadonlySet<ClientMessage['type']> = new Set([
  'create_session',
  'submit_prompt',
  'interrupt',
  'permission_decision',
]);

const CAPABILITIES: Capability[] = [
  { id: 'session.list', status: 'available' },
  { id: 'session.message.send', status: 'available' },
  { id: 'session.interrupt', status: 'available' },
  { id: 'permission.review', status: 'available' },
  { id: 'diff.read', status: 'unavailable', detail: 'not simulated by the mock runtime' },
  { id: 'files.read', status: 'limited', detail: 'metadata only' },
];

/** Simulates the native runtime's HTTP layer (json mode answers 200/JSON). */
function nativeCall(opts: AdapterFaultOptions): { contentType: string; body: string } {
  if (opts.nativeResponse === 'html') {
    return { contentType: 'text/html', body: '<!doctype html><title>404</title>' };
  }
  return { contentType: 'application/json', body: '{"accepted":true}' };
}

export function createMockAdapter(opts: AdapterFaultOptions = {}): AgentAdapter {
  const subs = new Set<(m: ServerMessage) => void>();
  const timers = new Set<ReturnType<typeof setTimeout>>();
  /** The adapter is the event authority: it re-stamps every outgoing envelope. */
  let seq = 0;
  let disposed = false;

  const emit = (m: EmitPayload): void => {
    seq += 1;
    const stamped = {
      ...m,
      seq,
      eventId: nextId('evt'),
      occurredAt: Date.now(),
    } as ServerMessage;
    for (const cb of subs) cb(stamped);
  };

  const engine = new MockEngine((m) => {
    // engine events arrive carrying the engine's own envelope — the
    // adapter is the authority here, so re-stamp over it
    emit(m as unknown as EmitPayload);
  });

  const guardOwned = (): void => {
    if (disposed) throw new AdapterError('runtime-unavailable', 'adapter disposed');
    if (opts.existingInstance === 'refuse') {
      throw new AdapterError(
        'runtime-unavailable',
        'another runtime instance owns this data directory (simulated) — refusing to spawn a second one',
      );
    }
  };

  return {
    id: 'mock',
    kind: 'custom' as AgentKind,
    displayName: 'Mock Runtime',

    capabilities: () => CAPABILITIES.slice(),

    health: async (): Promise<AdapterHealth> => {
      if (disposed) return { ok: false, detail: 'adapter disposed' };
      if (opts.existingInstance === 'refuse') {
        return { ok: false, detail: 'another runtime instance owns this data directory (simulated)' };
      }
      if (opts.existingInstance === 'attach') {
        return { ok: true, detail: 'attached to existing runtime instance (simulated)' };
      }
      return { ok: true, detail: 'simulated runtime' };
    },

    listAgents: async (): Promise<Agent[]> => {
      guardOwned();
      return engine.snapshot().agents;
    },

    listSessions: async (): Promise<Session[]> => {
      guardOwned();
      return engine.snapshot().sessions;
    },

    send: async (msg: ClientMessage): Promise<ServerMessage | undefined> => {
      guardOwned();
      if (!ADAPTER_COMMANDS.has(msg.type)) {
        throw new AdapterError('unsupported', `adapters do not handle '${msg.type}'`);
      }
      // G4.2: validate the native response BEFORE trusting anything
      const raw = nativeCall(opts);
      validateJsonResponse(raw.contentType, raw.body);

      const reply = engine.handle(msg);
      if (
        opts.lateError &&
        msg.type === 'submit_prompt' &&
        reply?.type === 'ack' &&
        reply.ok === true
      ) {
        // G4.1: the runtime fails asynchronously AFTER acceptance — the
        // failure must surface on the event channel, not the ack.
        const timer = setTimeout(() => {
          timers.delete(timer);
          if (disposed) return;
          emit({
            type: 'command_result',
            commandId: msg.commandId,
            status: 'failed',
            sessionId: reply.sessionId,
            error: 'native runtime reported failure after acceptance (simulated)',
          });
        }, 20);
        timers.add(timer);
      }
      return reply;
    },

    subscribe: (cb: (m: ServerMessage) => void): (() => void) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },

    dispose: async (): Promise<void> => {
      disposed = true;
      for (const t of timers) clearTimeout(t);
      timers.clear();
      subs.clear();
      engine.dispose();
    },
  };
}
