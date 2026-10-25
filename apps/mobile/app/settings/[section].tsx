/**
 * Settings detail — one route, six sections: hosts, integrations,
 * notifications, appearance, security and the local audit trail.
 */
import { useEffect, useState, type ReactNode } from 'react';
import { Platform, StyleSheet, Switch, View } from 'react-native';
import type { StyleProp, ViewStyle } from 'react-native';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import type { AgentKind } from '@chernobyl/protocol';
import { colors, radii, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { transportKind } from '@/data/transport';
import { readAudit } from '@/platform/storage';
import { clearHostToken, clearPairing, readHostToken, readPairing } from '@/platform/secure';
import { parsePairingPayload, type PairingCredential } from '@chernobyl/e2e';
import {
  agentKindLabel,
  hostStatusLabel,
  hostTone,
  relativeTime,
  transportStatusLabel,
  transportTone,
} from '@/utils/format';
import { useNow } from '@/hooks/useReducedMotion';
import { Button, StatusDot, Text } from '@/ui/primitives';
import { Screen, PageHeader, Card, ConfirmModal, EmptyState } from '@/ui/surface';
import { DetailRow } from '@/ui/lists';
import { Icon } from '@/ui/icons';

const TITLES: Record<string, string> = {
  hosts: 'Connected hosts',
  integrations: 'Agent integrations',
  notifications: 'Notification settings',
  appearance: 'Appearance',
  security: 'Security & device',
  audit: 'Audit history',
};

function ToggleRow({
  label,
  caption,
  value,
  onValueChange,
  disabled = false,
}: {
  label: string;
  caption?: string;
  value: boolean;
  onValueChange: (v: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <View style={styles.toggleRow}>
      <View style={{ flex: 1, gap: 2 }}>
        <Text variant="bodyStrong" color={disabled ? colors.text3 : colors.text}>
          {label}
        </Text>
        {caption ? (
          <Text variant="xs" color={colors.text3}>
            {caption}
          </Text>
        ) : null}
      </View>
      <Switch
        value={value}
        onValueChange={onValueChange}
        disabled={disabled}
        trackColor={{ false: colors.border, true: colors.black }}
        thumbColor={colors.white}
        ios_backgroundColor={colors.border}
        accessibilityLabel={label}
      />
    </View>
  );
}

function SectionCard({ children, style }: { children: ReactNode; style?: StyleProp<ViewStyle> }) {
  return (
    <Card style={[{ gap: 0, paddingVertical: spacing.xs }, style]}>{children}</Card>
  );
}

export default function SettingsSectionScreen() {
  const authed = useAppStore((s) => s.authed);
  const { section } = useLocalSearchParams<{ section: string }>();
  const key = String(section);
  const router = useRouter();
  const snapshot = useSnapshot();
  const now = useNow(30_000);

  const settings = useAppStore((s) => s.settings);
  const updateSettings = useAppStore((s) => s.updateSettings);
  const toggleIntegration = useAppStore((s) => s.toggleIntegration);
  const connection = useAppStore((s) => s.connection);
  const [hasToken, setHasToken] = useState<boolean | null>(null);
  const [pairedCredential, setPairedCredential] = useState<PairingCredential | null | undefined>(undefined);
  // paired device id (from the stored pairing), null = not paired, undefined = loading
  const [pairedDevice, setPairedDevice] = useState<string | null | undefined>(undefined);
  const [audit] = useState(() => readAudit(50));
  const [confirmUnpair, setConfirmUnpair] = useState(false);

  useEffect(() => {
    void readHostToken().then((t) => setHasToken(!!t));
    void readPairing().then((json) => {
      try {
        if (json) {
          const cred = parsePairingPayload(json);
          setPairedCredential(cred);
          setPairedDevice(cred.deviceId);
        } else {
          setPairedCredential(null);
          setPairedDevice(null);
        }
      } catch {
        setPairedCredential(null);
        setPairedDevice(null); // corrupt payload: TransportProvider already surfaced it
      }
    });
  }, []);

  if (!authed) return <Redirect href="/sign-in" />;

  const title = TITLES[key] ?? 'Settings';
  const known = key in TITLES;

  // Local deletion (PRD Phase 9): drop the pairing key and token from the
  // keystore and rebuild the transport, which falls back to Simulation.
  const unpair = () => {
    setConfirmUnpair(false);
    void Promise.all([clearPairing(), clearHostToken()]).then(() => {
      setPairedCredential(null);
      setPairedDevice(null);
      setHasToken(false);
      useAppStore.getState().bumpTransport();
      useAppStore.getState().addToast('Host unpaired — keys removed from this device', 'success');
    });
  };

  let body: ReactNode = null;

  if (key === 'hosts') {
    const isMock = transportKind() === 'mock';
    const hasHosts = Boolean(pairedCredential) || snapshot.hosts.length > 0;
    body = (
      <>
        <Text variant="sm" color={colors.text2}>
          Hosts run your agents and enforce every decision made in this app.
        </Text>
        {!hasHosts ? (
          <EmptyState
            icon="link"
            title="No hosts yet"
            caption="Add a host to discover the agents running on it."
            actionLabel="Connect a host"
            onAction={() => router.push('/connect-host')}
          />
        ) : (
          <SectionCard>
            {pairedCredential ? (
              <View style={snapshot.hosts.length > 0 ? styles.divider : undefined}>
                <View style={styles.hostRow}>
                  <View style={{ flex: 1, gap: 4 }}>
                    <View style={styles.badgeRow}>
                      <Text variant="bodyStrong">Paired Host</Text>
                      <View style={styles.realBadge}>
                        <Text variant="xs" color={colors.white} style={{ fontWeight: '700' }}>
                          Real
                        </Text>
                      </View>
                    </View>
                    <Text variant="mono" color={colors.text2} numberOfLines={1}>
                      Host ID: {pairedCredential.host}
                    </Text>
                    <Text variant="mono" color={colors.text3} numberOfLines={1}>
                      Device ID: {pairedCredential.deviceId}
                    </Text>
                    <View style={{ marginTop: 2, gap: 2 }}>
                      <Text variant="xs" color={colors.text3} style={{ fontWeight: '600' }}>
                        Endpoints ({pairedCredential.endpoints.length}):
                      </Text>
                      {pairedCredential.endpoints.map((ep, idx) => (
                        <Text
                          key={ep}
                          variant="mono"
                          color={idx === 0 ? colors.text : colors.text3}
                          numberOfLines={1}
                        >
                          {idx === 0 ? `• ${ep} (primary)` : `• ${ep}`}
                        </Text>
                      ))}
                    </View>
                  </View>
                  <StatusDot
                    tone={transportTone(connection)}
                    label={transportStatusLabel(connection)}
                  />
                </View>
              </View>
            ) : null}

            {snapshot.hosts.map((h, i) => (
              <View key={h.id} style={i < snapshot.hosts.length - 1 ? styles.divider : undefined}>
                <View style={styles.hostRow}>
                  <View style={{ flex: 1, gap: 2 }}>
                    <View style={styles.badgeRow}>
                      <Text variant="bodyStrong">{h.name}</Text>
                      {isMock ? (
                        <View style={styles.simBadge}>
                          <Text variant="xs" color={colors.text2} style={{ fontWeight: '600' }}>
                            Simulation
                          </Text>
                        </View>
                      ) : null}
                    </View>
                    <Text variant="mono" color={colors.text3} numberOfLines={1}>
                      {h.address}
                    </Text>
                    <Text variant="xs" color={colors.text3}>
                      {h.platform} · seen {relativeTime(h.lastSeenAt, now)}
                    </Text>
                  </View>
                  <StatusDot tone={hostTone(h.status)} label={hostStatusLabel(h.status)} />
                </View>
              </View>
            ))}
          </SectionCard>
        )}
        <Button label="Connect a host" icon="plus" variant="secondary" onPress={() => router.push('/connect-host')} />
        <Text variant="xs" color={colors.text3} center>
          {pairedCredential
            ? 'Paired with a real desktop companion via E2E encrypted WebSocket.'
            : 'Prototype: hosts are simulated locally and no WebSocket handshake is performed.'}
        </Text>
      </>
    );
  }

  if (key === 'integrations') {
    const kinds = Object.keys(settings.integrations) as AgentKind[];
    body = (
      <>
        <Text variant="sm" color={colors.text2}>
          Enabled integrations can be picked when you start a new task.
        </Text>
        <SectionCard>
          {kinds.map((kind, i) => (
            <View key={kind} style={i < kinds.length - 1 ? styles.divider : undefined}>
              <ToggleRow
                label={agentKindLabel(kind)}
                caption={`kind: ${kind}`}
                value={settings.integrations[kind]}
                onValueChange={() => toggleIntegration(kind)}
              />
            </View>
          ))}
        </SectionCard>
      </>
    );
  }

  if (key === 'notifications') {
    body = (
      <>
        <Text variant="sm" color={colors.text2}>
          Local notifications for approvals and finished sessions.
        </Text>
        <SectionCard>
          <ToggleRow
            label="Push notifications"
            caption={Platform.OS === 'web' ? 'Unavailable in the browser preview' : 'Device-level alerts'}
            value={settings.push}
            onValueChange={(v) => updateSettings({ push: v })}
            disabled={Platform.OS === 'web'}
          />
          <View style={styles.divider} />
          <ToggleRow
            label="Approval alerts"
            caption="When an agent requests permission"
            value={settings.approvalAlerts}
            onValueChange={(v) => updateSettings({ approvalAlerts: v })}
            disabled={!settings.push}
          />
          <View style={styles.divider} />
          <ToggleRow
            label="Session finished"
            caption="When a task completes"
            value={settings.sessionAlerts}
            onValueChange={(v) => updateSettings({ sessionAlerts: v })}
            disabled={!settings.push}
          />
        </SectionCard>
        <Text variant="xs" color={colors.text3} center>
          This build shows notifications while the app is open. Silent push while backgrounded
          requires FCM/APNs configuration on the host.
        </Text>
      </>
    );
  }

  if (key === 'appearance') {
    body = (
      <>
        <Text variant="sm" color={colors.text2}>
          CHERNOBYL is monochrome by design — the only theme option is motion.
        </Text>
        <SectionCard>
          <View style={styles.appearRow}>
            <View style={{ flex: 1, gap: 2 }}>
              <Text variant="bodyStrong">Theme</Text>
              <Text variant="xs" color={colors.text3}>
                Monochrome (light) — fixed
              </Text>
            </View>
            <Icon name="check" size={18} color={colors.black} />
          </View>
          <View style={styles.divider} />
          <ToggleRow
            label="Reduce motion"
            caption={
              settings.reduceMotion === 'on'
                ? 'Animations are off'
                : 'Follows the system accessibility setting'
            }
            value={settings.reduceMotion === 'on'}
            onValueChange={(v) => updateSettings({ reduceMotion: v ? 'on' : 'system' })}
          />
        </SectionCard>
      </>
    );
  }

  if (key === 'security') {
    body = (
      <>
        <Text variant="sm" color={colors.text2}>
          Secrets live in the OS keystore. Nothing sensitive is stored in the app database.
        </Text>
        <SectionCard>
          <View style={styles.hostRow}>
            <Icon name="phone" size={18} color={colors.text2} />
            <View style={{ flex: 1, gap: 2 }}>
              <Text variant="bodyStrong">This device</Text>
              <Text variant="xs" color={colors.text3}>
                Active now · {Platform.OS}
              </Text>
            </View>
            <StatusDot tone="success" label="Active" />
          </View>
          <View style={styles.divider} />
          <View style={{ paddingHorizontal: spacing.md, paddingVertical: spacing.md, gap: spacing.sm }}>
            <DetailRow
              label="Pairing"
              value={
                pairedDevice === undefined
                  ? 'Checking…'
                  : pairedDevice
                    ? `Paired · device ${pairedDevice}`
                    : 'Not paired'
              }
              mono={false}
            />
            <DetailRow
              label="Host token"
              value={hasToken === null ? 'Checking…' : hasToken ? 'Stored in keystore' : 'Not stored'}
              mono={false}
            />
          </View>
        </SectionCard>
        <Button
          label="Remove stored token"
          variant="secondary"
          onPress={() => {
            void clearHostToken().then(() => {
              setHasToken(false);
              useAppStore.getState().addToast('Stored token removed', 'success');
            });
          }}
        />
        {pairedDevice ? (
          <Button
            label="Unpair this host"
            variant="secondary"
            icon="logout"
            onPress={() => setConfirmUnpair(true)}
          />
        ) : null}
        <Text variant="xs" color={colors.text3} center>
          {pairedDevice
            ? `To cut this phone off from the desktop side, run: chernobyl-companion --revoke ${pairedDevice}`
            : 'There are no accounts: access is per paired phone, revoked from the desktop companion.'}
        </Text>
      </>
    );
  }

  if (key === 'audit') {
    body = (
      <>
        <Text variant="sm" color={colors.text2}>
          Every approval decision recorded on this device, newest first.
        </Text>
        {audit.length === 0 ? (
          <EmptyState
            icon="clock"
            title="No decisions yet"
            caption="Approve or reject a request and the entry appears here."
            contour={false}
          />
        ) : (
          <SectionCard>
            {audit.map((row, i) => (
              <View key={row.id} style={i < audit.length - 1 ? styles.divider : undefined}>
                <View style={styles.auditRow}>
                  <View style={{ flex: 1, gap: 3 }}>
                    <View style={styles.auditHead}>
                      <Text variant="smStrong" color={row.decision === 'reject' ? colors.error : colors.success}>
                        {row.decision === 'reject' ? 'Rejected' : 'Approved'}
                      </Text>
                      <Text variant="xs" color={colors.text3}>
                        {row.agent} · {row.risk} risk
                      </Text>
                    </View>
                    <Text variant="mono" color={colors.text2} numberOfLines={2}>
                      {row.command}
                    </Text>
                  </View>
                  <Text variant="xs" color={colors.text3}>
                    {relativeTime(row.ts, now)}
                  </Text>
                </View>
              </View>
            ))}
          </SectionCard>
        )}
        <Text variant="xs" color={colors.text3} center>
          Stored in local SQLite on this device only.
        </Text>
      </>
    );
  }

  return (
    <Screen header={<PageHeader title={title} />}>
      {!known ? (
        <EmptyState
          icon="sliders"
          title="Unknown section"
          caption="That settings section does not exist."
          actionLabel="Back"
          onAction={() => router.back()}
        />
      ) : (
        body
      )}

      <ConfirmModal
        visible={confirmUnpair}
        title="Unpair this host?"
        message="The pairing key and token are deleted from this phone. Pairing again needs a new QR from the host."
        note="The host keeps enforcing permissions — agents are never left unattended."
        confirmLabel="Unpair"
        onConfirm={unpair}
        onCancel={() => setConfirmUnpair(false)}
      />
    </Screen>
  );
}

const styles = StyleSheet.create({
  toggleRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    paddingVertical: spacing.md,
    paddingHorizontal: spacing.md,
  },
  appearRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    paddingVertical: spacing.md,
    paddingHorizontal: spacing.md,
  },
  hostRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    paddingVertical: spacing.md,
    paddingHorizontal: spacing.md,
  },
  auditRow: {
    flexDirection: 'row',
    alignItems: 'flex-start',
    gap: spacing.md,
    paddingVertical: spacing.md,
    paddingHorizontal: spacing.md,
  },
  auditHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, flexWrap: 'wrap' },
  badgeRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  realBadge: {
    backgroundColor: colors.black,
    borderRadius: radii.pill,
    paddingHorizontal: spacing.sm,
    paddingVertical: 2,
  },
  simBadge: {
    backgroundColor: colors.surface2,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.pill,
    paddingHorizontal: spacing.sm,
    paddingVertical: 2,
  },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
