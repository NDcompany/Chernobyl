/**
 * Connect a host (modal) — form validation via RHF + Zod. The prototype
 * simulates the handshake locally; the token goes to the keystore only.
 */
import { useState } from 'react';
import { StyleSheet, View } from 'react-native';
import { Redirect, useRouter } from 'expo-router';
import { useForm } from 'react-hook-form';
import { z } from 'zod';
import { nextId, type EventEnvelope, type Host } from '@chernobyl/protocol';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { applyMessage } from '@/data/state';
import { zodHandler } from '@/forms/zodBridge';
import { saveHostToken } from '@/platform/secure';
import { Button, Chip, Text, TextField } from '@/ui/primitives';
import { Screen, PageHeader, Card } from '@/ui/surface';
import { Icon } from '@/ui/icons';

/** This screen's local mock events still need an envelope (protocol v2). */
let localSeq = 0;
function localEvent<T extends { type: string }>(msg: T): T & EventEnvelope {
  localSeq += 1;
  return { ...msg, seq: localSeq, eventId: nextId('evt'), occurredAt: Date.now() };
}

const hostSchema = z.object({
  name: z.string().trim().min(2, 'Give the host a name (2+ characters)'),
  address: z
    .string()
    .trim()
    .min(1, 'Enter a hostname or IP')
    .regex(/^[a-zA-Z0-9._-]+(:\d{1,5})?$/, 'Use a hostname or IP, optionally with :port'),
  token: z
    .string()
    .trim()
    .refine((v) => v.length === 0 || v.length >= 8, 'Tokens are at least 8 characters'),
  kind: z.enum(['local', 'remote']),
});

type HostForm = z.infer<typeof hostSchema>;

export default function ConnectHostScreen() {
  const authed = useAppStore((s) => s.authed);
  const router = useRouter();
  const [busy, setBusy] = useState(false);
  const form = useForm<HostForm>({
    defaultValues: { name: '', address: '', token: '', kind: 'local' },
  });
  const kind = form.watch('kind');

  if (!authed) return <Redirect href="/sign-in" />;

  const submit = zodHandler(hostSchema, form, (data) => {
    setBusy(true);
    const host: Host = {
      id: nextId('h'),
      name: data.name,
      kind: data.kind,
      platform: data.kind === 'local' ? 'This device' : 'Linux',
      address: data.address,
      status: 'connecting',
      lastSeenAt: Date.now(),
    };
    if (data.token) void saveHostToken(data.token);
    applyMessage(localEvent({ type: 'host_updated', host }));
    // simulated handshake — a real build waits for the host's hello_ack
    setTimeout(() => {
      applyMessage(
        localEvent({
          type: 'host_updated',
          host: { ...host, status: 'connected', lastSeenAt: Date.now() },
        }),
      );
      applyMessage(
        localEvent({
          type: 'activity',
          event: {
            id: nextId('ev'),
            ts: Date.now(),
            kind: 'host',
            text: `${host.name} connected`,
            refId: host.id,
          },
        }),
      );
      useAppStore.getState().addToast(`${host.name} connected`, 'success');
      setBusy(false);
      router.back();
    }, 1100);
  });

  return (
    <Screen header={<PageHeader title="Connect a host" />}>
      <Card style={{ gap: spacing.sm }}>
        <View style={{ flexDirection: 'row', gap: spacing.sm, alignItems: 'center' }}>
          <Icon name="link" size={16} color={colors.text2} />
          <Text variant="smStrong">How it works</Text>
        </View>
        <Text variant="sm" color={colors.text2}>
          The app talks to a CHERNOBYL host service over WebSocket. The host runs your agents and
          enforces every permission request — the phone only decides.
        </Text>
      </Card>

      <TextField form={form} name="name" label="Name" placeholder="Studio Mac" />
      <TextField
        form={form}
        name="address"
        label="Address"
        placeholder="10.0.4.21:8787"
        hint="Hostname or IP, optionally with port"
        keyboardType="url"
        autoCapitalize="none"
      />
      <TextField
        form={form}
        name="token"
        label="Access token (optional)"
        placeholder="Paste the host token"
        hint="Stored in the device keystore — never in the database"
        secureTextEntry
        autoCapitalize="none"
      />

      <View style={{ gap: 6 }}>
        <Text variant="smStrong">Type</Text>
        <View style={{ flexDirection: 'row', gap: spacing.sm }}>
          <Chip label="Local" active={kind === 'local'} onPress={() => form.setValue('kind', 'local')} />
          <Chip label="Remote" active={kind === 'remote'} onPress={() => form.setValue('kind', 'remote')} />
        </View>
      </View>

      <Button label="Connect host" icon="arrow-right" loading={busy} onPress={submit} />

      <View style={styles.note}>
        <Text variant="xs" color={colors.text3} center>
          Prototype: no real connection is made. The host is added to your dashboard so the flow
          can be tested end to end.
        </Text>
      </View>
    </Screen>
  );
}

const styles = StyleSheet.create({
  note: { paddingTop: spacing.xs },
});
