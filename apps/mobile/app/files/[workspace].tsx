/**
 * Files (ADR-016): read-only folder browsing in one allow-listed workspace.
 * One screen per folder (`?path=src/ui`), so Back walks up the tree; a file
 * opens the viewer at `file/[workspace]`. `.git` and anything outside the
 * workspace never reach the phone (the companion refuses them).
 */
import { ActivityIndicator, Pressable, StyleSheet } from 'react-native';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import type { WorkspaceResult } from '@chernobyl/protocol';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import { useWorkspaceEnabled, useWorkspaceRead } from '@/data/workspace';
import { baseName, childPath, findWorkspace, formatBytes } from '@/utils/workspace';
import { relativeTime } from '@/utils/format';
import { useNow } from '@/hooks/useReducedMotion';
import { Button, Text } from '@/ui/primitives';
import { Card, EmptyState, PageHeader, Screen } from '@/ui/surface';
import { Icon } from '@/ui/icons';
import { WorkspaceUnavailable } from '@/ui/workspace';

type Listing = Extract<WorkspaceResult, { kind: 'fs_list' }>;

export default function FilesScreen() {
  const authed = useAppStore((s) => s.authed);
  const connected = useAppStore((s) => s.connection === 'connected');
  const { workspace, path = '' } = useLocalSearchParams<{ workspace: string; path?: string }>();
  const snapshot = useSnapshot();
  const router = useRouter();
  const enabled = useWorkspaceEnabled();
  const now = useNow(60_000);
  const ws = findWorkspace(snapshot.agents, String(workspace));
  const read = useWorkspaceRead(ws && enabled ? { type: 'fs_list', workspaceId: ws.id, path } : null);
  const listing: Listing | null = read.result?.kind === 'fs_list' ? read.result : null;
  const { error, loading, reload: load } = read;

  if (!authed) return <Redirect href="/sign-in" />;

  const title = ws ? baseName(path, ws.label) : 'Files';
  const header = <PageHeader title={title} subtitle={ws ? (path ? `${ws.label}/${path}` : ws.path) : undefined} />;

  if (!enabled || !ws) {
    return (
      <Screen header={header}>
        <WorkspaceUnavailable known={Boolean(ws)} enabled={enabled} />
      </Screen>
    );
  }

  return (
    <Screen header={header}>
      {!path ? (
        <Button
          label="Changes"
          icon="diff"
          variant="secondary"
          onPress={() => router.push({ pathname: '/changes/[workspace]', params: { workspace: ws.id } })}
          accessibilityLabel={`Open the git changes of ${ws.label}`}
        />
      ) : null}
      {!connected ? (
        <Text variant="sm" color={colors.warning}>
          Not connected: files load when the connection to your PC is back.
        </Text>
      ) : null}
      {error ? (
        <Card style={{ gap: spacing.sm }}>
          <Text variant="sm" color={colors.error} accessibilityRole="alert">
            {error}
          </Text>
          <Button label="Try again" variant="secondary" size="sm" onPress={load} />
        </Card>
      ) : null}
      {loading && !listing ? <ActivityIndicator color={colors.black} /> : null}
      {listing ? (
        listing.entries.length === 0 ? (
          <EmptyState icon="folder" contour={false} title="Empty folder" />
        ) : (
          <Card style={{ paddingVertical: spacing.xs }}>
            {listing.entries.map((e, i) => {
              const target = childPath(listing.path, e.name);
              const dir = e.kind === 'dir';
              return (
                <Pressable
                  key={e.name}
                  onPress={() =>
                    router.push({
                      pathname: dir ? '/files/[workspace]' : '/file/[workspace]',
                      params: { workspace: ws.id, path: target },
                    })
                  }
                  accessibilityRole="button"
                  accessibilityLabel={`${dir ? 'Folder' : 'File'} ${e.name}`}
                  style={({ pressed }) => [
                    styles.row,
                    i < listing.entries.length - 1 && styles.divider,
                    pressed && styles.pressed,
                  ]}
                >
                  <Icon name={dir ? 'folder' : 'file'} size={18} color={dir ? colors.black : colors.text2} />
                  <Text variant={dir ? 'smStrong' : 'sm'} numberOfLines={1} style={styles.flex}>
                    {e.name}
                  </Text>
                  <Text variant="xs" color={colors.text3}>
                    {dir ? relativeTime(e.mtime, now) : `${formatBytes(e.size)} · ${relativeTime(e.mtime, now)}`}
                  </Text>
                  {dir ? <Icon name="chevron-right" size={16} color={colors.text3} /> : null}
                </Pressable>
              );
            })}
          </Card>
        )
      ) : null}
      {listing?.truncated ? (
        <Text variant="xs" color={colors.text3} center>
          Showing the first {listing.entries.length} entries; the rest are on the PC.
        </Text>
      ) : null}
    </Screen>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  pressed: { opacity: 0.7 },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    minHeight: 48,
    paddingVertical: spacing.sm,
  },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
