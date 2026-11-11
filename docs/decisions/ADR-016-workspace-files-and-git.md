# ADR-016: Workspace files and git from the phone

**Status:** Accepted 2026-11-12 (delegated by the owner). Implemented on branch `feat/files-git`.
**Relates to:** ADR-009 (mock honesty), ADR-010 (std-only Rust), ADR-011 (E2E), ADR-015 (live sessions; workspace allow-list §4.2.3, T1-T11), threat model `PHASE5-THREAT-MODEL.md` §3.12, `apps/mobile/DESIGN.md`.

## 1. Context

With ADR-015 the owner can start and steer an agent from the phone, but not review or ship what it did: the conversation shows per-tool diffs, not the working tree, and committing or pushing still needs the PC. Comparable phone clients (CC Pocket, CloudCLI, Paseo) offer a file browser and a git panel. CHERNOBYL already has the right scope for both: the operator's workspace allow-list (`Agent.live.workspaces`, ids = first 12 hex of sha256 of the canonical path), which is the folder-trust decision.

## 2. Decision

Additive protocol, still v2, advertised as `hello_ack.features` `workspace.v1`. The phone shows Files and Changes only when the companion advertises it and the agent lists the workspace; the TS `MockEngine` and the Node reference host do not advertise it and refuse the messages ("connect a PC to browse files and git").

| Message | Kind | Answer |
|---|---|---|
| `fs_list {workspaceId, path?}` | read | `workspace_result {result: {kind: fs_list, path, entries[{name, kind: dir\|file, size, mtime}], truncated}}` then `ack` |
| `fs_read {workspaceId, path}` | read | `{kind: fs_read, path, size, truncated, redacted, sensitive?, content}` then `ack` |
| `git_status {workspaceId}` | read | `{kind: git_status, branch\|null, upstream?, ahead, behind, staged[], unstaged[], untracked[], conflicted[], truncated}` (`staged`/`unstaged` items: `{path, status: M\|A\|D\|R\|C\|T\|U, origPath?}`) then `ack` |
| `git_diff {workspaceId, path?, staged?}` | read | `{kind: git_diff, path?, staged, truncated, patch}` then `ack` |
| `git_stage {workspaceId, paths[]}` | command | `ack` on acceptance, one `command_result` |
| `git_unstage {workspaceId, paths[]}` | command | same |
| `git_commit {workspaceId, message}` | command | same; audit record `git-commit` |
| `git_push {workspaceId}` | command | same; audit record `git-push` |
| `git_pull {workspaceId}` | command | same; audit record `git-pull` (addendum, §7) |

- **Reads** run on the connection's read loop and are answered to that connection only: the `workspace_result` goes out right before the ack (`server.rs`), so the phone's transport resolves the ack after the result arrived. They are never journaled.
- **Commands** take the existing path in `Companion::handle_client_msg`: persisted before execution, deduplicated by `commandId` (a replay returns the original ack and runs nothing), acked on acceptance, run on a worker thread, closed by exactly one `command_result` whose `error` is git's own summary (G1).
- **Rust:** one `ClientMsg::Workspace {command_id, workspace_id, op: WorkspaceOp}` variant and `ServerMsg::WorkspaceResult` (`protocol.rs`); the work is in `src/workspace.rs`. **TS:** `WorkspaceClientMessage`, `WorkspaceResult`, `WorkspaceServerMessage` and the validators (`packages/protocol/src/index.ts`). Golden fixtures: `packages/protocol/fixtures/live/client-valid.json`, `client-invalid.json`, `workspace-results.json`, read by both stacks.

## 3. Scope and bounds (T1)

| Bound | Value |
|---|---|
| Workspace | an `Agent.live.workspaces` id of any configured live runtime; anything else is "unknown workspace" |
| Path | relative, ≤ 1,024 UTF-16 units; ≤ 256 paths per stage/unstage |
| Commit message | 1-4,096 units, not blank |
| Listing | 2,000 entries (`truncated`) |
| File read | 256 KiB (`truncated`); UTF-8 only, a NUL in the first 8 KiB or invalid UTF-8 is "binary file" |
| Diff | 256 KiB (`truncated`) |
| Status | 2,000 files, 4 MiB of git output (`truncated`) |
| Concurrency | one git command at a time per workspace ("another git action is still running"); reads are one at a time per connection |
| Timeouts | reads 20 s, stage/unstage 30 s, commit 120 s (hooks), push 60 s; git is killed at the deadline |

## 4. Threats and mitigations

| # | Threat | Mitigation | Tests |
|---|---|---|---|
| W1 | Path traversal out of the workspace (`..`, absolute, drive, UNC, ADS `:`, NUL) | Rejected lexically; then the canonical result must stay under the canonical workspace root (`strip_prefix`), so a symlink or junction that points out is refused. Git paths that no longer exist (a deleted file) are checked through their deepest existing ancestor | `paths_are_relative_clean_and_never_git`, `containment_refuses_escapes_and_lists_dirs_first` (junction via `mklink /J`) |
| W2 | Reading repository internals or the companion's data | `.git` (also `.git.`, `.GIT `, and a junction resolving into it) is never listed or opened; the CHERNOBYL data dir is never listed or opened (config already forbids it inside a workspace) | same, `data_dir_is_never_shown` |
| W3 | Secrets reach the phone | File content and diffs go through `runtime_io::redact`; `.env`, `.env.*`, `*.env` files are `sensitive` and every non-empty value is masked (`KEY=«redacted:env»`) before redaction; git error summaries mask URL credentials (`https://user:token@`) and are redacted | `reads_are_redacted_capped_and_refuse_binary`, `git_status_stage_unstage_diff_and_commit`, `summaries_mask_credentials_and_drop_hints`, golden `fs_read` fixture |
| W4 | Command injection through git | `git.exe` resolved to an absolute path (absolute PATH entries, then Git for Windows' default), run with an argument vector and no shell, `CREATE_NO_WINDOW`, stdin closed. Paths go after `--` with `--literal-pathspecs` (no globs, no `:(magic)`). The push refspec is built by the companion: `refs/heads/<branch>:<upstream ref>`, no `+`; a remote name starting with `-` is refused | `git_status_stage_unstage_diff_and_commit` (`*.txt` matches nothing), `push_reaches_the_upstream_and_reports_rejection` |
| W5 | Git inherits the companion's secrets or prompts on the desktop | `child_env` (no `HOST_TOKEN`, `CHERNOBYL_*`, `NODE_OPTIONS`) plus `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`, `GIT_OPTIONAL_LOCKS=0`, `LC_ALL=C`; `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and friends are dropped | design; `child_env` tests (ADR-015 T6) |
| W6 | Destructive or history-rewriting git from a stolen phone | Only stage, unstage (index only, the working tree is never touched), commit (never `--amend`, never `--no-verify`: hooks run) and push (current branch to its configured upstream only, never forced; a rejected push changes nothing). No pull, no checkout, no reset, no file writes. One git command at a time per workspace. Commit and push leave audit records; every command is journaled; revocation cuts the phone off | `push_reaches_the_upstream_and_reports_rejection`, `git_commit_runs_once_per_command_id_and_is_audited`, `one_git_action_at_a_time_per_workspace` |
| W7 | A workspace inside a larger repository leaks or commits outside the allow-list | Git actions require the workspace to be the repository top level (`rev-parse --show-prefix` empty); otherwise an honest refusal | `honest_states_not_a_repo_and_subfolder` |
| W8 | Resource exhaustion | §3 caps and timeouts; output kept up to the cap while the rest is drained; a grandchild holding the pipes open cannot stall the reply past 2 s | `listing_is_capped`, `reads_are_redacted_capped_and_refuse_binary` |
| W9 | Transport | Every new message needs a live E2E session beyond loopback (the existing D2 gate); `workspace_result` goes through `Outbound::enqueue_sealed` | `workspace_reads_require_session_at_remote_posture` |

**Found while building it.** Every child the companion spawns (git here, and already claude.exe and codex) inherited the server's sockets: the backlog listener came from plain `socket()`, so it and every socket it accepted were inheritable, and std's Windows `try_clone` returns an inheritable handle. A child kept a connection the companion had closed open until it exited, then reset it (the old `control_routes_refuse_a_missing_or_wrong_control_key` flake, reproduced every run beside the git tests). The listener is now created with `WSA_FLAG_NO_HANDLE_INHERIT` and every server clone goes through `ffi::clone_stream` / `clone_listener` (`server_sockets_are_never_inherited_by_children`).

**Residuals.** Redaction recognises known shapes only: a secret in an unusual format in a source file is shown. Git runs the repository's own config and hooks (as it does on the desktop): `core.fsmonitor`, hooks and credential helpers are the trusted workspace's, and allow-listing a folder already means trusting it (ADR-015 T11). Commit messages are journaled with the command (the 5,000-record retention) and their first line is in the audit log. The phone holds results in screen state only, never persisted.

## 5. Phone

Entry points: the agent screen's Workspaces section (Files, Changes per workspace) and the live conversation's header chips (Files, Changes for the session's workspace, matched by `Session.project` = workspace path). Screens: `files/[workspace]` (one screen per folder, `?path=`), `file/[workspace]` (mono on black, sideways scroll, notes for masked, sensitive and truncated content, "Binary file" state), `changes/[workspace]` (branch and ahead/behind, sections Conflicts / Staged / Changes / Untracked, tap for the `DiffView`, Stage / Unstage and Stage all / Unstage all, commit box, Push with a confirm naming branch → upstream; git's own error text in a "Git said" card; honest "Not a git repository", "Git is not installed on the PC", "Not the repository root" and "Nothing to commit" states). Without `workspace.v1` (the simulation, an older companion) the entry points are hidden and a deep link shows "Connect a PC".

## 6. Deliberately not included

No terminal or arbitrary command execution (since added as the opt-in ADR-020, off by default); no file editing, creation, rename or delete; no fetch, checkout, branch switching, reset, revert, stash, rebase or merge (a fast-forward-only pull was added, §7); no force-push, no amend, no hook bypass; no image or binary preview; no search across files. Each would widen T1 beyond "review and ship"; they need their own decision.

## 7. Addendum 2026-11-12: `git_pull` (fast-forward only)

So the owner can bring a branch up to date before reviewing or pushing, `git_pull {workspaceId}` runs `git pull --ff-only --no-rebase --no-autostash --no-edit <remote> <merge ref>` for the checked-out branch's configured upstream (the same `upstream()` lookup as push: a detached HEAD, no upstream, a `.` or `-`-leading remote are refused before git runs). It can never create a merge commit, rebase or stash, whatever `pull.rebase` or `merge.autostash` say. Diverged history, or a local change the incoming commits would overwrite, is refused by git with its own summary and changes nothing; unrelated local changes stay. Same lifecycle as push: journaled, deduplicated, one git command at a time per workspace, 60 s timeout, `command_result` with git's summary, audit record `git-pull`. W6 now reads "stage, unstage, commit, push, and a fast-forward pull". Phone: a Pull button on Changes (label "Pull N commit(s)" when the last status knew it was behind) with a confirm naming `upstream → branch`; the status is re-read after it. Tests: `pull_fast_forwards_only` (up to date with `pull.rebase=true` set, a fast-forward keeping an unrelated local edit, an overwriting local edit refused, diverged refused with no merge commit, no upstream refused), `git_commit_runs_once_per_command_id_and_is_audited` (the `git-pull` audit), fixtures `git_pull` in `client-valid.json` / `client-invalid.json`.
