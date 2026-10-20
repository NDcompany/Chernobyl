/**
 * Agent details — dark header slab with identity, then capabilities,
 * sessions and the activity of this agent.
 */
import { StyleSheet, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import {
  activityFor,
  agentById,
  hostById,
  activeSessions,
  sessionsForAgent,
} from '@/data/selectors';
import { agentKindLabel, agentStatusLabel, agentTone, relativeTime } from '@/utils/format';
import { useNow } from '@/hooks/useReducedMotion';
import { Button, Chip, StatusDot, Text } from '@/ui/primitives';
import { Screen, Card, EmptyState, PageHeader } from '@/ui/surface';
import { ActivityRow, DetailRow, SessionRow } from '@/ui/lists';
import { AgentIcon, BlackShape } from '@/ui/graphics';

export default function AgentDetailScreen() {
  const authed = useAppStore((s) => s.authed);
  const { id } = useLocalSearchParams<{ id: string }>();
  const snapshot = useSnapshot();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const now = useNow(30_000);

  if (!authed) return <Redirect href="/sign-in" />;

  const agent = agentById(snapshot, String(id));
  const host = agent ? hostById(snapshot, agent.hostId) : undefined;
  const sessions = agent ? sessionsForAgent(snapshot, agent.id) : [];
  const activeCount = agent ? activeSessions(snapshot, agent.id).length : 0;
  const activity = agent
    ? activityFor(snapshot, new Set([agent.id, ...sessions.map((s) => s.id)]))
    : [];

  const header = agent ? (
    <View style={styles.headerBlock}>
      <View style={[styles.headerContent, { paddingTop: insets.top + spacing.sm }]}>
        <View style={styles.headerRow}>
          <Button
            label="Back"
            variant="primary"
            size="sm"
            fullWidth={false}
            ringColor={colors.white}
            style={styles.backBtn}
            onPress={() => router.back()}
            accessibilityLabel="Go back"
          />
        </View>
        <View style={styles.identity}>
          <AgentIcon kind={agent.kind} size={56} variant="outline" />
          <View style={{ flex: 1, gap: 4 }}>
            <Text variant="h2" color={colors.white} numberOfLines={1}>
              {agent.name}
            </Text>
            <View style={styles.identityMeta}>
              <StatusDot tone={agentTone(agent.state)} label={agentStatusLabel(agent.state)} inverse />
              <Text variant="xs" color="rgba(255,255,255,0.72)" numberOfLines={1}>
                {agentKindLabel(agent.kind)} · {host?.name ?? 'unknown host'}
              </Text>
            </View>
          </View>
        </View>
      </View>
      <View style={styles.headerWave}>
        <BlackShape variant="detail" />
      </View>
    </View>
  ) : undefined;

  if (!agent) {
    return (
      <Screen header={<PageHeader title="Agent" />}>
        <EmptyState
          icon="bot"
          title="Agent not found"
          caption="It may have been removed from its host since you opened this link."
          actionLabel="Back"
          onAction={() => (router.canGoBack() ? router.back() : router.replace('/agents'))}
        />
      </Screen>
    );
  }

  return (
    <Screen safeTop={false} header={header}>
      <StatusBar style="light" />

      <Card style={{ gap: 0, paddingVertical: spacing.xs }}>
        <DetailRow label="Type" value={agentKindLabel(agent.kind)} />
        <View style={styles.divider} />
        <DetailRow label="Host" value={host?.name ?? 'Unknown host'} />
        <View style={styles.divider} />
        <DetailRow label="Status" value={agentStatusLabel(agent.state)} />
        <View style={styles.divider} />
        <DetailRow
          label="Active sessions"
          value={`${activeCount} of ${sessions.length}`}
        />
      </Card>

      <View style={{ gap: spacing.md }}>
        <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
          Capabilities
        </Text>
        <View style={styles.chips}>
          {agent.capabilities.length > 0 ? (
            agent.capabilities.map((c) => <Chip key={c} label={c} />)
          ) : (
            <Text variant="sm" color={colors.text3}>
              No capabilities declared.
            </Text>
          )}
        </View>
      </View>

      <Button
        label="Start Session"
        icon="arrow-right"
        disabled={!agent.acceptsNewSession}
        onPress={() => router.push(`/new-task?agent=${agent.id}`)}
        accessibilityLabel={`Start a session with ${agent.name}`}
      />
      {!agent.acceptsNewSession ? (
        <Text variant="xs" color={colors.text3} center>
          This agent is not accepting new sessions right now.
        </Text>
      ) : null}

      <View style={{ gap: spacing.md }}>
        <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
          Sessions
        </Text>
        {sessions.length === 0 ? (
          <Card>
            <Text variant="sm" color={colors.text2}>
              No sessions yet for this agent.
            </Text>
          </Card>
        ) : (
          sessions.slice(0, 4).map((s) => (
            <SessionRow
              key={s.id}
              session={s}
              agent={agent}
              host={host}
              onPress={() => router.push(`/session/${s.id}`)}
            />
          ))
        )}
        {sessions.length > 4 ? (
          <Button label="See all sessions" variant="secondary" onPress={() => router.push('/sessions')} />
        ) : null}
      </View>

      <View style={{ gap: spacing.xs }}>
        <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
          Recent activity
        </Text>
        <Card style={{ paddingVertical: spacing.xs }}>
          {activity.length === 0 ? (
            <Text variant="sm" color={colors.text2} style={{ paddingVertical: spacing.md }}>
              Nothing has happened on this agent yet.
            </Text>
          ) : (
            activity.map((e, i) => (
              <View key={e.id} style={i < activity.length - 1 ? styles.divider : undefined}>
                <ActivityRow event={e} time={relativeTime(e.ts, now)} />
              </View>
            ))
          )}
        </Card>
      </View>
    </Screen>
  );
}

const styles = StyleSheet.create({
  headerBlock: { backgroundColor: colors.white },
  headerContent: { backgroundColor: colors.black, paddingBottom: spacing.sm },
  headerWave: { height: 50, overflow: 'hidden', backgroundColor: colors.white },
  headerRow: { flexDirection: 'row', paddingHorizontal: spacing.md },
  backBtn: {
    minHeight: 40,
    borderColor: 'rgba(255,255,255,0.45)',
    backgroundColor: 'transparent',
  },
  identity: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    paddingHorizontal: spacing.xl,
    paddingVertical: spacing.md,
  },
  identityMeta: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, flexWrap: 'wrap' },
  chips: { flexDirection: 'row', flexWrap: 'wrap', gap: spacing.sm },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
