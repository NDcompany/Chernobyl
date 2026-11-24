/**
 * Confirm-step warnings for a pairing (pure; unit-tested). A `chernobyl://connect-host`
 * link can come from any app or web page, so the step says what the tap would
 * replace and where the payload came from.
 */
export function pairingNotices(currentHost: string | null, fromLink: boolean): string[] {
  const notices: string[] = [];
  if (currentHost) notices.push(`This replaces your current host ${currentHost}`);
  if (fromLink) {
    notices.push('Opened from another app or link \u2014 only continue if you started this pairing');
  }
  return notices;
}

/**
 * Whether a stored access token survives saving a pairing for `newHost` (pure;
 * unit-tested). A token belongs to the host it was entered for, so it is never
 * sent to another host's endpoints (security review): kept when it is bound to
 * `newHost`, or when it is unbound and either no pairing is stored (typed before
 * the first pairing, the documented setup order) or the stored pairing is
 * `newHost` (a token saved before tokens were bound belongs to the current pairing).
 */
export function keepTokenOnPair(
  tokenHost: string | null,
  currentHost: string | null,
  newHost: string,
): boolean {
  const owner = tokenHost ?? currentHost;
  return owner === null || owner === newHost;
}
