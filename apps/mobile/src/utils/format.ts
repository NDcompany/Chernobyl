/** Display formatting shared across screens. */
import type { AgentState, HostStatus, SessionStatus } from '@chernobyl/protocol';
import type { Tone } from '@/ui/primitives';

export function relativeTime(ts: number, now: number): string {
  const d = Math.max(0, now - ts);
  if (d < 45_000) return 'just now';
  if (d < 90_000) return '1m ago';
  const m = Math.floor(d / 60_000);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const days = Math.floor(h / 24);
  if (days === 1) return 'yesterday';
  return `${days}d ago`;
}

export function countdown(expiresAt: number, now: number): string {
  const left = expiresAt - now;
  if (left <= 0) return 'expired';
  const total = Math.ceil(left / 1000);
  if (total < 60) return `expires in ${total}s`;
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `expires in ${m}:${String(s).padStart(2, '0')}`;
}

export function hostTone(status: HostStatus): Tone {
  switch (status) {
    case 'connected':
      return 'success';
    case 'connecting':
      return 'warning';
    default:
      return 'neutral';
  }
}

export function hostStatusLabel(status: HostStatus): string {
  switch (status) {
    case 'connected':
      return 'Connected';
    case 'connecting':
      return 'Connecting…';
    default:
      return 'Offline';
  }
}

export function agentTone(state: AgentState): Tone {
  switch (state) {
    case 'online':
      return 'success';
    case 'busy':
      return 'warning';
    default:
      return 'neutral';
  }
}

export function agentStatusLabel(state: AgentState): string {
  switch (state) {
    case 'online':
      return 'Online';
    case 'busy':
      return 'Busy';
    case 'idle':
      return 'Idle';
    default:
      return 'Offline';
  }
}

export function sessionStatusLabel(status: SessionStatus): string {
  switch (status) {
    case 'running':
      return 'Running';
    case 'waiting':
      return 'Waiting';
    case 'completed':
      return 'Completed';
    case 'interrupted':
      return 'Interrupted';
    default:
      return 'Idle';
  }
}

export function agentKindLabel(kind: string): string {
  switch (kind) {
    case 'claude-code':
      return 'Claude Code';
    case 'opencode':
      return 'OpenCode';
    case 'codex':
      return 'Codex';
    case 'browser':
      return 'Browser automation';
    case 'local':
      return 'Local agent';
    default:
      return 'Custom agent';
  }
}
