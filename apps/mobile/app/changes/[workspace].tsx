/**
 * Changes (ADR-016): review and ship an agent's work from the phone. Git
 * status of one allow-listed workspace grouped Conflicts / Staged / Changes /
 * Untracked; tap a file for its diff (DiffView), Stage / Unstage toggles, a
 * commit message box, Pull (a fast-forward from the upstream only, after a
 * confirm naming upstream → branch) and Push (current branch to its upstream,
 * never forced, after a confirm naming branch → upstream). Every change is a
 * command with its own result; git's own error text is shown when it fails.
 */
import { useCallback, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';
import { Redirect, useLocalSearchParams, useRouter } from 'expo-router';
import { useForm } from 'react-hook-form';
import { z } from 'zod';
import { WORKSPACE_MESSAGE_MAX, type WorkspaceResult } from '@chernobyl/protocol';
import { colors, spacing } from '@/theme/tokens';
import { useAppStore } from '@/store/appStore';
import { useSnapshot } from '@/data/state';
import {
  useGitChange,
  useWorkspaceEnabled,
  useWorkspaceRead,
  workspaceQuery,
  type WorkspaceChange,
} from '@/data/workspace';
import { zodHandler } from '@/forms/zodBridge';
import {
  aheadBehind,
  changeSections,
  findWorkspace,
  pullSource,
  pushTarget,
  type ChangeSection,
} from '@/utils/workspace';
import { Button, Text, TextField } from '@/ui/primitives';
import { Card, ConfirmModal, EmptyState, PageHeader, Screen } from '@/ui/surface';
import { DiffView } from '@/ui/transcript';
import { Icon } from '@/ui/icons';
import { GitFileRow, WorkspaceUnavailable } from '@/ui/workspace';

type GitStatus = Extract<WorkspaceResult, { kind: 'git_status' }>;
type Diff = { key: string; patch?: string; truncated?: boolean; error?: string };

const commitSchema = z.object({
  message: z
    .string()
    .trim()
    .min(1, 'Write a commit message')
    .max(WORKSPACE_MESSAGE_MAX, 'Keep the message under 4,096 characters'),
});
type CommitForm = z.infer<typeof commitSchema>;

/** Honest states the companion reports as plain errors. */
const STATE_TITLES: Record<string, { title: string; icon: 'folder' | 'monitor' }> = {
  'not a git repository': { title: 'Not a git repository', icon: 'folder' },
  'git is not installed on the PC (git.exe is not on PATH)': { title: 'Git is not installed on the PC', icon: 'monitor' },
};

export default function ChangesScreen() {
  const authed = useAppStore((s) => s.authed);
  const connected = useAppStore((s) => s.connection === 'connected');
  const addToast = useAppStore((s) => s.addToast);
  const { workspace } = useLocalSearchParams<{ workspace: string }>();
  const snapshot = useSnapshot();
  const router = useRouter();
  const enabled = useWorkspaceEnabled();
  const change = useGitChange();
  const ws = findWorkspace(snapshot.agents, String(workspace));
  const read = useWorkspaceRead(ws && enabled ? { type: 'git_status', workspaceId: ws.id } : null);
  const status: GitStatus | null = read.result?.kind === 'git_status' ? read.result : null;
  const { error, loading, reload: refresh } = read;
  const [diff, setDiff] = useState<Diff | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [confirmPush, setConfirmPush] = useState(false);
  const [confirmPull, setConfirmPull] = useState(false);
  const form = useForm<CommitForm>({ defaultValues: { message: '' } });

  /** One git change at a time; the status is re-read after it, whatever happened. */
  const run = useCallback(
    async (what: string, msg: WorkspaceChange, done?: string) => {
      setBusy(what);
      setFailure(null);
      const outcome = await change(msg);
      setBusy(null);
      if (outcome && !outcome.ok) setFailure(outcome.error ?? `${what} failed`);
      if (outcome?.ok && done) addToast(done, 'success');
      refresh();
      return outcome?.ok === true;
    },
    [change, refresh, addToast],
  );

  if (!authed) return <Redirect href="/sign-in" />;

  const header = (
    <PageHeader
      title="Changes"
      subtitle={ws ? ws.label : undefined}
      right={
        ws && enabled ? (
          <Pressable
            onPress={refresh}
            disabled={loading}
            accessibilityRole="button"
            accessibilityLabel="Refresh the changes"
            style={({ pressed }) => [styles.iconBtn, (pressed || loading) && styles.pressed]}
          >
            <Icon name="refresh" size={20} color={colors.black} />
          </Pressable>
        ) : undefined
      }
    />
  );

  if (!enabled || !ws) {
    return (
      <Screen header={header}>
        <WorkspaceUnavailable known={Boolean(ws)} enabled={enabled} />
      </Screen>
    );
  }

  const state = error ? STATE_TITLES[error] : undefined;
  if (state || (error && error.includes('subfolder of a git repository'))) {
    return (
      <Screen header={header}>
        <EmptyState icon={state?.icon ?? 'folder'} title={state?.title ?? 'Not the repository root'} caption={error ?? undefined} />
        <Button
          label="Browse files"
          icon="folder"
          variant="secondary"
          onPress={() => router.push({ pathname: '/files/[workspace]', params: { workspace: ws.id } })}
        />
      </Screen>
    );
  }

  const sections = status ? changeSections(status) : [];
  const target = status ? pushTarget(status) : undefined;
  const source = status ? pullSource(status) : undefined;
  const sync = status ? aheadBehind(status) : '';

  const toggleDiff = (section: ChangeSection, path: string) => {
    if (section.key === 'untracked') {
      router.push({ pathname: '/file/[workspace]', params: { workspace: ws.id, path } });
      return;
    }
    const key = `${section.key}:${path}`;
    if (diff?.key === key) {
      setDiff(null);
      return;
    }
    setDiff({ key });
    workspaceQuery({ type: 'git_diff', workspaceId: ws.id, path, staged: section.key === 'staged' })
      .then((r) =>
        setDiff((d) =>
          d?.key === key && r.kind === 'git_diff' ? { key, patch: r.patch, truncated: r.truncated } : d,
        ),
      )
      .catch((e: unknown) =>
        setDiff((d) => (d?.key === key ? { key, error: e instanceof Error ? e.message : String(e) } : d)),
      );
  };

  const actionFor = (section: ChangeSection, path: string) => {
    if (section.key === 'conflicted') return undefined;
    const staged = section.key === 'staged';
    return {
      label: staged ? 'Unstage' : 'Stage',
      disabled: busy !== null,
      onPress: () =>
        void run(staged ? 'Unstage' : 'Stage', {
          type: staged ? 'git_unstage' : 'git_stage',
          workspaceId: ws.id,
          paths: [path],
        }),
    };
  };

  const commit = zodHandler(commitSchema, form, (data) => {
    void run('Commit', { type: 'git_commit', workspaceId: ws.id, message: data.message }, 'Committed').then(
      (ok) => {
        if (ok) form.reset({ message: '' });
      },
    );
  });

  const stagedCount = status?.staged.length ?? 0;

  return (
    <Screen header={header}>
      {!connected ? (
        <Text variant="sm" color={colors.warning}>
          Not connected: changes load when the connection to your PC is back.
        </Text>
      ) : null}

      {error ? (
        <Card style={{ gap: spacing.sm }}>
          <Text variant="sm" color={colors.error} accessibilityRole="alert">
            {error}
          </Text>
          <Button label="Try again" variant="secondary" size="sm" onPress={refresh} />
        </Card>
      ) : null}

      {!status ? (
        loading ? <ActivityIndicator color={colors.black} /> : null
      ) : (
        <>
          <Card style={{ gap: spacing.xs }}>
            <View style={styles.branchRow}>
              <Icon name="code" size={18} color={colors.black} />
              <Text variant="bodyStrong" style={styles.flex} numberOfLines={1}>
                {status.branch ?? 'Detached HEAD'}
              </Text>
              {sync ? (
                <Text variant="xs" color={colors.text2}>
                  {sync}
                </Text>
              ) : null}
            </View>
            <Text variant="xs" color={colors.text3} numberOfLines={1}>
              {status.upstream ? `Tracks ${status.upstream}` : 'No upstream branch'}
            </Text>
          </Card>

          {failure ? (
            <Card style={{ borderColor: colors.error, gap: spacing.xs }}>
              <Text variant="smStrong" color={colors.error} accessibilityRole="alert">
                Git said
              </Text>
              <Text variant="mono" color={colors.text} selectable>
                {failure}
              </Text>
            </Card>
          ) : null}

          {sections.length === 0 ? (
            <EmptyState
              icon="check"
              contour={false}
              title="Nothing to commit"
              caption="The working tree is clean."
            />
          ) : (
            sections.map((section) => (
              <View key={section.key} style={{ gap: spacing.xs }}>
                <View style={styles.sectionHead}>
                  <Text variant="eyebrow" color={colors.text3} accessibilityRole="header" style={styles.flex}>
                    {section.title} · {section.files.length}
                  </Text>
                  {section.key !== 'conflicted' ? (
                    <Pressable
                      disabled={busy !== null}
                      onPress={() =>
                        void run(section.key === 'staged' ? 'Unstage all' : 'Stage all', {
                          type: section.key === 'staged' ? 'git_unstage' : 'git_stage',
                          workspaceId: ws.id,
                          paths: section.files.slice(0, 256).map((f) => f.path),
                        })
                      }
                      accessibilityRole="button"
                      accessibilityLabel={`${section.key === 'staged' ? 'Unstage' : 'Stage'} all ${section.title.toLowerCase()}`}
                      style={({ pressed }) => [styles.allBtn, (pressed || busy !== null) && styles.pressed]}
                    >
                      <Text variant="smStrong">{section.key === 'staged' ? 'Unstage all' : 'Stage all'}</Text>
                    </Pressable>
                  ) : null}
                </View>
                {section.key === 'conflicted' ? (
                  <Text variant="xs" color={colors.error}>
                    Resolve conflicts on the PC.
                  </Text>
                ) : null}
                <Card style={{ paddingVertical: spacing.xs }}>
                  {section.files.map((f, i) => {
                    const key = `${section.key}:${f.path}`;
                    const open = diff?.key === key;
                    return (
                      <View key={key}>
                        <GitFileRow
                          file={f}
                          open={open}
                          last={i === section.files.length - 1 && !open}
                          onOpen={() => toggleDiff(section, f.path)}
                          action={actionFor(section, f.path)}
                        />
                        {open ? (
                          <View style={styles.diffBox}>
                            {diff?.error ? (
                              <Text variant="sm" color={colors.error}>
                                {diff.error}
                              </Text>
                            ) : diff?.patch === undefined ? (
                              <ActivityIndicator color={colors.black} />
                            ) : diff.patch ? (
                              <DiffView diff={{ path: f.path, patch: diff.patch, truncated: diff.truncated || undefined }} />
                            ) : (
                              <Text variant="sm" color={colors.text2}>
                                No text changes (a binary file or a mode change).
                              </Text>
                            )}
                          </View>
                        ) : null}
                      </View>
                    );
                  })}
                </Card>
              </View>
            ))
          )}

          {status.truncated ? (
            <Text variant="xs" color={colors.text3} center>
              Too many changes to list them all; the rest are on the PC.
            </Text>
          ) : null}

          <Card style={{ gap: spacing.md }}>
            <TextField
              form={form}
              name="message"
              label="Commit message"
              placeholder="Describe what changed"
              multiline
              hint={stagedCount === 0 ? 'Nothing staged: stage files to commit them.' : `${stagedCount} staged file(s)`}
            />
            <Button
              label="Commit"
              icon="check"
              loading={busy === 'Commit'}
              disabled={busy !== null || stagedCount === 0}
              onPress={commit}
            />
          </Card>

          {target && source ? (
            <View style={{ gap: spacing.xs }}>
              <Button
                label={status.behind > 0 ? `Pull ${status.behind} commit(s)` : 'Pull'}
                icon="arrow-left"
                variant="secondary"
                loading={busy === 'Pull'}
                disabled={busy !== null || !source.canPull}
                onPress={() => setConfirmPull(true)}
                accessibilityLabel={`Pull ${source.label}`}
              />
              <Button
                label={status.ahead > 0 ? `Push ${status.ahead} commit(s)` : 'Push'}
                icon="arrow-right"
                variant="secondary"
                loading={busy === 'Push'}
                disabled={busy !== null || !target.canPush}
                onPress={() => setConfirmPush(true)}
                accessibilityLabel={`Push ${target.label}`}
              />
              <Text variant="xs" color={colors.text3} center>
                {target.label}
              </Text>
            </View>
          ) : null}
        </>
      )}

      <ConfirmModal
        visible={confirmPull}
        title={`Pull into ${status?.branch ?? ''}?`}
        message={source?.label ?? ''}
        note="Fast-forward only: no merge commit, no rebase. If the branches have diverged or a local change would be overwritten, git refuses and nothing changes."
        confirmLabel="Pull"
        onConfirm={() => {
          setConfirmPull(false);
          void run('Pull', { type: 'git_pull', workspaceId: ws.id }, `Pulled ${source?.label ?? ''}`);
        }}
        onCancel={() => setConfirmPull(false)}
      />

      <ConfirmModal
        visible={confirmPush}
        title={`Push ${status?.branch ?? ''}?`}
        message={target?.label ?? ''}
        note="Never forced. If the remote has commits you do not have, the push is rejected and nothing changes."
        confirmLabel="Push"
        onConfirm={() => {
          setConfirmPush(false);
          void run('Push', { type: 'git_push', workspaceId: ws.id }, `Pushed ${target?.label ?? ''}`);
        }}
        onCancel={() => setConfirmPush(false)}
      />
    </Screen>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  pressed: { opacity: 0.6 },
  iconBtn: { minWidth: 44, minHeight: 44, alignItems: 'center', justifyContent: 'center' },
  branchRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  sectionHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  allBtn: { minHeight: 36, justifyContent: 'center', paddingHorizontal: spacing.sm },
  diffBox: { paddingBottom: spacing.sm, gap: spacing.sm },
});
