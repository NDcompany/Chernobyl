/**
 * ADR-016 pure helpers for the Files and Changes screens (unit-tested in
 * workspace.test.ts). Paths are workspace-relative and `/`-separated; '' is
 * the workspace root.
 */
import type { Agent, GitFile, WorkspaceResult } from '@chernobyl/protocol';

export interface WorkspaceRef {
  id: string;
  label: string;
  path: string;
}

type GitStatus = Extract<WorkspaceResult, { kind: 'git_status' }>;

/** The allow-listed workspace with this id, from any agent's `live.workspaces`. */
export function findWorkspace(agents: readonly Agent[], id: string): WorkspaceRef | undefined {
  for (const a of agents) {
    const ws = a.live?.workspaces?.find((w) => w.id === id);
    if (ws) return ws;
  }
  return undefined;
}

/** The workspace a session runs in (`Session.project` is the workspace path). */
export function workspaceForProject(agents: readonly Agent[], project: string): WorkspaceRef | undefined {
  for (const a of agents) {
    const ws = a.live?.workspaces?.find((w) => w.path === project);
    if (ws) return ws;
  }
  return undefined;
}

export function childPath(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

/** The last path component, or `fallback` at the root. */
export function baseName(path: string, fallback: string): string {
  const i = path.lastIndexOf('/');
  return path ? path.slice(i + 1) : fallback;
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10 * 1024 ? 1 : 0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

const STATUS_LABEL: Record<string, string> = {
  M: 'Modified',
  A: 'Added',
  D: 'Deleted',
  R: 'Renamed',
  C: 'Copied',
  T: 'Type changed',
  U: 'Conflict',
};

export function gitStatusLabel(letter: string): string {
  return STATUS_LABEL[letter] ?? letter;
}

export interface ChangeSection {
  key: 'conflicted' | 'staged' | 'unstaged' | 'untracked';
  title: string;
  files: GitFile[];
}

/** Non-empty sections in review order: conflicts first, then staged, changes, untracked. */
export function changeSections(s: GitStatus): ChangeSection[] {
  const sections: ChangeSection[] = [
    { key: 'conflicted', title: 'Conflicts', files: s.conflicted.map((path) => ({ path, status: 'U' })) },
    { key: 'staged', title: 'Staged', files: s.staged },
    { key: 'unstaged', title: 'Changes', files: s.unstaged },
    { key: 'untracked', title: 'Untracked', files: s.untracked.map((path) => ({ path, status: '?' })) },
  ];
  return sections.filter((x) => x.files.length > 0);
}

/** "main → origin/main", or why there is nowhere to push. */
export function pushTarget(s: GitStatus): { label: string; canPush: boolean } {
  if (s.branch === null) return { label: 'Detached HEAD: check out a branch on the PC', canPush: false };
  if (!s.upstream) return { label: `${s.branch} has no upstream: set one on the PC`, canPush: false };
  return { label: `${s.branch} → ${s.upstream}`, canPush: true };
}

/** "↑2 ↓1", "up to date", or '' without an upstream. */
export function aheadBehind(s: GitStatus): string {
  if (!s.upstream) return '';
  if (s.ahead === 0 && s.behind === 0) return 'up to date';
  return [s.ahead ? `↑${s.ahead}` : '', s.behind ? `↓${s.behind}` : ''].filter(Boolean).join(' ');
}

/** The file viewer's code block, split so no single Text holds the whole file. */
export function lineChunks(content: string, size = 200): string[] {
  const lines = content.split('\n');
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  const chunks: string[] = [];
  for (let i = 0; i < lines.length; i += size) chunks.push(lines.slice(i, i + size).join('\n'));
  return chunks;
}
