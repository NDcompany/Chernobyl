/**
 * Permission decision evaluation — the single fail-closed authority
 * (PRD §13, gate requirement G2).
 *
 * Rules, in precedence order:
 *  1. Incomplete native metadata (missing native request id, no supported
 *     decisions, missing revision) → refuse. The runtime's shape is
 *     unknowable, so no decision is safe.
 *  2. Not pending / already resolved → refuse (never re-drive a resolved
 *     native request).
 *  3. Past the native presentation deadline → refuse (expired).
 *  4. Revision mismatch → refuse (stale revision; the request changed).
 *  5. Decision not in supportedDecisions → refuse. NEVER translate a
 *     decision into one the runtime did not offer.
 *
 * Evaluation never mutates: the caller applies the resolution and bumps
 * the revision only after an `ok` verdict.
 */

export type PermissionDecisionCode =
  | 'approve-once'
  | 'approve-session'
  | 'reject';

export type PermissionDecisionErrorCode =
  | 'incomplete-metadata'
  | 'not-pending'
  | 'expired'
  | 'stale-revision'
  | 'unsupported-decision';

export interface PermissionDecisionRequest {
  status: string;
  expiresAt: number;
  /** Opaque id the native runtime uses for this request, if known. */
  nativeRequestId?: string;
  /** Decisions the native runtime actually offers. */
  supportedDecisions?: readonly PermissionDecisionCode[];
  revision?: number;
}

export interface PermissionDecisionInput {
  decision: PermissionDecisionCode;
  /** Revision the decision was made against (fresh from the rendered record). */
  revision: number;
}

export type PermissionVerdict =
  | { ok: true }
  | { ok: false; code: PermissionDecisionErrorCode };

export function evaluatePermissionDecision(
  request: PermissionDecisionRequest,
  input: PermissionDecisionInput,
  now: number = Date.now(),
): PermissionVerdict {
  if (
    !request.nativeRequestId ||
    !Array.isArray(request.supportedDecisions) ||
    request.supportedDecisions.length === 0 ||
    typeof request.revision !== 'number'
  ) {
    return { ok: false, code: 'incomplete-metadata' };
  }
  if (request.status !== 'pending') return { ok: false, code: 'not-pending' };
  if (now >= request.expiresAt) return { ok: false, code: 'expired' };
  if (input.revision !== request.revision) return { ok: false, code: 'stale-revision' };
  if (!request.supportedDecisions.includes(input.decision)) {
    return { ok: false, code: 'unsupported-decision' };
  }
  return { ok: true };
}

/** Human-readable, secret-free messages for refusal codes. */
export const PERMISSION_DECISION_ERRORS: Readonly<
  Record<PermissionDecisionErrorCode, string>
> = {
  'incomplete-metadata': 'permission request metadata is incomplete',
  'not-pending': 'request is no longer pending',
  expired: 'request expired before a decision was made',
  'stale-revision': 'request changed — review it again',
  'unsupported-decision': 'decision is not supported by this runtime',
};
