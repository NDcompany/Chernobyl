/**
 * Capability model (PRD §7.3). Adapters declare capabilities individually
 * and honestly: unsupported capabilities are explicit values, never
 * absent promises (gate requirement: "unsupported capabilities are
 * represented explicitly").
 */

export type CapabilityStatus = 'available' | 'unavailable' | 'limited' | 'dependent-config';

export interface Capability {
  /** Stable identifier, e.g. 'session.message.send'. */
  id: string;
  status: CapabilityStatus;
  /** Required whenever status is not 'available'. */
  detail?: string;
}

/** A capability may be used when it is fully available. */
export function isUsable(c: Capability): boolean {
  return c.status === 'available';
}

/** Structural validity for capability declarations (adapters + conformance). */
export function isValidCapability(c: Capability): boolean {
  if (!c.id || typeof c.id !== 'string') return false;
  if (c.status !== 'available' && !c.detail) return false;
  return (
    c.status === 'available' ||
    c.status === 'unavailable' ||
    c.status === 'limited' ||
    c.status === 'dependent-config'
  );
}
