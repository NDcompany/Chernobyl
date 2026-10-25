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
 *  5. The decision must be one the runtime offered. NEVER translate it.
 *     ADR-015: an explicit `optionId` must name one of the request's options
 *     in the same bucket. Without one, the decision must be in
 *     supportedDecisions and, on a request with options, selects the first
 *     non-persistent option of its bucket: a legacy decision never lands on
 *     a persistent rule.
 *  6. ADR-015 `message`: only when the selected option takes one, or for a
 *     reject on a request without options.
 *  7. ADR-015 `answers`: only for a `question` detail, keyed by its question
 *     ids, at most one answer unless multiSelect, labels from that
 *     question's options unless allowFreeText.
 * Rules 5–7 refuse as `unsupported-decision`.
 *
 * Evaluation never mutates: the caller applies the resolution and bumps
 * the revision only after an `ok` verdict. The verdict carries the selected
 * option for the adapter that writes the native answer.
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

/** ADR-015: what the authority reads of a runtime's own choice. */
export interface PermissionDecisionOption {
  id: string;
  decision: PermissionDecisionCode;
  scope: 'once' | 'session' | 'persistent';
  takesMessage?: boolean;
}

/** ADR-015: what the authority reads of a `question` detail's question. */
export interface PermissionDecisionQuestion {
  id: string;
  options: ReadonlyArray<{ label: string }>;
  multiSelect: boolean;
  allowFreeText: boolean;
}

export interface PermissionDecisionRequest {
  status: string;
  expiresAt: number;
  /** Opaque id the native runtime uses for this request, if known. */
  nativeRequestId?: string;
  /** Decisions the native runtime actually offers. */
  supportedDecisions?: readonly PermissionDecisionCode[];
  revision?: number;
  /** ADR-015: the runtime's own choices; never empty when present. */
  options?: readonly PermissionDecisionOption[];
  /** ADR-015: only a `question` detail is read (for `answers`). */
  detail?: { kind: string; questions?: readonly PermissionDecisionQuestion[] };
}

export interface PermissionDecisionInput {
  decision: PermissionDecisionCode;
  /** Revision the decision was made against (fresh from the rendered record). */
  revision: number;
  /** ADR-015: a PermissionOption id; its bucket must equal `decision`. */
  optionId?: string;
  /** ADR-015: free-text reason the model sees. */
  message?: string;
  /** ADR-015: question id → chosen labels or free text. */
  answers?: Readonly<Record<string, readonly string[]>>;
}

export type PermissionVerdict =
  | { ok: true; option?: PermissionDecisionOption }
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
  const options = request.options ?? [];
  if (input.optionId === undefined && !request.supportedDecisions.includes(input.decision)) {
    return { ok: false, code: 'unsupported-decision' };
  }
  const option =
    input.optionId !== undefined
      ? options.find((o) => o.id === input.optionId && o.decision === input.decision)
      : legacyOption(options, input.decision);
  // an explicit option must exist; a request with options must select one
  if (!option && (input.optionId !== undefined || options.length > 0)) {
    return { ok: false, code: 'unsupported-decision' };
  }
  if (
    input.message !== undefined &&
    !(option ? option.takesMessage === true : input.decision === 'reject')
  ) {
    return { ok: false, code: 'unsupported-decision' };
  }
  if (
    input.answers !== undefined &&
    Object.keys(input.answers).length > 0 &&
    !answersFit(request.detail, input.answers)
  ) {
    return { ok: false, code: 'unsupported-decision' };
  }
  return option ? { ok: true, option } : { ok: true };
}

/**
 * ADR-015: the supportedDecisions of a request with options, exactly the
 * buckets a legacy decision can select, so an old phone never shows a
 * button that maps to a persistent rule.
 */
export function legacyDecisions(
  options: readonly PermissionDecisionOption[],
): PermissionDecisionCode[] {
  return (['approve-once', 'approve-session', 'reject'] as const).filter(
    (d) => legacyOption(options, d) !== undefined,
  );
}

/**
 * The option a legacy (option-unaware) decision selects: the first
 * non-persistent one of its bucket. `takesMessage` does not disqualify it,
 * because the message is optional.
 */
function legacyOption(
  options: readonly PermissionDecisionOption[],
  decision: PermissionDecisionCode,
): PermissionDecisionOption | undefined {
  return options.find((o) => o.decision === decision && o.scope !== 'persistent');
}

function answersFit(
  detail: PermissionDecisionRequest['detail'],
  answers: Readonly<Record<string, readonly string[]>>,
): boolean {
  const questions = (detail?.kind === 'question' ? detail.questions : undefined) ?? [];
  return Object.entries(answers).every(([id, labels]) => {
    const q = questions.find((x) => x.id === id);
    return (
      q !== undefined &&
      (q.multiSelect || labels.length <= 1) &&
      (q.allowFreeText || labels.every((l) => q.options.some((o) => o.label === l)))
    );
  });
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
