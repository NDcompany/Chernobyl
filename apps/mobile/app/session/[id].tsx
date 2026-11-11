/**
 * Session workspace. A transcript session on a host that speaks ADR-015
 * (`hello_ack.features` has transcript.v1) opens as the live conversation:
 * streaming transcript, approvals inside the tool cards, follow-ups while the
 * agent works, Stop, MCP and permission-mode chips. Every other session keeps
 * the classic view: output, file changes and activity, with the prompt
 * composer and interrupt control pinned to the bottom.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ActivityIndicator, Linking, Pressable, StyleSheet, Text as RNText, View } from 'react-native';
import type { ScrollViewInstance } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import type { Agent, Host, Session, TranscriptItem } from '@chernobyl/protocol';
import { colors, radii, spacing, typography } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useCommands, commands } from '@/data/commands';
import { useLiveEnabled, useLiveSession } from '@/data/live';
import { useWorkspaceEnabled } from '@/data/workspace';
import { workspaceForProject } from '@/utils/workspace';
import {
  activityFor,
  agentById,
  hostById,
  pendingPermissions,
  permissionById,
  sessionById,
} from '@/data/selectors';
import { relativeTime } from '@/utils/format';
import { useReducedMotion, useNow } from '@/hooks/useReducedMotion';
import { ProgressBar, SimulatedTag, StatusDot, StatusPill, Text } from '@/ui/primitives';
import { Screen, PageHeader, Card, ConfirmModal, EmptyState } from '@/ui/surface';
import {
  ActivityRow,
  FileRow,
  OutputBlock,
  PromptComposer,
  SegmentedTabs,
  Timeline,
} from '@/ui/lists';
import { McpChip, McpSheet, ModeChip, TranscriptList, type ApprovalDecision } from '@/ui/transcript';
import { Icon } from '@/ui/icons';

type TabKey = 'output' | 'files' | 'activity';

export default function SessionScreen() {
  const authed = useAppStore((s) => s.authed);
  // `focus`: a tool item to scroll to (Approvals tab and notification deep links)
  const { id, focus } = useLocalSearchParams<{ id: string; focus?: string }>();
  const liveOn = useLiveEnabled();
  const snapshot = useSnapshot();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const reduced = useReducedMotion();
  const send = useCommands();
  const now = useNow(30_000);
  const scrollRef = useRef<ScrollViewInstance>(null);
  const [tab, setTab] = useState<TabKey>('output');

  const session = sessionById(snapshot, String(id));
  const agent = session ? agentById(snapshot, session.agentId) : undefined;
  const host = session ? hostById(snapshot, session.hostId) : undefined;
  const agentName = agent?.name ?? 'Agent';
  const sessionPermissions = session
    ? snapshot.permissions.filter((p) => p.sessionId === session.id)
    : [];
  const pendingPermission = sessionPermissions.find((p) => p.status === 'pending');
  const activity = session
    ? activityFor(snapshot, new Set([session.id, ...sessionPermissions.map((p) => p.id)]))
    : [];

  const commandStates = useAppStore((s) => s.commandStates);
  const unknownCommand = session
    ? Object.values(commandStates).find(
        (c) => c.sessionId === session.id && c.state === 'unknown',
      )
    : undefined;
  const hasUnknown = Boolean(unknownCommand);

  const outputCount = session?.output.length ?? 0;
  useEffect(() => {
    if (tab !== 'output' || reduced || outputCount === 0) return;
    const t = setTimeout(() => scrollRef.current?.scrollToEnd({ animated: true }), 80);
    return () => clearTimeout(t);
  }, [outputCount, tab, reduced]);

  if (!authed) return <Redirect href="/sign-in" />;

  if (!session) {
    return (
      <Screen header={<PageHeader title="Session" />}>
        <EmptyState
          icon="layers"
          title="Session not found"
          caption="It may have been removed or the link is outdated."
          actionLabel="Back"
          onAction={() => (router.canGoBack() ? router.back() : router.replace('/sessions'))}
        />
      </Screen>
    );
  }

  if (session.transcript && liveOn) {
    return <ConversationScreen session={session} agent={agent} host={host} focusItemId={focus} />;
  }

  const busy = session.status === 'running' || session.status === 'waiting';

  const header = (
    <View style={styles.headerBlock}>
      <View style={[styles.headerRow, { paddingTop: insets.top + spacing.sm }]}>
        <PageHeader
          dark
          bordered={false}
          title={session.title}
          subtitle={`${agentName}${host ? ` · ${host.name}` : ''}`}
          right={<StatusPill status={session.status} inverse />}
        />
      </View>
    </View>
  );

  const tabs = [
    { key: 'output', label: 'Output' },
    { key: 'files', label: 'Files', badge: session.files.length || undefined },
    { key: 'activity', label: 'Activity', badge: activity.length || undefined },
  ];

  return (
    <Screen
      safeTop={false}
      header={header}
      scrollRef={scrollRef}
      footer={
        <PromptComposer
          busy={busy}
          disabled={hasUnknown}
          placeholder={`Message ${agentName}…`}
          hint={
            hasUnknown
              ? 'Outcome unknown — the host may have run it'
              : busy
                ? `${agentName} is working…`
                : undefined
          }
          onSubmit={(text) => void send(commands.submitPrompt(session.id, text), 'Prompt sent')}
          onInterrupt={() => void send(commands.interrupt(session.id), 'Session interrupted')}
        />
      }
      contentStyle={{ gap: spacing.xl }}
    >
      <StatusBar style="light" />

      {/* ADR-009: a session on a demo agent/host never reads as live */}
      {agent?.simulated || host?.simulated ? <SimulatedTag /> : null}

      {pendingPermission ? (
        <Card
          strong
          onPress={() => router.push(`/permission/${pendingPermission.id}`)}
          accessibilityLabel={`Approval needed: ${pendingPermission.title}. Open the request.`}
          style={styles.permBanner}
        >
          <View style={styles.permBannerRow}>
            <Icon name="shield-check" size={20} color={colors.black} />
            <View style={{ flex: 1, gap: 2 }}>
              <Text variant="smStrong">Approval needed</Text>
              <Text variant="xs" color={colors.text2} numberOfLines={1}>
                {pendingPermission.title} — {pendingPermission.command}
              </Text>
            </View>
            <Icon name="chevron-right" size={18} color={colors.text3} />
          </View>
        </Card>
      ) : null}

      {hasUnknown ? <UnknownOutcomeCard /> : null}

      <Card style={{ gap: spacing.sm }}>
        <ProgressBar value={session.progress} tone={session.status === 'completed' ? 'success' : 'dark'} />
        <View style={styles.progressMeta}>
          <Text variant="mono" color={colors.text3} numberOfLines={1} style={{ flex: 1 }}>
            {session.project}
          </Text>
          <Text variant="xs" color={colors.text2}>
            {session.progress}% · {relativeTime(session.lastActivityAt, now)}
          </Text>
        </View>
        {session.timeline.length > 0 ? <Timeline steps={session.timeline} /> : null}
      </Card>

      <SegmentedTabs tabs={tabs} active={tab} onChange={(k) => setTab(k as TabKey)} />

      {tab === 'output' ? (
        <View style={{ gap: spacing.md }}>
          {session.prompt ? (
            <View style={{ gap: 4 }}>
              <Text variant="eyebrow" color={colors.text3}>
                Last prompt
              </Text>
              <Text variant="sm" color={colors.text2}>
                {session.prompt}
              </Text>
            </View>
          ) : null}
          <OutputBlock lines={session.output} />
        </View>
      ) : null}

      {tab === 'files' ? (
        session.files.length === 0 ? (
          <EmptyState
            icon="folder"
            contour={false}
            title="No files changed"
            caption="Every file the agent creates, edits or deletes shows up here with its diff counts."
          />
        ) : (
          <Card style={{ paddingVertical: spacing.xs }}>
            {session.files.map((f, i) => (
              <View key={f.path} style={i < session.files.length - 1 ? styles.divider : undefined}>
                <FileRow file={f} />
              </View>
            ))}
          </Card>
        )
      ) : null}

      {tab === 'activity' ? (
        <Card style={{ paddingVertical: spacing.xs }}>
          {activity.length === 0 ? (
            <Text variant="sm" color={colors.text2} style={{ paddingVertical: spacing.md }}>
              No activity recorded for this session yet.
            </Text>
          ) : (
            activity.map((e, i) => (
              <View key={e.id} style={i < activity.length - 1 ? styles.divider : undefined}>
                <ActivityRow event={e} time={relativeTime(e.ts, now)} />
              </View>
            ))
          )}
        </Card>
      ) : null}
    </Screen>
  );
}

/** Mandate section 9: a command whose ack never came stays visibly unresolved. */
function UnknownOutcomeCard() {
  return (
    <Card style={{ borderColor: colors.warning, gap: spacing.xs }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing.sm }}>
        <StatusDot tone="warning" label="Outcome unknown" />
        <Text variant="bodyStrong" color={colors.warning}>
          Outcome unknown — the host may have run it
        </Text>
      </View>
      <Text variant="xs" color={colors.text2}>
        A command was sent to this session, but the acknowledgement timed out or the connection dropped.
      </Text>
    </Card>
  );
}

/** Agent links and sign-in pages open in the phone browser: http(s) only, after the UI's own confirm (T3). */
function openExternal(url: string): void {
  if (/^https?:\/\//i.test(url)) void Linking.openURL(url).catch(() => undefined);
}

interface ConversationProps {
  session: Session;
  agent?: Agent;
  host?: Host;
  /** deep link: the tool card to scroll to */
  focusItemId?: string;
}

/** ADR-015 §5: the live conversation, fed by useLiveSession (same transport stream as the snapshot). */
function ConversationScreen({ session, agent, host, focusItemId }: ConversationProps) {
  const insets = useSafeAreaInsets();
  const send = useCommands();
  const snapshot = useSnapshot();
  const now = useNow(30_000);
  const connected = useAppStore((s) => s.connection === 'connected');
  const commandStates = useAppStore((s) => s.commandStates);
  const { view, error } = useLiveSession(session.id);
  const router = useRouter();
  const workspaceOn = useWorkspaceEnabled();
  const [mcpOpen, setMcpOpen] = useState(false);
  const [confirmEnd, setConfirmEnd] = useState(false);

  const sessionId = session.id;
  const live = agent?.live;
  const agentName = agent?.name ?? 'Agent';
  const items = useMemo(() => view.items.filter((i): i is TranscriptItem => i.kind !== 'unknown'), [view.items]);
  const turns = useMemo(() => [...view.turns.values()], [view.turns]);
  // approval buttons come only from these snapshot records, never from transcript text (T3)
  const permissions = useMemo(
    () => pendingPermissions(snapshot, now).filter((p) => p.sessionId === sessionId),
    [snapshot, now, sessionId],
  );
  // the existing command path: ack errors toast, a lost ack becomes a visible unknown outcome
  const onDecide = useCallback(
    ({ permissionId, decision, revision, ...extra }: ApprovalDecision) =>
      send(commands.approve(permissionId, decision, revision, extra)),
    [send],
  );

  const hasUnknown = Object.values(commandStates).some(
    (c) =>
      c.state === 'unknown' &&
      (c.sessionId === sessionId ||
        (c.permissionId !== undefined && permissionById(snapshot, c.permissionId)?.sessionId === sessionId)),
  );
  const status = view.state.status;
  const busy = turns.some((t) => t.status === 'running') || status === 'running' || status === 'waiting';
  const ended = status === 'ended';
  const canEnd = live?.end === true && !ended;
  // ADR-016: the session's folder (Session.project is the workspace path)
  const ws = workspaceOn ? workspaceForProject(snapshot.agents, session.project) : undefined;

  const header = (
    <View style={styles.headerBlock}>
      <View style={[styles.headerRow, { paddingTop: insets.top + spacing.sm }]}>
        <PageHeader
          dark
          bordered={false}
          title={session.title}
          subtitle={`${agentName}${host ? ` · ${host.name}` : ''}`}
          right={<StatusPill status={session.status} inverse />}
        />
      </View>
      {view.mcp.length > 0 || live?.permissionModes || canEnd || ws ? (
        <View style={styles.chipRow}>
          <McpChip dark servers={view.mcp} onPress={() => setMcpOpen(true)} />
          {ws ? (
            <>
              <Pressable
                onPress={() => router.push({ pathname: '/files/[workspace]', params: { workspace: ws.id } })}
                accessibilityRole="button"
                accessibilityLabel={`Files of ${ws.label}`}
                style={({ pressed }) => [styles.endChip, pressed && styles.pressed]}
              >
                <Icon name="folder" size={12} color={colors.white} />
                <RNText style={[typography.xs, styles.endText]}>Files</RNText>
              </Pressable>
              <Pressable
                onPress={() => router.push({ pathname: '/changes/[workspace]', params: { workspace: ws.id } })}
                accessibilityRole="button"
                accessibilityLabel={`Git changes of ${ws.label}`}
                style={({ pressed }) => [styles.endChip, pressed && styles.pressed]}
              >
                <Icon name="diff" size={12} color={colors.white} />
                <RNText style={[typography.xs, styles.endText]}>Changes</RNText>
              </Pressable>
            </>
          ) : null}
          {live?.permissionModes ? (
            <ModeChip
              dark
              mode={view.state.permissionMode}
              modes={live.permissionModes}
              onChange={(mode) => void send(commands.setPermissionMode(sessionId, mode))}
            />
          ) : null}
          <View style={styles.flex} />
          {canEnd ? (
            <Pressable
              onPress={() => setConfirmEnd(true)}
              accessibilityRole="button"
              accessibilityLabel="End session"
              style={({ pressed }) => [styles.endChip, pressed && styles.pressed]}
            >
              <Icon name="stop" size={12} color={colors.white} />
              <RNText style={[typography.xs, styles.endText]}>End session</RNText>
            </Pressable>
          ) : null}
        </View>
      ) : null}
    </View>
  );

  return (
    <Screen
      safeTop={false}
      scroll={false}
      header={header}
      footer={
        <PromptComposer
          busy={busy}
          promptWhileBusy={live?.promptWhileBusy}
          disabled={hasUnknown || ended}
          placeholder={`Message ${agentName}…`}
          hint={
            hasUnknown
              ? 'Outcome unknown — the host may have run it'
              : ended
                ? 'This session has ended'
                : status === 'starting'
                  ? `Starting ${agentName}…`
                  : undefined
          }
          onSubmit={(text) => void send(commands.submitPrompt(sessionId, text))}
          onInterrupt={live?.interrupt ? () => void send(commands.interrupt(sessionId)) : undefined}
        />
      }
    >
      <StatusBar style="light" />

      {/* ADR-009: a session on a demo agent/host never reads as live */}
      {agent?.simulated || host?.simulated || hasUnknown || (!connected && view.epoch) ? (
        <View style={styles.notes}>
          {agent?.simulated || host?.simulated ? <SimulatedTag /> : null}
          {!connected && view.epoch ? (
            <Text variant="xs" color={colors.warning}>
              Not connected: showing what arrived so far. The conversation catches up on reconnect.
            </Text>
          ) : null}
          {hasUnknown ? <UnknownOutcomeCard /> : null}
        </View>
      ) : null}

      {view.epoch ? (
        <TranscriptList
          items={items}
          turns={turns}
          permissions={permissions}
          onDecide={onDecide}
          onOpenLink={openExternal}
          promptWhileBusy={live?.promptWhileBusy}
          agentKind={agent?.kind}
          truncatedBefore={view.truncatedBefore}
          focusItemId={focusItemId}
        />
      ) : (
        <View style={styles.loading}>
          {error ? (
            <Text variant="sm" color={colors.error} center accessibilityRole="alert">
              The live conversation is unavailable: {error}
            </Text>
          ) : (
            <>
              <ActivityIndicator color={colors.black} />
              <Text variant="sm" color={colors.text2} center>
                {connected ? 'Loading the conversation…' : 'Waiting for the connection to your desktop…'}
              </Text>
            </>
          )}
        </View>
      )}

      <McpSheet
        visible={mcpOpen}
        servers={view.mcp}
        onClose={() => setMcpOpen(false)}
        onAuth={(server, flow) => void send(commands.mcpAuth(sessionId, server, flow))}
        onAuthCallback={(server, url) => void send(commands.mcpAuthCallback(sessionId, server, url))}
        onReconnect={(server) => void send(commands.mcpReconnect(sessionId, server))}
        onOpenUrl={openExternal}
      />
      <ConfirmModal
        visible={confirmEnd}
        title="End this session?"
        message={`${agentName} stops on your PC and this conversation takes no more prompts.`}
        note="The conversation history stays on your PC."
        confirmLabel="End session"
        onConfirm={() => {
          setConfirmEnd(false);
          void send(commands.endSession(sessionId));
        }}
        onCancel={() => setConfirmEnd(false)}
      />
    </Screen>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  pressed: { opacity: 0.7 },
  headerBlock: { backgroundColor: colors.black },
  headerRow: { paddingBottom: spacing.sm },
  chipRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.sm,
    paddingHorizontal: spacing.xl,
    paddingBottom: spacing.md,
  },
  endChip: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.xs,
    minHeight: 32,
    paddingHorizontal: spacing.md,
    borderRadius: radii.pill,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.text2,
  },
  endText: { color: colors.white, fontWeight: '700' },
  notes: { gap: spacing.sm, paddingTop: spacing.md },
  loading: { flex: 1, alignItems: 'center', justifyContent: 'center', gap: spacing.md },
  progressMeta: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  permBanner: { borderColor: colors.black, gap: spacing.sm },
  permBannerRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
