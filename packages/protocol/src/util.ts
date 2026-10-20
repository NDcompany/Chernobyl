/**
 * Protocol constants and small shared helpers (dependency-free, platform-neutral).
 */

/** Protocol version. Bumped to 2 for the command lifecycle + native permissions. */
export const PROTOCOL_VERSION = 2 as const;

/** Permission requests older than this are stale and must not be decided on. */
export const PERMISSION_TTL_MS = 2 * 60_000;

/** Heartbeat cadence; a connection with 3 missed beats is considered dead. */
export const HEARTBEAT_MS = 15_000;
export const HEARTBEAT_MISS_LIMIT = 3;

let seq = 0;
/** Monotonic-ish id generator; host and client prefix with their scope. */
export function nextId(prefix: string): string {
  seq += 1;
  return `${prefix}_${Date.now().toString(36)}${seq.toString(36)}`;
}

interface Unrefable {
  unref?: () => void;
}

/** unref a timer when the platform has one (Node), no-op elsewhere (RN). */
export function unrefTimer(timer: unknown): void {
  (timer as Unrefable | null | undefined)?.unref?.();
}

/** True when a pending request has passed its deadline. */
export function isStalePermission(
  p: { status: string; expiresAt: number },
  now: number = Date.now(),
): boolean {
  return p.status === 'pending' && now >= p.expiresAt;
}
