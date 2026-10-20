/**
 * Sign in — black flowing shape carries the wordmark; the form area below
 * uses the mock auth (no credentials are checked or stored).
 */
import { useState } from 'react';
import { ScrollView, StyleSheet, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { Redirect, useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { Button, Text } from '@/ui/primitives';
import { BlackShape, Wordmark } from '@/ui/graphics';

export default function SignInScreen() {
  const authed = useAppStore((s) => s.authed);
  const signIn = useAppStore((s) => s.signIn);
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const [busy, setBusy] = useState(false);

  if (authed) return <Redirect href="/" />;

  const submit = () => {
    setBusy(true);
    // mock auth: no password is checked and nothing leaves the device
    setTimeout(() => {
      signIn();
      setBusy(false);
      router.replace('/');
    }, 900);
  };

  return (
    <View style={styles.root}>
      <StatusBar style="light" />
      <View style={styles.hero}>
        <BlackShape variant="signin" />
        <View style={[styles.heroContent, { paddingTop: insets.top + 52 }]}>
          <Wordmark size={24} color={colors.white} />
        </View>
      </View>

      <ScrollView
        contentContainerStyle={styles.body}
        keyboardShouldPersistTaps="handled"
        showsVerticalScrollIndicator={false}
      >
        <View style={{ gap: spacing.sm }}>
          <Text variant="h1" accessibilityRole="header">
            Welcome back
          </Text>
          <Text variant="body" color={colors.text2}>
            One interface for every agent you run — sessions, output and approvals in one place.
          </Text>
        </View>

        <View style={{ gap: spacing.md, marginTop: spacing.lg }}>
          <Button label="Sign In" onPress={submit} loading={busy} icon="arrow-right" />
          <Button
            label="Connect a Host"
            variant="secondary"
            onPress={() => router.push('/connect-host')}
          />
        </View>

        <View style={styles.note}>
          <Text variant="xs" color={colors.text3} center>
            Prototype sign-in — no password is checked and no credentials leave the device. Host
            tokens, when added, are kept in the device keystore only.
          </Text>
        </View>
      </ScrollView>
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1, backgroundColor: colors.white },
  hero: {
    height: 300,
    overflow: 'hidden',
    backgroundColor: colors.white,
  },
  heroContent: { alignItems: 'center' },
  body: {
    flexGrow: 1,
    paddingHorizontal: spacing.xl,
    paddingTop: spacing.huge,
    paddingBottom: spacing.huge,
    gap: spacing.sm,
  },
  note: { marginTop: 'auto', paddingTop: spacing.huge },
});
