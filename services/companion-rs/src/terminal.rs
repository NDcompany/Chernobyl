//! ADR-020: the opt-in one-shot terminal. One command line at a time per
//! workspace runs through the platform shell (`cmd.exe /d /s /c` on Windows,
//! `sh -c` elsewhere): this IS arbitrary execution, without the agents'
//! approval prompts, which is why it exists only when the operator sets
//! `terminal.enabled` in companion.json and only for a paired E2E device.
//!
//! - cwd = the allow-listed workspace; the scrubbed child env (T6); no stdin.
//! - [`TIMEOUT`], or a stop from the phone, kills the whole process tree.
//! - Output (stdout and stderr merged) is line-buffered, ANSI-stripped and
//!   redacted before it leaves, capped at [`OUTPUT_MAX`] bytes, and never
//!   journaled. The command line is journaled and audited, redacted.

use crate::runtime_io::{child_env, no_window, redact};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes of output sent per run; the rest is drained and dropped.
pub const OUTPUT_MAX: usize = 1 << 20;
/// At most one start per this interval, companion-wide.
pub const START_INTERVAL: Duration = Duration::from_secs(2);
pub const OFF: &str =
    "the terminal is off on this PC (set \"terminal\": {\"enabled\": true} in companion.json)";
pub const NEEDS_E2E: &str = "the terminal needs a paired device with a live E2E session";
/// A partial line goes out after this long, or at this size.
const PARTIAL_AFTER: Duration = Duration::from_millis(250);
const PARTIAL_MAX: usize = 8 << 10;
/// Output is batched into frames of about this size or age.
const FRAME_BYTES: usize = 16 << 10;
const FRAME_AGE: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(20);
/// A grandchild holding the pipes open cannot delay the end past this.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/* -------------------------------------------------------------------- */
/* One run at a time per workspace, one start per START_INTERVAL         */
/* -------------------------------------------------------------------- */

#[derive(Default)]
pub struct Terminals {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Workspace id → its run's stop flag.
    running: HashMap<String, Arc<AtomicBool>>,
    last_start: Option<Instant>,
}

impl Terminals {
    /// Reserves `workspace` for a new run; its stop flag on success.
    pub fn begin(&self, workspace: &str) -> Result<Arc<AtomicBool>, String> {
        let mut st = self.state.lock().unwrap();
        if st.running.contains_key(workspace) {
            return Err("a command is still running in this workspace".into());
        }
        if st.last_start.is_some_and(|t| t.elapsed() < START_INTERVAL) {
            return Err("wait a moment before starting another command".into());
        }
        st.last_start = Some(Instant::now());
        let stop = Arc::new(AtomicBool::new(false));
        st.running.insert(workspace.to_string(), stop.clone());
        Ok(stop)
    }

    pub fn end(&self, workspace: &str) {
        self.state.lock().unwrap().running.remove(workspace);
    }

    /// Asks the workspace's run to stop; false when nothing runs there.
    pub fn stop(&self, workspace: &str) -> bool {
        match self.state.lock().unwrap().running.get(workspace) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }
}

/* -------------------------------------------------------------------- */
/* Process                                                               */
/* -------------------------------------------------------------------- */

/// Starts `command` in `cwd` through the shell, stdin closed, output piped.
pub fn spawn(cwd: &Path, command: &str) -> Result<Child, String> {
    if command.trim().is_empty() {
        return Err("the command is empty".into());
    }
    if command.contains(['\0', '\r', '\n']) {
        return Err("one command line only (no line breaks)".into());
    }
    let mut cmd = shell(command);
    cmd.current_dir(cwd)
        .env_clear()
        .envs(child_env(std::env::vars_os(), &[]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    no_window(&mut cmd)
        .spawn()
        .map_err(|e| format!("could not start the shell: {e}"))
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    // `/s /c "<line>"`: cmd drops the outer quotes and runs the line as
    // typed; `raw_arg` keeps std from re-quoting it. `/d`: no AutoRun.
    let mut cmd = Command::new(system32("cmd.exe"));
    std::os::windows::process::CommandExt::raw_arg(&mut cmd, format!("/d /s /c \"{command}\""));
    cmd
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", command]);
    cmd
}

#[cfg(windows)]
fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(exe)
}

/// Kills the child and everything it started.
fn kill_tree(child: &mut Child) {
    kill_pid(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Kills process `pid` and its descendants (`taskkill /T`; elsewhere the
/// shell's process group, which `spawn` made it lead).
pub fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let mut tree = Command::new(system32("taskkill.exe"));
    #[cfg(windows)]
    tree.args(["/T", "/F", "/PID", &pid.to_string()]);
    #[cfg(not(windows))]
    let mut tree = Command::new("kill");
    #[cfg(not(windows))]
    tree.args(["-KILL", "--", &format!("-{pid}")]);
    let _ = no_window(&mut tree)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/* -------------------------------------------------------------------- */
/* Run                                                                   */
/* -------------------------------------------------------------------- */

#[derive(Debug, PartialEq)]
pub enum End {
    /// Exited by itself (the code; `None` if killed by a signal).
    Exit(Option<i64>),
    Timeout,
    Stopped,
}

impl End {
    pub fn reason(&self) -> &'static str {
        match self {
            End::Exit(_) => "exit",
            End::Timeout => "timeout",
            End::Stopped => "stopped",
        }
    }

    /// The `command_result` error, `None` for a clean exit.
    pub fn error(&self, timeout: Duration) -> Option<String> {
        match self {
            End::Exit(Some(0)) => None,
            End::Exit(Some(code)) => Some(format!("exited with code {code}")),
            End::Exit(None) => Some("ended without an exit code".into()),
            End::Timeout => Some(format!("timed out after {} s", timeout.as_secs())),
            End::Stopped => Some("stopped from the phone".into()),
        }
    }
}

/// Waits for `child`, sending its cleaned output to `emit` in batches, until
/// it exits, `stop` is set or `timeout` passes (then the tree is killed).
/// Returns how it ended and whether output was cut at [`OUTPUT_MAX`].
pub fn run(
    mut child: Child,
    stop: &AtomicBool,
    timeout: Duration,
    cap: usize,
    emit: &mut dyn FnMut(String),
) -> (End, bool) {
    let (tx, rx) = mpsc::channel();
    let mut open = 0;
    for (i, pipe) in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(pipe) = pipe {
            open += 1;
            let tx = tx.clone();
            let _ = std::thread::Builder::new()
                .name("terminal-pipe".into())
                .spawn(move || read_pipe(i, pipe, tx));
        }
    }
    drop(tx);
    let mut out = Output::new(cap);
    let deadline = Instant::now() + timeout;
    let mut end = None;
    let mut grace = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok((i, Some(bytes))) => out.push(i, &bytes),
            Ok((_, None)) => open -= 1,
            Err(mpsc::RecvTimeoutError::Disconnected) => open = 0,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        out.tick(false, emit);
        if end.is_none() {
            end = if stop.load(Ordering::SeqCst) {
                kill_tree(&mut child);
                Some(End::Stopped)
            } else if Instant::now() >= deadline {
                kill_tree(&mut child);
                Some(End::Timeout)
            } else {
                match child.try_wait() {
                    Ok(Some(status)) => Some(End::Exit(status.code().map(i64::from))),
                    Ok(None) => None,
                    Err(_) => {
                        kill_tree(&mut child);
                        Some(End::Exit(None))
                    }
                }
            };
            if end.is_some() {
                grace = Some(Instant::now() + PIPE_GRACE);
            }
        }
        if end.is_some() && (open == 0 || grace.is_some_and(|g| Instant::now() >= g)) {
            break;
        }
    }
    out.tick(true, emit);
    (end.unwrap_or(End::Exit(None)), out.truncated)
}

fn read_pipe(i: usize, mut pipe: Box<dyn Read + Send>, tx: mpsc::Sender<(usize, Option<Vec<u8>>)>) {
    let mut buf = [0u8; 16 << 10];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send((i, Some(buf[..n].to_vec()))).is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send((i, None));
}

/// Line buffers per stream, the frame being built and the byte budget.
struct Output {
    pending: [(Vec<u8>, Option<Instant>); 2],
    frame: String,
    frame_since: Option<Instant>,
    left: usize,
    truncated: bool,
}

impl Output {
    fn new(cap: usize) -> Output {
        Output {
            pending: Default::default(),
            frame: String::new(),
            frame_since: None,
            left: cap,
            truncated: false,
        }
    }

    fn push(&mut self, i: usize, bytes: &[u8]) {
        let (buf, since) = &mut self.pending[i];
        since.get_or_insert_with(Instant::now);
        buf.extend_from_slice(bytes);
    }

    /// Moves what is ready into the frame and sends the frame when it is
    /// big or old enough; `all` flushes everything (the end of the run).
    fn tick(&mut self, all: bool, emit: &mut dyn FnMut(String)) {
        for i in 0..2 {
            let (buf, since) = &mut self.pending[i];
            let cut = if all
                || buf.len() >= PARTIAL_MAX
                || since.is_some_and(|s| s.elapsed() >= PARTIAL_AFTER)
            {
                char_boundary(buf, all)
            } else {
                buf.iter().rposition(|&b| b == b'\n').map_or(0, |n| n + 1)
            };
            if cut == 0 {
                continue;
            }
            let ready: Vec<u8> = buf.drain(..cut).collect();
            *since = (!buf.is_empty()).then(Instant::now);
            let text = clean(&String::from_utf8_lossy(&ready));
            self.add(&text);
        }
        let due = all
            || self.frame.len() >= FRAME_BYTES
            || self.frame_since.is_some_and(|s| s.elapsed() >= FRAME_AGE);
        if due && !self.frame.is_empty() {
            self.frame_since = None;
            emit(std::mem::take(&mut self.frame));
        }
    }

    fn add(&mut self, text: &str) {
        if text.is_empty() || self.truncated {
            return;
        }
        let mut take = text.len().min(self.left);
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        if take < text.len() {
            self.truncated = true;
        }
        self.left -= take;
        self.frame.push_str(&text[..take]);
        self.frame_since.get_or_insert_with(Instant::now);
    }
}

/// How much of `buf` decodes whole: never cut inside a UTF-8 sequence
/// unless this is the end (`all`).
fn char_boundary(buf: &[u8], all: bool) -> usize {
    match std::str::from_utf8(buf) {
        Err(e) if !all && e.error_len().is_none() => e.valid_up_to(),
        _ => buf.len(),
    }
}

/// What the phone may see: ANSI stripped, line endings `\n`, redacted.
pub fn clean(text: &str) -> String {
    redact(&strip_ansi(text).replace("\r\n", "\n").replace('\r', "\n"))
}

/// Drops ANSI/VT escape sequences (CSI, OSC, DCS and two-byte escapes) and
/// other control characters except tab, CR and LF.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                Some('[') => skip_csi(&mut it),
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    // a string ends at BEL or ST (ESC \)
                    while let Some(c) = it.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                // ESC ( B and friends: intermediates, then one final char
                Some(' '..='/') => {
                    while it.next_if(|c| (' '..='/').contains(c)).is_some() {}
                    it.next();
                }
                _ => {}
            },
            '\u{9b}' => skip_csi(&mut it),
            '\t' | '\n' | '\r' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Parameters and intermediates up to the final byte `@`..`~`.
fn skip_csi(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in it.by_ref() {
        if ('@'..='~').contains(&c) {
            break;
        }
    }
}

/* -------------------------------------------------------------------- */
/* Tests                                                                 */
/* -------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn run_line(command: &str, timeout: Duration, cap: usize) -> (End, bool, String, usize) {
        let dir = crate::workspace::tests::temp("term");
        let child = spawn(&dir, command).unwrap();
        let stop = AtomicBool::new(false);
        let (mut text, mut frames) = (String::new(), 0);
        let (end, truncated) = run(child, &stop, timeout, cap, &mut |chunk| {
            frames += 1;
            text.push_str(&chunk)
        });
        (end, truncated, text, frames)
    }

    #[test]
    fn strips_ansi_and_controls() {
        assert_eq!(
            strip_ansi("\u{1b}[1;31mred\u{1b}[0m \u{1b}]0;title\u{7}ok\u{1b}(B\u{8}!\tx\r\n"),
            "red ok!\tx\r\n"
        );
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\"),
            "link"
        );
        assert_eq!(strip_ansi("a\u{9b}2Kb\u{1b}"), "ab");
        assert_eq!(clean("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn runs_in_the_workspace_and_reports_the_exit_code() {
        let (end, truncated, text, _) = run_line("echo hello", TIMEOUT, OUTPUT_MAX);
        assert_eq!(end, End::Exit(Some(0)));
        assert!(!truncated);
        assert_eq!(text.trim_end(), "hello");
        assert_eq!(end.error(TIMEOUT), None);
        let (end, _, _, _) = run_line("exit 3", TIMEOUT, OUTPUT_MAX);
        assert_eq!(end, End::Exit(Some(3)));
        assert_eq!(end.error(TIMEOUT).as_deref(), Some("exited with code 3"));
        // cwd is the workspace; stderr is merged
        let dir = crate::workspace::tests::temp("cwd");
        std::fs::write(dir.join("marker.txt"), "").unwrap();
        let list = if cfg!(windows) {
            "dir /b & echo oops 1>&2"
        } else {
            "ls; echo oops 1>&2"
        };
        let child = spawn(&dir, list).unwrap();
        let mut text = String::new();
        run(
            child,
            &AtomicBool::new(false),
            TIMEOUT,
            OUTPUT_MAX,
            &mut |c| text.push_str(&c),
        );
        assert!(
            text.contains("marker.txt") && text.contains("oops"),
            "{text}"
        );
    }

    #[test]
    fn refuses_blank_and_multi_line_commands() {
        let dir = crate::workspace::tests::temp("refuse");
        for bad in ["", "   ", "echo a\necho b", "echo a\rb", "a\0b"] {
            assert!(spawn(&dir, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_scrubbed_env_hides_the_token() {
        // whatever the test process inherited, the child sees PATH and
        // never HOST_TOKEN or a CHERNOBYL_* variable (T6)
        let probe = if cfg!(windows) { "set" } else { "env" };
        let (_, _, text, _) = run_line(probe, TIMEOUT, OUTPUT_MAX);
        let names: Vec<String> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(n, _)| n.to_ascii_uppercase())
            .collect();
        assert!(names.iter().any(|n| n == "PATH"), "{text}");
        assert!(
            !names
                .iter()
                .any(|n| n == "HOST_TOKEN" || n.starts_with("CHERNOBYL_")),
            "{names:?}"
        );
    }

    #[test]
    fn a_timeout_kills_the_process_tree() {
        // the shell starts a grandchild that would outlive a plain kill
        let sleeper = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >nul"
        } else {
            "sleep 30"
        };
        let started = Instant::now();
        let (end, _, _, _) = run_line(sleeper, Duration::from_millis(500), OUTPUT_MAX);
        assert_eq!(end, End::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            end.error(Duration::from_secs(120)).as_deref(),
            Some("timed out after 120 s")
        );
    }

    #[test]
    fn a_stop_kills_the_run() {
        let dir = crate::workspace::tests::temp("stop");
        let sleeper = if cfg!(windows) {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };
        let child = spawn(&dir, sleeper).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let (end, _) = run(child, &stop, TIMEOUT, OUTPUT_MAX, &mut |_| {});
        assert_eq!(end, End::Stopped);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_is_capped_redacted_and_batched() {
        // ~600 KB of output against a 64 KiB cap
        let flood = if cfg!(windows) {
            "for /L %i in (1,1,6000) do @echo 0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789"
        } else {
            "i=0; while [ $i -lt 6000 ]; do echo 0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789; i=$((i+1)); done"
        };
        let (end, truncated, text, frames) = run_line(flood, TIMEOUT, 64 << 10);
        assert_eq!(end, End::Exit(Some(0)));
        assert!(truncated);
        assert!(
            text.len() <= 64 << 10 && text.len() > 60 << 10,
            "{}",
            text.len()
        );
        assert!(frames < 64, "batched into few frames: {frames}");

        let secret = "echo token=ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let (_, _, text, _) = run_line(secret, TIMEOUT, OUTPUT_MAX);
        assert!(
            !text.contains("ghp_") && text.contains("redacted"),
            "{text}"
        );
    }

    #[test]
    fn one_run_per_workspace_and_a_start_interval() {
        let t = Terminals::default();
        let stop = t.begin("w1").unwrap();
        assert!(t.begin("w1").unwrap_err().contains("still running"));
        // the interval is companion-wide
        assert!(t.begin("w2").unwrap_err().contains("wait a moment"));
        assert!(t.stop("w1") && stop.load(Ordering::SeqCst));
        assert!(!t.stop("w2"));
        t.end("w1");
        assert!(!t.stop("w1"));
        t.state.lock().unwrap().last_start = Some(Instant::now() - START_INTERVAL);
        assert!(t.begin("w1").is_ok());
    }
}
