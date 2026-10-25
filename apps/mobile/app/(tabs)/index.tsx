/**
 * Home dashboard — black flowing header with live status + search, then
 * overview stats, active agents, pending approvals and recent activity.
 */
import { Pressable, ScrollView, StyleSheet, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import type { Snapshot } from '@chernobyl/protocol';
import { colors, spacing, radii, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useLiveEnabled } from '@/data/live';
import {
  activeSessions,
  agentById,
  counts,
  hostById,
  pendingPermissions,
  permissionHref,
  recentActivity,
  searchAgents,
  searchSessions,
  sessionById,
} from '@/data/selectors';
import { useNow } from '@/hooks/useReducedMotion';
import { relativeTime, transportStatusLabel, transportTone } from '@/utils/format';
import { Button, SearchField, SectionTitle, StatCard, StatusDot, Text } from '@/ui/primitives';
import { Screen } from '@/ui/surface';
import { AgentCard, ActivityRow, PermissionCard, SessionRow } from '@/ui/lists';
import { BlackShape, Wordmark } from '@/ui/graphics';

/** Maps an activity refId to the screen it belongs to. */
function routeForRef(snapshot: Snapshot, refId: string | undefined): string | undefined {
  if (!refId) return undefined;
  if (sessionById(snapshot, refId)) return `/session/${refId}`;
  if (snapshot.permissions.some((p) => p.id === refId)) return `/permission/${refId}`;
  if (snapshot.agents.some((a) => a.id === refId)) return `/agent/${refId}`;
  if (hostById(snapshot, refId)) return '/settings/hosts';
  return undefined;
}

export default function DashboardScreen() {
  const snapshot = useSnapshot();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const now = useNow(1000);
  const query = useAppStore((s) => s.homeQuery);
  const setQuery = useAppStore((s) => s.setQuery);
  const connection = useAppStore((s) => s.connection);
  const user = useAppStore((s) => s.user);
  const live = useLiveEnabled();

  const c = counts(snapshot, now);
  const pending = pendingPermissions(snapshot, now);
  const waiting = snapshot.sessions.filter((s) => s.status === 'waiting');
  const connectedHosts = snapshot.hosts.filter((h) => h.status === 'connected');
  const offlineHosts = snapshot.hosts.length - connectedHosts.length;
  const offlineCaption = offlineHosts > 0 ? `${offlineHosts} offline` : 'all online';
  // ADR-009: a hosts summary built from demo data says so
  const simulatedHosts = snapshot.hosts.filter((h) => h.simulated).length;
  const hostsCaption =
    simulatedHosts === 0
      ? offlineCaption
      : `${simulatedHosts === snapshot.hosts.length ? 'Simulated' : `${simulatedHosts} simulated`} · ${offlineCaption}`;

  const searching = query.trim().length > 0;
  const foundAgents = searching ? searchAgents(snapshot, query) : [];
  const foundSessions = searching ? searchSessions(snapshot, query, 'all') : [];

  // every transport state gets its own truthful label: a denied credential
  // must never read as "Connecting…" (section 9)
  const connectionLabel =
    connection === 'connected'
      ? `${c.hosts} host${c.hosts === 1 ? '' : 's'} online`
      : connection === 'denied'
        ? 'Access denied — re-pair this phone'
        : transportStatusLabel(connection);
  const connectionTone = transportTone(connection);

  const header = (
    <View style={styles.headerBlock}>
      <View style={[styles.headerContent, { paddingTop: insets.top + spacing.md }]}>
        <View style={styles.headerTop}>
          <StatusDot tone={connectionTone} label={connectionLabel} inverse />
          <Pressable
            onPress={() => router.push('/profile')}
            focusable
            accessible
            accessibilityRole="button"
            accessibilityLabel="Open profile"
            style={({ pressed }) => [styles.avatar, pressed && { opacity: 0.7 }]}
          >
            <Text variant="xs" color={colors.white} style={{ fontWeight: '700' }}>
              {user.initials}
            </Text>
          </Pressable>
        </View>
        <Wordmark size={16} color={colors.white} />
        <SearchField
          value={query}
          onChangeText={(v) => setQuery('homeQuery', v)}
          placeholder="Search agents and sessions…"
          onDark
        />
      </View>
      <View style={styles.headerWave}>
        <BlackShape variant="header" />
      </View>
    </View>
  );

  return (
    <Screen
      safeTop={false}
      header={header}
      bottomInset={TAB_BAR_HEIGHT}
      contentStyle={{ gap: spacing.xxl }}
    >
      <StatusBar style="light" />
      {searching ? (
        <View style={{ gap: spacing.xl }}>
          <SectionTitle>Results</SectionTitle>
          {foundAgents.length === 0 && foundSessions.length === 0 ? (
            <View style={styles.emptyBox}>
              <Text variant="sm" color={colors.text2} center>
                Nothing matches “{query.trim()}”.
              </Text>
              <Button
                label="Clear search"
                variant="secondary"
                size="sm"
                fullWidth={false}
                onPress={() => setQuery('homeQuery', '')}
              />
            </View>
          ) : (
            <>
              {foundAgents.slice(0, 5).map((a) => (
                <AgentCard
                  key={a.id}
                  agent={a}
                  host={hostById(snapshot, a.hostId)}
                  activeCount={activeSessions(snapshot, a.id).length}
                  totalCount={snapshot.sessions.filter((s) => s.agentId === a.id).length}
                  onPress={() => router.push(`/agent/${a.id}`)}
                />
              ))}
              {foundSessions.slice(0, 5).map((s) => (
                <SessionRow
                  key={s.id}
                  session={s}
                  agent={agentById(snapshot, s.agentId)}
                  host={hostById(snapshot, s.hostId)}
                  onPress={() => router.push(`/session/${s.id}`)}
                />
              ))}
            </>
          )}
        </View>
      ) : (
        <>
          {/* overview */}
          <View style={{ gap: spacing.md }}>
            <SectionTitle>Overview</SectionTitle>
            <View style={styles.statGrid}>
              <StatCard
                style={styles.statCell}
                label="Connected hosts"
                value={String(c.hosts)}
                caption={hostsCaption}
                onPress={() => router.push('/settings/hosts')}
              />
              <StatCard
                style={styles.statCell}
                label="Active agents"
                value={String(c.agents)}
                caption={`${snapshot.agents.length} total`}
                onPress={() => router.push('/agents')}
              />
              <StatCard
                style={styles.statCell}
                label="Running sessions"
                value={String(c.running)}
                caption={waiting.length > 0 ? `${waiting.length} waiting` : 'nothing waiting'}
                onPress={() => router.push('/sessions')}
              />
              <StatCard
                style={styles.statCell}
                label="Pending approvals"
                value={String(c.pending)}
                caption={c.pending > 0 ? 'needs your review' : 'all clear'}
                onPress={() => router.push('/approvals')}
              />
            </View>
          </View>

          {/* active agents */}
          <View style={{ gap: spacing.md }}>
            <SectionTitle actionLabel="See all" onAction={() => router.push('/agents')}>
              Active agents
            </SectionTitle>
            {snapshot.agents.length === 0 ? (
              <View style={styles.emptyBox}>
                <Text variant="sm" color={colors.text2} center>
                  No agents yet. Connect a host to discover them.
                </Text>
              </View>
            ) : (
              <ScrollView
                horizontal
                showsHorizontalScrollIndicator={false}
                contentContainerStyle={{ gap: spacing.md, paddingRight: spacing.xl }}
              >
                {snapshot.agents.map((a) => (
                  <AgentCard
                    key={a.id}
                    variant="tile"
                    agent={a}
                    host={hostById(snapshot, a.hostId)}
                    activeCount={activeSessions(snapshot, a.id).length}
                    totalCount={snapshot.sessions.filter((s) => s.agentId === a.id).length}
                    onPress={() => router.push(`/agent/${a.id}`)}
                  />
                ))}
              </ScrollView>
            )}
          </View>

          {/* pending approvals */}
          <View style={{ gap: spacing.md }}>
            <SectionTitle actionLabel={c.pending > 0 ? 'Review all' : undefined} onAction={() => router.push('/approvals')}>
              Pending actions
            </SectionTitle>
            {pending.length === 0 ? (
              <View style={styles.emptyBox}>
                <Text variant="sm" color={colors.text2} center>
                  No approvals waiting — you&apos;re up to date.
                </Text>
              </View>
            ) : (
              pending.slice(0, 3).map((p) => (
                <PermissionCard
                  key={p.id}
                  now={now}
                  permission={p}
                  agent={agentById(snapshot, p.agentId)}
                  host={hostById(snapshot, p.hostId)}
                  onPress={() => router.push(permissionHref(snapshot, p, live))}
                />
              ))
            )}
          </View>

          {/* recent activity */}
          <View style={{ gap: spacing.xs }}>
            <SectionTitle>Recent activity</SectionTitle>
            <View style={styles.activityCard}>
              {recentActivity(snapshot, 6).map((e, i, arr) => (
                <View key={e.id} style={i < arr.length - 1 ? styles.activityDivider : undefined}>
                  <ActivityRow
                    event={e}
                    time={relativeTime(e.ts, now)}
                    onPress={
                      routeForRef(snapshot, e.refId)
                        ? () => router.push(routeForRef(snapshot, e.refId) as string)
                        : undefined
                    }
                  />
                </View>
              ))}
              {snapshot.activity.length === 0 ? (
                <Text variant="sm" color={colors.text2} style={{ paddingVertical: spacing.md }}>
                  Activity from your agents will show up here.
                </Text>
              ) : null}
            </View>
          </View>

          <Button
            label="New Task"
            icon="arrow-right"
            onPress={() => router.push('/new-task')}
            accessibilityLabel="Start a new task"
            style={{ marginTop: spacing.sm }}
          />
        </>
      )}
    </Screen>
  );
}

const styles = StyleSheet.create({
  headerBlock: { backgroundColor: colors.white },
  headerContent: {
    paddingHorizontal: spacing.xl,
    paddingBottom: spacing.lg,
    gap: spacing.lg,
    backgroundColor: colors.black,
  },
  headerTop: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
  },
  avatar: {
    width: 34,
    height: 34,
    borderRadius: 17,
    borderWidth: 1,
    borderColor: 'rgba(255,255,255,0.45)',
    alignItems: 'center',
    justifyContent: 'center',
  },
  headerWave: { height: 54, overflow: 'hidden', backgroundColor: colors.white },

  statGrid: { flexDirection: 'row', flexWrap: 'wrap', gap: spacing.md },
  statCell: { flexGrow: 1, minWidth: '45%' },

  emptyBox: {
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.surface2,
    padding: spacing.lg,
    gap: spacing.md,
    alignItems: 'center',
  },
  activityCard: {
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.white,
    paddingHorizontal: spacing.lg,
    paddingVertical: spacing.xs,
  },
  activityDivider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
