/**
 * Session workspace — live output, file changes and activity, with the
 * prompt composer and interrupt control pinned to the bottom.
 */
import { useEffect, useRef, useState } from 'react';
import { StyleSheet, View } from 'react-native';
import type { ScrollViewInstance } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useCommands, commands } from '@/data/commands';
import { activityFor, agentById, hostById, sessionById } from '@/data/selectors';
import { relativeTime } from '@/utils/format';
import { useReducedMotion, useNow } from '@/hooks/useReducedMotion';
import { ProgressBar, StatusPill, Text } from '@/ui/primitives';
import { Screen, PageHeader, Card, EmptyState } from '@/ui/surface';
import {
  ActivityRow,
  FileRow,
  OutputBlock,
  PromptComposer,
  SegmentedTabs,
  Timeline,
} from '@/ui/lists';
import { Icon } from '@/ui/icons';

type TabKey = 'output' | 'files' | 'activity';

export default function SessionScreen() {
  const authed = useAppStore((s) => s.authed);
  const { id } = useLocalSearchParams<{ id: string }>();
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
          placeholder={`Message ${agentName}…`}
          hint={busy ? `${agentName} is working…` : undefined}
          onSubmit={(text) => void send(commands.submitPrompt(session.id, text), 'Prompt sent')}
          onInterrupt={() => void send(commands.interrupt(session.id), 'Session interrupted')}
        />
      }
      contentStyle={{ gap: spacing.xl }}
    >
      <StatusBar style="light" />

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

const styles = StyleSheet.create({
  headerBlock: { backgroundColor: colors.black },
  headerRow: { paddingBottom: spacing.sm },
  progressMeta: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  permBanner: { borderColor: colors.black, gap: spacing.sm },
  permBannerRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
