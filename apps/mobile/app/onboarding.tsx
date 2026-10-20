/**
 * Onboarding — three slides: control, connectivity, approvals.
 * Decorative contours sit behind everything and never intercept touches.
 */
import { useState } from 'react';
import { ScrollView, StyleSheet, View } from 'react-native';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';
import { Redirect, useRouter } from 'expo-router';
import type { Agent, Host, PermissionRequest } from '@chernobyl/protocol';
import { colors, spacing, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { Button, Text } from '@/ui/primitives';
import { AgentIllustration, Contours, Wordmark } from '@/ui/graphics';
import { AgentCard, PermissionCard } from '@/ui/lists';

const SLIDES: { title: string; body: string; art: 'hero' | 'agents' | 'approval' }[] = [
  {
    title: 'Your agents. Under control.',
    body: 'Monitor, command and authorize every AI agent from one screen — wherever you are.',
    art: 'hero',
  },
  {
    title: 'Connect your agents.',
    body: 'Claude Code, OpenCode, Codex, browser runners and custom agents — one shared workspace.',
    art: 'agents',
  },
  {
    title: 'Approve from anywhere.',
    body: 'See the exact command, path and risk. Approve once, allow for the session, or reject.',
    art: 'approval',
  },
];

const DEMO_AGENTS: Agent[] = [
  {
    id: 'demo-1',
    hostId: 'demo-host',
    kind: 'claude-code',
    name: 'Claude Code',
    state: 'online',
    capabilities: ['refactor', 'tests'],
    acceptsNewSession: true,
  },
  {
    id: 'demo-2',
    hostId: 'demo-host',
    kind: 'opencode',
    name: 'OpenCode',
    state: 'busy',
    capabilities: ['web search'],
    acceptsNewSession: false,
  },
];

const DEMO_HOST: Host = {
  id: 'demo-host',
  name: 'Studio Mac',
  kind: 'local',
  platform: 'macOS',
  address: 'localhost:8787',
  status: 'connected',
  lastSeenAt: Date.now(),
};

const DEMO_PERMISSION: PermissionRequest = {
  id: 'demo-permission',
  // the demo mock runtime is its own native authority (G2 fields included)
  nativeRequestId: 'demo-permission',
  sessionId: 'demo-session',
  agentId: 'demo-1',
  hostId: 'demo-host',
  action: 'shell',
  title: 'Run shell command',
  command: 'rm -rf .cache/build',
  args: ['-rf', '.cache/build'],
  directory: '~/projects/atlas',
  risk: 'high',
  status: 'pending',
  supportedDecisions: ['approve-once', 'approve-session', 'reject'],
  revision: 1,
  fingerprint: 'demo:shell:rm -rf .cache/build',
  requestedAt: Date.now(),
  expiresAt: Date.now() + 110_000,
  context: 'Clearing the stale build cache before rebuilding.',
};

function Dots({ index }: { index: number }) {
  return (
    <View
      style={styles.dots}
      accessible
      accessibilityRole="text"
      accessibilityLabel={`Slide ${index + 1} of ${SLIDES.length}`}
    >
      {SLIDES.map((_, i) => (
        <View key={i} style={[styles.dot, i === index && styles.dotActive]} />
      ))}
    </View>
  );
}

export default function OnboardingScreen() {
  const onboarded = useAppStore((s) => s.onboarded);
  const complete = useAppStore((s) => s.completeOnboarding);
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const [slide, setSlide] = useState(0);

  if (onboarded) return <Redirect href="/sign-in" />;

  const current = SLIDES[slide];
  const last = slide === SLIDES.length - 1;
  const finish = () => {
    complete();
    router.replace('/sign-in');
  };

  return (
    <View style={styles.root}>
      <Contours variant="frame" style={{ opacity: 0.4 }} />
      <SafeAreaView style={{ flex: 1 }} edges={['top', 'bottom']}>
        <View style={styles.topBar}>
          <Wordmark size={15} />
          <Button
            label="Skip"
            variant="ghost"
            size="sm"
            fullWidth={false}
            onPress={finish}
            accessibilityLabel="Skip onboarding"
          />
        </View>

        <ScrollView
          contentContainerStyle={[
            styles.body,
            { paddingBottom: insets.bottom + TAB_BAR_HEIGHT * 0.4 },
          ]}
          showsVerticalScrollIndicator={false}
        >
          <View style={styles.art}>
            {current.art === 'hero' ? <AgentIllustration size={210} /> : null}
            {current.art === 'agents' ? (
              <View style={{ gap: spacing.md, alignSelf: 'stretch' }}>
                {DEMO_AGENTS.map((a) => (
                  <AgentCard
                    key={a.id}
                    agent={a}
                    host={DEMO_HOST}
                    activeCount={a.id === 'demo-1' ? 2 : 1}
                  />
                ))}
              </View>
            ) : null}
            {current.art === 'approval' ? (
              <View style={{ alignSelf: 'stretch' }}>
                <PermissionCard
                  permission={DEMO_PERMISSION}
                  agent={DEMO_AGENTS[0]}
                  host={DEMO_HOST}
                  variant="full"
                />
              </View>
            ) : null}
          </View>

          <Text variant="h1" accessibilityRole="header">
            {current.title}
          </Text>
          <Text variant="body" color={colors.text2}>
            {current.body}
          </Text>
        </ScrollView>

        <View style={styles.footer}>
          <Dots index={slide} />
          <Button
            label={last ? 'Get Started' : 'Next'}
            icon="arrow-right"
            onPress={last ? finish : () => setSlide(slide + 1)}
            accessibilityLabel={last ? 'Get started' : `Next, slide ${slide + 2} of ${SLIDES.length}`}
          />
        </View>
      </SafeAreaView>
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1, backgroundColor: colors.white },
  topBar: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingHorizontal: spacing.xl,
    paddingTop: spacing.sm,
  },
  body: {
    flexGrow: 1,
    paddingHorizontal: spacing.xl,
    paddingTop: spacing.xl,
    gap: spacing.lg,
    justifyContent: 'center',
  },
  art: {
    minHeight: 240,
    alignItems: 'center',
    justifyContent: 'center',
    gap: spacing.lg,
    marginBottom: spacing.lg,
  },
  footer: {
    paddingHorizontal: spacing.xl,
    paddingBottom: spacing.lg,
    gap: spacing.lg,
  },
  dots: {
    flexDirection: 'row',
    gap: 6,
    alignSelf: 'center',
    alignItems: 'center',
  },
  dot: {
    width: 7,
    height: 7,
    borderRadius: 4,
    borderWidth: 1,
    borderColor: colors.black,
    backgroundColor: 'transparent',
  },
  dotActive: { width: 22, backgroundColor: colors.black },
});
