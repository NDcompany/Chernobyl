/**
 * Agents tab — search + full list with per-agent session counts.
 */
import { Redirect, useRouter } from 'expo-router';
import { StyleSheet, View } from 'react-native';
import { colors, spacing, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { activeSessions, hostById, searchAgents, sessionsForAgent } from '@/data/selectors';
import { Button, SearchField, Text } from '@/ui/primitives';
import { Screen, PageHeader, EmptyState } from '@/ui/surface';
import { AgentCard } from '@/ui/lists';

export default function AgentsScreen() {
  const authed = useAppStore((s) => s.authed);
  const query = useAppStore((s) => s.agentsQuery);
  const setQuery = useAppStore((s) => s.setQuery);
  const snapshot = useSnapshot();
  const router = useRouter();

  if (!authed) return <Redirect href="/sign-in" />;

  const agents = searchAgents(snapshot, query);

  return (
    <Screen header={<PageHeader title="Agents" onBack={null} />} bottomInset={TAB_BAR_HEIGHT}>
      <SearchField
        value={query}
        onChangeText={(v) => setQuery('agentsQuery', v)}
        placeholder="Search agents, hosts, capabilities…"
      />

      <View style={styles.metaRow}>
        <Text variant="xs" color={colors.text3}>
          {agents.length} of {snapshot.agents.length} agents
        </Text>
        <Button
          label="New task"
          variant="ghost"
          size="sm"
          fullWidth={false}
          onPress={() => router.push('/new-task')}
        />
      </View>

      {agents.length === 0 ? (
        query.trim() ? (
          <EmptyState
            icon="search"
            title="No matching agents"
            caption={`Nothing matches “${query.trim()}”. Try another name, host or capability.`}
            actionLabel="Clear search"
            onAction={() => setQuery('agentsQuery', '')}
          />
        ) : (
          <EmptyState
            icon="bot"
            title="No agents connected"
            caption="Connect a host and its agents appear here automatically."
            actionLabel="Connect a host"
            onAction={() => router.push('/connect-host')}
          />
        )
      ) : (
        agents.map((a) => (
          <AgentCard
            key={a.id}
            agent={a}
            host={hostById(snapshot, a.hostId)}
            activeCount={activeSessions(snapshot, a.id).length}
            totalCount={sessionsForAgent(snapshot, a.id).length}
            onPress={() => router.push(`/agent/${a.id}`)}
          />
        ))
      )}
    </Screen>
  );
}

const styles = StyleSheet.create({
  metaRow: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    marginTop: -spacing.sm,
  },
});
