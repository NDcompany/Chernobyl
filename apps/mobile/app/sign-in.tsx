/**
 * Entry screen — black flowing shape carries the wordmark. There are no
 * accounts (ADR-002, C14): identity is the per-device pairing credential.
 * "Pair with a host" is the real path; "Explore the demo" enters Simulation
 * (ADR-009). `signIn()` only records that the user is past this screen.
 */
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

  if (authed) return <Redirect href="/" />;

  const explore = () => {
    signIn();
    router.replace('/');
  };
  const pair = () => {
    signIn();
    router.replace('/');
    router.push('/connect-host');
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
            Welcome
          </Text>
          <Text variant="body" color={colors.text2}>
            One interface for every agent you run — sessions, output and approvals in one place.
          </Text>
        </View>

        <View style={{ gap: spacing.md, marginTop: spacing.lg }}>
          <Button label="Pair with a host" onPress={pair} icon="arrow-right" />
          <Button
            label="Explore the demo (Simulation)"
            variant="secondary"
            onPress={explore}
          />
        </View>

        <View style={styles.note}>
          <Text variant="xs" color={colors.text3} center>
            No account needed: your phone pairs directly with your desktop companion by QR. The
            pairing key stays in the keystore on this phone. The demo shows simulated data only.
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
