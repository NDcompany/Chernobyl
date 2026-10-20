/**
 * Client-side state (Zustand): session/auth flags, settings, search and
 * filter state, toasts, connection status and in-flight command dedupe.
 * Server state (hosts/agents/sessions/permissions) lives in TanStack Query.
 */
import { create } from 'zustand';
import { createJSONStorage, persist } from 'zustand/middleware';
import { nextId, type AgentKind } from '@chernobyl/protocol';
import { sqliteStorage } from '@/platform/storage';

export type SessionFilter = 'all' | 'running' | 'waiting' | 'completed';
export type ConnectionStatus = 'connecting' | 'connected' | 'reconnecting' | 'offline';
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
  toasts: ToastItem[];
  homeQuery: string;
  agentsQuery: string;
  sessionsQuery: string;
  sessionFilter: SessionFilter;
  /** identical concurrent commands share a key and are sent only once */
  inFlight: Record<string, true>;

  completeOnboarding: () => void;
  signIn: () => void;
  signOut: () => void;
  addToast: (message: string, tone?: ToastTone) => void;
  dismissToast: (id: string) => void;
  setConnection: (c: ConnectionStatus) => void;
  setQuery: (key: 'homeQuery' | 'agentsQuery' | 'sessionsQuery', value: string) => void;
  setSessionFilter: (f: SessionFilter) => void;
  /** returns false when an identical command is already in flight */
  markInFlight: (key: string) => boolean;
  clearInFlight: (key: string) => void;
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
      toasts: [],
      homeQuery: '',
      agentsQuery: '',
      sessionsQuery: '',
      sessionFilter: 'all',
      inFlight: {},

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
