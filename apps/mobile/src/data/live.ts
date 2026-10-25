/**
 * ADR-015 live sessions: one TranscriptView per session a screen shows,
 * folded by `applyTranscript` from the SAME transport message stream the
 * snapshot reducer consumes (TransportProvider feeds both). No second
 * connection and no fetch path. Views live in memory only and are dropped
 * when the last screen showing them blurs (threat model T2: never persisted).
 */
import { useEffect } from 'react';
import { useIsFocused } from 'expo-router';
import { create } from 'zustand';
import {
  FEATURE_TRANSCRIPT,
  applyTranscript,
  emptyTranscriptView,
  nextId,
  type ServerMessage,
  type TranscriptView,
} from '@chernobyl/protocol';
import { getTransport } from './transport';
import { useAppStore } from '@/store/appStore';

export interface LiveSession {
  /** `epoch === ''` until the first snapshot arrives */
  view: TranscriptView;
  /** the companion refused the subscription; shown instead of an endless spinner */
  error?: string;
}

export const useLiveStore = create<{ sessions: Record<string, LiveSession> }>(() => ({ sessions: {} }));

const EMPTY: LiveSession = { view: emptyTranscriptView() };
/** At most one re-subscribe per second per session: a burst of gapped frames sends one. */
const RESUBSCRIBE_GAP_MS = 1000;
/** Screens showing each session (the same session can sit twice on the stack). */
const holders = new Map<string, number>();
const lastSent = new Map<string, number>();
const retries = new Map<string, ReturnType<typeof setTimeout>>();
/** Sessions whose unwanted stream we already asked the companion to stop. */
const strays = new Set<string>();

function put(sessionId: string, next: LiveSession | undefined): void {
  useLiveStore.setState((s) => {
    const sessions = { ...s.sessions };
    if (next) sessions[sessionId] = next;
    else delete sessions[sessionId];
    return { sessions };
  });
}

/**
 * `session_subscribe` with the view's cursor: the companion replays the ops
 * after `seq` while the epoch matches, else sends a snapshot. Straight to
 * the transport: subscriptions never toast.
 */
function subscribe(sessionId: string): void {
  const live = useLiveStore.getState().sessions[sessionId];
  if (!live) return;
  lastSent.set(sessionId, Date.now());
  const { epoch, seq } = live.view;
  getTransport()
    .send({
      type: 'session_subscribe',
      commandId: nextId('sub'),
      sessionId,
      ...(epoch ? { epoch, afterSeq: seq } : {}),
    })
    .then((ack) => {
      const cur = useLiveStore.getState().sessions[sessionId];
      if (!cur || (ack.ok && !cur.error)) return;
      put(sessionId, { view: cur.view, error: ack.ok ? undefined : (ack.error ?? 'refused by the companion') });
    })
    // not sent, or no ack: the hook subscribes again when the transport is back
    .catch(() => undefined);
}

/** The reducer found a gap or a new epoch: re-subscribe from the cursor, at most once a second. */
function resubscribeSoon(sessionId: string): void {
  if (retries.has(sessionId)) return;
  const wait = Math.max(0, (lastSent.get(sessionId) ?? 0) + RESUBSCRIBE_GAP_MS - Date.now());
  retries.set(
    sessionId,
    setTimeout(() => {
      retries.delete(sessionId);
      subscribe(sessionId);
    }, wait),
  );
}

/** TransportProvider hands every server message here, next to the snapshot reducer. */
export function applyLiveMessage(msg: ServerMessage): void {
  if (msg.type !== 'transcript' && msg.type !== 'transcript_snapshot') return;
  const live = useLiveStore.getState().sessions[msg.sessionId];
  if (!live) {
    // no screen shows it: a frame in flight after the unsubscribe, or a subscribe the
    // transport replayed after a reconnect. End that subscription once.
    if (!strays.has(msg.sessionId)) {
      strays.add(msg.sessionId);
      getTransport()
        .send({ type: 'session_unsubscribe', commandId: nextId('sub'), sessionId: msg.sessionId })
        .catch(() => undefined);
    }
    return;
  }
  const { view, resubscribe } = applyTranscript(live.view, msg);
  if (view !== live.view) put(msg.sessionId, { view });
  if (resubscribe) resubscribeSoon(msg.sessionId);
}

function retain(sessionId: string): void {
  strays.delete(sessionId);
  holders.set(sessionId, (holders.get(sessionId) ?? 0) + 1);
  if (!useLiveStore.getState().sessions[sessionId]) put(sessionId, { view: emptyTranscriptView() });
}

function release(sessionId: string): void {
  const left = (holders.get(sessionId) ?? 1) - 1;
  if (left > 0) {
    holders.set(sessionId, left);
    return;
  }
  holders.delete(sessionId);
  clearTimeout(retries.get(sessionId));
  retries.delete(sessionId);
  put(sessionId, undefined);
  // only a live connection holds a subscription: offline, the companion dropped it with the
  // connection (and getTransport() must not build a transport while the provider boots)
  if (useAppStore.getState().connection !== 'connected') return;
  getTransport()
    .send({ type: 'session_unsubscribe', commandId: nextId('sub'), sessionId })
    .catch(() => undefined);
}

/** The host speaks ADR-015 (`hello_ack.features`). */
export function useLiveEnabled(): boolean {
  return useAppStore((s) => s.features.includes(FEATURE_TRANSCRIPT));
}

/**
 * ADR-015 §5 lifecycle: subscribed while the screen is focused, again each
 * time the transport returns to `connected` (with the view's epoch and last
 * seq), unsubscribed on blur. Mount it only for a transcript session on a
 * host with FEATURE_TRANSCRIPT.
 */
export function useLiveSession(sessionId: string): LiveSession {
  const focused = useIsFocused();
  const connected = useAppStore((s) => s.connection === 'connected');
  useEffect(() => {
    if (!focused) return;
    retain(sessionId);
    return () => release(sessionId);
  }, [focused, sessionId]);
  useEffect(() => {
    if (focused && connected) subscribe(sessionId);
  }, [focused, connected, sessionId]);
  return useLiveStore((s) => s.sessions[sessionId]) ?? EMPTY;
}
