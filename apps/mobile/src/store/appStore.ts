/**
 * Client-side state (Zustand): session/auth flags, settings, search and
 * filter state, toasts, connection status and in-flight command dedupe.
 * Server state (hosts/agents/sessions/permissions) lives in TanStack Query.
 */
import { create } from 'zustand';
import { createJSONStorage, persist } from 'zustand/middleware';
import { applyCommandLifecycle, type CommandEntry } from '@chernobyl/domain';
import { nextId, type AgentKind, type ServerMessage } from '@chernobyl/protocol';
import type { TransportStatus } from '@/data/transport';
import { sqliteStorage } from '@/platform/storage';

export interface CommandRecord extends CommandEntry {
  permissionId?: string;
}

export type CommandRecordTracker = Record<string, CommandRecord>;

export type SessionFilter = 'all' | 'running' | 'waiting' | 'completed';
/** Mirrors the transport union exactly — includes handshaking/denied (E2E). */
export type ConnectionStatus = TransportStatus;
export type ToastTone = 'default' | 'success' | 'warning' | 'error';

export interface ToastItem {
  id: string;
  message: string;
  tone: ToastTone;
}

export interface Settings {
  push: boolean;
  approvalAlerts: boolean;
  sessionAlerts: boolean;
  reduceMotion: 'system' | 'on';
  integrations: Record<AgentKind, boolean>;
}

export interface AppUser {
  name: string;
  email: string;
  initials: string;
}

const DEFAULT_SETTINGS: Settings = {
  push: true,
  approvalAlerts: true,
  sessionAlerts: true,
  reduceMotion: 'system',
  integrations: {
    'claude-code': true,
    opencode: true,
    codex: true,
    browser: true,
    local: false,
    custom: true,
  },
};

const DEFAULT_USER: AppUser = {
  name: 'Nadia Petrov',
  email: 'nadia@chernobyl.dev',
  initials: 'NP',
};

export interface AppState {
  onboarded: boolean;
  authed: boolean;
  user: AppUser;
  settings: Settings;
  connection: ConnectionStatus;
  /**
   * Bumped when the active transport must be rebuilt (a new pairing was
   * saved). The provider re-runs its boot effect on change; not persisted.
   */
  transportEpoch: number;
  toasts: ToastItem[];
  homeQuery: string;
  agentsQuery: string;
  sessionsQuery: string;
  sessionFilter: SessionFilter;
  /** identical concurrent commands share a key and are sent only once */
  inFlight: Record<string, true>;
  /**
   * Command lifecycle per commandId (G1): acceptance and outcome are
   * distinct, and a terminal state is final. Server messages merge here
   * through the domain FSM — NOT through the ack.
   */
  commandStates: CommandRecordTracker;

  completeOnboarding: () => void;
  signIn: () => void;
  signOut: () => void;
  addToast: (message: string, tone?: ToastTone) => void;
  dismissToast: (id: string) => void;
  setConnection: (c: ConnectionStatus) => void;
  bumpTransport: () => void;
  setQuery: (key: 'homeQuery' | 'agentsQuery' | 'sessionsQuery', value: string) => void;
  setSessionFilter: (f: SessionFilter) => void;
  /** returns false when an identical command is already in flight */
  markInFlight: (key: string) => boolean;
  clearInFlight: (key: string) => void;
  /** records a command with unknown outcome (timed out or dropped after send) */
  setCommandUnknown: (
    commandId: string,
    meta?: { sessionId?: string; permissionId?: string },
  ) => void;
  /** feeds command_progress / command_result / ack through the domain FSM and reconciles */
  trackCommand: (msg: ServerMessage) => void;
  updateSettings: (patch: Partial<Settings>) => void;
  toggleIntegration: (kind: AgentKind) => void;
}

const TOAST_MS = 3400;

export const useAppStore = create<AppState>()(
  persist(
    (set, get) => ({
      onboarded: false,
      authed: false,
      user: DEFAULT_USER,
      settings: DEFAULT_SETTINGS,
      connection: 'connecting',
      transportEpoch: 0,
      toasts: [],
      homeQuery: '',
      agentsQuery: '',
      sessionsQuery: '',
      sessionFilter: 'all',
      inFlight: {},
      commandStates: {},

      completeOnboarding: () => set({ onboarded: true }),
      signIn: () => set({ authed: true }),
      signOut: () => set({ authed: false, homeQuery: '', agentsQuery: '', sessionsQuery: '' }),

      addToast: (message, tone = 'default') => {
        const id = nextId('t');
        set((s) => ({ toasts: [...s.toasts.slice(-2), { id, message, tone }] }));
        setTimeout(() => get().dismissToast(id), TOAST_MS);
      },
      dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),

      setConnection: (connection) => set({ connection }),

      bumpTransport: () => set((s) => ({ transportEpoch: s.transportEpoch + 1 })),

      setQuery: (key, value) => set({ [key]: value } as Pick<AppState, typeof key>),

      setSessionFilter: (sessionFilter) => set({ sessionFilter }),

      markInFlight: (key) => {
        if (get().inFlight[key]) return false;
        set((s) => ({ inFlight: { ...s.inFlight, [key]: true } }));
        return true;
      },
      clearInFlight: (key) =>
        set((s) => {
          const next = { ...s.inFlight };
          delete next[key];
          return { inFlight: next };
        }),

      setCommandUnknown: (commandId, meta) =>
        set((s) => ({
          commandStates: {
            ...s.commandStates,
            [commandId]: {
              state: 'unknown',
              sessionId: meta?.sessionId ?? s.commandStates[commandId]?.sessionId,
              permissionId: meta?.permissionId ?? s.commandStates[commandId]?.permissionId,
              error: 'outcome unknown — the host may have run it',
            },
          },
        })),

      trackCommand: (msg) => {
        if (msg.type !== 'command_progress' && msg.type !== 'command_result' && msg.type !== 'ack') {
          return;
        }

        set((s) => {
          const current = s.commandStates[msg.commandId];

          // If current command has an UNKNOWN outcome, reconcile to the real result
          // when replayed command's ack/progress/result arrives after reconnect
          if (current?.state === 'unknown') {
            if (msg.type === 'command_result') {
              return {
                commandStates: {
                  ...s.commandStates,
                  [msg.commandId]: {
                    state: msg.status,
                    sessionId: msg.sessionId ?? current.sessionId,
                    permissionId: msg.permissionId ?? current.permissionId,
                    error: msg.error,
                  },
                },
              };
            }
            if (msg.type === 'command_progress') {
              return {
                commandStates: {
                  ...s.commandStates,
                  [msg.commandId]: {
                    state: msg.state,
                    sessionId: msg.sessionId ?? current.sessionId,
                    permissionId: current.permissionId,
                  },
                },
              };
            }
            if (msg.type === 'ack') {
              if (msg.ok) {
                return {
                  commandStates: {
                    ...s.commandStates,
                    [msg.commandId]: {
                      state: 'accepted',
                      sessionId: msg.sessionId ?? current.sessionId,
                      permissionId: msg.permissionId ?? current.permissionId,
                    },
                  },
                };
              } else {
                return {
                  commandStates: {
                    ...s.commandStates,
                    [msg.commandId]: {
                      state: 'failed',
                      error: msg.error ?? 'Command rejected',
                      sessionId: msg.sessionId ?? current.sessionId,
                      permissionId: msg.permissionId ?? current.permissionId,
                    },
                  },
                };
              }
            }
          }

          // Standard lifecycle for non-unknown commands
          if (msg.type === 'command_progress' || msg.type === 'command_result') {
            const next = applyCommandLifecycle(s.commandStates, msg);
            if (next === s.commandStates) return {};
            const merged: CommandRecordTracker = { ...(next as CommandRecordTracker) };
            if (current?.permissionId && merged[msg.commandId]) {
              merged[msg.commandId] = {
                ...merged[msg.commandId],
                permissionId: current.permissionId,
              };
            }
            return { commandStates: merged };
          }

          return {};
        });
      },

      updateSettings: (patch) => set((s) => ({ settings: { ...s.settings, ...patch } })),
      toggleIntegration: (kind) =>
        set((s) => ({
          settings: {
            ...s.settings,
            integrations: { ...s.settings.integrations, [kind]: !s.settings.integrations[kind] },
          },
        })),
    }),
    {
      name: 'chernobyl-app',
      storage: createJSONStorage(() => sqliteStorage),
      partialize: (s) => ({
        onboarded: s.onboarded,
        authed: s.authed,
        user: s.user,
        settings: s.settings,
      }),
    },
  ),
);
