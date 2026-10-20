/**
 * Sessions tab — status filter chips (All/Running/Waiting/Completed) plus
 * search; every row opens the session workspace.
 */
import { Redirect, useRouter } from 'expo-router';
import { StyleSheet, View } from 'react-native';
import { spacing, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { useAppStore, type SessionFilter } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { agentById, hostById, searchSessions } from '@/data/selectors';
import { Button, Chip, SearchField } from '@/ui/primitives';
import { Screen, PageHeader, EmptyState } from '@/ui/surface';
import { SessionRow } from '@/ui/lists';

const FILTERS: { key: SessionFilter; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'running', label: 'Running' },
  { key: 'waiting', label: 'Waiting' },
  { key: 'completed', label: 'Completed' },
];

export default function SessionsScreen() {
  const authed = useAppStore((s) => s.authed);
  const query = useAppStore((s) => s.sessionsQuery);
  const setQuery = useAppStore((s) => s.setQuery);
  const filter = useAppStore((s) => s.sessionFilter);
  const setFilter = useAppStore((s) => s.setSessionFilter);
  const snapshot = useSnapshot();
  const router = useRouter();

  if (!authed) return <Redirect href="/sign-in" />;

  const sessions = searchSessions(snapshot, query, filter);
  const countFor = (f: SessionFilter) =>
    searchSessions(snapshot, query, f).length;

  return (
    <Screen header={<PageHeader title="Sessions" onBack={null} />} bottomInset={TAB_BAR_HEIGHT}>
      <SearchField
        value={query}
        onChangeText={(v) => setQuery('sessionsQuery', v)}
        placeholder="Search sessions…"
      />

      <View style={styles.filters}>
        {FILTERS.map((f) => (
          <Chip
            key={f.key}
            label={f.label}
            count={countFor(f.key)}
            active={filter === f.key}
            onPress={() => setFilter(f.key)}
          />
        ))}
      </View>

      <Button
        label="New task"
        variant="secondary"
        icon="plus"
        onPress={() => router.push('/new-task')}
      />

      {sessions.length === 0 ? (
        filter === 'all' && !query.trim() ? (
          <EmptyState
            icon="layers"
            title="No sessions yet"
            caption="Start a task and the session workspace appears here."
            actionLabel="New task"
            onAction={() => router.push('/new-task')}
          />
        ) : (
          <EmptyState
            icon="search"
            title="Nothing in this filter"
            caption="No session matches the current filter and search."
            actionLabel="Reset filters"
            onAction={() => {
              setFilter('all');
              setQuery('sessionsQuery', '');
            }}
          />
        )
      ) : (
        sessions.map((s) => (
          <SessionRow
            key={s.id}
            session={s}
            agent={agentById(snapshot, s.agentId)}
            host={hostById(snapshot, s.hostId)}
            onPress={() => router.push(`/session/${s.id}`)}
          />
        ))
      )}
    </Screen>
  );
}

const styles = StyleSheet.create({
  filters: { flexDirection: 'row', flexWrap: 'wrap', gap: spacing.sm, marginTop: -spacing.sm },
});
