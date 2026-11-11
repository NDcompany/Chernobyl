//! ADR-016: workspace files (read-only) and a small git surface, so the owner
//! can review and ship an agent's work from the phone. Everything is scoped
//! to one operator-allow-listed workspace (ADR-015 §4.2.3); the phone names
//! it by id and sends workspace-relative paths.
//!
//! - Paths: relative only, no `..`, no drive, stream or NUL (`:`/`\0`), never
//!   `.git`. The canonical result must stay inside the canonical workspace
//!   and outside the CHERNOBYL data dir, so a symlink or junction cannot
//!   escape.
//! - Files: listing (dirs first, at most [`LIST_MAX`] entries) and UTF-8
//!   text reads (at most [`READ_MAX`] bytes; binary refused), redacted with
//!   `runtime_io::redact`; `.env*` files have every value masked.
//! - Git: an absolute-path `git` binary with an argument vector (no shell),
//!   cwd = the workspace, the scrubbed child env plus no prompts, timeouts
//!   and output caps. No force, no amend, no hook bypass; pull is a
//!   fast-forward only (ADR-016 addendum).

use crate::claude_live::plain;
use crate::json::{self, Json};
use crate::runtime_io::{child_env, no_window, redact};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Entries per folder listing (T1).
pub const LIST_MAX: usize = 2000;
/// Bytes of one file read; the rest is cut and marked `truncated`.
pub const READ_MAX: usize = 256 << 10;
/// Bytes of one diff.
pub const DIFF_MAX: usize = 256 << 10;
/// Files per `git_status` (staged + unstaged + untracked + conflicted).
pub const STATUS_MAX: usize = 2000;
const GIT_OUT_MAX: usize = 4 << 20;
const GIT_ERR_MAX: usize = 64 << 10;
/// A NUL in the first bytes means binary.
const BINARY_SNIFF: usize = 8192;
const SUMMARY_LINES: usize = 6;
const SUMMARY_CHARS: usize = 600;

pub const READ_TIMEOUT: Duration = Duration::from_secs(20);
pub const STAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Commit hooks (lint, tests) run exactly as on the desktop.
pub const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
pub const PULL_TIMEOUT: Duration = Duration::from_secs(60);
/// A grandchild (ssh, a credential helper) may keep git's pipes open.
const PIPE_GRACE: Duration = Duration::from_secs(2);

pub const NOT_A_REPO: &str = "not a git repository";
pub const NO_GIT: &str = "git is not installed on the PC (git.exe is not on PATH)";
pub const NOTHING_TO_COMMIT: &str = "nothing to commit: stage some changes first";
const GIT_INTERNALS: &str = "the .git folder is not shown";
const SUBFOLDER: &str = "this workspace is a subfolder of a git repository; git actions need the repository root as the workspace";

/// One allow-listed workspace, opened for one action.
pub struct Workspace {
    /// Canonical, without the `\\?\` prefix.
    root: PathBuf,
    data_dir: PathBuf,
}

impl Workspace {
    pub fn open(root: &str, data_dir: &Path) -> Result<Workspace, String> {
        let canon = std::fs::canonicalize(root)
            .map_err(|_| "the workspace folder no longer exists".to_string())?;
        let data = std::fs::canonicalize(data_dir)
            .map(|d| PathBuf::from(plain(&d)))
            .unwrap_or_else(|_| data_dir.to_path_buf());
        Ok(Workspace {
            root: PathBuf::from(plain(&canon)),
            data_dir: data,
        })
    }

    /* ---------------------------------------------------------------- */
    /* Containment                                                       */
    /* ---------------------------------------------------------------- */

    /// The canonical path of an existing entry, inside the workspace.
    fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let joined = components(rel)?
            .iter()
            .fold(self.root.clone(), |p, c| p.join(c));
        let canon =
            std::fs::canonicalize(&joined).map_err(|_| "no such file or folder".to_string())?;
        let canon = PathBuf::from(plain(&canon));
        self.contain(&canon)?;
        Ok(canon)
    }

    /// The canonical result is checked, never the request: this is what
    /// refuses a symlink or junction that points out of the workspace.
    fn contain(&self, canon: &Path) -> Result<(), String> {
        let rest = canon
            .strip_prefix(&self.root)
            .map_err(|_| "the path leaves the workspace".to_string())?;
        if rest
            .components()
            .any(|c| is_git_dir(&c.as_os_str().to_string_lossy()))
        {
            return Err(GIT_INTERNALS.into());
        }
        if canon.starts_with(&self.data_dir) {
            return Err("the CHERNOBYL data folder is never shown".into());
        }
        Ok(())
    }

    /// A git pathspec for `rel`, which may no longer exist (a deleted file):
    /// lexically clean, and its deepest existing ancestor resolves inside.
    /// Used with `--literal-pathspecs`, so `*` or `:(top)` mean nothing.
    fn git_path(&self, rel: &str) -> Result<String, String> {
        let parts = components(rel)?;
        if parts.is_empty() {
            return Ok(".".into());
        }
        let mut probe = parts.iter().fold(self.root.clone(), |p, c| p.join(c));
        while !probe.exists() && probe.pop() {}
        let canon = std::fs::canonicalize(&probe)
            .map_err(|_| "the path leaves the workspace".to_string())?;
        self.contain(&PathBuf::from(plain(&canon)))?;
        Ok(parts.join("/"))
    }

    /* ---------------------------------------------------------------- */
    /* Files (read-only)                                                 */
    /* ---------------------------------------------------------------- */

    /// `fs_list`: folders first, then files, by name ignoring case. `.git`
    /// is left out; so is an entry whose target is gone.
    pub fn list(&self, rel: &str) -> Result<Json, String> {
        self.list_capped(rel, LIST_MAX)
    }

    fn list_capped(&self, rel: &str, cap: usize) -> Result<Json, String> {
        let dir = self.resolve(rel)?;
        let read = std::fs::read_dir(&dir).map_err(|_| "not a folder".to_string())?;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_git_dir(&name) || entry.path() == self.data_dir {
                continue;
            }
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if entries.len() == cap {
                truncated = true;
                break;
            }
            let size = if meta.is_dir() { 0 } else { meta.len() as i64 };
            entries.push((meta.is_dir(), name, size, mtime_ms(&meta)));
        }
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        let entries = entries
            .into_iter()
            .map(|(dir, name, size, mtime)| {
                json::obj(vec![
                    ("name", json::s(&name)),
                    ("kind", json::s(if dir { "dir" } else { "file" })),
                    ("size", Json::Int(size)),
                    ("mtime", Json::Int(mtime)),
                ])
            })
            .collect();
        Ok(json::obj(vec![
            ("kind", json::s("fs_list")),
            ("path", json::s(&display(rel))),
            ("entries", Json::Arr(entries)),
            ("truncated", Json::Bool(truncated)),
        ]))
    }

    /// `fs_read`: UTF-8 text only, at most [`READ_MAX`] bytes, redacted
    /// (T2: the phone never receives a raw secret). `.env*` files are
    /// `sensitive` and show their keys only.
    pub fn read(&self, rel: &str) -> Result<Json, String> {
        let file = self.resolve(rel)?;
        let meta = std::fs::metadata(&file).map_err(|_| "no such file".to_string())?;
        if !meta.is_file() {
            return Err("not a file".into());
        }
        let mut buf = Vec::new();
        std::fs::File::open(&file)
            .and_then(|f| f.take(READ_MAX as u64 + 1).read_to_end(&mut buf))
            .map_err(|e| format!("could not read the file: {e}"))?;
        let truncated = buf.len() > READ_MAX;
        buf.truncate(READ_MAX);
        let text = decode_text(buf, truncated).ok_or("binary file")?;
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sensitive = is_env_file(&name);
        let shown = if sensitive {
            redact(&mask_env(&text))
        } else {
            redact(&text)
        };
        let mut fields = vec![
            ("kind", json::s("fs_read")),
            ("path", json::s(&display(rel))),
            ("size", Json::Int(meta.len() as i64)),
            ("truncated", Json::Bool(truncated)),
            ("redacted", Json::Bool(shown != text)),
        ];
        if sensitive {
            fields.push(("sensitive", Json::Bool(true)));
        }
        fields.push(("content", json::s(&shown)));
        Ok(json::obj(fields))
    }

    /* ---------------------------------------------------------------- */
    /* Git                                                               */
    /* ---------------------------------------------------------------- */

    /// Runs `git <args>` in the workspace: argv only, scrubbed env, stdin
    /// closed, killed at `timeout`, stdout kept up to `cap` bytes.
    fn git(&self, args: &[&str], timeout: Duration, cap: usize) -> Result<GitOut, String> {
        let mut cmd = Command::new(git_binary()?);
        cmd.args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(git_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = no_window(&mut cmd)
            .spawn()
            .map_err(|e| format!("could not start git: {e}"))?;
        let out = drain(child.stdout.take(), cap);
        let err = drain(child.stderr.take(), GIT_ERR_MAX);
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let (stdout, truncated) = out.recv_timeout(PIPE_GRACE).unwrap_or_default();
        let (stderr, _) = err.recv_timeout(PIPE_GRACE).unwrap_or_default();
        let status =
            status.ok_or_else(|| format!("git timed out after {} s", timeout.as_secs()))?;
        Ok(GitOut {
            ok: status.success(),
            stdout,
            truncated,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Trimmed stdout of a small successful query, `None` otherwise.
    fn git_text(&self, args: &[&str]) -> Option<String> {
        self.git(args, READ_TIMEOUT, 4096)
            .ok()
            .filter(|o| o.ok)
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// A git action that changes something: `Ok(stdout)` or the summary.
    fn git_change(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let out = self.git(args, timeout, 64 << 10)?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.ok {
            Ok(stdout)
        } else if out.stderr.trim().is_empty() {
            Err(summary(&stdout))
        } else {
            Err(summary(&out.stderr))
        }
    }

    /// The workspace is the top level of a repository. A subfolder is
    /// refused: status paths are repository-relative and a commit takes the
    /// whole index, both reaching outside the allow-listed folder.
    fn repo(&self) -> Result<(), String> {
        let out = self.git(&["rev-parse", "--show-prefix"], READ_TIMEOUT, 4096)?;
        if !out.ok {
            return Err(if out.stderr.contains("not a git repository") {
                NOT_A_REPO.into()
            } else {
                summary(&out.stderr)
            });
        }
        if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
            return Err(SUBFOLDER.into());
        }
        Ok(())
    }

    /// `git_status`: branch, upstream, ahead/behind and the changed files.
    pub fn status(&self) -> Result<Json, String> {
        self.repo()?;
        let out = self.git(
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=normal",
            ],
            READ_TIMEOUT,
            GIT_OUT_MAX,
        )?;
        if !out.ok {
            return Err(summary(&out.stderr));
        }
        Ok(parse_status(&out.stdout, out.truncated).to_json())
    }

    /// `git_diff`: the unified diff of one path or of everything, staged or
    /// not; no external diff drivers or textconv, capped and redacted.
    pub fn diff(&self, path: Option<&str>, staged: bool) -> Result<Json, String> {
        self.repo()?;
        let spec = path.map(|p| self.git_path(p)).transpose()?;
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
        ];
        if staged {
            args.push("--cached");
        }
        args.push("--");
        args.extend(spec.as_deref());
        let out = self.git(&args, READ_TIMEOUT, DIFF_MAX)?;
        if !out.ok {
            return Err(summary(&out.stderr));
        }
        let mut fields = vec![("kind", json::s("git_diff"))];
        if let Some(p) = &spec {
            fields.push(("path", json::s(p)));
        }
        fields.push(("staged", Json::Bool(staged)));
        fields.push(("truncated", Json::Bool(out.truncated)));
        fields.push((
            "patch",
            json::s(&redact(&String::from_utf8_lossy(&out.stdout))),
        ));
        Ok(json::obj(fields))
    }

    /// `git_stage`: `git add` of exactly these paths (deletions included).
    pub fn stage(&self, paths: &[String]) -> Result<String, String> {
        self.repo()?;
        let specs = self.git_paths(paths)?;
        let mut args = vec!["--literal-pathspecs", "add", "--"];
        args.extend(specs.iter().map(String::as_str));
        self.git_change(&args, STAGE_TIMEOUT)?;
        Ok(format!("staged {} path(s)", specs.len()))
    }

    /// `git_unstage`: back to HEAD in the index only; the working tree is
    /// never touched. A branch without a commit drops the index entries.
    pub fn unstage(&self, paths: &[String]) -> Result<String, String> {
        self.repo()?;
        let specs = self.git_paths(paths)?;
        let born = self
            .git_text(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_some();
        let mut args = if born {
            vec!["--literal-pathspecs", "restore", "--staged", "--"]
        } else {
            vec![
                "--literal-pathspecs",
                "rm",
                "--cached",
                "-r",
                "--quiet",
                "--",
            ]
        };
        args.extend(specs.iter().map(String::as_str));
        self.git_change(&args, STAGE_TIMEOUT)?;
        Ok(format!("unstaged {} path(s)", specs.len()))
    }

    /// `git_commit`: the staged changes with this message. Hooks run (never
    /// `--no-verify`), never `--amend`.
    pub fn commit(&self, message: &str) -> Result<String, String> {
        if message.trim().is_empty() {
            return Err("the commit message is empty".into());
        }
        self.repo()?;
        // exit 0: the index equals HEAD
        if self
            .git(&["diff", "--cached", "--quiet"], READ_TIMEOUT, 4096)?
            .ok
        {
            return Err(NOTHING_TO_COMMIT.into());
        }
        let out = self.git_change(&["commit", "-m", message], COMMIT_TIMEOUT)?;
        Ok(out.lines().next().unwrap_or("committed").trim().to_string())
    }

    /// `git_push`: the current branch to its configured upstream, never
    /// forced (no `--force`, and the refspec has no `+`). Pre-push hooks run.
    pub fn push(&self) -> Result<String, String> {
        self.repo()?;
        let (head, remote, merge) = self.upstream()?;
        let refspec = format!("refs/heads/{head}:{merge}");
        self.git_change(&["push", &remote, &refspec], PUSH_TIMEOUT)?;
        Ok(format!(
            "{head} -> {remote}/{}",
            merge.trim_start_matches("refs/heads/")
        ))
    }

    /// `git_pull`: a fast-forward of the current branch from its configured
    /// upstream, nothing else: `--ff-only` (never a merge commit),
    /// `--no-rebase` and `--no-autostash` over whatever the repository's
    /// config says. Diverged history, or local changes the incoming commits
    /// would overwrite, are refused by git and change nothing.
    pub fn pull(&self) -> Result<String, String> {
        self.repo()?;
        let (head, remote, merge) = self.upstream()?;
        let out = self.git_change(
            &[
                "pull",
                "--ff-only",
                "--no-rebase",
                "--no-autostash",
                "--no-edit",
                &remote,
                &merge,
            ],
            PULL_TIMEOUT,
        )?;
        let from = format!("{remote}/{}", merge.trim_start_matches("refs/heads/"));
        Ok(if out.contains("Already up to date") {
            format!("{head} is up to date with {from}")
        } else {
            format!("{from} -> {head}")
        })
    }

    /// The canonical workspace folder (ADR-020: the terminal's cwd).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// (branch, remote, remote ref) of the checked-out branch.
    fn upstream(&self) -> Result<(String, String, String), String> {
        let head = self
            .git_text(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok_or("HEAD is detached: check out a branch on the PC first")?;
        let remote = self.git_text(&["config", "--get", &format!("branch.{head}.remote")]);
        let merge = self.git_text(&["config", "--get", &format!("branch.{head}.merge")]);
        match (remote, merge) {
            (Some(r), Some(m))
                if r != "." && !r.starts_with('-') && m.starts_with("refs/heads/") =>
            {
                Ok((head, r, m))
            }
            _ => Err(format!(
                "branch {head} has no upstream; set one on the PC (git push -u)"
            )),
        }
    }

    fn git_paths(&self, paths: &[String]) -> Result<Vec<String>, String> {
        paths.iter().map(|p| self.git_path(p)).collect()
    }
}

/* -------------------------------------------------------------------- */
/* Paths                                                                 */
/* -------------------------------------------------------------------- */

/// `rel` split into its normal components; "" and "." are the root.
fn components(rel: &str) -> Result<Vec<&str>, String> {
    if rel.starts_with(['/', '\\']) || rel.contains([':', '\0']) {
        return Err("the path must be relative to the workspace".into());
    }
    let parts: Vec<&str> = rel
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    for p in &parts {
        if *p == ".." {
            return Err("the path must stay inside the workspace".into());
        }
        if is_git_dir(p) {
            return Err(GIT_INTERNALS.into());
        }
    }
    Ok(parts)
}

/// Windows ignores trailing dots and spaces: `.git.` opens `.git`.
fn is_git_dir(name: &str) -> bool {
    name.trim_end_matches(['.', ' '])
        .eq_ignore_ascii_case(".git")
}

/// The path as the phone shows it: `/`-separated, no leading `./`.
fn display(rel: &str) -> String {
    components(rel).unwrap_or_default().join("/")
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/* -------------------------------------------------------------------- */
/* Text                                                                  */
/* -------------------------------------------------------------------- */

/// UTF-8 text, or `None` for binary (a NUL early on, or invalid UTF-8 that
/// is not just a character cut by the read cap).
fn decode_text(buf: Vec<u8>, truncated: bool) -> Option<String> {
    if buf[..buf.len().min(BINARY_SNIFF)].contains(&0) {
        return None;
    }
    match String::from_utf8(buf) {
        Ok(s) => Some(s),
        Err(e) if truncated && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

/// `.env`, `.env.local`, `prod.env`: dotenv files hold secrets by design.
pub fn is_env_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == ".env" || n.starts_with(".env.") || n.ends_with(".env")
}

/// Every non-empty `KEY=value` keeps its key only; comments stay.
fn mask_env(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let body = line.trim_end_matches(['\r', '\n']);
            let ending = &line[body.len()..];
            match body.find('=') {
                Some(eq)
                    if !body.trim_start().starts_with('#') && !body[eq + 1..].trim().is_empty() =>
                {
                    format!("{}«redacted:env»{ending}", &body[..=eq])
                }
                _ => line.to_string(),
            }
        })
        .collect()
}

/// What a person needs from git's error output: the last lines without
/// `hint:` noise, credentials in URLs masked, redacted, short.
pub fn summary(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.starts_with("hint:"))
        .collect();
    let tail = lines[lines.len().saturating_sub(SUMMARY_LINES)..].join("\n");
    let out: String = redact(&mask_url_credentials(&tail))
        .chars()
        .take(SUMMARY_CHARS)
        .collect();
    if out.is_empty() {
        "git failed without a message".into()
    } else {
        out
    }
}

/// `https://user:token@host` → `https://«redacted:credentials»@host`.
fn mask_url_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("«redacted:credentials»");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/* -------------------------------------------------------------------- */
/* git process                                                           */
/* -------------------------------------------------------------------- */

struct GitOut {
    ok: bool,
    stdout: Vec<u8>,
    truncated: bool,
    stderr: String,
}

/// `git.exe` in an absolute PATH directory, else Git for Windows' default.
fn git_binary() -> Result<PathBuf, String> {
    let exe = if cfg!(windows) { "git.exe" } else { "git" };
    let on_path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    on_path
        .into_iter()
        .filter(|d| d.is_absolute())
        .map(|d| d.join(exe))
        .chain(cfg!(windows).then(|| PathBuf::from(r"C:\Program Files\Git\cmd\git.exe")))
        .find(|p| p.is_file())
        .ok_or_else(|| NO_GIT.to_string())
}

/// The scrubbed child env (T6) plus: never prompt (terminal or credential
/// manager), skip optional index locks, English messages. Variables that
/// would point git at another repository are dropped.
fn git_env() -> Vec<(OsString, OsString)> {
    let mut env = child_env(
        std::env::vars_os(),
        &[
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GCM_INTERACTIVE", "never"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("LC_ALL", "C"),
        ],
    );
    env.retain(|(k, _)| {
        !matches!(
            k.to_string_lossy().to_ascii_uppercase().as_str(),
            "GIT_DIR"
                | "GIT_WORK_TREE"
                | "GIT_INDEX_FILE"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_COMMON_DIR"
                | "GIT_NAMESPACE"
        )
    });
    env
}

/// Reads a pipe to its end on a thread, keeping at most `cap` bytes (the
/// rest is drained so git never blocks on a full pipe).
fn drain<R: Read + Send + 'static>(pipe: Option<R>, cap: usize) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let (mut kept, mut truncated, mut buf) = (Vec::new(), false, [0u8; 16 << 10]);
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = cap.saturating_sub(kept.len());
                        kept.extend_from_slice(&buf[..n.min(room)]);
                        truncated |= n > room;
                    }
                }
            }
            let _ = tx.send((kept, truncated));
        });
    }
    rx
}

/* -------------------------------------------------------------------- */
/* git status --porcelain=v2 --branch -z                                 */
/* -------------------------------------------------------------------- */

#[derive(Debug, PartialEq)]
pub struct GitFile {
    pub path: String,
    /// The porcelain letter: M A D R C T U.
    pub status: char,
    /// Renames and copies: the old path.
    pub orig: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Status {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    pub staged: Vec<GitFile>,
    pub unstaged: Vec<GitFile>,
    pub untracked: Vec<String>,
    pub conflicted: Vec<String>,
    pub truncated: bool,
}

pub fn parse_status(raw: &[u8], truncated: bool) -> Status {
    let text = String::from_utf8_lossy(raw);
    let mut records: Vec<&str> = text.split('\0').collect();
    if truncated {
        records.pop(); // cut mid-record
    }
    let mut s = Status {
        truncated,
        ..Status::default()
    };
    let mut records = records.into_iter();
    let mut files = 0;
    while let Some(rec) = records.next() {
        if let Some(header) = rec.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" if value != "(detached)" => s.branch = Some(value.into()),
                "branch.upstream" => s.upstream = Some(value.into()),
                "branch.ab" => {
                    for n in value.split(' ') {
                        if let Some(a) = n.strip_prefix('+') {
                            s.ahead = a.parse().unwrap_or(0);
                        } else if let Some(b) = n.strip_prefix('-') {
                            s.behind = b.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if files == STATUS_MAX && !rec.is_empty() {
            s.truncated = true;
            break;
        }
        // field counts: `1` has 8 before the path, `2` 9, `u` 10
        let (fields, path_at) = match rec.as_bytes().first() {
            Some(b'1') => (rec.splitn(9, ' ').collect::<Vec<_>>(), 8),
            Some(b'2') => (rec.splitn(10, ' ').collect(), 9),
            Some(b'u') => (rec.splitn(11, ' ').collect(), 10),
            Some(b'?') => {
                s.untracked.push(rec[2.min(rec.len())..].to_string());
                files += 1;
                continue;
            }
            _ => continue,
        };
        let (Some(xy), Some(path)) = (fields.get(1), fields.get(path_at)) else {
            continue;
        };
        files += 1;
        if rec.starts_with('u') {
            s.conflicted.push(path.to_string());
            continue;
        }
        let orig = if rec.starts_with('2') {
            records.next().map(str::to_string)
        } else {
            None
        };
        let mut xy = xy.chars();
        let (x, y) = (xy.next().unwrap_or('.'), xy.next().unwrap_or('.'));
        if x != '.' {
            s.staged.push(GitFile {
                path: path.to_string(),
                status: x,
                orig: orig.clone(),
            });
        }
        if y != '.' {
            s.unstaged.push(GitFile {
                path: path.to_string(),
                status: y,
                orig: None,
            });
        }
    }
    s
}

impl Status {
    pub fn to_json(&self) -> Json {
        let files = |list: &[GitFile]| {
            Json::Arr(
                list.iter()
                    .map(|f| {
                        let mut fields = vec![
                            ("path", json::s(&f.path)),
                            ("status", json::s(&f.status.to_string())),
                        ];
                        if let Some(o) = &f.orig {
                            fields.push(("origPath", json::s(o)));
                        }
                        json::obj(fields)
                    })
                    .collect(),
            )
        };
        let paths = |list: &[String]| Json::Arr(list.iter().map(|p| json::s(p)).collect());
        let mut fields = vec![
            ("kind", json::s("git_status")),
            ("branch", self.branch.as_deref().map_or(Json::Null, json::s)),
        ];
        if let Some(u) = &self.upstream {
            fields.push(("upstream", json::s(u)));
        }
        fields.extend([
            ("ahead", Json::Int(self.ahead)),
            ("behind", Json::Int(self.behind)),
            ("staged", files(&self.staged)),
            ("unstaged", files(&self.unstaged)),
            ("untracked", paths(&self.untracked)),
            ("conflicted", paths(&self.conflicted)),
            ("truncated", Json::Bool(self.truncated)),
        ]);
        json::obj(fields)
    }
}

/* -------------------------------------------------------------------- */
/* Tests                                                                 */
/* -------------------------------------------------------------------- */

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("chb-ws-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Test setup only: plain git with the repo's own identity.
    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new(git_binary().unwrap())
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo on `main` with one commit, its own identity, no signing.
    pub(crate) fn repo(tag: &str) -> PathBuf {
        let dir = temp(tag);
        git(&dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
        for (k, v) in [
            ("user.name", "Test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            git(&dir, &["config", k, v]);
        }
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(&dir, &["add", "README.md"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn open(root: &Path) -> (Workspace, PathBuf) {
        let data = temp("data");
        (
            Workspace::open(&root.to_string_lossy(), &data).unwrap(),
            data,
        )
    }

    fn get<'a>(v: &'a Json, k: &str) -> &'a Json {
        json::get(v, k).unwrap_or_else(|| panic!("no {k} in {}", json::stringify(v)))
    }

    #[test]
    fn paths_are_relative_clean_and_never_git() {
        assert_eq!(components("").unwrap(), Vec::<&str>::new());
        assert_eq!(components("./a\\b/").unwrap(), ["a", "b"]);
        for bad in [
            "..",
            "a/../../x",
            "/etc/passwd",
            "\\\\server\\share",
            "C:\\Windows",
            "C:x",
            "file.txt:stream",
            "a\0b",
            ".git",
            ".git/config",
            "sub/.GIT/HEAD",
            ".git.",
            ".git ",
        ] {
            assert!(components(bad).is_err(), "{bad:?} passed");
        }
    }

    #[test]
    fn containment_refuses_escapes_and_lists_dirs_first() {
        let root = temp("contain");
        let outside = temp("outside");
        std::fs::write(outside.join("secret.txt"), "outside").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        std::fs::write(root.join("A.txt"), "a").unwrap();
        let (ws, _) = open(&root);
        for bad in ["..", "../outside", &outside.to_string_lossy(), ".git"] {
            assert!(ws.list(bad).is_err(), "{bad} listed");
            assert!(ws.read(bad).is_err(), "{bad} read");
        }
        let listing = ws.list("").unwrap();
        let names: Vec<&str> = json::as_arr(get(&listing, "entries"))
            .unwrap()
            .iter()
            .map(|e| json::get_str(e, "name").unwrap())
            .collect();
        assert_eq!(names, ["src", "A.txt", "b.txt"], ".git hidden, dirs first");
        assert_eq!(json::get_str(&listing, "path"), Some(""));
        assert!(ws.read("src").is_err(), "a folder is not a file");
        assert!(ws.list("b.txt").is_err(), "a file is not a folder");
        // a junction out of the workspace is refused on its canonical path
        #[cfg(windows)]
        {
            let link = root.join("escape");
            let made = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .output()
                .unwrap();
            assert!(made.status.success(), "mklink /J failed");
            let err = ws.read("escape/secret.txt").unwrap_err();
            assert!(err.contains("leaves the workspace"), "{err}");
            assert!(ws.list("escape").is_err());
            assert!(ws.git_path("escape/new.txt").is_err());
            // a junction to .git is refused too
            let to_git = root.join("g");
            Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&to_git)
                .arg(root.join(".git"))
                .output()
                .unwrap();
            assert_eq!(ws.list("g").unwrap_err(), GIT_INTERNALS);
        }
    }

    #[test]
    fn data_dir_is_never_shown() {
        let root = temp("datadir");
        std::fs::create_dir_all(root.join("data")).unwrap();
        let ws = Workspace::open(&root.to_string_lossy(), &root.join("data")).unwrap();
        assert!(ws.list("data").is_err());
        let listing = ws.list("").unwrap();
        assert_eq!(json::as_arr(get(&listing, "entries")).unwrap().len(), 0);
    }

    #[test]
    fn reads_are_redacted_capped_and_refuse_binary() {
        let root = temp("read");
        std::fs::write(
            root.join(".env.local"),
            "# db\r\nDATABASE_URL=postgres://u:hunter2@db/app\r\nEMPTY=\r\nAPI_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz\r\n",
        )
        .unwrap();
        std::fs::write(
            root.join("config.js"),
            "const token = \"ghp_abcdefghijklmnopqrstuvwxyz0123456789\";\nconst x = 1;\n",
        )
        .unwrap();
        std::fs::write(root.join("big.txt"), "é".repeat(READ_MAX)).unwrap();
        std::fs::write(root.join("img.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        let (ws, _) = open(&root);

        let env = ws.read(".env.local").unwrap();
        let content = json::get_str(&env, "content").unwrap();
        assert!(
            !content.contains("hunter2") && !content.contains("sk-ant"),
            "{content}"
        );
        assert!(content.starts_with("# db\r\nDATABASE_URL=«redacted:env»\r\nEMPTY=\r\n"));
        assert_eq!(json::get_bool(&env, "sensitive"), Some(true));
        assert_eq!(json::get_bool(&env, "redacted"), Some(true));

        let js = ws.read("config.js").unwrap();
        let content = json::get_str(&js, "content").unwrap();
        assert!(!content.contains("ghp_"), "{content}");
        assert!(content.contains("const x = 1;"));
        assert_eq!(json::get(&js, "sensitive"), None);

        let big = ws.read("big.txt").unwrap();
        assert_eq!(json::get_bool(&big, "truncated"), Some(true));
        assert_eq!(json::get_i64(&big, "size"), Some(2 * READ_MAX as i64));
        let content = json::get_str(&big, "content").unwrap();
        assert!(content.len() <= READ_MAX && content.chars().all(|c| c == 'é'));

        assert_eq!(ws.read("img.png").unwrap_err(), "binary file");
    }

    #[test]
    fn listing_is_capped() {
        // the cap is a parameter so the test needs 4 files, not 2,001
        let root = temp("cap");
        for i in 0..4 {
            std::fs::write(root.join(format!("f{i}")), "").unwrap();
        }
        let (ws, _) = open(&root);
        let listing = ws.list_capped("", 3).unwrap();
        assert_eq!(json::as_arr(get(&listing, "entries")).unwrap().len(), 3);
        assert_eq!(json::get_bool(&listing, "truncated"), Some(true));
        let all = ws.list("").unwrap();
        assert_eq!(json::get_bool(&all, "truncated"), Some(false));
    }

    #[test]
    fn parses_porcelain_v2_status() {
        let raw = [
            "# branch.oid 1234",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 M. N... 100644 100644 100644 aaa bbb src/a.rs",
            "1 .M N... 100644 100644 100644 aaa bbb src/b with space.rs",
            "1 MD N... 100644 100644 000000 aaa bbb c.rs",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs",
            "? notes/",
            "",
        ]
        .join("\0");
        let s = parse_status(raw.as_bytes(), false);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        let staged: Vec<(&str, char)> = s
            .staged
            .iter()
            .map(|f| (f.path.as_str(), f.status))
            .collect();
        assert_eq!(staged, [("src/a.rs", 'M'), ("c.rs", 'M'), ("new.rs", 'R')]);
        assert_eq!(s.staged[2].orig.as_deref(), Some("old.rs"));
        let unstaged: Vec<(&str, char)> = s
            .unstaged
            .iter()
            .map(|f| (f.path.as_str(), f.status))
            .collect();
        assert_eq!(unstaged, [("src/b with space.rs", 'M'), ("c.rs", 'D')]);
        assert_eq!(s.conflicted, ["conflict.rs"]);
        assert_eq!(s.untracked, ["notes/"]);
        assert!(!s.truncated);
        // detached head; a cut record is dropped, not half-parsed
        let s = parse_status(b"# branch.head (detached)\x00? a\x00? b", true);
        assert_eq!(s.branch, None);
        assert_eq!(s.untracked, ["a"]);
        assert!(s.truncated);
        let json = s.to_json();
        assert_eq!(json::get(&json, "branch"), Some(&Json::Null));
    }

    #[test]
    fn summaries_mask_credentials_and_drop_hints() {
        let s = summary(
            "To https://bob:ghp_abcdefghijklmnopqrstuvwxyz0123456789@github.com/o/r.git\n ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs to 'https://bob:pw@github.com/o/r.git'\nhint: Updates were rejected\nhint: git pull\n",
        );
        assert!(
            !s.contains("ghp_") && !s.contains("bob") && !s.contains(":pw@"),
            "{s}"
        );
        assert!(s.contains("[rejected]") && !s.contains("hint:"), "{s}");
        assert_eq!(summary("  \n"), "git failed without a message");
    }

    #[test]
    fn git_status_stage_unstage_diff_and_commit() {
        let root = repo("flow");
        let (ws, _) = open(&root);
        let clean = ws.status().unwrap();
        assert_eq!(json::get_str(&clean, "branch"), Some("main"));
        assert_eq!(json::as_arr(get(&clean, "staged")).unwrap().len(), 0);

        std::fs::write(root.join("README.md"), "hello\nworld\n").unwrap();
        std::fs::write(
            root.join("new.txt"),
            "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789\n",
        )
        .unwrap();
        let s = ws.status().unwrap();
        assert_eq!(
            json::get_str(&json::as_arr(get(&s, "unstaged")).unwrap()[0], "path"),
            Some("README.md")
        );
        assert_eq!(
            json::as_arr(get(&s, "untracked")).unwrap(),
            [json::s("new.txt")]
        );

        let d = ws.diff(Some("README.md"), false).unwrap();
        assert!(json::get_str(&d, "patch").unwrap().contains("+world"));
        assert_eq!(ws.commit("nothing yet").unwrap_err(), NOTHING_TO_COMMIT);

        ws.stage(&["README.md".into(), "new.txt".into()]).unwrap();
        let staged = ws.diff(None, true).unwrap();
        let patch = json::get_str(&staged, "patch").unwrap();
        assert!(patch.contains("+world") && patch.contains("new.txt"));
        assert!(!patch.contains("ghp_"), "diffs are redacted: {patch}");

        ws.unstage(&["new.txt".into()]).unwrap();
        let s = ws.status().unwrap();
        assert_eq!(json::as_arr(get(&s, "staged")).unwrap().len(), 1);
        assert_eq!(
            json::as_arr(get(&s, "untracked")).unwrap(),
            [json::s("new.txt")]
        );

        assert!(ws.commit("  \n").is_err());
        let line = ws.commit("--amend is just text here").unwrap();
        assert!(line.contains("--amend is just text here"), "{line}");
        assert_eq!(git(&root, &["rev-list", "--count", "HEAD"]).trim(), "2");
        assert!(ws.stage(&["../x".into()]).is_err());
        assert!(ws.stage(&[".git/config".into()]).is_err());
        // a literal pathspec: `*` matches nothing
        assert!(ws.stage(&["*.txt".into()]).is_err());
    }

    #[test]
    fn honest_states_not_a_repo_and_subfolder() {
        let plain_dir = temp("norepo");
        let (ws, _) = open(&plain_dir);
        assert_eq!(ws.status().unwrap_err(), NOT_A_REPO);
        let root = repo("sub");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        let (ws, _) = open(&root.join("pkg"));
        assert_eq!(ws.status().unwrap_err(), SUBFOLDER);
    }

    #[test]
    fn push_reaches_the_upstream_and_reports_rejection() {
        let remote = temp("remote");
        git(
            &remote,
            &["-c", "init.defaultBranch=main", "init", "-q", "--bare"],
        );
        let root = repo("push");
        let (ws, _) = open(&root);
        let detached = ws.push().unwrap_err();
        assert!(detached.contains("no upstream"), "{detached}");
        git(
            &root,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&root, &["push", "-q", "-u", "origin", "main"]);

        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        ws.stage(&["a.txt".into()]).unwrap();
        ws.commit("add a").unwrap();
        let s = ws.status().unwrap();
        assert_eq!(json::get_i64(&s, "ahead"), Some(1));
        assert_eq!(ws.push().unwrap(), "main -> origin/main");
        assert_eq!(
            git(&remote, &["rev-parse", "main"]),
            git(&root, &["rev-parse", "HEAD"])
        );

        // someone else pushed first: rejected, never forced
        let other = temp("other");
        git(&other, &["clone", "-q", &remote.to_string_lossy(), "."]);
        for (k, v) in [
            ("user.name", "O"),
            ("user.email", "o@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git(&other, &["config", k, v]);
        }
        std::fs::write(other.join("b.txt"), "b\n").unwrap();
        git(&other, &["add", "b.txt"]);
        git(&other, &["commit", "-q", "-m", "b"]);
        git(&other, &["push", "-q"]);
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        ws.stage(&["c.txt".into()]).unwrap();
        ws.commit("add c").unwrap();
        let err = ws.push().unwrap_err();
        assert!(err.contains("rejected"), "{err}");
        assert_ne!(
            git(&remote, &["rev-parse", "main"]),
            git(&root, &["rev-parse", "HEAD"]),
            "the remote keeps the other commit"
        );
    }

    /// A clone of `remote` with its own identity, tracking main.
    fn clone_of(remote: &Path, tag: &str) -> PathBuf {
        let dir = temp(tag);
        git(&dir, &["clone", "-q", &remote.to_string_lossy(), "."]);
        for (k, v) in [
            ("user.name", "O"),
            ("user.email", "o@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            git(&dir, &["config", k, v]);
        }
        dir
    }

    fn commit_file(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-q", "-m", name]);
    }

    #[test]
    fn pull_fast_forwards_only() {
        let remote = temp("pull-remote");
        git(
            &remote,
            &["-c", "init.defaultBranch=main", "init", "-q", "--bare"],
        );
        let root = repo("pull");
        let (ws, _) = open(&root);
        // no upstream: refused before git runs
        let err = ws.pull().unwrap_err();
        assert!(err.contains("no upstream"), "{err}");
        git(
            &root,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&root, &["push", "-q", "-u", "origin", "main"]);
        // pull.rebase / merge.autostash in the repo's config change nothing
        git(&root, &["config", "pull.rebase", "true"]);
        assert_eq!(ws.pull().unwrap(), "main is up to date with origin/main");

        // someone else pushed: a fast-forward
        let other = clone_of(&remote, "pull-other");
        commit_file(&other, "b.txt", "b\n");
        git(&other, &["push", "-q"]);
        // an unrelated local edit stays where it is
        std::fs::write(root.join("README.md"), "hello\nlocal\n").unwrap();
        assert_eq!(ws.pull().unwrap(), "origin/main -> main");
        assert_eq!(
            git(&remote, &["rev-parse", "main"]),
            git(&root, &["rev-parse", "HEAD"])
        );
        assert!(root.join("b.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "hello\nlocal\n"
        );

        // a local edit the incoming commit would overwrite: git refuses
        commit_file(&other, "README.md", "hello\nremote\n");
        git(&other, &["push", "-q"]);
        let before = git(&root, &["rev-parse", "HEAD"]);
        let err = ws.pull().unwrap_err();
        assert!(err.contains("overwritten"), "{err}");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), before);
        git(&root, &["checkout", "--", "README.md"]);
        ws.pull().unwrap();

        // diverged: refused, never merged or rebased
        commit_file(&other, "c.txt", "c\n");
        git(&other, &["push", "-q"]);
        commit_file(&root, "d.txt", "d\n");
        let before = git(&root, &["rev-parse", "HEAD"]);
        let err = ws.pull().unwrap_err();
        assert!(
            err.contains("fast-forward") || err.contains("Not possible"),
            "{err}"
        );
        assert_eq!(
            git(&root, &["rev-parse", "HEAD"]),
            before,
            "no merge commit"
        );
        assert_eq!(
            git(&root, &["rev-list", "--count", "--merges", "HEAD"]).trim(),
            "0"
        );
        assert!(!root.join("c.txt").exists());
    }
}
