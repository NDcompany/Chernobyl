/**
 * File viewer (ADR-016): one text file of an allow-listed workspace,
 * read-only, redacted on the PC before it leaves (`.env*` files show their
 * keys only). Binary files and anything over the cap are said plainly.
 */
import { ActivityIndicator, View } from 'react-native';
import { Redirect, useLocalSearchParams } from 'expo-router';
import type { WorkspaceResult } from '@chernobyl/protocol';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useWorkspaceEnabled, useWorkspaceRead } from '@/data/workspace';
import { baseName, findWorkspace, formatBytes } from '@/utils/workspace';
import { Button, Text } from '@/ui/primitives';
import { Card, EmptyState, PageHeader, Screen } from '@/ui/surface';
import { CodeView, WorkspaceUnavailable } from '@/ui/workspace';

type FileText = Extract<WorkspaceResult, { kind: 'fs_read' }>;

export default function FileScreen() {
  const authed = useAppStore((s) => s.authed);
  const connected = useAppStore((s) => s.connection === 'connected');
  const { workspace, path = '' } = useLocalSearchParams<{ workspace: string; path?: string }>();
  const snapshot = useSnapshot();
  const enabled = useWorkspaceEnabled();
  const ws = findWorkspace(snapshot.agents, String(workspace));
  const read = useWorkspaceRead(ws && enabled && path ? { type: 'fs_read', workspaceId: ws.id, path } : null);
  const file: FileText | null = read.result?.kind === 'fs_read' ? read.result : null;
  const { error, reload: load } = read;

  if (!authed) return <Redirect href="/sign-in" />;

  const header = <PageHeader title={baseName(path, 'File')} subtitle={ws ? `${ws.label}/${path}` : undefined} />;

  if (!enabled || !ws) {
    return (
      <Screen header={header}>
        <WorkspaceUnavailable known={Boolean(ws)} enabled={enabled} />
      </Screen>
    );
  }

  return (
    <Screen header={header}>
      {error === 'binary file' ? (
        <EmptyState icon="file" title="Binary file" caption="Only text files open on the phone." />
      ) : error ? (
        <Card style={{ gap: spacing.sm }}>
          <Text variant="sm" color={colors.error} accessibilityRole="alert">
            {error}
          </Text>
          <Button label="Try again" variant="secondary" size="sm" onPress={load} />
        </Card>
      ) : !file ? (
        <View style={{ alignItems: 'center', gap: spacing.md }}>
          <ActivityIndicator color={colors.black} />
          {!connected ? (
            <Text variant="sm" color={colors.warning} center>
              Not connected: the file loads when the connection to your PC is back.
            </Text>
          ) : null}
        </View>
      ) : (
        <>
          {file.sensitive || file.redacted || file.truncated ? (
            <View style={{ gap: spacing.xs }}>
              {file.sensitive ? (
                <Text variant="xs" color={colors.warning}>
                  Environment file: every value is hidden. Only the keys leave your PC.
                </Text>
              ) : file.redacted ? (
                <Text variant="xs" color={colors.warning}>
                  Secrets in this file were masked on your PC.
                </Text>
              ) : null}
              {file.truncated ? (
                <Text variant="xs" color={colors.text3}>
                  Showing the first 256 KB of {formatBytes(file.size)}; the rest is on the PC.
                </Text>
              ) : null}
            </View>
          ) : null}
          <CodeView content={file.content} />
        </>
      )}
    </Screen>
  );
}
