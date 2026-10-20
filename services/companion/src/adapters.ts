/**
 * Adapter discovery and lifecycle management (PRD §25 Phase 2, §19.3).
 *
 * Discovery resolves configured entries into adapter instances. An entry
 * that cannot be resolved is RECORDED as an error, never silently ignored
 * (PRD §7.3 honesty applies to configuration too).
 *
 * Lifecycle guarantees:
 *  - One adapter failing (send, subscribe callback, dispose) never crashes
 *    the companion or touches sibling adapters — each adapter's handlers
 *    are isolated (exit criterion: "adapter failure does not crash the
 *    companion").
 *  - A failing adapter goes `degraded` with a bounded exponential-backoff
 *    probe (jittered) and returns to `healthy` when health() passes
 *    (PRD §19.3).
 */
import {
  AdapterError,
  createMockAdapter,
  type AgentAdapter,
  type AdapterHealth,
} from '@chernobyl/adapter-sdk';
import type { ClientMessage, ServerMessage } from '@chernobyl/protocol';
import type { AdapterEntryConfig } from './config';

export type AdapterStatus = 'healthy' | 'degraded' | 'discovery-failed';

export interface AdapterReport {
  id: string;
  type: string;
  status: AdapterStatus;
  detail?: string;
  failures: number;
}

export interface DiscoveredAdapter {
  entry: AdapterEntryConfig;
  adapter?: AgentAdapter;
  /** Discovery failure reason (unsupported type, factory error...). */
  error?: string;
}

export interface AdapterManagerHooks {
  /** Called for every event an adapter emits (already enveloped by the adapter). */
  onEvent: (adapterId: string, msg: ServerMessage) => void;
  /** Called on every status transition (healthy/degraded/discovery-failed). */
  onStatus: (report: AdapterReport) => void;
}

const PROBE_BASE_MS = 5_000;
const PROBE_CAP_MS = 60_000;

export type Factories = Record<string, (options?: Record<string, unknown>) => AgentAdapter>;

const BUILTIN_FACTORIES: Factories = {
  mock: (options) => createMockAdapter(options),
};

/** Resolves configured entries into adapter instances (or recorded errors). */
export function discoverAdapters(
  entries: AdapterEntryConfig[],
  factories: Factories = BUILTIN_FACTORIES,
): DiscoveredAdapter[] {
  return entries
    .filter((e) => e.enabled !== false)
    .map((entry) => {
      const factory = factories[entry.type];
      if (!factory) {
        return { entry, error: `unsupported adapter type '${entry.type}'` };
      }
      try {
        return { entry, adapter: factory(entry.options) };
      } catch (err) {
        return { entry, error: `adapter factory failed: ${(err as Error).message}` };
      }
    });
}

export class AdapterManager {
  private reports = new Map<string, AdapterReport>();
  private unsubs = new Map<string, () => void>();
  private disposed = new Set<string>();
  private nextProbeAt = new Map<string, number>();
  private probeTimer?: ReturnType<typeof setInterval>;
  private stopped = false;

  constructor(
    private readonly discovered: DiscoveredAdapter[],
    private readonly hooks: AdapterManagerHooks,
    private readonly probeIntervalMs: number,
  ) {
    for (const d of discovered) {
      this.reports.set(d.entry.id, {
        id: d.entry.id,
        type: d.entry.type,
        status: d.error ? 'discovery-failed' : 'healthy',
        detail: d.error ?? 'started',
        failures: 0,
      });
    }
  }

  async start(): Promise<void> {
    for (const d of this.discovered) {
      if (!d.adapter) {
        this.hooks.onStatus({ ...this.reports.get(d.entry.id)! });
        continue;
      }
      // subscribe BEFORE health so no event is missed; isolate the handler
      const unsub = d.adapter.subscribe((msg) => {
        try {
          this.hooks.onEvent(d.entry.id, msg);
        } catch {
          // a storage/persist failure must not break the adapter's loop —
          // it is latched fail-closed elsewhere
        }
      });
      this.unsubs.set(d.entry.id, unsub);
      await this.markHealth(d.entry);
    }
    if (this.discovered.some((d) => this.reports.get(d.entry.id)?.status === 'degraded')) {
      this.scheduleProbe();
    }
  }

  private async markHealth(entry: AdapterEntryConfig): Promise<void> {
    const d = this.discovered.find((x) => x.entry.id === entry.id);
    if (!d?.adapter || this.disposed.has(entry.id)) return;
    let health: AdapterHealth;
    try {
      health = await d.adapter.health();
    } catch (err) {
      this.setReport(entry.id, { status: 'degraded', detail: `health() threw: ${(err as Error).message}` });
      return;
    }
    if (health.ok) {
      this.setReport(entry.id, { status: 'healthy', detail: health.detail ?? 'healthy', resetFailures: true });
      this.nextProbeAt.delete(entry.id);
    } else {
      this.setReport(entry.id, {
        status: 'degraded',
        detail: health.detail ?? 'unhealthy',
        bumpFailure: true,
      });
    }
  }

  private setReport(
    id: string,
    patch: { status?: AdapterStatus; detail?: string; bumpFailure?: boolean; resetFailures?: boolean },
  ): void {
    const prev = this.reports.get(id);
    if (!prev) return;
    const next: AdapterReport = { ...prev };
    if (patch.status) next.status = patch.status;
    if (patch.detail !== undefined) next.detail = patch.detail;
    if (patch.bumpFailure) next.failures += 1;
    if (patch.resetFailures) next.failures = 0;
    const changed =
      prev.status !== next.status ||
      prev.detail !== next.detail ||
      prev.failures !== next.failures;
    this.reports.set(id, next);
    if (changed) this.hooks.onStatus({ ...next });
  }

  private scheduleProbe(): void {
    if (this.probeTimer || this.stopped) return;
    this.probeTimer = setInterval(() => void this.probe(), this.probeIntervalMs);
    this.probeTimer.unref?.();
  }

  /** Bounded exponential backoff with jitter before re-probing (PRD §19.3). */
  private backoffMs(failures: number): number {
    const exp = Math.min(PROBE_BASE_MS * 2 ** Math.min(failures, 6), PROBE_CAP_MS);
    return Math.round(exp * (0.8 + Math.random() * 0.4));
  }

  async probe(): Promise<void> {
    if (this.stopped) return;
    const now = Date.now();
    for (const d of this.discovered) {
      const report = this.reports.get(d.entry.id);
      if (!report || report.status !== 'degraded' || !d.adapter) continue;
      const notBefore = this.nextProbeAt.get(d.entry.id) ?? 0;
      if (now < notBefore) continue;
      this.nextProbeAt.set(d.entry.id, now + this.backoffMs(report.failures));
      await this.markHealth(d.entry);
    }
  }

  /**
   * Routes a command to an adapter and returns its acceptance reply.
   * AdapterError is mapped to `{ error }` — the caller decides fail-closed
   * semantics; a send failure also marks the adapter degraded.
   */
  async submit(
    adapterId: string,
    msg: ClientMessage,
  ): Promise<{ reply?: ServerMessage; error?: string }> {
    const d = this.discovered.find((x) => x.entry.id === adapterId);
    if (!d?.adapter || this.disposed.has(adapterId)) {
      return { error: `adapter '${adapterId}' is not available` };
    }
    try {
      const reply = await d.adapter.send(msg);
      return { reply };
    } catch (err) {
      const detail = err instanceof AdapterError ? `${err.code}: ${err.message}` : (err as Error).message;
      this.setReport(adapterId, { status: 'degraded', detail, bumpFailure: true });
      this.scheduleProbe();
      return { error: detail };
    }
  }

  /**
   * Picks an adapter for a command: explicit id when given, otherwise the
   * single healthy adapter (Phase 2 has no session→adapter routing table
   * yet — that lands with real adapters in Phase 3).
   */
  routeAdapter(preferred?: string): string | undefined {
    if (preferred) {
      const r = this.reports.get(preferred);
      return r && r.status === 'healthy' ? preferred : undefined;
    }
    const healthy = [...this.reports.values()].filter((r) => r.status === 'healthy');
    return healthy.length === 1 ? healthy[0].id : undefined;
  }

  reportsSnapshot(): AdapterReport[] {
    return [...this.reports.values()].map((r) => ({ ...r }));
  }

  async stop(): Promise<void> {
    this.stopped = true;
    if (this.probeTimer) clearInterval(this.probeTimer);
    this.probeTimer = undefined;
    for (const [, unsub] of this.unsubs) {
      try {
        unsub();
      } catch {
        /* isolate */
      }
    }
    this.unsubs.clear();
    // dispose every adapter even if one throws (isolation applies to shutdown too)
    for (const d of this.discovered) {
      if (!d.adapter) continue;
      this.disposed.add(d.entry.id);
      try {
        await d.adapter.dispose();
      } catch {
        this.setReport(d.entry.id, { status: 'degraded', detail: 'dispose() threw' });
      }
    }
  }
}
