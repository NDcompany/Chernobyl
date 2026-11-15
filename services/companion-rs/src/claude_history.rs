//! Claude Code sessions on disk (ADR-015 §10, "Claude session history"):
//! every session that ran in an allow-listed workspace, started from the
//! phone, a terminal or the VS Code extension, is listed from Claude's own
//! transcripts (`<config dir>/projects/<encoded cwd>/<uuid>.jsonl`), and its
//! past turns are replayed into the transcript hub through the live
//! [`Driver`], so history and live frames map the same way.
//!
//! - [`Scanner::scan`] lists at most [`LIST_MAX`] sessions, newest first. Per
//!   file it reads a bounded head and tail ([`HEAD_BYTES`], [`TAIL_BYTES`]);
//!   unchanged files come from its cache. The directory name is a lossy
//!   encoding of the cwd, so it only preselects: the `cwd` recorded in the
//!   entries decides, canonicalized and inside a workspace (T11).
//! - [`replay`] reads the last [`REPLAY_BYTES`] of one file and returns at
//!   most [`REPLAY_OPS`] ops. Tool calls end `completed`, turns end with the
//!   next prompt, and no permission request is ever raised.
//! - [`in_use`]: a transcript written in the last [`IN_USE_MS`] belongs to a
//!   process elsewhere on the PC, so it is never resumed (T10); it can be
//!   forked (ADR-018).
//! - [`Tail`] (ADR-018 live mirror): a session in use is followed from
//!   where [`replay`] stopped, through the same driver, a bounded read per
//!   poll.
//!
//! Transcripts are the user's conversations: nothing here logs them, titles
//! are redacted, and the hub redacts and caps every op it is given.

use crate::claude_live::{Driver, Event, is_uuid, plain};
use crate::json::{self, Json};
use crate::protocol::LiveWorkspace;
use crate::runtime_io::redact;
use crate::transcript::{Item, ItemKind, Op, ToolStatus};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Sessions listed (the most recently written).
pub const LIST_MAX: usize = 50;
/// Files looked at per scan, newest first, before giving up on [`LIST_MAX`].
const CANDIDATES_MAX: usize = 500;
const HEAD_BYTES: u64 = 64 << 10;
const TAIL_BYTES: u64 = 256 << 10;
const TITLE_CHARS: usize = 80;
/// History read per replay: the end of the file.
pub const REPLAY_BYTES: u64 = 4 << 20;
/// Ops one replay hands the hub (items and turns, newest kept).
pub const REPLAY_OPS: usize = 300;
/// A transcript written this recently is in use by a process the companion
/// does not own (a terminal, VS Code): prompts are refused.
pub const IN_USE_MS: i64 = 2 * 60 * 1000;
pub const IN_USE: &str = "This session is active on the PC right now; continue it there, \
     or fork it to continue on the phone";

/// `CLAUDE_CONFIG_DIR`, else `%USERPROFILE%\.claude`, plus `projects`.
pub fn projects_dir(config_dir: Option<&OsStr>, profile: Option<&OsStr>) -> Option<PathBuf> {
    let base = match config_dir.filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => Path::new(profile?).join(".claude"),
    };
    Some(base.join("projects"))
}

/// One session found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub id: String,
    /// The canonical cwd, inside a workspace.
    pub cwd: String,
    pub title: String,
    pub created_at: i64,
    /// The file's mtime.
    pub updated_at: i64,
    pub path: PathBuf,
    /// The CLI's cumulative cost (the last `cost-state`): `--resume` restores
    /// it, so the driver starts from it.
    pub cost: f64,
    /// The mtime the hub's copy was replayed at (kept by the runtime).
    pub hydrated: Option<i64>,
    /// ADR-018: written by another process on the PC right now (kept by
    /// the runtime; the phone shows "Live on PC").
    pub live: bool,
}

#[derive(Debug, Clone)]
struct Meta {
    cwd: String,
    title: String,
    created_at: i64,
    cost: f64,
}

/// Lists sessions; keeps what it read per (path, mtime, size).
#[derive(Default)]
pub struct Scanner {
    cache: Vec<(PathBuf, i64, u64, Option<Meta>)>,
}

impl Scanner {
    pub fn scan(&mut self, projects: &Path, workspaces: &[LiveWorkspace]) -> Vec<Found> {
        let prefixes: Vec<String> = workspaces.iter().map(|w| encode(&w.path)).collect();
        let mut files = Vec::new();
        for dir in std::fs::read_dir(projects).into_iter().flatten().flatten() {
            let name = dir.file_name().to_string_lossy().to_ascii_lowercase();
            if !prefixes.iter().any(|p| name.starts_with(p.as_str())) {
                continue;
            }
            // `<uuid>.jsonl` only: subagent transcripts live in subfolders
            for f in std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = f.path();
                let id_ok = path.extension() == Some(OsStr::new("jsonl"))
                    && path
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .is_some_and(is_uuid);
                if let (true, Ok(md)) = (id_ok, f.metadata())
                    && md.is_file()
                    && let Some(mtime) = md.modified().ok().and_then(ms)
                {
                    files.push((mtime, md.len(), path));
                }
            }
        }
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        files.truncate(CANDIDATES_MAX);
        let old = std::mem::take(&mut self.cache);
        let mut found = Vec::new();
        for (mtime, len, path) in files {
            if found.len() >= LIST_MAX {
                break;
            }
            let meta = match old
                .iter()
                .find(|c| c.0 == path && c.1 == mtime && c.2 == len)
            {
                Some(c) => c.3.clone(),
                None => read_meta(&path, len, workspaces),
            };
            if let Some(m) = &meta {
                let id = path.file_stem().and_then(OsStr::to_str).unwrap_or("");
                found.push(Found {
                    id: id.to_string(),
                    cwd: m.cwd.clone(),
                    title: m.title.clone(),
                    created_at: if m.created_at > 0 {
                        m.created_at
                    } else {
                        mtime
                    },
                    updated_at: mtime,
                    path: path.clone(),
                    cost: m.cost,
                    hydrated: None,
                    live: false,
                });
            }
            self.cache.push((path, mtime, len, meta));
        }
        found
    }
}

/// Claude's project folder name for a path, lowercased: every UTF-16 unit
/// that is not ASCII alphanumeric becomes `-`.
pub(crate) fn encode(path: &str) -> String {
    path.encode_utf16()
        .map(|u| match char::from_u32(u32::from(u)) {
            Some(c) if c.is_ascii_alphanumeric() => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect()
}

fn ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

pub fn mtime_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path).ok()?.modified().ok().and_then(ms)
}

/// Written within [`IN_USE_MS`] (or unreadable): leave it to its owner.
pub fn in_use(path: &Path, now: i64) -> bool {
    mtime_ms(path).is_none_or(|m| now - m < IN_USE_MS)
}

/// The canonical form of `cwd` when it is an existing folder inside one of
/// `workspaces` (the same canonical paths `validate_workspace` produced).
pub fn inside(cwd: &str, workspaces: &[LiveWorkspace]) -> Option<String> {
    let canon = std::fs::canonicalize(cwd).ok().filter(|p| p.is_dir())?;
    let canon = plain(&canon);
    let p = PathBuf::from(&canon);
    workspaces
        .iter()
        .any(|w| p.starts_with(&w.path))
        .then_some(canon)
}

/// `bytes` from `start`, as whole lines: a cut first or last line is dropped.
fn lines_at(f: &mut File, start: u64, bytes: u64, len: u64) -> io::Result<Vec<String>> {
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(bytes).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if start + (buf.len() as u64) < len {
        lines.pop();
    }
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

fn read_meta(path: &Path, len: u64, workspaces: &[LiveWorkspace]) -> Option<Meta> {
    let mut f = File::open(path).ok()?;
    let mut lines = lines_at(&mut f, 0, HEAD_BYTES, len).ok()?;
    if len > HEAD_BYTES {
        let start = len.saturating_sub(TAIL_BYTES).max(HEAD_BYTES);
        lines.extend(lines_at(&mut f, start, TAIL_BYTES, len).ok()?);
    }
    let (mut cwd, mut created, mut first) = (None, None, None);
    let (mut custom, mut ai, mut last, mut cost) = (None, None, None, 0.0);
    for line in &lines {
        let Ok(v) = json::parse(line) else { continue };
        if json::get_bool(&v, "isSidechain") == Some(true) {
            continue;
        }
        let text = |k: &str| json::get_str(&v, k).map(str::to_string);
        match json::get_str(&v, "type").unwrap_or("") {
            "custom-title" => custom = text("customTitle").or(custom),
            "ai-title" => ai = text("aiTitle").or(ai),
            "last-prompt" => last = text("lastPrompt").or(last),
            "cost-state" => {
                cost = json::get(&v, "totalCostUSD")
                    .and_then(json::as_f64)
                    .unwrap_or(cost)
            }
            "user" if first.is_none() => first = user_text(&v),
            _ => {}
        }
        cwd = cwd.or_else(|| text("cwd"));
        created = created.or_else(|| json::get_str(&v, "timestamp").and_then(iso_ms));
    }
    let cwd = inside(&cwd?, workspaces)?;
    let title = [custom, ai, last, first]
        .into_iter()
        .flatten()
        .map(|t| title(&t))
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| "Claude Code session".into());
    Some(Meta {
        cwd,
        title,
        created_at: created.unwrap_or(0),
        cost,
    })
}

/// One line, redacted, then cut (a secret is never half-matched).
fn title(s: &str) -> String {
    let one = redact(&s.split_whitespace().collect::<Vec<_>>().join(" "));
    one.chars().take(TITLE_CHARS).collect()
}

/// What the user typed: not meta, not a compaction summary, not a slash
/// command's wrapper or output, not an interruption marker.
fn user_text(v: &Json) -> Option<String> {
    if json::get_bool(v, "isMeta") == Some(true)
        || json::get_bool(v, "isCompactSummary") == Some(true)
    {
        return None;
    }
    let text = match json::get(v, "message").and_then(|m| json::get(m, "content"))? {
        Json::Str(s) => s.clone(),
        Json::Arr(blocks) => blocks
            .iter()
            .filter(|b| json::get_str(b, "type") == Some("text"))
            .filter_map(|b| json::get_str(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let t = text.trim();
    (!t.is_empty() && !t.starts_with('<') && !t.starts_with("[Request interrupted"))
        .then(|| t.to_string())
}

/// `2026-11-12T10:20:30.123Z` (UTC, as Claude writes it) → epoch ms.
pub(crate) fn iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |from: usize, to: usize| s.get(from..to)?.parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let frac = if b[19] == b'.' {
        n(20, 23).unwrap_or(0)
    } else {
        0
    };
    // days from civil (H. Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + frac)
}

/// One transcript line into hub ops. `assistant` lines go through the live
/// driver as they are; `user` lines too, with `toolUseResult` under the
/// stream-json name. A user prompt closes the turn before it, and so does
/// an assistant message that ended the turn (`end_turn`, …).
fn feed(d: &mut Driver, line: &str, ts: &mut i64, ops: &mut Vec<Op>) {
    let Ok(mut v) = json::parse(line) else {
        return;
    };
    if json::get_bool(&v, "isSidechain") == Some(true) {
        return;
    }
    *ts = json::get_str(&v, "timestamp")
        .and_then(iso_ms)
        .unwrap_or(*ts);
    let ts = *ts;
    match json::get_str(&v, "type") {
        Some("assistant") => {
            keep(ops, d.on_line(line, ts), false);
            let stop = json::get(&v, "message").and_then(|m| json::get_str(m, "stop_reason"));
            if stop.is_some_and(|s| s != "tool_use") {
                close(d, ts, ops);
            }
        }
        Some("user") if json::get_bool(&v, "isMeta") != Some(true) => {
            let content = json::get(&v, "message").and_then(|m| json::get(m, "content"));
            let tools = content.and_then(json::as_arr).is_some_and(|b| {
                b.iter()
                    .any(|b| json::get_str(b, "type") == Some("tool_result"))
            });
            // ADR-017: only how many images the prompt had, never their bytes
            let images = content.and_then(json::as_arr).map_or(0, |b| {
                b.iter()
                    .filter(|b| json::get_str(b, "type") == Some("image"))
                    .count() as u32
            });
            let text = user_text(&v).or_else(|| (images > 0 && !tools).then(String::new));
            if let Some(text) = text {
                if !tools {
                    close(d, ts, ops);
                }
                ops.push(Op::Item(Item {
                    id: json::get_str(&v, "uuid").unwrap_or("user").to_string(),
                    kind: ItemKind::User,
                    ts,
                    text,
                    done: true,
                    images,
                    ..Default::default()
                }));
            }
            if let Json::Obj(fields) = &mut v
                && let Some(at) = fields.iter().position(|(k, _)| k == "toolUseResult")
            {
                fields[at].0 = "tool_use_result".into();
            }
            keep(ops, d.on_line(&json::stringify(&v), ts), false);
        }
        _ => {}
    }
}

/// Each item and turn once, where it first appeared, in its last state;
/// the newest `max` kept. Returns (ops, how many were cut).
fn dedupe(ops: Vec<Op>, max: usize) -> (Vec<Op>, usize) {
    let mut out: Vec<Op> = Vec::new();
    let mut at: HashMap<(bool, String), usize> = HashMap::new();
    for op in ops {
        let key = match &op {
            Op::Item(i) => (true, i.id.clone()),
            Op::Turn(t) => (false, t.id.clone()),
            _ => continue,
        };
        match at.get(&key) {
            Some(&i) => out[i] = op,
            None => {
                at.insert(key, out.len());
                out.push(op);
            }
        }
    }
    let cut = out.len().saturating_sub(max);
    (out.split_off(cut), cut)
}

/// The end of one session's transcript as hub ops (items and turns, each
/// once in its final state, oldest first), and the [`Tail`] that goes on
/// from there. `live`: another process is writing it, so its last turn
/// stays open for the tail; otherwise it is closed and its tools end
/// `completed`.
pub fn replay(
    path: &Path,
    adapter: &str,
    sid: &str,
    cwd: &str,
    live: bool,
) -> io::Result<(Vec<Op>, Tail)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(REPLAY_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    (&mut f).take(REPLAY_BYTES).read_to_end(&mut buf)?;
    // whole lines only: a last line still being written is the tail's
    let whole = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let read_to = start + whole as u64;
    let text = String::from_utf8_lossy(&buf[..whole]);
    let mut d = Driver::new(adapter, sid, cwd);
    let (mut ops, mut ts) = (Vec::new(), 0);
    // a cut first line is dropped
    for line in text.split('\n').skip(usize::from(start > 0)) {
        feed(&mut d, line, &mut ts, &mut ops);
    }
    if !live {
        close(&mut d, ts, &mut ops);
    }
    let (mut out, cut) = dedupe(ops, REPLAY_OPS);
    if start > 0 || cut > 0 {
        out.insert(
            0,
            Op::Item(Item {
                id: "history-truncated".into(),
                kind: ItemKind::Notice,
                text: "Earlier messages of this session are not shown".into(),
                level: Some("info"),
                done: true,
                ..Default::default()
            }),
        );
    }
    let tail = Tail {
        head: head(&mut f)?,
        path: path.to_path_buf(),
        offset: read_to,
        partial: Vec::new(),
        skipping: false,
        driver: d,
        ts,
    };
    Ok((out, tail))
}

/// The first bytes of a transcript: a different head is a different file.
fn head(f: &mut File) -> io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    f.take(HEAD_ID_BYTES).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Bytes one [`Tail::poll`] reads at most.
pub const TAIL_POLL_BYTES: u64 = 256 << 10;
/// Ops one [`Tail::poll`] hands the hub at most (the newest kept).
pub const TAIL_OPS: usize = 200;
/// A line longer than this is skipped: a tail never buffers more.
pub const LINE_MAX: usize = 4 << 20;
const HEAD_ID_BYTES: u64 = 256;

/// ADR-018 live mirror: follows a transcript another process is writing,
/// from where [`replay`] stopped, through the same driver. Whole lines
/// only (a line being written waits for its newline); a truncated or
/// replaced file is followed again from near its end.
pub struct Tail {
    path: PathBuf,
    head: Vec<u8>,
    offset: u64,
    partial: Vec<u8>,
    /// Dropping an over-long line up to its newline.
    skipping: bool,
    driver: Driver,
    ts: i64,
}

impl Tail {
    /// New ops since the last poll (at most [`TAIL_POLL_BYTES`] read and
    /// [`TAIL_OPS`] returned), and whether the file had new bytes.
    pub fn poll(&mut self) -> io::Result<(Vec<Op>, bool)> {
        let mut f = File::open(&self.path)?;
        let len = f.metadata()?.len();
        let mut ops = Vec::new();
        let now_head = head(&mut f)?;
        let same = now_head.len() >= self.head.len() && now_head.starts_with(&self.head);
        if len < self.offset || !same {
            // truncated or replaced: go on near its end, after a cut line
            self.offset = len.saturating_sub(TAIL_POLL_BYTES);
            self.skipping = self.offset > 0;
            self.partial.clear();
            ops.push(Op::Item(Item {
                id: format!("mirror-rewritten-{}", self.ts),
                kind: ItemKind::Notice,
                text: "The session file on the PC was rewritten; following it from here".into(),
                level: Some("info"),
                done: true,
                ..Default::default()
            }));
        }
        self.head = now_head;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.take(TAIL_POLL_BYTES).read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        let mut rest = &buf[..];
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            let line = &rest[..i];
            rest = &rest[i + 1..];
            if std::mem::take(&mut self.skipping) {
                self.partial.clear();
                continue;
            }
            self.partial.extend_from_slice(line);
            let text = String::from_utf8_lossy(&self.partial).into_owned();
            self.partial.clear();
            feed(&mut self.driver, &text, &mut self.ts, &mut ops);
        }
        if !self.skipping {
            self.partial.extend_from_slice(rest);
            if self.partial.len() > LINE_MAX {
                self.partial.clear();
                self.skipping = true;
            }
        }
        Ok((dedupe(ops, TAIL_OPS).0, !buf.is_empty()))
    }

    /// The writer went quiet: its open turn ends here.
    pub fn close(&mut self) -> Vec<Op> {
        let mut ops = Vec::new();
        close(&mut self.driver, self.ts, &mut ops);
        dedupe(ops, TAIL_OPS).0
    }
}

/// Items and turns only; history tools always end `completed`.
fn keep(ops: &mut Vec<Op>, events: Vec<Event>, done: bool) {
    for e in events {
        if let Event::Op(mut op) = e {
            if let Op::Item(i) = &mut op
                && let Some(t) = i.tool.as_mut()
                && done
            {
                t.status = ToolStatus::Completed;
                t.error = None;
            }
            if matches!(op, Op::Item(_) | Op::Turn(_)) {
                ops.push(op);
            }
        }
    }
}

/// Ends the open turn (history has no `result` frames).
fn close(d: &mut Driver, ts: i64, ops: &mut Vec<Op>) {
    if d.turn_running() {
        let end = d.on_line(
            r#"{"type":"result","subtype":"success","is_error":false}"#,
            ts,
        );
        keep(ops, end, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_live::{new_uuid, validate_workspace};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chb-claude-hist-{}", new_uuid().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Fake {
        root: PathBuf,
        projects: PathBuf,
        ws: LiveWorkspace,
        outside: PathBuf,
    }

    impl Fake {
        fn new() -> Fake {
            let root = scratch();
            let (work, outside) = (root.join("work"), root.join("work2"));
            std::fs::create_dir_all(work.join("sub")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            let ws = validate_workspace(work.to_str().unwrap(), &root.join("data"), None).unwrap();
            Fake {
                projects: root.join("claude").join("projects"),
                root,
                ws,
                outside,
            }
        }

        /// Writes `<projects>/<encoded cwd>/<id>.jsonl`, `age_ms` old.
        fn write(&self, cwd: &Path, id: &str, lines: &[String], age_ms: i64) -> PathBuf {
            let dir = self.projects.join(encode(&cwd.to_string_lossy()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            let when =
                std::time::SystemTime::now() - std::time::Duration::from_millis(age_ms as u64);
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
            path
        }

        fn scan(&self) -> Vec<Found> {
            Scanner::default().scan(&self.projects, std::slice::from_ref(&self.ws))
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn q(s: &str) -> String {
        json::stringify(&json::s(s))
    }

    fn user(cwd: &Path, uuid: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","isSidechain":false,"cwd":{},"sessionId":"x","timestamp":"2026-11-12T10:00:00.000Z","message":{{"role":"user","content":{}}}}}"#,
            q(&cwd.to_string_lossy()),
            q(text)
        )
    }

    fn assistant(uuid: &str, mid: &str, block: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","isSidechain":false,"timestamp":"2026-11-12T10:00:05.000Z","message":{{"id":"{mid}","role":"assistant","content":[{block}]}}}}"#
        )
    }

    fn tool_result(uuid: &str, tool: &str, out: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","isSidechain":false,"timestamp":"2026-11-12T10:00:09.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool}","content":{}}}]}},"toolUseResult":{{"stdout":{},"stderr":""}}}}"#,
            q(out),
            q(out)
        )
    }

    const ID1: &str = "11111111-1111-4111-8111-111111111111";
    const ID2: &str = "22222222-2222-4222-8222-222222222222";
    const ID3: &str = "33333333-3333-4333-8333-333333333333";

    #[test]
    fn discovery_filters_by_workspace_titles_and_skips_noise() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let ai = r#"{"type":"ai-title","aiTitle":"Fix the   build","sessionId":"x"}"#.to_string();
        let meta = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta"}}"#;
        // titled by ai-title; a subfolder cwd counts; malformed lines are skipped
        f.write(
            &work.join("sub"),
            ID1,
            &[
                "{oops".into(),
                meta.into(),
                user(&work.join("sub"), "u1", "hello"),
                ai,
            ],
            5_000,
        );
        // no ai-title: the first user text (a slash-command wrapper is not one), redacted
        let cmd = user(&work, "u0", "<command-name>/clear</command-name>");
        let key = format!("deploy with sk-ant-api03-{} please", "a".repeat(40));
        f.write(&work, ID2, &[cmd, user(&work, "u1", &key)], 10_000);
        // the same folder name prefix, but outside the workspace
        f.write(
            &f.outside,
            ID3,
            &[user(&f.outside, "u1", "elsewhere")],
            1_000,
        );
        // a sidechain-only file has no cwd of its own; a subagent folder is ignored
        let side = r#"{"type":"user","isSidechain":true,"cwd":"x","message":{"role":"user","content":"side"}}"#;
        f.write(
            &work,
            "44444444-4444-4444-8444-444444444444",
            &[side.into()],
            1_000,
        );
        let sub = f
            .projects
            .join(encode(&f.ws.path))
            .join(ID1)
            .join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-1.jsonl"), user(&work, "u", "agent") + "\n").unwrap();

        let found = f.scan();
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, [ID1, ID2], "newest first, workspace only");
        assert_eq!(found[0].title, "Fix the build");
        assert!(found[0].cwd.ends_with("sub"), "{}", found[0].cwd);
        assert!(
            found[1].title.starts_with("deploy with «redacted"),
            "{}",
            found[1].title
        );
        assert!(!found[1].title.contains("aaaa"));
        assert_eq!(
            found[1].created_at,
            iso_ms("2026-11-12T10:00:00.000Z").unwrap()
        );
        assert!(found[0].updated_at > found[1].updated_at);
    }

    #[test]
    fn discovery_caps_the_list_and_reads_bounded_tails() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        for i in 0..LIST_MAX + 5 {
            let id = format!("{i:08x}-0000-4000-8000-000000000000");
            f.write(
                &work,
                &id,
                &[user(&work, "u", &format!("task {i}"))],
                i as i64 * 1000,
            );
        }
        // a long file: the latest title sits in the tail, the head has the cwd
        let filler = format!(
            r#"{{"type":"attachment","pad":"{}"}}"#,
            "x".repeat(200 << 10)
        );
        let mut lines = vec![user(&work, "u", "first prompt")];
        lines.extend((0..3).map(|_| filler.clone()));
        lines.push(r#"{"type":"ai-title","aiTitle":"late title"}"#.into());
        f.write(&work, ID1, &lines, 0);
        let mut scanner = Scanner::default();
        let found = scanner.scan(&f.projects, std::slice::from_ref(&f.ws));
        assert_eq!(found.len(), LIST_MAX);
        assert_eq!(found[0].title, "late title");
        assert_eq!(found[1].title, "task 0");
        // a second scan reuses the cache and agrees
        assert_eq!(
            scanner.scan(&f.projects, std::slice::from_ref(&f.ws)),
            found
        );
    }

    #[test]
    fn in_use_is_a_recent_write() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let now = crate::protocol::now_ms();
        let busy = f.write(&work, ID1, &[user(&work, "u", "a")], 10_000);
        let old = f.write(&work, ID2, &[user(&work, "u", "a")], IN_USE_MS + 10_000);
        assert!(in_use(&busy, now));
        assert!(!in_use(&old, now));
        assert!(in_use(&f.root.join("missing.jsonl"), now));
    }

    #[test]
    fn replay_maps_turns_tools_and_prompts() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let tool = r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}"#;
        let pending =
            r#"{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"sleep 9"}}"#;
        let lines = vec![
            user(&work, "u1", "list the files"),
            assistant("a1", "msg_1", r#"{"type":"text","text":"Sure."}"#),
            assistant("a2", "msg_1", tool),
            tool_result("u2", "toolu_1", "a.txt"),
            assistant("a3", "msg_2", r#"{"type":"text","text":"Done."}"#),
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta"}}"#.into(),
            user(&work, "u3", "now wait"),
            assistant("a4", "msg_3", pending),
            "{broken".into(),
        ];
        let path = f.write(&work, ID1, &lines, 0);
        let ops = replay(&path, "claude-1", ID1, &f.ws.path, false).unwrap().0;
        let items: Vec<&Item> = ops
            .iter()
            .filter_map(|o| match o {
                Op::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        let shape: Vec<(ItemKind, &str)> = items.iter().map(|i| (i.kind, i.id.as_str())).collect();
        assert_eq!(
            shape,
            [
                (ItemKind::User, "u1"),
                (ItemKind::Text, "msg_1#0"),
                (ItemKind::Tool, "toolu_1"),
                (ItemKind::Text, "msg_2#0"),
                (ItemKind::User, "u3"),
                (ItemKind::Tool, "toolu_2"),
            ]
        );
        let t1 = items[2].tool.as_ref().unwrap();
        assert_eq!(
            (t1.status, t1.output.as_str()),
            (ToolStatus::Completed, "a.txt")
        );
        assert_eq!(
            items[5].tool.as_ref().unwrap().status,
            ToolStatus::Completed
        );
        let turns: Vec<_> = ops
            .iter()
            .filter_map(|o| match o {
                Op::Turn(t) => Some((t.id.as_str(), t.status, t.ended_at.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            [("a1", "completed", true), ("a4", "completed", true)]
        );
        assert!(!ops.iter().any(|o| matches!(o, Op::State(_))));
    }

    /// ADR-017: a prompt's images come back as a count, never as bytes.
    #[test]
    fn replay_counts_prompt_images_without_their_bytes() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEUlEQVR4nGP4z8DwnwGMgRQAH+4D/dJQfRoAAAAASUVORK5CYII=";
        let block = format!(
            r#"{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{data}"}}}}"#
        );
        let prompt = |uuid: &str, text: &str| {
            let mut content = format!("[{block},{block}");
            if !text.is_empty() {
                content.push_str(&format!(r#",{{"type":"text","text":{}}}"#, q(text)));
            }
            format!(
                r#"{{"type":"user","uuid":"{uuid}","isSidechain":false,"cwd":{},"timestamp":"2026-11-12T10:00:00.000Z","message":{{"role":"user","content":{content}]}}}}"#,
                q(&work.to_string_lossy())
            )
        };
        let lines = vec![prompt("u1", "what color"), prompt("u2", "")];
        let path = f.write(&work, ID1, &lines, 0);
        let ops = replay(&path, "claude-1", ID1, &f.ws.path, false).unwrap().0;
        let users: Vec<(&str, &str, u32)> = ops
            .iter()
            .filter_map(|o| match o {
                Op::Item(i) if i.kind == ItemKind::User => {
                    Some((i.id.as_str(), i.text.as_str(), i.images))
                }
                _ => None,
            })
            .collect();
        assert_eq!(users, [("u1", "what color", 2), ("u2", "", 2)]);
        let text: String = ops
            .iter()
            .filter_map(|o| match o {
                Op::Item(i) => Some(json::stringify(&i.to_json())),
                _ => None,
            })
            .collect();
        assert!(!text.contains(&data[..20]));
    }

    #[test]
    fn replay_keeps_the_newest_ops_and_says_so() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let lines: Vec<String> = (0..REPLAY_OPS + 10)
            .map(|i| user(&work, &format!("u{i}"), &format!("prompt {i}")))
            .collect();
        let path = f.write(&work, ID1, &lines, 0);
        let ops = replay(&path, "claude-1", ID1, &f.ws.path, false).unwrap().0;
        assert_eq!(ops.len(), REPLAY_OPS + 1);
        let Op::Item(first) = &ops[0] else { panic!() };
        assert_eq!(
            (first.id.as_str(), first.kind),
            ("history-truncated", ItemKind::Notice)
        );
        let Op::Item(last) = ops.last().unwrap() else {
            panic!()
        };
        assert_eq!(last.text, format!("prompt {}", REPLAY_OPS + 9));
    }

    fn append(path: &Path, bytes: &str) {
        use std::io::Write;
        File::options()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes.as_bytes())
            .unwrap();
    }

    fn ids(ops: &[Op]) -> Vec<String> {
        ops.iter()
            .filter_map(|o| match o {
                Op::Item(i) => Some(i.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// ADR-018: the mirror goes on where replay stopped, keeps the last turn
    /// open while the PC writes, waits for a line's newline, and closes the
    /// turn on `end_turn`.
    #[test]
    fn tail_follows_new_lines_and_waits_for_whole_ones() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let tool = r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}"#;
        let path = f.write(
            &work,
            ID1,
            &[user(&work, "u1", "list"), assistant("a1", "msg_1", tool)],
            0,
        );
        // replay of a live session leaves the turn and the tool open
        let (ops, mut tail) = replay(&path, "claude-1", ID1, &f.ws.path, true).unwrap();
        assert_eq!(ids(&ops), ["u1", "toolu_1"]);
        assert!(
            !ops.iter()
                .any(|o| matches!(o, Op::Turn(t) if t.ended_at.is_some()))
        );
        assert!(tail.poll().unwrap().0.is_empty());

        // half a line: nothing yet; the rest: the tool result
        let result = tool_result("u2", "toolu_1", "a.txt") + "\n";
        let (a, b) = result.split_at(40);
        append(&path, a);
        let (ops, grew) = tail.poll().unwrap();
        assert!(grew && ops.is_empty());
        append(&path, b);
        let (ops, _) = tail.poll().unwrap();
        let Some(Op::Item(t)) = ops
            .iter()
            .find(|o| matches!(o, Op::Item(i) if i.id == "toolu_1"))
        else {
            panic!("{ops:?}")
        };
        assert_eq!(t.tool.as_ref().unwrap().output, "a.txt");

        // an end_turn message ends the turn
        let done = r#"{"type":"assistant","uuid":"a2","isSidechain":false,"timestamp":"2026-11-12T10:00:10.000Z","message":{"id":"msg_2","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;
        append(&path, &format!("{done}\n"));
        let (ops, _) = tail.poll().unwrap();
        assert!(ids(&ops).contains(&"msg_2#0".to_string()), "{ops:?}");
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::Turn(t) if t.ended_at.is_some()))
        );
        assert_eq!(tail.poll().unwrap(), (vec![], false));
        assert!(tail.close().is_empty(), "nothing left open");
    }

    #[test]
    fn tail_survives_truncation_and_replacement() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let path = f.write(&work, ID1, &[user(&work, "u1", "first")], 0);
        let (_, mut tail) = replay(&path, "claude-1", ID1, &f.ws.path, true).unwrap();
        // truncated: a notice, then the new lines
        std::fs::write(&path, user(&work, "u9", "again") + "\n").unwrap();
        let (ops, _) = tail.poll().unwrap();
        let got = ids(&ops);
        assert!(got[0].starts_with("mirror-rewritten"), "{got:?}");
        assert_eq!(&got[1..], ["u9"]);
        // replaced by a longer file with another head: followed again
        let lines: Vec<String> = (0..3)
            .map(|i| user(&work, &format!("r{i}"), &format!("{i} {}", "x".repeat(300))))
            .collect();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let got = ids(&tail.poll().unwrap().0);
        assert!(got[0].starts_with("mirror-rewritten"), "{got:?}");
        assert_eq!(&got[1..], ["r0", "r1", "r2"]);
    }

    /// Bounded: one poll reads at most TAIL_POLL_BYTES and returns at most
    /// TAIL_OPS; an over-long line is skipped whole, never buffered.
    #[test]
    fn tail_is_bounded_per_poll_and_skips_huge_lines() {
        let f = Fake::new();
        let work = PathBuf::from(&f.ws.path);
        let path = f.write(&work, ID1, &[user(&work, "u0", "start")], 0);
        let (_, mut tail) = replay(&path, "claude-1", ID1, &f.ws.path, true).unwrap();
        let huge = format!(
            r#"{{"type":"attachment","pad":"{}"}}"#,
            "x".repeat(LINE_MAX + 10)
        );
        let many: String = (0..TAIL_OPS + 50)
            .map(|i| user(&work, &format!("m{i}"), "p") + "\n")
            .collect();
        append(&path, &format!("{huge}\n{many}"));
        let mut seen = Vec::new();
        let mut polls = 0;
        loop {
            let (ops, grew) = tail.poll().unwrap();
            assert!(ops.len() <= TAIL_OPS);
            seen.extend(ids(&ops));
            if !grew {
                break;
            }
            polls += 1;
        }
        let file = std::fs::metadata(&path).unwrap().len();
        assert!(
            polls as u64 >= file / TAIL_POLL_BYTES,
            "{polls} polls for {file} bytes"
        );
        assert_eq!(seen.last().map(String::as_str), Some("m249"));
        assert!(seen.iter().all(|i| i.starts_with('m')));
    }

    #[test]
    fn iso_timestamps_parse_as_utc() {
        assert_eq!(iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_ms("2000-03-01T00:00:01.500Z"), Some(951_868_801_500));
        assert_eq!(iso_ms("not a date"), None);
    }

    #[test]
    fn projects_dir_prefers_the_config_dir() {
        let p = projects_dir(
            Some(OsStr::new("D:\\cfg")),
            Some(OsStr::new("C:\\Users\\u")),
        );
        assert_eq!(p, Some(PathBuf::from("D:\\cfg\\projects")));
        let p = projects_dir(None, Some(OsStr::new("C:\\Users\\u")));
        assert_eq!(p, Some(PathBuf::from("C:\\Users\\u\\.claude\\projects")));
        assert_eq!(projects_dir(None, None), None);
    }
}
