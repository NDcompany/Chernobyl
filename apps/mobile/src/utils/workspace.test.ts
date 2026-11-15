/**
 * Unit tests for the Files / Changes helpers (ADR-016). Run with `npm test -w @chernobyl/mobile`.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { Agent, WorkspaceResult } from '@chernobyl/protocol';
import {
  aheadBehind,
  baseName,
  changeSections,
  childPath,
  findWorkspace,
  formatBytes,
  gitStatusLabel,
  lineChunks,
  pullSource,
  pushTarget,
  workspaceForProject,
  agentConfigWarning,
} from './workspace.ts';

type GitStatus = Extract<WorkspaceResult, { kind: 'git_status' }>;

const status = (over: Partial<GitStatus> = {}): GitStatus => ({
  kind: 'git_status',
  branch: 'main',
  upstream: 'origin/main',
  ahead: 0,
  behind: 0,
  staged: [],
  unstaged: [],
  untracked: [],
  conflicted: [],
  truncated: false,
  ...over,
});

test('workspaces are found by id and by session project across agents', () => {
  const ws = { id: '3f2a9c1d0e4b', label: 'demo', path: 'C:\\work\\demo' };
  const agents = [
    { id: 'a', live: undefined },
    { id: 'b', live: { workspaces: [ws] } },
  ] as unknown as Agent[];
  assert.deepEqual(findWorkspace(agents, ws.id), ws);
  assert.equal(findWorkspace(agents, 'nope'), undefined);
  assert.deepEqual(workspaceForProject(agents, 'C:\\work\\demo'), ws);
  assert.equal(workspaceForProject(agents, 'C:\\work'), undefined);
});

test('paths join and split on /', () => {
  assert.equal(childPath('', 'src'), 'src');
  assert.equal(childPath('src', 'ui'), 'src/ui');
  assert.equal(baseName('src/ui/App.tsx', 'root'), 'App.tsx');
  assert.equal(baseName('', 'demo'), 'demo');
});

test('sizes and status letters read naturally', () => {
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(1536), '1.5 KB');
  assert.equal(formatBytes(200 * 1024), '200 KB');
  assert.equal(formatBytes(3 * 1024 * 1024), '3.0 MB');
  assert.equal(gitStatusLabel('M'), 'Modified');
  assert.equal(gitStatusLabel('X'), 'X');
});

test('change sections: conflicts first, empty ones dropped', () => {
  const s = status({
    staged: [{ path: 'a.ts', status: 'M' }],
    untracked: ['new.ts'],
    conflicted: ['c.ts'],
  });
  assert.deepEqual(
    changeSections(s).map((x) => [x.key, x.files.map((f) => `${f.status} ${f.path}`)]),
    [
      ['conflicted', ['U c.ts']],
      ['staged', ['M a.ts']],
      ['untracked', ['? new.ts']],
    ],
  );
  assert.deepEqual(changeSections(status()), []);
});

test('push target is honest about detached heads and missing upstreams', () => {
  assert.deepEqual(pushTarget(status()), { label: 'main → origin/main', canPush: true });
  assert.equal(pushTarget(status({ upstream: undefined })).canPush, false);
  assert.equal(pushTarget(status({ branch: null })).canPush, false);
  assert.deepEqual(pullSource(status()), { label: 'origin/main → main', canPull: true });
  assert.equal(pullSource(status({ upstream: undefined })).canPull, false);
  assert.equal(pullSource(status({ branch: null })).canPull, false);
  assert.equal(aheadBehind(status()), 'up to date');
  assert.equal(aheadBehind(status({ ahead: 2, behind: 1 })), '↑2 ↓1');
  assert.equal(aheadBehind(status({ upstream: undefined })), '');
});

test('file content is chunked by lines without a trailing empty line', () => {
  assert.deepEqual(lineChunks('a\nb\nc\n', 2), ['a\nb', 'c']);
  assert.deepEqual(lineChunks(''), ['']);
});

test('agentConfigWarning names the changed paths and caps long lists', () => {
  assert.equal(agentConfigWarning(undefined), undefined);
  assert.equal(agentConfigWarning([]), undefined);
  const one = agentConfigWarning(['.claude/settings.json'])!;
  assert.match(one, /^This pull changed agent settings: \.claude\/settings\.json\. Agents in this workspace follow them/);
  assert.match(one, /tool permissions and hooks/);
  assert.match(one, /review them on your PC\.$/);
  const many = agentConfigWarning(['a', 'b', 'c', 'd', 'e', 'f', 'g'])!;
  assert.match(many, /a, b, c, d, e and 2 more\./);
});
