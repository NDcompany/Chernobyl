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
