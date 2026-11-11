/**
 * Terminal (ADR-018): one command line at a time in an allow-listed
 * workspace, run by the PC's shell WITHOUT the agents' approval prompts.
 * Only when the PC turned it on (`terminal.v1`). Output streams in (ANSI
 * stripped and redacted on the PC), then the exit code; Stop kills the
 * process tree. The last 20 command lines per workspace stay on this phone.
 */
import { useState } from 'react';
import { ActivityIndicator, ScrollView, StyleSheet, View } from 'react-native';
import { Redirect, useLocalSearchParams } from 'expo-router';
import { useForm } from 'react-hook-form';
import { z } from 'zod';
import { TERMINAL_COMMAND_MAX } from '@chernobyl/protocol';
import { colors, radii, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useWorkspaceEnabled } from '@/data/workspace';
import { useTerminal, useTerminalEnabled } from '@/data/terminal';
import { zodHandler } from '@/forms/zodBridge';
import { findWorkspace } from '@/utils/workspace';
import { exitLabel, exitOk } from '@/utils/terminal';
import { Button, Chip, Text, TextField } from '@/ui/primitives';
import { Card, EmptyState, PageHeader, Screen } from '@/ui/surface';
import { Icon } from '@/ui/icons';
import { CodeView, WorkspaceUnavailable } from '@/ui/workspace';

const schema = z.object({
  command: z
    .string()
    .trim()
    .min(1, 'Type a command')
    .max(TERMINAL_COMMAND_MAX, 'Keep the command under 4,096 characters')
    .refine((c) => !/[\r\n]/.test(c), 'One line only'),
});
type Form = z.infer<typeof schema>;

const NO_HISTORY: string[] = [];

export default function TerminalScreen() {
  const authed = useAppStore((s) => s.authed);
  const connected = useAppStore((s) => s.connection === 'connected');
  const { workspace } = useLocalSearchParams<{ workspace: string }>();
  const snapshot = useSnapshot();
  const workspaceOn = useWorkspaceEnabled();
  const terminalOn = useTerminalEnabled();
  const ws = findWorkspace(snapshot.agents, String(workspace));
  const history = useAppStore((s) => (ws ? s.terminalHistory[ws.id] : undefined) ?? NO_HISTORY);
  const { run, start, stop } = useTerminal(ws?.id ?? '');
  const [stopping, setStopping] = useState(false);
  const form = useForm<Form>({ defaultValues: { command: '' } });

  if (!authed) return <Redirect href="/sign-in" />;

  const header = <PageHeader title="Terminal" subtitle={ws ? ws.label : undefined} />;

  if (!workspaceOn || !ws) {
    return (
      <Screen header={header}>
        <WorkspaceUnavailable known={Boolean(ws)} enabled={workspaceOn} />
      </Screen>
    );
  }
  if (!terminalOn) {
    return (
      <Screen header={header}>
        <EmptyState
          icon="terminal"
          title="The terminal is off on this PC"
          caption='It is opt-in: run setup with -EnableTerminal, or set "terminal": {"enabled": true} in companion.json and restart the companion.'
        />
      </Screen>
    );
  }

  const running = run?.running === true;
  const submit = zodHandler(schema, form, (data) => {
    setStopping(false);
    void start(data.command);
  });

  return (
    <Screen header={header}>
      <View style={styles.banner} accessibilityRole="alert">
        <Icon name="terminal" size={18} color={colors.warning} />
        <View style={styles.flex}>
          <Text variant="smStrong" color={colors.warning}>
            Runs on your PC without approval prompts
          </Text>
          <Text variant="xs" color={colors.text2}>
            Any command, in {ws.path}. Two minutes at most; every run is logged on the PC.
          </Text>
        </View>
      </View>

      {!connected ? (
        <Text variant="sm" color={colors.warning}>
          Not connected: commands run when the connection to your PC is back.
        </Text>
      ) : null}

      <Card style={{ gap: spacing.md }}>
        <TextField
          form={form}
          name="command"
          label="Command"
          placeholder={'e.g. npm test'}
          autoCapitalize="none"
          onSubmitEditing={submit}
        />
        {running ? (
          <Button
            label="Stop"
            icon="stop"
            variant="secondary"
            loading={stopping}
            onPress={() => {
              setStopping(true);
              void stop();
            }}
            accessibilityLabel="Stop the running command"
          />
        ) : (
          <Button label="Run" icon="arrow-right" disabled={!connected} onPress={submit} />
        )}
      </Card>

      {history.length ? (
        <View style={{ gap: spacing.xs }}>
          <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
            Recent
          </Text>
          <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerStyle={styles.history}>
            {history.map((h) => (
              <Chip key={h} label={h} onPress={() => form.setValue('command', h)} />
            ))}
          </ScrollView>
        </View>
      ) : null}

      {run ? (
        <View style={{ gap: spacing.sm }}>
          <Text variant="mono" color={colors.text2} numberOfLines={2}>
            {'> '}
            {run.command}
          </Text>
          {run.output ? <CodeView content={run.output} /> : null}
          {running ? (
            <View style={styles.row}>
              <ActivityIndicator color={colors.black} />
              <Text variant="sm" color={colors.text2}>
                Running…
              </Text>
            </View>
          ) : null}
          {run.exit ? (
            <Text
              variant="smStrong"
              color={exitOk(run.exit) ? colors.success : colors.error}
              accessibilityRole="alert"
            >
              {exitLabel(run.exit)}
            </Text>
          ) : null}
          {run.failed ? (
            <Text variant="sm" color={colors.error} accessibilityRole="alert">
              {run.failed}
            </Text>
          ) : null}
        </View>
      ) : null}
    </Screen>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  banner: {
    flexDirection: 'row',
    gap: spacing.sm,
    padding: spacing.md,
    borderRadius: radii.md,
    backgroundColor: colors.warningBg,
  },
  history: { gap: spacing.sm },
  row: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
});
