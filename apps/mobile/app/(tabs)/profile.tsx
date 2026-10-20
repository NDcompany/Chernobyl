/**
 * Profile & settings hub — dark identity header, settings entries and a
 * sign-out that requires confirmation.
 */
import { useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { Redirect, useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { colors, spacing, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { Button, Text } from '@/ui/primitives';
import { Screen, PageHeader, Card, ConfirmModal } from '@/ui/surface';
import { DetailRow } from '@/ui/lists';
import { BlackShape } from '@/ui/graphics';
import { Icon, type IconName } from '@/ui/icons';
import { hostStatusLabel } from '@/utils/format';

interface Entry {
  key: string;
  label: string;
  caption: string;
  icon: IconName;
}

export default function ProfileScreen() {
  const authed = useAppStore((s) => s.authed);
  const user = useAppStore((s) => s.user);
  const settings = useAppStore((s) => s.settings);
  const signOut = useAppStore((s) => s.signOut);
  const snapshot = useSnapshot();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

  if (!authed) return <Redirect href="/sign-in" />;

  const connected = snapshot.hosts.filter((h) => h.status === 'connected').length;
  const integrationsOn = Object.values(settings.integrations).filter(Boolean).length;
  const integrationsTotal = Object.keys(settings.integrations).length;

  const entries: Entry[] = [
    {
      key: 'hosts',
      label: 'Connected hosts',
      caption: `${snapshot.hosts.length} configured · ${connected} ${hostStatusLabel('connected').toLowerCase()}`,
      icon: 'link',
    },
    {
      key: 'integrations',
      label: 'Agent integrations',
      caption: `${integrationsOn} of ${integrationsTotal} enabled`,
      icon: 'bot',
    },
    {
      key: 'notifications',
      label: 'Notification settings',
      caption: settings.push ? 'Push on' : 'Push off',
      icon: 'bell',
    },
    {
      key: 'appearance',
      label: 'Appearance',
      caption: settings.reduceMotion === 'on' ? 'Reduce motion on' : 'Reduce motion follows system',
      icon: 'sliders',
    },
    {
      key: 'security',
      label: 'Security & device',
      caption: 'Token storage, sessions',
      icon: 'lock',
    },
    {
      key: 'audit',
      label: 'Audit history',
      caption: 'Every approval decision, on this device',
      icon: 'clock',
    },
  ];

  const doSignOut = () => {
    setBusy(true);
    setTimeout(() => {
      setConfirming(false);
      setBusy(false);
      signOut();
      router.replace('/sign-in');
    }, 350);
  };

  const header = (
    <View style={styles.headerBlock}>
      <View style={[styles.headerContent, { paddingTop: insets.top + spacing.md }]}>
        <PageHeader
          dark
          bordered={false}
          title="Profile"
          onBack={null}
          right={
            <Pressable
              onPress={() => setConfirming(true)}
              hitSlop={10}
              focusable
              accessible
              accessibilityRole="button"
              accessibilityLabel="Sign out"
            >
              <Icon name="logout" size={20} color={colors.white} />
            </Pressable>
          }
        />
        <View style={styles.identity}>
          <View style={styles.avatar}>
            <Text variant="h3" color={colors.white}>
              {user.initials}
            </Text>
          </View>
          <View style={{ gap: 2 }}>
            <Text variant="h3" color={colors.white}>
              {user.name}
            </Text>
            <Text variant="xs" color="rgba(255,255,255,0.72)">
              {user.email}
            </Text>
          </View>
        </View>
      </View>
      <View style={styles.headerWave}>
        <BlackShape variant="detail" />
      </View>
    </View>
  );

  return (
    <Screen safeTop={false} header={header} bottomInset={TAB_BAR_HEIGHT}>
      <StatusBar style="light" />
      <Card style={{ gap: 0, paddingVertical: spacing.xs }}>
        {entries.map((e, i) => (
          <View key={e.key} style={i < entries.length - 1 ? styles.divider : undefined}>
            <DetailRow
              label={e.label}
              value={e.caption}
              onPress={() => router.push(`/settings/${e.key}`)}
            />
          </View>
        ))}
      </Card>

      <Button
        label="Sign Out"
        variant="secondary"
        icon="logout"
        onPress={() => setConfirming(true)}
      />

      <Text variant="xs" color={colors.text3} center>
        CHERNOBYL prototype v0.1 — mock data until the host service is connected.
      </Text>

      <ConfirmModal
        visible={confirming}
        title="Sign out of this device?"
        message="You'll need to sign in again to see agents, sessions and approvals."
        note="In-flight commands are not cancelled by signing out — the host keeps enforcing them."
        confirmLabel="Sign Out"
        busy={busy}
        onConfirm={doSignOut}
        onCancel={() => setConfirming(false)}
      />
    </Screen>
  );
}

const styles = StyleSheet.create({
  headerBlock: { backgroundColor: colors.white },
  headerContent: { backgroundColor: colors.black, paddingBottom: spacing.lg, gap: spacing.sm },
  headerWave: { height: 54, overflow: 'hidden', backgroundColor: colors.white },
  identity: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    paddingHorizontal: spacing.xl,
    paddingBottom: spacing.sm,
  },
  avatar: {
    width: 54,
    height: 54,
    borderRadius: 27,
    borderWidth: 1,
    borderColor: 'rgba(255,255,255,0.5)',
    backgroundColor: 'rgba(255,255,255,0.08)',
    alignItems: 'center',
    justifyContent: 'center',
  },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
