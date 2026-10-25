/**
 * Permission request — exact command, args, path, risk and honest scope
 * explanations. Approve once / Reject decide immediately; approving for the
 * whole session goes through a confirmation.
 */
import { useState } from 'react';
import { StyleSheet, View } from 'react-native';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import type { PermissionDecision } from '@chernobyl/protocol';
import { isStalePermission } from '@chernobyl/protocol';
import { colors, spacing, radii } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useCommands, commands } from '@/data/commands';
import { agentById, hostById, permissionById, sessionById } from '@/data/selectors';
import { useNow } from '@/hooks/useReducedMotion';
import { countdown, relativeTime } from '@/utils/format';
import { Button, RiskPill, SimulatedTag, StatusDot, Text } from '@/ui/primitives';
import { Screen, PageHeader, Card, ConfirmModal, EmptyState } from '@/ui/surface';
import { MonoBlock } from '@/ui/lists';
import { AgentIcon } from '@/ui/graphics';
import { Icon } from '@/ui/icons';

export default function PermissionScreen() {
  const authed = useAppStore((s) => s.authed);
  const { id } = useLocalSearchParams<{ id: string }>();
  const snapshot = useSnapshot();
  const router = useRouter();
  const now = useNow(1000);
  const send = useCommands();
  const [confirmSession, setConfirmSession] = useState(false);
  const [busy, setBusy] = useState<'once' | 'reject' | 'scope' | null>(null);
  const commandStates = useAppStore((s) => s.commandStates);

  if (!authed) return <Redirect href="/sign-in" />;

  const permission = permissionById(snapshot, String(id));

  if (!permission) {
    return (
      <Screen header={<PageHeader title="Approval request" />}>
        <EmptyState
          icon="shield-check"
          title="Request not found"
          caption="It may have expired and been cleaned up by the host."
          actionLabel="Back to approvals"
          onAction={() => (router.canGoBack() ? router.back() : router.replace('/approvals'))}
        />
      </Screen>
    );
  }

  const agent = agentById(snapshot, permission.agentId);
  const host = hostById(snapshot, permission.hostId);
  const session = sessionById(snapshot, permission.sessionId);

  const unknownCommand = Object.values(commandStates).find(
    (c) => c.permissionId === permission.id && c.state === 'unknown',
  );
  const isUnknown = Boolean(unknownCommand);

  const decided = permission.status === 'approved' || permission.status === 'rejected';
  const livePending = permission.status === 'pending' && !isStalePermission(permission, now);
  const expired = permission.status === 'pending' && !livePending;

  const decide = (decision: PermissionDecision, key: 'once' | 'reject' | 'scope') => {
    setBusy(key);
    void (async () => {
      const ack = await send(
        commands.approve(permission.id, decision, permission.revision),
        decision === 'reject' ? 'Request rejected' : 'Approved for this run',
      );
      setBusy(null);
      if (ack?.ok) {
        if (router.canGoBack()) router.back();
        else router.replace('/approvals');
      }
    })();
  };

  const leave = () => {
    if (router.canGoBack()) router.back();
    else router.replace('/approvals');
  };

  return (
    <Screen
      header={
        <PageHeader
          title="Approval request"
          subtitle={relativeTime(permission.requestedAt, now)}
        />
      }
    >
      {/* identity */}
      <Card style={styles.head}>
        <AgentIcon kind={agent?.kind ?? 'custom'} size={44} variant="outline" />
        <View style={{ flex: 1, gap: 3 }}>
          <Text variant="h3" numberOfLines={2}>
            {permission.title}
          </Text>
          <Text variant="xs" color={colors.text2} numberOfLines={1}>
            {agent?.name ?? 'Agent'} · {host?.name ?? 'host'}
            {session ? ` · ${session.title}` : ''}
          </Text>
          {agent?.simulated || host?.simulated ? <SimulatedTag /> : null}
        </View>
        <RiskPill risk={permission.risk} />
      </Card>

      {/* context + exact command */}
      <Card style={{ gap: spacing.md }}>
        <View style={{ gap: 4 }}>
          <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
            Why the agent asks
          </Text>
          <Text variant="sm" color={colors.text}>
            {permission.context}
          </Text>
        </View>
        <MonoBlock caption={`Exact command (${permission.action})`} text={permission.command} />
        {permission.args.length > 0 ? (
          <View style={{ gap: 4 }}>
            <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
              Arguments
            </Text>
            <Text variant="mono" color={colors.text2} selectable>
              {permission.args.join('  ')}
            </Text>
          </View>
        ) : null}
        {permission.directory ? (
          <View style={styles.pathRow}>
            <Icon name="folder" size={14} color={colors.text3} />
            <Text variant="mono" color={colors.text2} numberOfLines={1} style={{ flex: 1 }} selectable>
              {permission.directory}
            </Text>
          </View>
        ) : null}
        <View style={styles.expiryRow}>
          <Icon name="clock" size={14} color={expired ? colors.error : colors.text3} />
          <Text variant="xs" color={expired ? colors.error : colors.text3}>
            {livePending
              ? countdown(permission.expiresAt, now)
              : expired
                ? 'Expired — the host will no longer accept a decision'
                : `Decided ${permission.decidedAt ? relativeTime(permission.decidedAt, now) : 'just now'}`}
          </Text>
        </View>
      </Card>

      {decided ? (
        <Card style={[styles.decidedBox, { borderColor: permission.status === 'approved' ? colors.success : colors.error }]}>
          <Text variant="bodyStrong" color={permission.status === 'approved' ? colors.success : colors.error}>
            {permission.status === 'approved'
              ? permission.decision === 'approve-session'
                ? 'Approved for this session'
                : 'Approved once'
              : 'Rejected'}
          </Text>
          <Text variant="sm" color={colors.text2}>
            {permission.status === 'approved'
              ? 'The host has let the action through. The decision is recorded in the audit trail.'
              : 'The agent did not run this action and must ask again or stop.'}
          </Text>
          <Button label="Back to approvals" variant="secondary" onPress={leave} />
        </Card>
      ) : null}

      {expired ? (
        <Card style={[styles.decidedBox, { borderColor: colors.error }]}>
          <Text variant="bodyStrong" color={colors.error}>
            This request expired
          </Text>
          <Text variant="sm" color={colors.text2}>
            Requests are valid for two minutes. The host stopped waiting for an answer — the agent
            will retry or halt on its own.
          </Text>
          <Button label="Back to approvals" variant="secondary" onPress={leave} />
        </Card>
      ) : null}

      {isUnknown && !decided ? (
        <Card style={[styles.decidedBox, { borderColor: colors.warning }]}>
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing.sm }}>
            <StatusDot tone="warning" label="Outcome unknown" />
            <Text variant="bodyStrong" color={colors.warning}>
              Outcome unknown — the host may have run it
            </Text>
          </View>
          <Text variant="sm" color={colors.text2}>
            Your decision was sent, but the acknowledgement timed out or the connection dropped. It may or may not have been applied — the approval list shows the host’s real state once connected.
          </Text>
        </Card>
      ) : null}

      {livePending ? (
        <>
          <View style={{ gap: spacing.sm }}>
            <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
              Your decision
            </Text>
            <Card style={{ gap: spacing.md }}>
              <View style={styles.scopeRow}>
                <Icon name="eye" size={16} color={colors.text2} />
                <Text variant="sm" color={colors.text2} style={{ flex: 1 }}>
                  <Text variant="smStrong" color={colors.text}>
                    Approve once:{' '}
                  </Text>
                  this exact command runs one time. The next ask is a new request.
                </Text>
              </View>
              <View style={styles.scopeRow}>
                <Icon name="x" size={16} color={colors.text2} />
                <Text variant="sm" color={colors.text2} style={{ flex: 1 }}>
                  <Text variant="smStrong" color={colors.text}>
                    Reject:{' '}
                  </Text>
                  the action does not run. The agent adapts or stops the task.
                </Text>
              </View>
            </Card>
            <Button
              label="Approve Once"
              icon="check"
              loading={busy === 'once'}
              disabled={busy !== null || isUnknown}
              onPress={() => decide('approve-once', 'once')}
            />
            <Button
              label="Reject"
              variant="secondary"
              icon="x"
              loading={busy === 'reject'}
              disabled={busy !== null || isUnknown}
              onPress={() => decide('reject', 'reject')}
            />
          </View>

          {/* separate, deliberately-not-primary persistent scope action */}
          <Card style={styles.persistent}>
            <View style={styles.scopeRow}>
              <Icon name="shield-check" size={16} color={colors.warning} />
              <Text variant="sm" color={colors.text2} style={{ flex: 1 }}>
                <Text variant="smStrong" color={colors.text}>
                  Separate action:{' '}
                </Text>
                approve this action for the whole session. The same command stays allowed until the
                session ends — no further prompts for it.
              </Text>
            </View>
            <Button
              label="Approve for Session"
              variant="secondary"
              loading={busy === 'scope'}
              disabled={busy !== null || isUnknown}
              onPress={() => setConfirmSession(true)}
              accessibilityLabel="Approve this action for the entire session"
            />
          </Card>
        </>
      ) : null}

      <Text variant="xs" color={colors.text3} center>
        Decisions are enforced by the host service. Nothing here changes OS-level permissions.
      </Text>

      <ConfirmModal
        visible={confirmSession}
        title="Allow for the whole session?"
        message={`The agent may repeat “${permission.command}” without asking again until ${session ? session.title : 'the session'} ends.`}
        note="Wider scope than approve-once. The command itself never changes — only how often it may run."
        confirmLabel="Approve for Session"
        cancelLabel="Cancel"
        busy={busy === 'scope'}
        onConfirm={() => {
          setConfirmSession(false);
          decide('approve-session', 'scope');
        }}
        onCancel={() => setConfirmSession(false)}
      />
    </Screen>
  );
}

const styles = StyleSheet.create({
  head: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  pathRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  expiryRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  scopeRow: { flexDirection: 'row', alignItems: 'flex-start', gap: spacing.sm },
  decidedBox: { gap: spacing.sm, borderWidth: 1.5, borderRadius: radii.md },
  persistent: { gap: spacing.md, borderStyle: 'dashed' },
});
