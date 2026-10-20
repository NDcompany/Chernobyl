/**
 * CHERNOBYL adapter SDK — the contract every agent runtime adapter must
 * satisfy (PRD §22). Defining this in Phase 1 with a conforming MockAdapter
 * means the contract is executable before any real adapter exists.
 *
 * Safety contract (G4, from the OpenCode probe — see
 * docs/reverse-engineering/opencode.md §8-9):
 *  1. An ack/HTTP-accept is NEVER the outcome. Runtime errors delivered
 *     asynchronously after acceptance must surface as `command_result`
 *     failures on the event channel (G1 + G4.1).
 *  2. Every native response is validated (content-type + shape). Unknown
 *     routes can answer HTML with HTTP 200 — a status code alone proves
 *     nothing (G4.2, see validateJsonResponse).
 *  3. Never operate on a data store the adapter does not exclusively own
 *     without a proven isolation strategy: report the conflict through
 *     health() and refuse commands instead of spawning a second runtime
 *     (G4.3).
 */
import type { Agent, AgentKind, ClientMessage, ServerMessage, Session } from '@chernobyl/protocol';
import type { Capability } from '@chernobyl/domain';

export type AdapterErrorCode =
  /** The adapter does not handle this command type at all (host-level). */
  | 'unsupported'
  /** The native runtime is unreachable, disposed, or owned by another instance. */
  | 'runtime-unavailable'
  /** A native response failed validation (bad content-type / unparseable). */
  | 'invalid-response'
  | 'unknown';

export class AdapterError extends Error {
  constructor(
    readonly code: AdapterErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'AdapterError';
  }
}

export interface AdapterHealth {
  ok: boolean;
  /** Human-readable diagnosis; required when not ok. */
  detail?: string;
}

/**
 * Fault-injection options every adapter must accept so the safety
 * conformance suite can exercise G4.1–G4.3. Real adapters implement these
 * against their real transport (e.g. `nativeResponse` reflects what the
 * runtime actually answered; `lateError` injects a post-accept failure).
 */
export interface AdapterFaultOptions {
  /** G4.3: another runtime instance owns the data directory. */
  existingInstance?: 'attach' | 'refuse';
  /** G4.2: the native transport answers HTML instead of JSON. */
  nativeResponse?: 'json' | 'html';
  /** G4.1: the runtime fails asynchronously after acceptance. */
  lateError?: boolean;
}

export interface AgentAdapter {
  readonly id: string;
  readonly kind: AgentKind;
  readonly displayName: string;

  /**
   * Capability declaration. Must be structurally valid (unique ids;
   * `detail` required for anything not 'available') and must include at
   * least one non-available entry — honesty over marketing (PRD §7.3).
   */
  capabilities(): Capability[];

  health(): Promise<AdapterHealth>;
  listAgents(): Promise<Agent[]>;
  listSessions(): Promise<Session[]>;

  /**
   * ACCEPTANCE ONLY. Returns the command's ack (`ok:false` = refused at
   * acceptance; no lifecycle follows). The outcome of an accepted mutating
   * command arrives later as exactly ONE `command_result` event (G1).
   *
   * Throws AdapterError for adapter/runtime faults — never a bare Error.
   */
  send(msg: ClientMessage): Promise<ServerMessage | undefined>;

  /**
   * Entity + command-lifecycle events. Delivery order is the authority's
   * order; every event carries an EventEnvelope with a strictly increasing
   * `seq` (G envelopes).
   */
  subscribe(cb: (m: ServerMessage) => void): () => void;

  dispose(): Promise<void>;
}
