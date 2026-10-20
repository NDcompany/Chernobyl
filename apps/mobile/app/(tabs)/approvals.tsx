/**
 * Approvals tab — pending requests (live expiry countdowns) on top,
 * decided ones below, dimmed, with an audit-style history feel.
 */
import { Redirect, useRouter } from 'expo-router';
import { View } from 'react-native';
import { colors, spacing, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import {
  agentById,
  decidedPermissions,
  hostById,
  pendingPermissions,
  sessionById,
} from '@/data/selectors';
import { useNow } from '@/hooks/useReducedMotion';
import { Text } from '@/ui/primitives';
import { Screen, PageHeader, EmptyState } from '@/ui/surface';
import { PermissionCard } from '@/ui/lists';

export default function ApprovalsScreen() {
  const authed = useAppStore((s) => s.authed);
  const snapshot = useSnapshot();
  const router = useRouter();
  const now = useNow(1000);

  if (!authed) return <Redirect href="/sign-in" />;

  const pending = pendingPermissions(snapshot, now);
  const decided = decidedPermissions(snapshot);
  const staleCount = snapshot.permissions.filter((p) => p.status !== 'pending').length - decided.length;

  return (
    <Screen header={<PageHeader title="Approvals" onBack={null} />} bottomInset={TAB_BAR_HEIGHT}>
      <Text variant="sm" color={colors.text2}>
        Review exactly what an agent wants to run. Decisions are enforced by the host, not by this
        screen.
      </Text>

      {pending.length === 0 ? (
        <EmptyState
          icon="shield-check"
          title="No approvals waiting"
          caption="When an agent needs permission, the request lands here with a live countdown."
          actionLabel="See activity"
          onAction={() => router.push('/sessions')}
        />
      ) : (
        <View style={{ gap: spacing.md }}>
          {pending.map((p) => (
            <PermissionCard
              key={p.id}
              variant="full"
              now={now}
              permission={p}
              agent={agentById(snapshot, p.agentId)}
              host={hostById(snapshot, p.hostId)}
              session={sessionById(snapshot, p.sessionId)}
              onPress={() => router.push(`/permission/${p.id}`)}
            />
          ))}
        </View>
      )}

      {decided.length > 0 ? (
        <View style={{ gap: spacing.md, marginTop: pending.length === 0 ? 0 : spacing.lg }}>
          <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
            Decided
          </Text>
          {decided.map((p) => (
            <PermissionCard
              key={p.id}
              variant="full"
              permission={p}
              resolved
              agent={agentById(snapshot, p.agentId)}
              host={hostById(snapshot, p.hostId)}
              session={sessionById(snapshot, p.sessionId)}
              onPress={() => router.push(`/permission/${p.id}`)}
            />
          ))}
          {staleCount > 0 ? (
            <Text variant="xs" color={colors.text3} center>
              {staleCount} expired without a decision.
            </Text>
          ) : null}
        </View>
      ) : null}
    </Screen>
  );
}
