//! Shared runtime plumbing for ADR-015 live sessions (§4.1, §4.2.4, §4.7):
//! `ChildIo` drives a spawned runtime's stdio (Claude Code, Codex), `child_env`
//! scrubs what it inherits, `redact` masks secret shapes before anything
//! reaches the phone, `open_url_on_desktop` opens a runtime-produced sign-in
//! URL without a shell, and `check_callback_url` + `PendingAuth` guard the
//! MCP OAuth phone-callback relay.
//!
//! Placement deviates from the ADR on purpose: §4.1 puts `ChildIo` and the
//! opener in `adapters.rs` and `redact` in `transcript.rs`. They live in this
//! one new module so the parallel ADR-015 work on `protocol.rs`,
//! `companion.rs`, `server.rs`, `engine.rs` and `adapters.rs` merges without
//! conflicts; callers can re-export them from there later.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/* ------------------------------------------------------------------ */
/* Redaction (§4.1, T2)                                                */
/* ------------------------------------------------------------------ */

/// Every redacted span reads `«redacted:<kind>»`.
const REDACTED: &str = "«redacted:";
/// An unterminated PEM block is masked this far; a 16384-bit RSA key is
/// ~13 KB, so a real block always ends inside the window.
const PEM_WINDOW: usize = 16 << 10;
/// Longer "keys" are data, not names; also bounds the keyword search.
const MAX_KEY_LEN: usize = 128;

/// Masks known secret shapes (ADR-015 §4.1): Anthropic/OpenAI/GitHub/Slack/
/// AWS/Google keys, JWTs, PEM private keys, `Authorization: Bearer <tok>`,
/// and `KEY=value` / `"key": "value"` where the key names a secret
/// (SECRET, TOKEN, PASSWORD, PASSWD, API_KEY, PRIVATE_KEY, CREDENTIAL; any
/// case, `-` = `_`).
///
/// A hand-written scanner, not a regex: it walks words (`[A-Za-z0-9_-]+`)
/// once, so it is linear on hostile input. Spans start at an ASCII byte and
/// end at an ASCII byte or a char boundary, so a multi-byte character is
/// never split. The marker can never
/// match again (`«` is not a word byte, kinds are not key-shaped, a value
/// already starting with the marker is left alone), so it is idempotent.
///
/// Values are found in two forms only: a bare `KEY=value` (no spaces, like
/// env, CLI flags and query strings) or a quoted value after `=`/`:`
/// (`"key": "v"`, `key = 'v'`, escaped `{\"key\":\"v\"}`). An unquoted value
/// after spaces (`password = read()`) is code, not a secret.
pub fn redact(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut copied) = (0, 0);
    // every word byte visited here starts a word (or follows a marker): whole
    // words are skipped at once, which is what keeps the scan linear
    while i < b.len() {
        if !is_word(b[i]) {
            i += 1;
            continue;
        }
        let j = word_end(b, i);
        match secret_at(s, i, j) {
            Some((start, end, kind)) => {
                out.push_str(&s[copied..start]);
                out.push_str(REDACTED);
                out.push_str(kind);
                out.push('»');
                (i, copied) = (end, end);
            }
            None => i = j,
        }
    }
    out.push_str(&s[copied..]);
    out
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn word_end(b: &[u8], mut k: usize) -> usize {
    while k < b.len() && is_word(b[k]) {
        k += 1;
    }
    k
}

fn skip_blanks(b: &[u8], mut k: usize) -> usize {
    while matches!(b.get(k), Some(b' ' | b'\t')) {
        k += 1;
    }
    k
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The span to mask for the word `b[i..j]`, with its kind.
fn secret_at(s: &str, i: usize, j: usize) -> Option<(usize, usize, &'static str)> {
    let b = s.as_bytes();
    let w = &b[i..j];
    if w == b"-----BEGIN" {
        return pem_end(s, j).map(|end| (i, end, "private-key"));
    }
    if let Some(kind) = token_kind(w) {
        return Some((i, j, kind));
    }
    if w.starts_with(b"eyJ")
        && let Some(end) = jwt_end(b, i, j)
    {
        return Some((i, end, "jwt"));
    }
    if w.len() >= 13 && w[w.len() - 13..].eq_ignore_ascii_case(b"authorization") {
        return bearer_token(b, j).map(|(start, end)| (start, end, "bearer"));
    }
    // separator first: it rejects almost every word in O(1)
    let (start, quote, bare) = after_key(b, j)?;
    let strong = secret_key(w)?;
    let end = match quote {
        Some(q) => quoted_end(b, start, q),
        None if bare => {
            start
                + b[start..]
                    .iter()
                    .position(|&c| ends_bare_value(c))
                    .unwrap_or(b.len() - start)
        }
        None => return None,
    };
    let v = &b[start..end];
    // TOKEN is overloaded (max_tokens=4096, TOKENIZERS_PARALLELISM=false):
    // under a TOKEN-only key, numbers and booleans are not secrets
    let harmless = !strong
        && (v.iter().all(u8::is_ascii_digit)
            || [&b"true"[..], b"false", b"null"]
                .iter()
                .any(|x| v.eq_ignore_ascii_case(x)));
    (!v.is_empty() && !v.starts_with(REDACTED.as_bytes()) && !harmless)
        .then_some((start, end, "secret"))
}

/// Whole-word token shapes. 20+ chars after a prefix keeps prose out; a bare
/// `sk-` must also carry a digit (`sk-this-is-a-long-hyphenated-name`).
fn token_kind(w: &[u8]) -> Option<&'static str> {
    const PREFIXED: [(&[u8], &str); 14] = [
        (b"sk-ant-", "anthropic-key"),
        (b"sk-proj-", "openai-key"),
        (b"github_pat_", "github-token"),
        (b"ghp_", "github-token"),
        (b"gho_", "github-token"),
        (b"ghs_", "github-token"),
        (b"ghu_", "github-token"),
        (b"ghr_", "github-token"),
        (b"glpat-", "gitlab-token"),
        (b"npm_", "npm-token"),
        (b"sk_live_", "stripe-key"),
        (b"rk_live_", "stripe-key"),
        (b"sk_test_", "stripe-key"),
        (b"sk-", "openai-key"),
    ];
    // every shape starts with one of these bytes; most words stop here
    if !matches!(w[0], b's' | b'g' | b'x' | b'A' | b'n' | b'r') {
        return None;
    }
    for (prefix, kind) in PREFIXED {
        if let Some(rest) = w.strip_prefix(prefix) {
            let ok = rest.len() >= 20 && (prefix != b"sk-" || rest.iter().any(u8::is_ascii_digit));
            return ok.then_some(kind);
        }
    }
    if w.len() >= 15 && w.starts_with(b"xox") && b"abprs".contains(&w[3]) && w[4] == b'-' {
        return Some("slack-token");
    }
    if let Some(rest) = w.strip_prefix(b"AKIA") {
        let ok = rest.len() == 16
            && rest
                .iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        return ok.then_some("aws-key");
    }
    (w.len() == 39 && w.starts_with(b"AIza")).then_some("google-key")
}

/// `header.payload.signature`; both JSON parts are base64url of `{"`, so
/// they start with `eyJ`.
fn jwt_end(b: &[u8], i: usize, j: usize) -> Option<usize> {
    if j - i < 10 || b.get(j) != Some(&b'.') {
        return None;
    }
    let p = j + 1;
    let q = word_end(b, p);
    if q - p < 10 || !b[p..q].starts_with(b"eyJ") || b.get(q) != Some(&b'.') {
        return None;
    }
    let r = word_end(b, q + 1);
    (r > q + 1).then_some(r)
}

/// `-----BEGIN [A-Z ]*PRIVATE KEY-----` through its `-----END …-----` line.
/// Searching only inside the window and masking all of it when no END shows
/// up keeps this linear; JSON-escaped blocks (`\n` as two bytes) work too.
fn pem_end(s: &str, j: usize) -> Option<usize> {
    let b = s.as_bytes();
    let head = &b[j..b.len().min(j + 64)];
    let at = find(head, b"PRIVATE KEY-----")?;
    if !head[..at]
        .iter()
        .all(|&c| c == b' ' || c.is_ascii_uppercase())
    {
        return None;
    }
    let body = j + at + 16;
    let limit = b.len().min(body + PEM_WINDOW);
    let mut end = match find(&b[body..limit], b"-----END") {
        Some(k) => {
            let e = body + k + 8;
            e + find(&b[e..b.len().min(e + 64)], b"-----").map_or(0, |t| t + 5)
        }
        None => limit,
    };
    while !s.is_char_boundary(end) {
        end += 1;
    }
    Some(end)
}

/// The token after `Authorization: Bearer ` (also `"authorization": "bearer …`).
fn bearer_token(b: &[u8], j: usize) -> Option<(usize, usize)> {
    let (k, _, _) = after_key(b, j)?;
    let word = b.get(k..k + 7)?;
    if !word[..6].eq_ignore_ascii_case(b"bearer") || word[6] != b' ' {
        return None;
    }
    let start = skip_blanks(b, k + 7);
    let token = |c: &u8| c.is_ascii_alphanumeric() || b"-._~+/=".contains(c);
    let end = start + b[start..].iter().take_while(|c| token(c)).count();
    (end > start).then_some((start, end))
}

/// What follows a key: optional closing quote, `=` / `:` / `:=` (never `==`,
/// `=>`, `::`), optional opening quote. Returns where the value starts, its
/// quote, and whether it was a bare `=` with no spaces around it.
fn after_key(b: &[u8], j: usize) -> Option<(usize, Option<&'static [u8]>, bool)> {
    let mut k = j + quote_at(b, j).map_or(0, <[u8]>::len);
    let before = k;
    k = skip_blanks(b, k);
    let mut bare = k == before;
    match b.get(k) {
        Some(b'=') if !matches!(b.get(k + 1), Some(b'=' | b'>')) => k += 1,
        Some(b':') if b.get(k + 1) == Some(&b'=') => (k, bare) = (k + 2, false),
        Some(b':') if b.get(k + 1) != Some(&b':') => (k, bare) = (k + 1, false),
        _ => return None,
    }
    let after = k;
    k = skip_blanks(b, k);
    bare &= k == after;
    let quote = quote_at(b, k);
    Some((k + quote.map_or(0, <[u8]>::len), quote, bare))
}

fn quote_at(b: &[u8], k: usize) -> Option<&'static [u8]> {
    match b.get(k..)? {
        [b'\\', b'"', ..] => Some(b"\\\""),
        [b'"', ..] => Some(b"\""),
        [b'\'', ..] => Some(b"'"),
        _ => None,
    }
}

/// End of a quoted value: its closing quote, a line end, or the input end.
/// A backslash escapes the next byte except inside `\"…\"`, where it starts
/// the closing quote (stopping late only masks more).
fn quoted_end(b: &[u8], start: usize, q: &[u8]) -> usize {
    let mut k = start;
    while k < b.len() {
        match b[k] {
            b'\n' | b'\r' => break,
            b'\\' if q != b"\\\"" => k += 1, // skip the escaped byte
            _ if b[k..].starts_with(q) => break,
            b'"' if q == b"\\\"" => break, // the enclosing JSON string ends
            _ => {}
        }
        k += 1;
    }
    k.min(b.len())
}

/// A bare value runs until whitespace, a control byte, a quote, a separator
/// (`&` in queries, `;` in cookies) or a bracket; `\` ends it so the next
/// line of JSON-escaped text (`\n`) is not swallowed.
fn ends_bare_value(c: u8) -> bool {
    c.is_ascii_whitespace() || c.is_ascii_control() || b"\"'`,;&|<>()[]{}\\".contains(&c)
}

/// `Some(strong)` when the key names a secret; `strong` is false when only
/// TOKEN matched.
fn secret_key(w: &[u8]) -> Option<bool> {
    // TOKEN is the shortest keyword
    if w.len() < 5 || w.len() > MAX_KEY_LEN {
        return None;
    }
    let key: Vec<u8> = w
        .iter()
        .map(|&c| {
            if c == b'-' {
                b'_'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect();
    let has = |k: &str| find(&key, k.as_bytes()).is_some();
    let strong = [
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
    ]
    .into_iter()
    .any(has);
    (strong || has("TOKEN")).then_some(strong)
}

/* ------------------------------------------------------------------ */
/* Child process IO (§4.1 "Child process helper", §4.2.4, T6, T7)      */
/* ------------------------------------------------------------------ */

/// Runtime NDJSON / JSON-RPC line cap (§3.5).
pub const MAX_LINE_BYTES: usize = 16 << 20;
/// stderr kept for diagnostics.
pub const STDERR_TAIL_BYTES: usize = 4096;
/// Frames queued for the child's stdin before `try_send` refuses.
pub const INPUT_QUEUE: usize = 64;
/// Grace between closing stdin and killing (§4.2.5 `session_end`).
pub const KILL_GRACE: Duration = Duration::from_secs(5);
// ponytail: exit is polled with try_wait so `kill_after` can share the Child;
// a WaitForSingleObject thread would remove the wakeups if they ever matter
const EXIT_POLL: Duration = Duration::from_millis(50);

/// What the child said or did, in order, on the owner's receiver.
#[derive(Debug, PartialEq)]
pub enum ChildEvent {
    /// One stdout line without its line ending; blank lines are dropped.
    Line(String),
    /// A line over [`MAX_LINE_BYTES`] was skipped to its newline; the running
    /// count (the driver reports it as a `notice`, §4.2.4).
    Skipped(u64),
    /// The child exited; `None` when there is no exit code (a pipe peer).
    /// Comes after every line written before the exit, unless a grandchild
    /// keeps stdout open for more than two seconds.
    Exit(Option<i32>),
}

/// A runtime child's stdio: a stdout line reader, a stderr tail and a stdin
/// writer, each on its own thread, so neither a chatty nor a stuck child can
/// block the companion. Dropping it closes stdin and kills a child that is
/// still there after [`KILL_GRACE`].
pub struct ChildIo {
    input: Mutex<Option<SyncSender<String>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    child: Option<Arc<Mutex<Child>>>,
}

impl ChildIo {
    /// Spawns `program` directly (never through a shell: `.cmd`/`.bat` are
    /// refused because Windows would run them through cmd.exe) with exactly
    /// `env` (see [`child_env`]), no console window, and piped stdio. Prompts
    /// go through [`ChildIo::try_send`], never argv.
    pub fn spawn<S: AsRef<OsStr>>(
        program: &Path,
        args: &[S],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> io::Result<(ChildIo, Receiver<ChildEvent>)> {
        let lower = program.to_string_lossy().to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing a .cmd/.bat runtime: Windows would run it through cmd.exe",
            ));
        }
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = no_window(&mut cmd).spawn()?;
        let piped = "stdio was piped above";
        let stdin = child.stdin.take().expect(piped);
        let stdout = child.stdout.take().expect(piped);
        let stderr = child.stderr.take().expect(piped);
        let child = Arc::new(Mutex::new(child));
        let tail = Arc::new(Mutex::new(Vec::new()));
        let (events, rx) = mpsc::channel();
        let (input, queue) = mpsc::sync_channel(INPUT_QUEUE);
        // the reader drops `eof` when stdout ends; the waiter reports the
        // exit only after that, so lines always precede Exit
        let (eof, eof_seen) = mpsc::channel::<()>();
        let (out_events, out_tail, waited) =
            (events.clone(), Arc::clone(&tail), Arc::clone(&child));
        let started = start("child-stdin", move || write_lines(stdin, queue))
            .and_then(|()| {
                start("child-stdout", move || {
                    read_lines(stdout, MAX_LINE_BYTES, &out_events);
                    drop(eof);
                })
            })
            .and_then(|()| start("child-stderr", move || drain_tail(stderr, &out_tail)))
            .and_then(|()| start("child-wait", move || wait_exit(&waited, &eof_seen, &events)));
        if let Err(e) = started {
            let _ = child.lock().unwrap().kill();
            return Err(e);
        }
        let io = ChildIo {
            input: Mutex::new(Some(input)),
            stderr: tail,
            child: Some(child),
        };
        Ok((io, rx))
    }

    /// The same protocol core over any byte pipes, so driver tests can script
    /// a fake peer (`std::io::pipe()` ends). Exit is reported as `None` when
    /// `stdout` reaches EOF.
    pub fn from_pipes<R, W>(stdout: R, stdin: W) -> io::Result<(ChildIo, Receiver<ChildEvent>)>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (events, rx) = mpsc::channel();
        let (input, queue) = mpsc::sync_channel(INPUT_QUEUE);
        start("child-stdin", move || write_lines(stdin, queue))?;
        start("child-stdout", move || {
            read_lines(stdout, MAX_LINE_BYTES, &events);
            let _ = events.send(ChildEvent::Exit(None));
        })?;
        let io = ChildIo {
            input: Mutex::new(Some(input)),
            stderr: Arc::default(),
            child: None,
        };
        Ok((io, rx))
    }

    /// Queues one frame (no line ending) for stdin. Never blocks: a full queue
    /// means the child stopped reading, and the command is refused instead of
    /// stalling the WS read loop (§4.1).
    pub fn try_send(&self, line: String) -> Result<(), &'static str> {
        if line.contains(['\n', '\r']) {
            return Err("a frame must be a single line");
        }
        let input = self.input.lock().unwrap();
        let tx = input.as_ref().ok_or("runtime input is closed")?;
        tx.try_send(line).map_err(|e| match e {
            TrySendError::Full(_) => "runtime is not reading input",
            TrySendError::Disconnected(_) => "runtime input is closed",
        })
    }

    /// Graceful exit: frames already queued are still written, then stdin
    /// closes and the runtime sees EOF.
    pub fn close_stdin(&self) {
        drop(self.input.lock().unwrap().take());
    }

    /// Kills the child if it is still running after `grace`. Returns at once;
    /// a no-op for pipe peers.
    pub fn kill_after(&self, grace: Duration) {
        let Some(child) = &self.child else { return };
        let deadline = Instant::now() + grace;
        let killer = Arc::clone(child);
        let started = start("child-kill", move || {
            while Instant::now() < deadline {
                if !matches!(killer.lock().unwrap().try_wait(), Ok(None)) {
                    return;
                }
                thread::sleep(EXIT_POLL);
            }
            let _ = killer.lock().unwrap().kill();
        });
        if started.is_err() {
            let _ = child.lock().unwrap().kill(); // fail closed: never leave it running
        }
    }

    /// The last [`STDERR_TAIL_BYTES`] of stderr, redacted: it ends up in
    /// notices and logs.
    pub fn stderr_tail(&self) -> String {
        redact(&String::from_utf8_lossy(&self.stderr.lock().unwrap()))
    }
}

impl Drop for ChildIo {
    fn drop(&mut self) {
        self.close_stdin();
        self.kill_after(KILL_GRACE);
    }
}

/// The child's environment (T6): the companion's minus `HOST_TOKEN`, every
/// `CHERNOBYL_*` variable and `NODE_OPTIONS` (names compared without case, as
/// Windows does), plus `extras`, which replace inherited values of the same
/// name (`CLAUDE_CODE_ENTRYPOINT=sdk-ts` over a VS Code `claude-vscode`).
/// `ChildIo::spawn` clears the environment first, so nothing else slips in.
pub fn child_env(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    extras: &[(&str, &str)],
) -> Vec<(OsString, OsString)> {
    let scrubbed = |name: &OsStr| {
        let n = name.to_string_lossy().to_ascii_uppercase();
        n == "HOST_TOKEN"
            || n == "NODE_OPTIONS"
            || n.starts_with("CHERNOBYL_")
            || extras.iter().any(|(k, _)| k.eq_ignore_ascii_case(&n))
    };
    let mut env: Vec<(OsString, OsString)> =
        base.into_iter().filter(|(k, _)| !scrubbed(k)).collect();
    env.extend(extras.iter().map(|&(k, v)| (k.into(), v.into())));
    env
}

fn start(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new().name(name.into()).spawn(f).map(drop)
}

/// `CREATE_NO_WINDOW`: a console runtime must not flash a window.
pub(crate) fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(cmd, 0x0800_0000);
    cmd
}

/// Splits stdout into lines of at most `cap` bytes without ever buffering
/// more: an over-long line is dropped as it streams and counted at its
/// newline. Keeps draining after the receiver is gone so the child never
/// blocks on a full pipe.
fn read_lines(r: impl Read, cap: usize, events: &Sender<ChildEvent>) {
    let mut r = BufReader::with_capacity(64 << 10, r);
    let (mut line, mut skipping, mut skipped) = (Vec::new(), false, 0u64);
    loop {
        let buf = match r.fill_buf() {
            Ok([]) => break,
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let newline = buf.iter().position(|&c| c == b'\n');
        let chunk = &buf[..newline.unwrap_or(buf.len())];
        if !skipping && line.len() + chunk.len() > cap {
            (skipping, line) = (true, Vec::new()); // free it, don't keep 16 MiB
        }
        if !skipping {
            line.extend_from_slice(chunk);
        }
        let used = chunk.len() + usize::from(newline.is_some());
        r.consume(used);
        if newline.is_some() {
            finish_line(&mut line, &mut skipping, &mut skipped, events);
        }
    }
    finish_line(&mut line, &mut skipping, &mut skipped, events); // unterminated last line
}

fn finish_line(
    line: &mut Vec<u8>,
    skipping: &mut bool,
    skipped: &mut u64,
    events: &Sender<ChildEvent>,
) {
    if std::mem::take(skipping) {
        *skipped += 1;
        let _ = events.send(ChildEvent::Skipped(*skipped));
        return;
    }
    let bytes = std::mem::take(line);
    let mut s = String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    if s.ends_with('\r') {
        s.pop();
    }
    if !s.is_empty() {
        let _ = events.send(ChildEvent::Line(s));
    }
}

fn write_lines(mut w: impl Write, queue: Receiver<String>) {
    for mut line in queue {
        line.push('\n');
        if w.write_all(line.as_bytes())
            .and_then(|()| w.flush())
            .is_err()
        {
            return; // the child is gone; try_send now reports the input closed
        }
    }
    // all senders dropped (close_stdin): dropping `w` closes the child's stdin
}

fn drain_tail(mut r: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let mut t = tail.lock().unwrap();
                t.extend_from_slice(&buf[..n]);
                let over = t.len().saturating_sub(STDERR_TAIL_BYTES);
                t.drain(..over);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn wait_exit(child: &Mutex<Child>, eof: &Receiver<()>, events: &Sender<ChildEvent>) {
    let code = loop {
        match child.lock().unwrap().try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => break None,
        }
        thread::sleep(EXIT_POLL);
    };
    // let the reader deliver what the child wrote before exiting; bounded so
    // a grandchild holding the pipe open cannot hide the exit
    let _ = eof.recv_timeout(Duration::from_secs(2));
    let _ = events.send(ChildEvent::Exit(code));
}

/* ------------------------------------------------------------------ */
/* Desktop browser opener (§4.1, §4.7 `flow: desktop`, T5)             */
/* ------------------------------------------------------------------ */

/// Far below CreateProcess's 32,767-char command line.
const MAX_OPEN_URL: usize = 8192;

/// The `rundll32.exe` argument vector that opens `url` in the default browser.
///
/// Only `http://`/`https://` with a host, and only RFC 3986 characters minus
/// `'`: no spaces, quotes, backticks, controls, non-ASCII or `|^<>\{}`. Nothing here
/// can add an argument or escape the browser's `"%1"`. `&` stays allowed:
/// every OAuth URL has a query string, and rundll32 is not a shell. That is
/// why the ADR picks it over `cmd /c start`, which would split at `&`.
pub fn opener_args(url: &str) -> Result<Vec<String>, &'static str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("only http and https URLs can be opened")?;
    if url.len() > MAX_OPEN_URL {
        return Err("URL is too long");
    }
    if rest.is_empty() || rest.starts_with(['/', '?', '#']) {
        return Err("URL has no host");
    }
    if !url
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&()*+,;=%".contains(&c))
    {
        return Err("URL has characters that are not allowed");
    }
    Ok(vec!["url.dll,FileProtocolHandler".into(), url.into()])
}

/// Opens a runtime-produced sign-in URL on the desktop: `rundll32.exe
/// url.dll,FileProtocolHandler <url>` as an argument vector, by absolute
/// path, with the scrubbed environment (a browser it starts must not inherit
/// `HOST_TOKEN` either). The phone never supplies this URL (§4.7).
pub fn open_url_on_desktop(url: &str) -> Result<(), &'static str> {
    let args = opener_args(url)?;
    let mut cmd = Command::new(system32("rundll32.exe"));
    cmd.args(&args)
        .env_clear()
        .envs(child_env(std::env::vars_os(), &[]))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // dropping the Child neither kills nor leaks it on Windows
    no_window(&mut cmd)
        .spawn()
        .map(drop)
        .map_err(|_| "could not start the desktop browser")
}

/// By absolute path, so a planted `rundll32.exe` on PATH is never picked.
fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(exe)
}

/* ------------------------------------------------------------------ */
/* MCP OAuth phone callback (§4.7 `flow: phone-callback`, T5)          */
/* ------------------------------------------------------------------ */

/// Protocol cap on `mcp_auth_callback.callbackUrl` (§3.4).
pub const MAX_CALLBACK_URL: usize = 4096;
/// A pending sign-in lives this long (§4.6 `mcp_auth` → `expired`).
pub const AUTH_TTL_MS: i64 = 10 * 60 * 1000;

/// A checked callback: the decoded `code` and `state` (OpenCode wants the
/// code) and the URL itself (Claude wants `mcp_oauth_callback_url`).
#[derive(Debug, Clone, PartialEq)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
    pub url: String,
}

/// Checks the dead-page URL the user pasted from the phone browser against
/// the sign-in the companion started: scheme exactly `http`, host exactly
/// `localhost` or `127.0.0.1` (no userinfo, IPv6 or other hosts), the exact
/// port and path, no fragment, `state` equal to the expected value
/// (constant time), a non-empty `code`, and neither repeated. Query values
/// are form-decoded like `URLSearchParams`, so the decoded `code` matches what
/// the runtime would read from the forwarded URL. Errors never echo the URL:
/// it carries the authorization code (T5).
pub fn check_callback_url(
    pasted: &str,
    expected_port: u16,
    expected_path: &str,
    expected_state: &str,
) -> Result<CallbackParams, &'static str> {
    let url = pasted.trim();
    if url.len() > MAX_CALLBACK_URL {
        return Err("callback URL is too long");
    }
    // a browser address bar gives percent-encoded ASCII; anything else could
    // parse differently here and in the runtime (WHATWG strips tabs, for one)
    if !url.bytes().all(|c| c.is_ascii_graphic()) {
        return Err("callback URL has characters that are not allowed");
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or("callback URL must start with http://")?;
    if url.contains('#') {
        return Err("callback URL must not carry a fragment");
    }
    let (authority, rest) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    if authority.contains('@') {
        return Err("callback URL must not carry credentials");
    }
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or("callback URL has no port")?;
    if host != "localhost" && host != "127.0.0.1" {
        return Err("callback host must be localhost or 127.0.0.1");
    }
    if port != expected_port.to_string() {
        return Err("callback port does not match the sign-in");
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if path != expected_path {
        return Err("callback path does not match the sign-in");
    }
    let (mut code, mut state) = (None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let slot = match form_decode(k)?.as_str() {
            "code" => &mut code,
            "state" => &mut state,
            _ => continue,
        };
        if slot.replace(form_decode(v)?).is_some() {
            return Err("callback URL repeats code or state");
        }
    }
    let state = state.ok_or("callback URL has no state")?;
    if state.is_empty() || !crate::crypto::ct_eq(state.as_bytes(), expected_state.as_bytes()) {
        return Err("callback state does not match the sign-in");
    }
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or("callback URL has no code")?;
    Ok(CallbackParams {
        code,
        state,
        url: url.to_string(),
    })
}

/// `application/x-www-form-urlencoded` decoding: `+` is a space, `%XX` a
/// byte. Bad escapes, invalid UTF-8 and control characters are refused.
pub(crate) fn form_decode(s: &str) -> Result<String, &'static str> {
    let b = s.as_bytes();
    let hex = |k: usize| b.get(k).and_then(|&c| (c as char).to_digit(16));
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let (hi, lo) = hex(i + 1)
                    .zip(hex(i + 2))
                    .ok_or("callback URL has a bad percent escape")?;
                out.push(((hi << 4) | lo) as u8);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    let s = String::from_utf8(out).map_err(|_| "callback URL is not valid UTF-8")?;
    if s.chars().any(char::is_control) {
        return Err("callback URL has control characters");
    }
    Ok(s)
}

/// Pending MCP sign-ins: at most one per (session, server), alive for
/// [`AUTH_TTL_MS`], single use. `T` is what the driver needs at callback
/// time (expected port/path/state, the `mcp_auth` command id).
pub struct PendingAuth<T> {
    map: HashMap<(String, String), (i64, T)>,
}

impl<T> Default for PendingAuth<T> {
    fn default() -> Self {
        PendingAuth {
            map: HashMap::new(),
        }
    }
}

impl<T> PendingAuth<T> {
    /// Starts a sign-in; one already pending for the pair is replaced and
    /// handed back so its command can be closed.
    pub fn insert(&mut self, session: &str, server: &str, value: T, now_ms: i64) -> Option<T> {
        let key = (session.to_string(), server.to_string());
        self.map
            .insert(key, (now_ms + AUTH_TTL_MS, value))
            .map(|(_, v)| v)
    }

    /// The pair's entry while it has not expired (what the phone is shown).
    pub fn get(&self, session: &str, server: &str, now_ms: i64) -> Option<&T> {
        let (deadline, value) = self.map.get(&(session.to_string(), server.to_string()))?;
        (now_ms < *deadline).then_some(value)
    }

    /// Removes the pair's entry and returns it if it has not expired. Taking
    /// is the single use: a callback that then fails its check needs a fresh
    /// sign-in, so `state` cannot be guessed across attempts.
    pub fn take(&mut self, session: &str, server: &str, now_ms: i64) -> Option<T> {
        let (deadline, value) = self
            .map
            .remove(&(session.to_string(), server.to_string()))?;
        (now_ms < deadline).then_some(value)
    }

    /// Drops expired entries and hands them back (their `mcp_auth` commands
    /// end `expired`).
    pub fn sweep(&mut self, now_ms: i64) -> Vec<T> {
        self.map
            .extract_if(|_, (deadline, _)| now_ms >= *deadline)
            .map(|(_, (_, value))| value)
            .collect()
    }
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// Secret-shaped test data built at run time, so no key-looking literal
    /// sits in the source for secret scanners to trip on.
    fn fake(prefix: &str, n: usize) -> String {
        let body: String = "aB3dE5fG7h".chars().cycle().take(n).collect();
        format!("{prefix}{body}")
    }

    fn jwt() -> String {
        format!("{}.{}.{}", fake("eyJ", 17), fake("eyJ", 24), fake("", 22))
    }

    fn pem(kind: &str, body: &str) -> String {
        let (begin, end) = ("-----BEGIN", "-----END");
        format!("{begin} {kind}PRIVATE KEY-----{body}{end} {kind}PRIVATE KEY-----")
    }

    /// (input, expected) pairs covering every shape.
    fn shapes() -> Vec<(String, String)> {
        let m = |kind: &str| format!("«redacted:{kind}»");
        let mut v = vec![
            (
                format!("key {} end", fake("sk-ant-api03-", 40)),
                format!("key {} end", m("anthropic-key")),
            ),
            (fake("sk-proj-", 30), m("openai-key")),
            (fake("ghu_", 36), m("github-token")),
            (fake("glpat-", 20), m("gitlab-token")),
            (fake("npm_", 36), m("npm-token")),
            (fake("sk_live_", 24), m("stripe-key")),
            (fake("rk_live_", 24), m("stripe-key")),
            (fake("sk-", 48), m("openai-key")),
            (
                format!("é{}é", fake("sk-ant-", 30)),
                format!("é{}é", m("anthropic-key")),
            ),
            (format!("{}.", jwt()), format!("{}.", m("jwt"))),
            (
                format!("AKIA{} ok", "Q7".repeat(8)),
                format!("{} ok", m("aws-key")),
            ),
            (format!("AIza{}", "x9_-Y".repeat(7)), m("google-key")),
            (
                format!("xoxb-1234567890-{}", fake("", 24)),
                m("slack-token"),
            ),
            (format!("xoxp-{}", fake("", 12)), m("slack-token")),
            (
                format!(
                    "before\n{}\nafter",
                    pem("RSA ", "\nMIIEpAIBAAKCAQEA0Z3\nabc+/=\n")
                ),
                format!("before\n{}\nafter", m("private-key")),
            ),
            (
                format!("{{\"k\":\"{}\\n\"}}", pem("", "\\nMIIEvQIBADANBg\\n")),
                format!("{{\"k\":\"{}\\n\"}}", m("private-key")),
            ),
            (
                "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIBc".into(),
                m("private-key"),
            ),
            (
                "curl -H \"Authorization: Bearer abc.DEF-123_~+/=\" x".into(),
                format!("curl -H \"Authorization: Bearer {}\" x", m("bearer")),
            ),
            (
                "authorization: bearer t0k3n".into(),
                format!("authorization: bearer {}", m("bearer")),
            ),
            (
                "{\"Authorization\": \"Bearer t0k\"}".into(),
                format!("{{\"Authorization\": \"Bearer {}\"}}", m("bearer")),
            ),
            (
                "DB_PASSWORD=hunter2 next".into(),
                format!("DB_PASSWORD={} next", m("secret")),
            ),
            ("--api-key=k3y".into(), format!("--api-key={}", m("secret"))),
            (
                "export AWS_SECRET_ACCESS_KEY=abc/def+g==".into(),
                format!("export AWS_SECRET_ACCESS_KEY={}", m("secret")),
            ),
            (
                "https://x.test/cb?access_token=abc123&x=1".into(),
                format!("https://x.test/cb?access_token={}&x=1", m("secret")),
            ),
            (
                "GITHUB_TOKEN=abcdef".into(),
                format!("GITHUB_TOKEN={}", m("secret")),
            ),
            (
                "PASSWORD=\"two words\"".into(),
                format!("PASSWORD=\"{}\"", m("secret")),
            ),
            (
                "{\"client_secret\": \"abc123\", \"name\": \"x\"}".into(),
                format!(
                    "{{\"client_secret\": \"{}\", \"name\": \"x\"}}",
                    m("secret")
                ),
            ),
            (
                "{\"Credentials\":\"a\\\"b\"}".into(),
                format!("{{\"Credentials\":\"{}\"}}", m("secret")),
            ),
            (
                "{\\\"password\\\":\\\"hunter2\\\"}".into(),
                format!("{{\\\"password\\\":\\\"{}\\\"}}", m("secret")),
            ),
            (
                "password = 'hunter2'".into(),
                format!("password = '{}'", m("secret")),
            ),
            (
                "passwd='pa\\'ss' ok".into(),
                format!("passwd='{}' ok", m("secret")),
            ),
            (
                "api_key := \"x\"".into(),
                format!("api_key := \"{}\"", m("secret")),
            ),
            (
                "пароль: PASSWORD=pässwörd🎉 ok".into(),
                format!("пароль: PASSWORD={} ok", m("secret")),
            ),
        ];
        for p in ["ghp_", "gho_", "ghs_"] {
            v.push((fake(p, 36), m("github-token")));
        }
        v.push((fake("github_pat_", 82), m("github-token")));
        v
    }

    /// Ordinary text, code and data that must pass untouched.
    const ORDINARY: &[&str] = &[
        "The task-runner on my desk-top ran a risk-free demo; sk-learn is not a key, nor is sk-SK.",
        "Tokens are counted per turn; password managers help. Authorization is required.",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 941ebc9 5340ea9f",
        "SGVsbG8sIFdvcmxkIQ== dGhlIHF1aWNrIGJyb3duIGZveCBqdW1wcyBvdmVyIHRoZSBsYXp5IGRvZw==",
        "if token == expected { return password; } let password = read_password();",
        "tokens.map(token => token.trim()); use crate::token::Kind;",
        "\"input_tokens\": 1234, \"max_tokens\": 4096, max_tokens=4096 TOKENIZERS_PARALLELISM=false",
        "Authorization: Bearer <token> or Authorization: Bearer $TOKEN; the --password flag",
        "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqh\n-----END PUBLIC KEY-----",
        "AKIA, AKIAshort, eyJ, eyJhbGciOiJIUzI1NiJ9 alone, ghp_short, xoxb-1, sk-abcdefghijklmnopqrstuvwxyz",
        "https://example.com/desk-top/task-runner?page=2&sort=asc",
        "«redacted:secret» «redacted:private-key» PASSWORD=«redacted:secret»",
    ];

    #[test]
    fn redacts_known_secret_shapes() {
        for (input, want) in shapes() {
            assert_eq!(redact(&input), want, "input: {input:?}");
        }
    }

    #[test]
    fn redaction_leaves_ordinary_text_alone() {
        for s in ORDINARY {
            assert_eq!(redact(s), *s, "false positive");
        }
    }

    #[test]
    fn redaction_is_idempotent_and_utf8_safe() {
        let mut corpus: Vec<String> = shapes().into_iter().map(|(input, _)| input).collect();
        corpus.extend(ORDINARY.iter().map(|s| s.to_string()));
        corpus.push(format!(
            "日本語 e\u{301} 👩‍💻 {} «x» PASSWORD=ü€😀 {}",
            jwt(),
            fake("ghp_", 36)
        ));
        let all = corpus.join(" | ");
        corpus.push(all);
        for s in &corpus {
            let once = redact(s);
            assert_eq!(redact(&once), once, "not idempotent: {s:?}");
            // every truncation point: no panic from a split char, still idempotent
            for (k, _) in s.char_indices() {
                let cut = redact(&s[..k]);
                assert_eq!(redact(&cut), cut, "prefix {k} of {s:?}");
            }
        }
    }

    #[test]
    fn redaction_is_linear_on_a_hostile_mebibyte() {
        const MIB: usize = 1 << 20;
        let fill = |unit: &str| unit.repeat(MIB / unit.len());
        let inputs = [
            fill("a"),
            fill("ab "),
            fill("é"),
            fill("sk-"),
            fill("x="),
            fill("token "),
            fill("password="),
            fill("\"token\": \""),
            fill("eyJaaaaaaaaaaa."),
            fill("-----BEGIN PRIVATE KEY-----"),
            fill("Authorization: Bearer "),
            format!("{}=x", "A".repeat(MIB)),
        ];
        let started = Instant::now();
        for s in &inputs {
            assert!(!redact(s).is_empty());
        }
        // a quadratic scan needs hours here; linear takes a few seconds
        // unoptimized (debug call overhead per word), ~0.3 s in release
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn redact_empty_string_is_empty() {
        assert_eq!(redact(""), "");
    }

    #[test]
    fn redact_leaves_plain_numbers_and_booleans_under_token_keys_alone() {
        assert_eq!(redact("TOKEN=1234"), "TOKEN=1234");
        assert_eq!(redact("max_tokens=4096"), "max_tokens=4096");
        assert_eq!(redact("use_token=true"), "use_token=true");
        assert_eq!(redact("token=null"), "token=null");
    }

    #[test]
    fn redact_masks_non_numeric_values_under_token_keys() {
        assert_eq!(redact("TOKEN=my_secret_token"), "TOKEN=«redacted:secret»");
        assert_eq!(redact("token=ghp_token123"), "token=«redacted:secret»");
    }

    #[test]
    fn redact_handles_single_quoted_secrets() {
        assert_eq!(
            redact("api_key='my-super-secret'"),
            "api_key='«redacted:secret»'"
        );
    }

    #[test]
    fn redact_handles_colon_with_quoted_yaml_style() {
        assert_eq!(
            redact("password: \"secret_val\""),
            "password: \"«redacted:secret»\""
        );
    }

    #[test]
    fn redact_bearer_token_with_various_casing() {
        assert_eq!(
            redact("AUTHORIZATION: BEARER secret123"),
            "AUTHORIZATION: BEARER «redacted:bearer»"
        );
        assert_eq!(
            redact("Authorization: Bearer my-custom-jwt-or-token"),
            "Authorization: Bearer «redacted:bearer»"
        );
    }

    #[test]
    fn redact_preserves_unicode_surrounding_tokens() {
        let input = "🔐 sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456789 🔐";
        assert_eq!(redact(input), "🔐 «redacted:anthropic-key» 🔐");
    }

    #[test]
    fn redact_multiple_different_secrets_in_single_line() {
        let input = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456789 with password=\"secret\"";
        assert_eq!(
            redact(input),
            "«redacted:anthropic-key» with password=\"«redacted:secret»\""
        );
    }

    #[test]
    fn redact_handles_unterminated_quotes() {
        let input = "password=\"unterminated";
        let out = redact(input);
        assert!(out.contains("«redacted:secret»"));
    }

    /* -------------------------- child IO -------------------------- */

    const NO_ARGS: &[&str] = &[];

    fn line(s: &str) -> ChildEvent {
        ChildEvent::Line(s.into())
    }

    /// Lines until the exit event, with its code.
    fn collect(rx: &Receiver<ChildEvent>) -> (Vec<String>, Option<i32>) {
        let mut lines = Vec::new();
        loop {
            match rx
                .recv_timeout(Duration::from_secs(20))
                .expect("child event within 20 s")
            {
                ChildEvent::Line(l) => lines.push(l),
                ChildEvent::Skipped(_) => {}
                ChildEvent::Exit(code) => return (lines, code),
            }
        }
    }

    /// Just enough environment for System32 tools; no real secret enters.
    fn system_env() -> Vec<(OsString, OsString)> {
        ["SystemRoot", "PATH"]
            .into_iter()
            .filter_map(|k| Some((OsString::from(k), std::env::var_os(k)?)))
            .collect()
    }

    #[test]
    fn stdout_lines_are_capped_and_skips_counted() {
        // the real cap: one byte over is skipped to its newline, neighbours survive
        let input = format!("first\r\n{}\nsecond\n\n", "x".repeat(MAX_LINE_BYTES + 1));
        let (tx, rx) = mpsc::channel();
        read_lines(input.as_bytes(), MAX_LINE_BYTES, &tx);
        drop(tx);
        assert_eq!(
            rx.iter().collect::<Vec<_>>(),
            [line("first"), ChildEvent::Skipped(1), line("second")]
        );
        // exactly the cap passes; an unterminated over-long tail still counts
        let (tx, rx) = mpsc::channel();
        read_lines(&b"12345678\n123456789\nok\n123456789"[..], 8, &tx);
        drop(tx);
        assert_eq!(
            rx.iter().collect::<Vec<_>>(),
            [
                line("12345678"),
                ChildEvent::Skipped(1),
                line("ok"),
                ChildEvent::Skipped(2)
            ]
        );
    }

    #[test]
    fn pipe_peer_round_trip_then_exit_after_the_last_line() {
        let (out_r, mut out_w) = io::pipe().unwrap();
        let (in_r, in_w) = io::pipe().unwrap();
        let (child, rx) = ChildIo::from_pipes(out_r, in_w).unwrap();
        child.try_send(r#"{"type":"user"}"#.into()).unwrap();
        let mut peer = BufReader::new(in_r);
        let mut got = String::new();
        peer.read_line(&mut got).unwrap();
        assert_eq!(got, "{\"type\":\"user\"}\n");
        assert_eq!(
            child.try_send("two\nframes".into()),
            Err("a frame must be a single line")
        );
        out_w
            .write_all(b"{\"type\":\"system\"}\r\n{\"type\":\"result\"}\n")
            .unwrap();
        drop(out_w);
        let want = vec![
            r#"{"type":"system"}"#.to_string(),
            r#"{"type":"result"}"#.to_string(),
        ];
        assert_eq!(collect(&rx), (want, None));
        child.close_stdin();
        assert_eq!(
            child.try_send("late".into()),
            Err("runtime input is closed")
        );
        got.clear();
        assert_eq!(peer.read_line(&mut got).unwrap(), 0, "the peer sees EOF");
    }

    /// A child that never reads: every write blocks until the test ends.
    struct Stuck(Receiver<()>);

    impl Write for Stuck {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn full_input_queue_refuses_without_blocking() {
        let (release, stuck) = mpsc::channel::<()>();
        let (child, _rx) = ChildIo::from_pipes(io::empty(), Stuck(stuck)).unwrap();
        let mut accepted = 0;
        let (err, took) = loop {
            let t = Instant::now();
            match child.try_send(format!("frame {accepted}")) {
                Ok(()) => accepted += 1,
                Err(e) => break (e, t.elapsed()),
            }
            assert!(accepted <= INPUT_QUEUE + 1, "the queue never filled");
        };
        assert_eq!(err, "runtime is not reading input");
        assert!(
            took < Duration::from_secs(1),
            "try_send blocked for {took:?}"
        );
        // the queue, plus the one frame the writer may already hold
        assert!(
            accepted == INPUT_QUEUE || accepted == INPUT_QUEUE + 1,
            "{accepted}"
        );
        drop(release);
    }

    #[test]
    fn stderr_tail_is_bounded_and_redacted() {
        let tail = Mutex::new(Vec::new());
        let noise: Vec<u8> = (0..100_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        drain_tail(&noise[..], &tail);
        assert_eq!(
            tail.lock().unwrap().as_slice(),
            &noise[noise.len() - STDERR_TAIL_BYTES..]
        );
        let key = fake("sk-ant-api03-", 40);
        let child = ChildIo {
            input: Mutex::new(None),
            stderr: Arc::new(Mutex::new(format!("boom {key}\n").into_bytes())),
            child: None,
        };
        assert_eq!(child.stderr_tail(), "boom «redacted:anthropic-key»\n");
    }

    #[test]
    fn child_env_drops_host_token_chernobyl_and_node_options() {
        let base = [
            ("PATH", "C:\\bin"),
            ("HOST_TOKEN", "t"),
            ("host_token", "t"),
            ("CHERNOBYL_HOME", "h"),
            ("chernobyl_control_key", "k"),
            ("NODE_OPTIONS", "--require x"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-vscode"),
            ("KEEP", "1"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let extras = [
            ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
            ("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1"),
        ];
        let env = child_env(base, &extras);
        let pairs: Vec<(&str, &str)> = env
            .iter()
            .map(|(k, v)| (k.to_str().unwrap(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("PATH", "C:\\bin"),
                ("KEEP", "1"),
                ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
                ("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1"),
            ]
        );
    }

    #[test]
    fn real_child_round_trip_exit_code_and_kill() {
        let (cwd, env) = (std::env::temp_dir(), child_env(system_env(), &[]));
        // findstr numbers every stdin line until EOF: proves the writer,
        // close_stdin and the reader (sort.exe sniffs short input as UTF-16)
        let findstr = system32("findstr.exe");
        let (child, rx) = ChildIo::spawn(&findstr, &["/N", "^"], &cwd, &env).unwrap();
        child.try_send("b".into()).unwrap();
        child.try_send("a".into()).unwrap();
        child.close_stdin();
        assert_eq!(
            collect(&rx),
            (vec!["1:b".to_string(), "2:a".to_string()], Some(0))
        );
        // cmd.exe only as the child under test: a real exit code comes through
        let (_child, rx) =
            ChildIo::spawn(&system32("cmd.exe"), &["/d", "/c", "exit", "7"], &cwd, &env).unwrap();
        assert_eq!(collect(&rx), (vec![], Some(7)));
        // a child that never exits on its own is killed after the grace
        let (child, rx) = ChildIo::spawn(&findstr, &["/N", "^"], &cwd, &env).unwrap();
        let t = Instant::now();
        child.kill_after(Duration::from_millis(200));
        let (lines, code) = collect(&rx);
        assert!(lines.is_empty() && code.is_some_and(|c| c != 0), "{code:?}");
        assert!(t.elapsed() >= Duration::from_millis(200));
        // no shell, ever
        let err = ChildIo::spawn(Path::new("C:\\tools\\claude.CMD"), NO_ARGS, &cwd, &env)
            .err()
            .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn real_child_sees_only_the_scrubbed_env() {
        let mut base = system_env();
        base.extend(
            [
                ("HOST_TOKEN", "t"),
                ("CHERNOBYL_HOME", "h"),
                ("NODE_OPTIONS", "--require x"),
                ("KEEP_ME", "1"),
            ]
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        let env = child_env(base, &[("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")]);
        let (_child, rx) = ChildIo::spawn(
            &system32("cmd.exe"),
            &["/d", "/c", "set"],
            &std::env::temp_dir(),
            &env,
        )
        .unwrap();
        let (lines, code) = collect(&rx);
        assert_eq!(code, Some(0));
        // names only in failure messages: values never reach the test log
        let names: Vec<String> = lines
            .iter()
            .filter_map(|l| l.split_once('='))
            .map(|(k, _)| k.to_ascii_uppercase())
            .collect();
        for gone in ["HOST_TOKEN", "CHERNOBYL_HOME", "NODE_OPTIONS"] {
            assert!(!names.iter().any(|n| n == gone), "{gone} leaked: {names:?}");
        }
        for kept in ["PATH", "KEEP_ME", "CLAUDE_CODE_ENTRYPOINT"] {
            assert!(names.iter().any(|n| n == kept), "{kept} missing: {names:?}");
        }
        assert!(lines.iter().any(|l| l == "CLAUDE_CODE_ENTRYPOINT=sdk-ts"));
    }

    /* ---------------------- opener and OAuth ---------------------- */

    #[test]
    fn opener_args_reject_non_http_and_shell_metacharacters() {
        // `&` must pass: every OAuth URL has one, and rundll32 is not a shell
        let url = "https://auth.example.com/authorize?response_type=code&client_id=c1\
                   &redirect_uri=http%3A%2F%2Flocalhost%3A54321%2Fcallback&state=s~1";
        assert_eq!(
            opener_args(url).unwrap(),
            ["url.dll,FileProtocolHandler", url]
        );
        assert!(opener_args("http://localhost:8080/x").is_ok());
        for bad in [
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "ftp://example.com/",
            "HTTPS://example.com/",
            "calc.exe",
            "https://",
            "https:///path",
            "\"https://example.com\"",
            "https://example.com/\"--gpu-launcher=calc",
            "https://example.com/a b",
            "https://example.com/a\tb",
            "https://example.com/a\nb",
            "https://example.com/a\rb",
            "https://example.com/a\0b",
            "https://example.com/a|b",
            "https://example.com/a^b",
            "https://example.com/a<b>",
            "https://example.com/a`b",
            "https://example.com/a'b",
            "https://example.com/a\\b",
            "https://example.com/{x}",
            "https://exämple.com/",
        ] {
            assert!(opener_args(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(opener_args(&format!("https://example.com/{}", "a".repeat(9000))).is_err());
    }

    #[test]
    fn mcp_callback_checks_host_port_path_state() {
        let check = |u: &str| check_callback_url(u, 54321, "/callback", "st-8f2a");
        let url = "http://localhost:54321/callback?code=ab%2Bc+d&state=st-8f2a";
        let want = CallbackParams {
            code: "ab+c d".into(),
            state: "st-8f2a".into(),
            url: url.into(),
        };
        assert_eq!(check(url), Ok(want));
        let ok = check(
            "  http://127.0.0.1:54321/callback?state=st-8f2a&iss=https%3A%2F%2Fa.test&code=xyz\n",
        )
        .unwrap();
        assert_eq!(ok.code, "xyz");
        assert_eq!(
            ok.url,
            "http://127.0.0.1:54321/callback?state=st-8f2a&iss=https%3A%2F%2Fa.test&code=xyz"
        );

        let (scheme, creds, host) = (
            "callback URL must start with http://",
            "callback URL must not carry credentials",
            "callback host must be localhost or 127.0.0.1",
        );
        let (port, path, state) = (
            "callback port does not match the sign-in",
            "callback path does not match the sign-in",
            "callback state does not match the sign-in",
        );
        let (chars, escape, repeat) = (
            "callback URL has characters that are not allowed",
            "callback URL has a bad percent escape",
            "callback URL repeats code or state",
        );
        let q = "?code=x&state=st-8f2a";
        let refused = [
            (format!("https://localhost:54321/callback{q}"), scheme),
            (format!("HTTP://localhost:54321/callback{q}"), scheme),
            (format!("http://evil@localhost:54321/callback{q}"), creds),
            (
                format!("http://localhost:54321@evil.test/callback{q}"),
                creds,
            ),
            (
                format!("http://evil.test\\@localhost:54321/callback{q}"),
                creds,
            ),
            (format!("http://evil.test:54321/callback{q}"), host),
            (format!("http://[::1]:54321/callback{q}"), host),
            (format!("http://LOCALHOST:54321/callback{q}"), host),
            (format!("http://localhost.:54321/callback{q}"), host),
            (format!("http://127.0.0.2:54321/callback{q}"), host),
            (
                format!("http://localhost/callback{q}"),
                "callback URL has no port",
            ),
            (format!("http://localhost:54322/callback{q}"), port),
            (format!("http://localhost:054321/callback{q}"), port),
            (format!("http://localhost:54321/callback/{q}"), path),
            (format!("http://localhost:54321/Callback{q}"), path),
            (format!("http://localhost:54321/callback%2F{q}"), path),
            (format!("http://localhost:54321{q}"), path),
            (
                format!("http://localhost:54321/callback{q}#code=y"),
                "callback URL must not carry a fragment",
            ),
            (
                "http://localhost:54321/callback?state=st-8f2a#code=x".into(),
                "callback URL must not carry a fragment",
            ),
            (
                "http://localhost:54321/callback?code=x".into(),
                "callback URL has no state",
            ),
            (
                "http://localhost:54321/callback?code=x&state=st-8f2b".into(),
                state,
            ),
            (
                "http://localhost:54321/callback?code=x&state=st-8f2".into(),
                state,
            ),
            (
                "http://localhost:54321/callback?code=x&state=".into(),
                state,
            ),
            (
                "http://localhost:54321/callback?code=x&state=st-8f2a&state=st-8f2a".into(),
                repeat,
            ),
            (
                "http://localhost:54321/callback?code=x&co%64e=y&state=st-8f2a".into(),
                repeat,
            ),
            (
                "http://localhost:54321/callback?state=st-8f2a".into(),
                "callback URL has no code",
            ),
            (
                "http://localhost:54321/callback?code=&state=st-8f2a".into(),
                "callback URL has no code",
            ),
            (
                "http://localhost:54321/callback?code=%G1&state=st-8f2a".into(),
                escape,
            ),
            (
                "http://localhost:54321/callback?code=%4&state=st-8f2a".into(),
                escape,
            ),
            (
                "http://localhost:54321/callback?code=%+1&state=st-8f2a".into(),
                escape,
            ),
            (
                "http://localhost:54321/callback?code=a%00&state=st-8f2a".into(),
                "callback URL has control characters",
            ),
            (
                "http://localhost:54321/callback?code=%FF&state=st-8f2a".into(),
                "callback URL is not valid UTF-8",
            ),
            (
                "http://localhost:54321/callback?code=a b&state=st-8f2a".into(),
                chars,
            ),
            (
                "http://localhost:54321/callback?code=é&state=st-8f2a".into(),
                chars,
            ),
            (format!("http://local\thost:54321/callback{q}"), chars),
        ];
        for (url, why) in refused {
            assert_eq!(check(&url), Err(why), "{url:?}");
        }
        let long = format!(
            "http://localhost:54321/callback?state=st-8f2a&code={}",
            "c".repeat(MAX_CALLBACK_URL)
        );
        assert_eq!(check(&long), Err("callback URL is too long"));
        // nothing pending matches nothing, not even an empty state
        let empty = check_callback_url(
            "http://localhost:54321/callback?code=x&state=",
            54321,
            "/callback",
            "",
        );
        assert_eq!(empty, Err(state));
    }

    #[test]
    fn pending_auth_expires_after_ten_minutes() {
        let mut p = PendingAuth::default();
        assert_eq!(p.insert("s1", "github", "a", 0), None);
        assert_eq!(p.take("s1", "github", AUTH_TTL_MS - 1), Some("a"));
        assert_eq!(p.take("s1", "github", AUTH_TTL_MS - 1), None, "single use");
        p.insert("s1", "github", "b", 0);
        assert_eq!(
            p.take("s1", "github", AUTH_TTL_MS),
            None,
            "expired at ten minutes"
        );
        assert_eq!(
            p.take("s1", "github", 0),
            None,
            "an expired take still consumes"
        );
        // one per (session, server): a newer start replaces and returns the old
        p.insert("s1", "github", "c", 0);
        assert_eq!(p.insert("s1", "github", "d", 1_000), Some("c"));
        p.insert("s2", "github", "e", 0);
        p.insert("s1", "linear", "f", 5 * 60_000);
        let mut gone = p.sweep(1_000 + AUTH_TTL_MS);
        gone.sort_unstable();
        assert_eq!(gone, ["d", "e"]);
        assert_eq!(p.take("s1", "linear", AUTH_TTL_MS), Some("f"));
        assert!(p.sweep(i64::MAX).is_empty());
    }
}
