/**
 * ADR-016 Files / Changes pieces, presentational only (data via props; pure
 * logic in `src/utils/workspace.ts`).
 */
import { useMemo } from 'react';
import { Pressable, Text as RNText, ScrollView, StyleSheet, View } from 'react-native';
import type { GitFile } from '@chernobyl/protocol';
import { colors, radii, spacing, typography } from '@/theme/tokens';
import { gitStatusLabel, lineChunks } from '@/utils/workspace';
import { Text } from '@/ui/primitives';
import { EmptyState } from '@/ui/surface';

/** ADR-009: the simulation has no files; an unknown workspace id is said plainly. */
export function WorkspaceUnavailable({ known, enabled }: { known: boolean; enabled: boolean }) {
  return enabled && !known ? (
    <EmptyState
      icon="folder"
      title="Workspace not found"
      caption="It is no longer allow-listed on your PC, or its agent is offline."
    />
  ) : (
    <EmptyState
      icon="monitor"
      title="Connect a PC"
      caption="Files and git changes come from the CHERNOBYL companion on your PC. This host does not offer them."
    />
  );
}

/**
 * A text file: mono on black like the transcript's code blocks, long lines
 * scroll sideways. Lines end in a space (the code-block trick against
 * Android's pixel rounding) and render in chunks, so no single Text holds a
 * 256 KB file.
 */
export function CodeView({ content }: { content: string }) {
  const chunks = useMemo(() => lineChunks(content), [content]);
  return (
    <ScrollView horizontal nestedScrollEnabled style={styles.code} contentContainerStyle={styles.codeInner}>
      <View>
        {chunks.map((c, i) => (
          <Text key={i} variant="mono" color={colors.white} selectable>
            {c.replace(/\n/g, ' \n')}{' '}
          </Text>
        ))}
      </View>
    </ScrollView>
  );
}

export interface GitFileRowProps {
  file: GitFile;
  /** Stage / Unstage; absent for conflicts (resolved on the PC) */
  action?: { label: string; onPress: () => void; disabled?: boolean };
  /** tap the path: show the diff or open the file */
  onOpen?: () => void;
  open?: boolean;
  last?: boolean;
}

/** One changed file: status letter, path (middle-truncated), and its toggle. */
export function GitFileRow({ file, action, onOpen, open = false, last = false }: GitFileRowProps) {
  const label = file.status === '?' ? 'Untracked' : gitStatusLabel(file.status);
  return (
    <View style={[styles.fileRow, !last && styles.divider]}>
      <Pressable
        onPress={onOpen}
        disabled={!onOpen}
        accessibilityRole="button"
        accessibilityState={{ expanded: open }}
        accessibilityLabel={`${label}: ${file.path}${file.origPath ? `, from ${file.origPath}` : ''}`}
        style={({ pressed }) => [styles.fileMain, pressed && styles.pressed]}
      >
        <View style={styles.letter}>
          <Text variant="mono" color={file.status === 'U' ? colors.error : colors.text}>
            {file.status === '?' ? 'U?' : file.status}
          </Text>
        </View>
        <View style={styles.flex}>
          {/* RN Text: the primitive has no ellipsizeMode, and paths read best cut in the middle */}
          <RNText style={[typography.mono, { color: colors.text }]} numberOfLines={1} ellipsizeMode="middle">
            {file.path}
          </RNText>
          {file.origPath ? (
            <RNText style={[typography.xs, { color: colors.text3 }]} numberOfLines={1} ellipsizeMode="middle">
              from {file.origPath}
            </RNText>
          ) : null}
        </View>
      </Pressable>
      {action ? (
        <Pressable
          onPress={action.onPress}
          disabled={action.disabled}
          accessibilityRole="button"
          accessibilityLabel={`${action.label} ${file.path}`}
          style={({ pressed }) => [styles.toggle, (pressed || action.disabled) && styles.pressed]}
        >
          <Text variant="smStrong">{action.label}</Text>
        </Pressable>
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  pressed: { opacity: 0.6 },
  code: { backgroundColor: colors.black, borderRadius: radii.sm },
  codeInner: { padding: spacing.md },
  fileRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, minHeight: 48 },
  fileMain: { flex: 1, flexDirection: 'row', alignItems: 'center', gap: spacing.sm, paddingVertical: spacing.sm },
  letter: { minWidth: 28 },
  toggle: {
    minHeight: 36,
    justifyContent: 'center',
    paddingHorizontal: spacing.md,
    borderRadius: radii.pill,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.black,
  },
  divider: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
});
