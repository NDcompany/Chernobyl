/**
 * New task (modal) — pick an agent, describe the job, start a session.
 * Pre-filled when opened from an agent detail screen (?agent=<id>).
 */
import { useEffect, useState } from 'react';
import { ScrollView, StyleSheet, View } from 'react-native';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import { useForm } from 'react-hook-form';
import { z } from 'zod';
import { colors, spacing, radii } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useCommands, commands } from '@/data/commands';
import { agentKindLabel } from '@/utils/format';
import { zodHandler } from '@/forms/zodBridge';
import { Button, Chip, StatusDot, Text, TextField } from '@/ui/primitives';
import { Screen, PageHeader, Card, EmptyState } from '@/ui/surface';
import { AgentIcon } from '@/ui/graphics';

const taskSchema = z.object({
  agentId: z.string().min(1, 'Choose an agent'),
  title: z
    .string()
    .trim()
    .min(3, 'Give the task a short title (3+ characters)')
    .max(80, 'Keep the title under 80 characters'),
  prompt: z
    .string()
    .trim()
    .min(10, 'Describe the task in at least 10 characters')
    .max(2000, 'The prompt is too long'),
  project: z.string().trim().max(80, 'Project name is too long'),
});

type TaskForm = z.infer<typeof taskSchema>;

export default function NewTaskScreen() {
  const authed = useAppStore((s) => s.authed);
  const integrations = useAppStore((s) => s.settings.integrations);
  const snapshot = useSnapshot();
  const send = useCommands();
  const router = useRouter();
  const params = useLocalSearchParams<{ agent?: string }>();
  const [busy, setBusy] = useState(false);
  const [submittedCommandId, setSubmittedCommandId] = useState<string | null>(null);
  const commandStates = useAppStore((s) => s.commandStates);

  const unknownCommand = submittedCommandId ? commandStates[submittedCommandId] : null;
  const isUnknown = unknownCommand?.state === 'unknown';

  const available = snapshot.agents.filter(
    (a) => a.acceptsNewSession && integrations[a.kind],
  );
  const preselect =
    available.find((a) => a.id === params.agent)?.id ?? available[0]?.id ?? '';

  const form = useForm<TaskForm>({
    defaultValues: { agentId: preselect, title: '', prompt: '', project: '' },
  });
  const selectedAgentId = form.watch('agentId');

  // Auto-navigate when reconciled on reconnect
  useEffect(() => {
    if (submittedCommandId) {
      const entry = commandStates[submittedCommandId];
      if (entry && (entry.state === 'accepted' || entry.state === 'completed') && entry.sessionId) {
        router.replace(`/session/${entry.sessionId}`);
      }
    }
  }, [submittedCommandId, commandStates, router]);

  if (!authed) return <Redirect href="/sign-in" />;

  const submit = zodHandler(taskSchema, form, (data) => {
    setBusy(true);
    void (async () => {
      const msg = commands.createSession(data.agentId, data.title, data.prompt, data.project || undefined);
      setSubmittedCommandId('commandId' in msg ? msg.commandId : null);
      const ack = await send(msg);
      setBusy(false);
      if (ack?.ok && ack.sessionId) {
        router.replace(`/session/${ack.sessionId}`);
      }
    })();
  });

  return (
    <Screen header={<PageHeader title="New task" />}>
      {available.length === 0 ? (
        <EmptyState
          icon="bot"
          title="No agent is accepting tasks"
          caption="Enable an integration in settings or wait for a busy agent to free up."
          actionLabel="Agent integrations"
          onAction={() => router.replace('/settings/integrations')}
        />
      ) : (
        <>
          <View style={{ gap: 6 }}>
            <Text variant="smStrong">Agent</Text>
            <ScrollView
              horizontal
              showsHorizontalScrollIndicator={false}
              contentContainerStyle={{ gap: spacing.sm, paddingRight: spacing.xl }}
            >
              {available.map((a) => (
                <Chip
                  key={a.id}
                  label={`${a.name} · ${agentKindLabel(a.kind)}`}
                  active={selectedAgentId === a.id}
                  onPress={() => form.setValue('agentId', a.id, { shouldDirty: true })}
                />
              ))}
            </ScrollView>
            {form.formState.errors.agentId ? (
              <Text variant="xs" color={colors.error} accessibilityRole="alert">
                {String(form.formState.errors.agentId.message)}
              </Text>
            ) : null}
          </View>

          <View style={styles.agentPreview}>
            {(() => {
              const agent = available.find((a) => a.id === selectedAgentId);
              if (!agent) return null;
              return (
                <>
                  <AgentIcon kind={agent.kind} size={36} />
                  <View style={{ flex: 1, gap: 2 }}>
                    <Text variant="smStrong">{agent.name}</Text>
                    <Text variant="xs" color={colors.text2} numberOfLines={1}>
                      {agent.capabilities.slice(0, 3).join(' · ') || agentKindLabel(agent.kind)}
                    </Text>
                  </View>
                </>
              );
            })()}
          </View>

          <TextField form={form} name="title" label="Task title" placeholder="Fix flaky API tests" />
          <TextField
            form={form}
            name="prompt"
            label="First message"
            placeholder="Describe what the agent should do…"
            multiline
            hint="This is sent to the agent immediately when the session starts"
          />
          <TextField
            form={form}
            name="project"
            label="Project (optional)"
            placeholder="workspace/atlas"
            hint="Working directory label for the session"
            autoCapitalize="none"
          />

          {isUnknown ? (
            <Card style={{ borderColor: colors.warning, gap: spacing.xs }}>
              <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing.sm }}>
                <StatusDot tone="warning" label="Outcome unknown" />
                <Text variant="bodyStrong" color={colors.warning}>
                  Outcome unknown — the host may have run it
                </Text>
              </View>
              <Text variant="xs" color={colors.text2}>
                Session creation was sent, but confirmation was not received before connection dropped or timed out.
              </Text>
            </Card>
          ) : null}

          <Button
            label="Start session"
            icon="arrow-right"
            loading={busy}
            disabled={isUnknown}
            onPress={submit}
            accessibilityLabel="Start a new agent session"
          />

          <View style={styles.note}>
            <Text variant="xs" color={colors.text3} center>
              Mock host — sessions are simulated. Nothing is executed on any machine.
            </Text>
          </View>
        </>
      )}
    </Screen>
  );
}

const styles = StyleSheet.create({
  agentPreview: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.sm,
    backgroundColor: colors.surface2,
    padding: spacing.md,
  },
  note: { paddingTop: spacing.xs },
});
