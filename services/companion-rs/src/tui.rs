//! Phase 8 terminal UI (ADR-011 §5): status, sessions, and approvals with
//! keybinds. The companion is a CLI with a terminal UI — the phone is the only
//! *remote* UI; this is the local operator console.
//!
//! Design: the interactive core is pure and deterministic — a [`TuiState`]
//! model, a [`render`] function (ANSI string), a [`decode_keys`] input decoder
//! and a [`key_to_action`] binder. All of those are unit-tested without a real
//! socket or console. [`run_tui`] is the thin shell that wires them to a live
//! companion over the in-repo WebSocket client and to raw console input.
//!
//! The TUI is a loopback operator console: `companion --tui` boots the daemon in
//! process and connects to it on `127.0.0.1`, where commands need no E2E
//! handshake (remote posture still requires one — a remote refusal is surfaced
//! verbatim in the status line, never silently ignored).

use crate::client::WsClient;
use crate::json::{self, Json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/* ------------------------------------------------------------------ */
/* Input decoding + keybinds (pure)                                    */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
    Other,
}

/// Decodes a raw input chunk (as read with VT input enabled) into keys. A
/// single read usually yields one keystroke, but arrow keys arrive as a
/// 3-byte `\x1b[X` sequence and a fast typist can coalesce several.
pub fn decode_keys(buf: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x1b => {
                if i + 2 < buf.len() && buf[i + 1] == b'[' {
                    let k = match buf[i + 2] {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        b'C' => Key::Right,
                        b'D' => Key::Left,
                        _ => Key::Other,
                    };
                    out.push(k);
                    i += 3;
                } else {
                    out.push(Key::Esc);
                    i += 1;
                }
            }
            b'\r' | b'\n' => {
                out.push(Key::Enter);
                i += 1;
            }
            0x7f | 0x08 => {
                out.push(Key::Backspace);
                i += 1;
            }
            c if (0x20..0x7f).contains(&c) => {
                out.push(Key::Char(c as char));
                i += 1;
            }
            _ => {
                out.push(Key::Other);
                i += 1;
            }
        }
    }
    out
}

/// Convenience: the last key decoded from a chunk (the common single-key case).
pub fn decode_key(buf: &[u8]) -> Option<Key> {
    decode_keys(buf).pop()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Refresh,
    Next,
    Prev,
    Approve,
    Reject,
    Help,
    Pair,
}

/// Binds a key to an operator action. `a`/Enter approve the selected request,
/// `x`/`d` reject it, `j`/`k` (and arrows) move, `r` refreshes, `p` opens the
/// pairing overlay, `?` toggles the help footer, `q`/Esc quit.
pub fn key_to_action(k: Key) -> Option<Action> {
    match k {
        Key::Char('q') | Key::Esc => Some(Action::Quit),
        Key::Char('r') => Some(Action::Refresh),
        Key::Char('j') | Key::Down => Some(Action::Next),
        Key::Char('k') | Key::Up => Some(Action::Prev),
        Key::Char('a') | Key::Enter => Some(Action::Approve),
        Key::Char('x') | Key::Char('d') => Some(Action::Reject),
        Key::Char('?') | Key::Char('h') => Some(Action::Help),
        Key::Char('p') => Some(Action::Pair),
        _ => None,
    }
}

/* ------------------------------------------------------------------ */
/* Model (pure, testable)                                              */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, PartialEq)]
pub struct SessRow {
    pub id: String,
    pub title: String,
    pub status: String,
    /// ADR-009: belongs to a simulated agent/host (rendered with `[sim]`).
    pub sim: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermRow {
    pub id: String,
    pub revision: i64,
    pub title: String,
    pub command: String,
    pub risk: String,
    pub sim: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnState {
    #[default]
    Live,
    Reconnecting {
        attempt: u32,
    },
}

#[derive(Debug, Default)]
pub struct TuiState {
    pub host_line: String,
    pub sessions: Vec<SessRow>,
    pub approvals: Vec<PermRow>,
    pub selected: usize,
    pub msg: String,
    pub help: bool,
    pub quit: bool,
    pub conn_state: ConnState,
    /// When set, the screen renders the pairing overlay (payload + QR) instead
    /// of the dashboard; `p`/Esc closes it.
    pub pairing: Option<String>,
    /// Ids the last snapshot marked `simulated` (ADR-009 provenance).
    pub sim_agents: HashSet<String>,
    pub sim_hosts: HashSet<String>,
}

impl TuiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mandate §8 (audit P8/P6): truthful reconnecting state on socket disconnect
    pub fn mark_disconnected(&mut self, attempt: u32) {
        self.conn_state = ConnState::Reconnecting { attempt };
        self.msg = format!("disconnected from companion — reconnecting (attempt {attempt})");
    }

    /// Mandate §8: normal state restored on reconnect
    pub fn mark_reconnected(&mut self) {
        self.conn_state = ConnState::Live;
        self.msg = String::new();
    }

    fn clamp(&mut self) {
        if self.approvals.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.approvals.len() {
            self.selected = self.approvals.len() - 1;
        }
    }

    pub fn select_next(&mut self) {
        if !self.approvals.is_empty() {
            self.selected = (self.selected + 1) % self.approvals.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.approvals.is_empty() {
            self.selected = if self.selected == 0 {
                self.approvals.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn selected_approval(&self) -> Option<&PermRow> {
        self.approvals.get(self.selected)
    }

    /// True when a session/permission belongs to an agent or host the last
    /// snapshot marked `simulated` (demo data, not a live runtime).
    fn is_sim(&self, v: &Json) -> bool {
        json::get_str(v, "agentId").is_some_and(|id| self.sim_agents.contains(id))
            || json::get_str(v, "hostId").is_some_and(|id| self.sim_hosts.contains(id))
    }

    /// Rebuilds the model from a full `state`/`hello_ack` snapshot. Only
    /// `pending` permissions become actionable rows.
    pub fn apply_snapshot(&mut self, snap: &Json) {
        let simulated_ids = |key: &str| -> HashSet<String> {
            json::get(snap, key)
                .and_then(json::as_arr)
                .map(|arr| {
                    arr.iter()
                        .filter(|e| json::get_bool(e, "simulated") == Some(true))
                        .filter_map(|e| json::get_str(e, "id").map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        self.sim_hosts = simulated_ids("hosts");
        self.sim_agents = simulated_ids("agents");
        self.sessions = json::get(snap, "sessions")
            .and_then(json::as_arr)
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        Some(SessRow {
                            id: json::get_str(s, "id")?.to_string(),
                            title: json::get_str(s, "title").unwrap_or("").to_string(),
                            status: json::get_str(s, "status").unwrap_or("").to_string(),
                            sim: self.is_sim(s),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.approvals = json::get(snap, "permissions")
            .and_then(json::as_arr)
            .map(|arr| {
                arr.iter()
                    .filter(|p| json::get_str(p, "status") == Some("pending"))
                    .filter_map(|p| perm_from_json(p, self.is_sim(p)))
                    .collect()
            })
            .unwrap_or_default();
        self.clamp();
    }

    /// Upserts a single session from a `session_created`/`session_updated` event.
    pub fn upsert_session(&mut self, s: &Json) {
        let Some(id) = json::get_str(s, "id") else {
            return;
        };
        let title = json::get_str(s, "title").unwrap_or("");
        let status = json::get_str(s, "status").unwrap_or("");
        let sim = self.is_sim(s);
        if let Some(row) = self.sessions.iter_mut().find(|r| r.id == id) {
            row.title = title.to_string();
            row.status = status.to_string();
            row.sim = sim;
        } else {
            self.sessions.push(SessRow {
                id: id.to_string(),
                title: title.to_string(),
                status: status.to_string(),
                sim,
            });
        }
    }

    /// Applies a `session_output` status change (the event carries no title).
    pub fn update_session_status(&mut self, id: &str, status: &str) {
        if let Some(row) = self.sessions.iter_mut().find(|r| r.id == id) {
            row.status = status.to_string();
        }
    }

    /// Upserts a `permission_requested`; a non-pending permission is dropped.
    pub fn upsert_permission(&mut self, p: &Json) {
        let Some(row) = perm_from_json(p, self.is_sim(p)) else {
            return;
        };
        if json::get_str(p, "status") != Some("pending") {
            self.remove_permission(&row.id);
            return;
        }
        if let Some(existing) = self.approvals.iter_mut().find(|r| r.id == row.id) {
            *existing = row;
        } else {
            self.approvals.push(row);
        }
        self.clamp();
    }

    /// Drops a resolved/expired permission from the actionable list.
    pub fn remove_permission(&mut self, id: &str) {
        self.approvals.retain(|r| r.id != id);
        self.clamp();
    }
}

fn perm_from_json(p: &Json, sim: bool) -> Option<PermRow> {
    Some(PermRow {
        id: json::get_str(p, "id")?.to_string(),
        revision: json::get_i64(p, "revision").unwrap_or(1),
        title: json::get_str(p, "title").unwrap_or("").to_string(),
        command: json::get_str(p, "command").unwrap_or("").to_string(),
        risk: json::get_str(p, "risk").unwrap_or("").to_string(),
        sim,
    })
}

/// ADR-009: demo rows never read as live work.
fn sim_tag(sim: bool) -> &'static str {
    if sim { "[sim] " } else { "" }
}

/* ------------------------------------------------------------------ */
/* Rendering (pure ANSI string)                                        */
/* ------------------------------------------------------------------ */

fn risk_ansi(risk: &str) -> &'static str {
    match risk {
        "high" => "\x1b[31m",
        "medium" => "\x1b[33m",
        "low" => "\x1b[32m",
        _ => "\x1b[37m",
    }
}

fn status_ansi(status: &str) -> &'static str {
    match status {
        "running" | "active" | "approved" => "\x1b[32m",
        "failed" | "error" | "rejected" => "\x1b[31m",
        _ => "\x1b[37m",
    }
}

/// Renders the whole screen as one ANSI string (cursor-home + clear, then the
/// status line, sessions, pending approvals with the selection highlighted, and
/// the keybind footer). With a pairing overlay active, renders that instead.
/// Pure: same state → same string.
pub fn render(state: &TuiState) -> String {
    let mut out = String::new();
    // home + clear screen
    out.push_str("\x1b[H\x1b[2J");
    out.push_str(&format!(
        "\x1b[1mCHERNOBYL companion\x1b[0m  {}\r\n",
        state.host_line
    ));
    if let Some(block) = &state.pairing {
        out.push_str("\r\n\x1b[36mPairing — scan with the phone\x1b[0m\r\n\r\n");
        for line in block.split('\n') {
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str("\r\n\x1b[2mp/esc close  q quit\x1b[0m\r\n");
        return out;
    }
    if !state.msg.is_empty() {
        out.push_str(&format!("\x1b[33m{}\x1b[0m\r\n", state.msg));
    }
    out.push_str(&format!(
        "\x1b[36m── Sessions ({}) ──\x1b[0m\r\n",
        state.sessions.len()
    ));
    if state.sessions.is_empty() {
        out.push_str("  (none)\r\n");
    }
    for s in &state.sessions {
        out.push_str(&format!(
            "  \u{2022} {}{} [\x1b[0m{}{}{}\x1b[0m]\r\n",
            sim_tag(s.sim),
            s.title,
            status_ansi(&s.status),
            s.status,
            ""
        ));
    }
    out.push_str(&format!(
        "\x1b[36m── Approvals ({} pending) ──\x1b[0m\r\n",
        state.approvals.len()
    ));
    if state.approvals.is_empty() {
        out.push_str("  (none pending)\r\n");
    }
    for (i, a) in state.approvals.iter().enumerate() {
        let selected = i == state.selected;
        let marker = if selected { "\x1b[7m>" } else { " " };
        let line = format!(
            "{} [{}{}{}] {}{}  $ {}",
            marker,
            risk_ansi(&a.risk),
            a.risk,
            "\x1b[0m",
            sim_tag(a.sim),
            a.title,
            a.command
        );
        if selected {
            out.push_str(&format!("{line}\x1b[0m\r\n"));
        } else {
            out.push_str(&format!("{line}\r\n"));
        }
    }
    out.push_str(
        "\x1b[2mj/k move  a/enter approve  x reject  r refresh  p pair  ? help  q quit\x1b[0m\r\n",
    );
    if state.help {
        out.push_str(
            "\x1b[36mhelp\x1b[0m approve = approve-once (this request only); the companion\r\n\
             \x1b[36m    \x1b[0m enforces the decision and streams the result back here.\r\n",
        );
    }
    out
}

/* ------------------------------------------------------------------ */
/* Interactive shell (thin wiring to socket + console)                 */
/* ------------------------------------------------------------------ */

fn send(client: &Arc<Mutex<WsClient>>, text: &str) -> Result<(), String> {
    client.lock().unwrap().send_text(text)
}

fn next_cid(seq: &mut u64) -> String {
    *seq += 1;
    format!("tui-{seq}")
}

/// Applies one inbound server message to the model. Returns true when the
/// visible state changed (so the caller re-renders). Unknown/irrelevant frames
/// (heartbeats, command progress) return false.
fn apply_inbound(state: &mut TuiState, text: &str) -> bool {
    let Ok(v) = json::parse(text) else {
        return false;
    };
    let Some(typ) = json::get_str(&v, "type") else {
        return false;
    };
    match typ {
        "state" | "hello_ack" => match json::get(&v, "snapshot") {
            Some(snap) => {
                state.apply_snapshot(snap);
                true
            }
            None => false,
        },
        "session_created" | "session_updated" => match json::get(&v, "session") {
            Some(s) => {
                state.upsert_session(s);
                true
            }
            None => false,
        },
        "session_output" => {
            if let Some(sid) = json::get_str(&v, "sessionId") {
                let status = json::get_str(&v, "status").unwrap_or("");
                state.update_session_status(sid, status);
                true
            } else {
                false
            }
        }
        "permission_requested" => match json::get(&v, "permission") {
            Some(p) => {
                state.upsert_permission(p);
                true
            }
            None => false,
        },
        "permission_resolved" => {
            match json::get(&v, "permission").and_then(|p| json::get_str(p, "id")) {
                Some(id) => {
                    state.remove_permission(id);
                    true
                }
                None => false,
            }
        }
        // surface a refused command (including the remote-posture E2E refusal)
        // verbatim — never pretend a decision was applied
        "ack" if json::get_bool(&v, "ok") == Some(false) => {
            state.msg = json::get_str(&v, "error")
                .unwrap_or("command refused")
                .to_string();
            true
        }
        _ => false,
    }
}

fn handle_action(
    state: &mut TuiState,
    action: Action,
    client: &Arc<Mutex<WsClient>>,
    seq: &mut u64,
    pairer: &mut dyn FnMut() -> Result<String, String>,
) {
    let res: Result<(), String> = match action {
        Action::Quit => {
            state.quit = true;
            Ok(())
        }
        Action::Pair => {
            // toggle: `p` opens the pairing overlay, `p`/Esc closes it (the
            // close path is intercepted in the event loop before dispatch)
            match pairer() {
                Ok(block) => {
                    state.pairing = Some(block);
                    Ok(())
                }
                Err(e) => Err(format!("pair failed: {e}")),
            }
        }
        Action::Refresh => send(
            client,
            &format!(
                "{{\"type\":\"list_state\",\"commandId\":\"{}\"}}",
                next_cid(seq)
            ),
        ),
        Action::Next => {
            state.select_next();
            Ok(())
        }
        Action::Prev => {
            state.select_prev();
            Ok(())
        }
        Action::Help => {
            state.help = !state.help;
            Ok(())
        }
        Action::Approve | Action::Reject => {
            let Some(sel) = state.selected_approval() else {
                state.msg = "no pending request selected".into();
                return;
            };
            let decision = if action == Action::Approve {
                "approve-once"
            } else {
                "reject"
            };
            let msg = format!(
                "{{\"type\":\"permission_decision\",\"commandId\":\"{}\",\"permissionId\":\"{}\",\"decision\":\"{}\",\"revision\":{}}}",
                next_cid(seq),
                sel.id,
                decision,
                sel.revision
            );
            let id = sel.id.clone();
            match send(client, &msg) {
                Ok(()) => {
                    state.msg = format!("{decision} sent for {id}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(e) = res {
        state.msg = format!("send failed: {e}");
    }
}

fn spawn_keys(tx: mpsc::Sender<Key>) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 16];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for k in decode_keys(&buf[..n]) {
                        if tx.send(k).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Runs the interactive TUI against a companion at `addr`. Connects, greets the
/// server, then loops: drain inbound events + keystrokes, re-render on change.
/// `pairer` renders a fresh pairing block (payload + QR) for the `p` overlay —
/// supplied by the caller so the TUI never touches storage itself. Returns when
/// the operator quits or a shutdown is requested.
fn spawn_reader(
    client: Arc<Mutex<WsClient>>,
    event_tx: mpsc::Sender<String>,
    reader_stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !reader_stop.load(Ordering::SeqCst) {
            let item = {
                let mut c = match client.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                c.next_text_timeoutable()
            };
            match item {
                Ok(Some(text)) => {
                    if event_tx.send(text).is_err() {
                        break;
                    }
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => {
                    let _ = event_tx.send("\u{0}disconnect".into());
                    break;
                }
            }
        }
    })
}

/// Runs the interactive TUI against a companion at `addr`. Connects, greets the
/// server, then loops: drain inbound events + keystrokes, re-render on change.
/// `pairer` renders a fresh pairing block (payload + QR) for the `p` overlay —
/// supplied by the caller so the TUI never touches storage itself. Returns when
/// the operator quits or a shutdown is requested.
pub fn run_tui(
    addr: &str,
    token: Option<&str>,
    mut pairer: impl FnMut() -> Result<String, String>,
) -> Result<(), String> {
    let (event_tx, event_rx) = mpsc::channel::<String>();
    let (key_tx, key_rx) = mpsc::channel::<Key>();

    // raw console input; if stdin is not a console, fall back to line input
    let _raw_guard = crate::ffi::enable_raw_console_input().ok();
    spawn_keys(key_tx);

    let mut state = TuiState::new();
    state.host_line = addr.to_string();

    let mut client: Option<Arc<Mutex<WsClient>>> = None;
    let mut reader: Option<std::thread::JoinHandle<()>> = None;
    let mut reader_stop: Option<Arc<AtomicBool>> = None;
    let mut connected = false;
    let mut attempt = 0u32;
    let mut backoff = Duration::from_millis(250);
    let mut next_retry = std::time::Instant::now();
    let mut seq: u64 = 0;

    // Initial connection attempt (audit P8/P6)
    match WsClient::connect(addr, "/", token).and_then(|mut c| {
        c.hello()?;
        c.set_read_timeout_ms(30)?;
        Ok(c)
    }) {
        Ok(c) => {
            let cl = Arc::new(Mutex::new(c));
            let stop = Arc::new(AtomicBool::new(false));
            let r = spawn_reader(cl.clone(), event_tx.clone(), stop.clone());
            let _ = send(
                &cl,
                &format!(
                    "{{\"type\":\"list_state\",\"commandId\":\"{}\"}}",
                    next_cid(&mut seq)
                ),
            );
            client = Some(cl);
            reader = Some(r);
            reader_stop = Some(stop);
            connected = true;
        }
        Err(_) => {
            attempt = 1;
            state.mark_disconnected(attempt);
            next_retry = std::time::Instant::now() + backoff;
            backoff = (backoff * 2).min(Duration::from_millis(5000));
        }
    }

    let mut last_frame = String::new();
    while !state.quit && !crate::ffi::shutdown_requested() {
        let mut changed = false;

        // Drain incoming messages
        while let Ok(text) = event_rx.try_recv() {
            if text == "\u{0}disconnect" {
                if connected {
                    connected = false;
                    attempt = 1;
                    state.mark_disconnected(attempt);
                    changed = true;
                    if let Some(stop) = reader_stop.take() {
                        stop.store(true, Ordering::SeqCst);
                    }
                    if let Some(r) = reader.take() {
                        let _ = r.join();
                    }
                    client = None;
                    backoff = Duration::from_millis(250);
                    next_retry = std::time::Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_millis(5000));
                }
                continue;
            }
            if apply_inbound(&mut state, &text) {
                changed = true;
            }
        }

        // Reconnect with bounded backoff if disconnected (audit P8/P6)
        if !connected && std::time::Instant::now() >= next_retry {
            match WsClient::connect(addr, "/", token).and_then(|mut c| {
                c.hello()?;
                c.set_read_timeout_ms(30)?;
                Ok(c)
            }) {
                Ok(c) => {
                    let cl = Arc::new(Mutex::new(c));
                    let stop = Arc::new(AtomicBool::new(false));
                    let r = spawn_reader(cl.clone(), event_tx.clone(), stop.clone());
                    let _ = send(
                        &cl,
                        &format!(
                            "{{\"type\":\"list_state\",\"commandId\":\"{}\"}}",
                            next_cid(&mut seq)
                        ),
                    );
                    client = Some(cl);
                    reader = Some(r);
                    reader_stop = Some(stop);
                    connected = true;
                    attempt = 0;
                    backoff = Duration::from_millis(250);
                    state.mark_reconnected();
                    changed = true;
                }
                Err(_) => {
                    attempt += 1;
                    state.mark_disconnected(attempt);
                    changed = true;
                    next_retry = std::time::Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_millis(5000));
                }
            }
        }

        // Process operator keys
        while let Ok(key) = key_rx.try_recv() {
            // While the pairing overlay is up only p/Esc (close) and q (quit) act
            if state.pairing.is_some() {
                match key {
                    Key::Esc | Key::Char('p') => {
                        state.pairing = None;
                        changed = true;
                    }
                    Key::Char('q') => {
                        state.quit = true;
                        break;
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(action) = key_to_action(key) {
                if action == Action::Quit {
                    state.quit = true;
                    break;
                }
                if let Some(ref cl) = client {
                    handle_action(&mut state, action, cl, &mut seq, &mut pairer);
                    changed = true;
                } else {
                    // Actions while disconnected
                    match action {
                        Action::Pair => match pairer() {
                            Ok(block) => {
                                state.pairing = Some(block);
                                changed = true;
                            }
                            Err(e) => {
                                state.msg = format!("pair failed: {e}");
                                changed = true;
                            }
                        },
                        Action::Next => {
                            state.select_next();
                            changed = true;
                        }
                        Action::Prev => {
                            state.select_prev();
                            changed = true;
                        }
                        Action::Help => {
                            state.help = !state.help;
                            changed = true;
                        }
                        _ => {
                            state.msg = format!(
                                "disconnected from companion — reconnecting (attempt {attempt})"
                            );
                            changed = true;
                        }
                    }
                }
            }
            if state.quit {
                break;
            }
        }

        if changed {
            let frame = render(&state);
            if frame != last_frame {
                print!("{frame}");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                last_frame = frame;
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    if let Some(stop) = reader_stop {
        stop.store(true, Ordering::SeqCst);
    }
    if let Some(cl) = client {
        let _ = cl.lock().unwrap().close(1000);
    }
    // leave the cursor on a fresh line after the last frame
    println!();
    if let Some(r) = reader {
        let _ = r.join();
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_keys_maps_arrows_chars_and_controls() {
        assert_eq!(decode_key(b"j"), Some(Key::Char('j')));
        assert_eq!(decode_key(b"\x1b[A"), Some(Key::Up));
        assert_eq!(decode_key(b"\x1b[B"), Some(Key::Down));
        assert_eq!(decode_key(b"\x1b[D"), Some(Key::Left));
        assert_eq!(decode_key(b"\r"), Some(Key::Enter));
        assert_eq!(decode_key(b"\x1b"), Some(Key::Esc));
        // a chunk can carry more than one key
        assert_eq!(decode_keys(b"ab"), vec![Key::Char('a'), Key::Char('b')]);
    }

    #[test]
    fn keybinds_map_to_actions() {
        assert_eq!(key_to_action(Key::Char('q')), Some(Action::Quit));
        assert_eq!(key_to_action(Key::Esc), Some(Action::Quit));
        assert_eq!(key_to_action(Key::Char('j')), Some(Action::Next));
        assert_eq!(key_to_action(Key::Down), Some(Action::Next));
        assert_eq!(key_to_action(Key::Char('k')), Some(Action::Prev));
        assert_eq!(key_to_action(Key::Char('a')), Some(Action::Approve));
        assert_eq!(key_to_action(Key::Enter), Some(Action::Approve));
        assert_eq!(key_to_action(Key::Char('x')), Some(Action::Reject));
        assert_eq!(key_to_action(Key::Char('r')), Some(Action::Refresh));
        assert_eq!(key_to_action(Key::Char('?')), Some(Action::Help));
        assert_eq!(key_to_action(Key::Char('p')), Some(Action::Pair));
        assert_eq!(key_to_action(Key::Other), None);
    }

    #[test]
    fn pairing_overlay_renders_block_and_hides_dashboard() {
        let mut s = TuiState::new();
        s.host_line = "127.0.0.1:8787".into();
        s.apply_snapshot(&snapshot_json());
        s.pairing = Some("payload: {\"hostId\":\"h\"}\n[QR]\ndevice d1 — valid 300s".into());
        let frame = render(&s);
        assert!(frame.contains("Pairing"));
        assert!(frame.contains("payload: {\"hostId\":\"h\"}"));
        assert!(frame.contains("p/esc close"));
        // the dashboard is replaced while the overlay is up
        assert!(!frame.contains("Refactor auth"));
        assert!(!frame.contains("cargo build"));
        // closing (None) returns to the dashboard
        s.pairing = None;
        let back = render(&s);
        assert!(back.contains("Refactor auth"));
        assert!(!back.contains("p/esc close"));
    }

    fn snapshot_json() -> Json {
        json::parse(
            r#"{
              "sessions": [
                {"id":"s1","title":"Refactor auth","status":"running"},
                {"id":"s2","title":"Write docs","status":"done"}
              ],
              "permissions": [
                {"id":"p1","revision":1,"title":"Run build","command":"cargo build","risk":"high","status":"pending"},
                {"id":"p2","revision":3,"title":"Write file","command":"cat > f","risk":"low","status":"pending"},
                {"id":"p3","revision":1,"title":"Old","command":"rm","risk":"high","status":"approved"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn render_marks_sessions_and_approvals_from_simulated_sources() {
        let mut s = TuiState::new();
        s.apply_snapshot(
            &json::parse(
                r#"{
                  "hosts": [{"id":"h1","simulated":true},{"id":"oc-1"}],
                  "agents": [
                    {"id":"a1","hostId":"h1","simulated":true},
                    {"id":"build","hostId":"oc-1"}
                  ],
                  "sessions": [
                    {"id":"s1","agentId":"a1","hostId":"h1","title":"Demo work","status":"running"},
                    {"id":"ses_1","agentId":"build","hostId":"oc-1","title":"Real work","status":"running"}
                  ],
                  "permissions": [
                    {"id":"p1","agentId":"a1","hostId":"h1","revision":1,"title":"Demo ask","command":"npm i","risk":"low","status":"pending"},
                    {"id":"p2","agentId":"build","hostId":"oc-1","revision":1,"title":"Real ask","command":"ls","risk":"low","status":"pending"}
                  ]
                }"#,
            )
            .unwrap(),
        );
        let frame = render(&s);
        assert!(frame.contains("[sim] Demo work"));
        assert!(frame.contains("[sim] Demo ask"));
        // real-runtime rows stay unmarked
        assert!(frame.contains("\u{2022} Real work"));
        assert!(frame.contains("] Real ask"));
        assert_eq!(frame.matches("[sim]").count(), 2);
        // a live event for a simulated agent keeps the marker
        s.upsert_session(
            &json::parse(
                r#"{"id":"s9","agentId":"a1","hostId":"h1","title":"New demo","status":"running"}"#,
            )
            .unwrap(),
        );
        assert!(render(&s).contains("[sim] New demo"));
    }

    #[test]
    fn apply_snapshot_lists_sessions_and_only_pending_approvals() {
        let mut s = TuiState::new();
        s.apply_snapshot(&snapshot_json());
        assert_eq!(s.sessions.len(), 2);
        // p3 is approved → not actionable
        assert_eq!(s.approvals.len(), 2);
        assert_eq!(s.approvals[0].id, "p1");
        assert_eq!(s.approvals[0].revision, 1);
        assert_eq!(s.approvals[1].risk, "low");
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn permission_events_upsert_and_remove() {
        let mut s = TuiState::new();
        s.apply_snapshot(&snapshot_json());
        // a new pending request appears live
        s.upsert_permission(
            &json::parse(
                r#"{"id":"p9","revision":1,"title":"Net","command":"curl","risk":"medium","status":"pending"}"#,
            )
            .unwrap(),
        );
        assert_eq!(s.approvals.len(), 3);
        // resolving it removes it
        s.remove_permission("p9");
        assert_eq!(s.approvals.len(), 2);
        // a resolved event for a listed request also removes it
        s.upsert_permission(
            &json::parse(r#"{"id":"p1","revision":2,"status":"rejected"}"#).unwrap(),
        );
        assert_eq!(s.approvals.len(), 1);
        assert_eq!(s.approvals[0].id, "p2");
    }

    #[test]
    fn selection_wraps_and_clamps() {
        let mut s = TuiState::new();
        s.apply_snapshot(&snapshot_json());
        s.select_prev(); // wraps to last
        assert_eq!(s.selected, 1);
        s.select_next(); // wraps back to first
        assert_eq!(s.selected, 0);
        s.remove_permission("p1");
        s.remove_permission("p2"); // empty list clamps to 0
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn render_shows_sessions_approvals_selection_and_keybinds() {
        let mut s = TuiState::new();
        s.host_line = "127.0.0.1:8787".into();
        s.apply_snapshot(&snapshot_json());
        let frame = render(&s);
        assert!(frame.contains("CHERNOBYL companion"));
        assert!(frame.contains("127.0.0.1:8787"));
        assert!(frame.contains("Refactor auth"));
        assert!(frame.contains("cargo build"));
        // the selected row is reverse-video marked
        assert!(frame.contains("\x1b[7m>"));
        // keybind footer present
        assert!(frame.contains("a/enter approve"));
        assert!(frame.contains("q quit"));
        // approvals that are not pending are not rendered as rows
        assert!(!frame.contains("  $ rm"));
        // screen is cleared + homed
        assert!(frame.starts_with("\x1b[H\x1b[2J"));
    }

    #[test]
    fn apply_inbound_dispatches_by_type() {
        let mut s = TuiState::new();
        // a state frame seeds the model
        let state_frame = json::stringify(&json::obj(vec![("type", json::s("state"))]));
        // (snapshot-less state → no change)
        assert!(!apply_inbound(&mut s, &state_frame));
        // a refused ack surfaces its error text
        let refused = r#"{"type":"ack","commandId":"tui-1","ok":false,"error":"e2e session required (remote posture)"}"#;
        assert!(apply_inbound(&mut s, refused));
        assert!(s.msg.contains("e2e session required"));
        // a permission_requested event adds a row
        let req = r#"{"type":"permission_requested","permission":{"id":"p5","revision":4,"title":"T","command":"ls","risk":"low","status":"pending"}}"#;
        assert!(apply_inbound(&mut s, req));
        assert_eq!(s.approvals.len(), 1);
        // garbage is ignored without a panic or a change
        assert!(!apply_inbound(&mut s, "not json"));
    }

    #[test]
    fn tui_reconnect_lifecycle_and_quit_while_disconnected() {
        // Regression for P8/P6: pure state transitions for disconnect, reconnect,
        // and quitting while disconnected.
        let mut state = TuiState::new();
        assert_eq!(state.conn_state, ConnState::Live);
        assert!(!state.quit);

        // 1. disconnect -> reconnecting state (attempt 1)
        state.mark_disconnected(1);
        assert_eq!(state.conn_state, ConnState::Reconnecting { attempt: 1 });
        assert_eq!(
            state.msg,
            "disconnected from companion — reconnecting (attempt 1)"
        );

        // Subsequent attempt
        state.mark_disconnected(2);
        assert_eq!(state.conn_state, ConnState::Reconnecting { attempt: 2 });
        assert_eq!(
            state.msg,
            "disconnected from companion — reconnecting (attempt 2)"
        );

        // 2. reconnect -> live
        state.mark_reconnected();
        assert_eq!(state.conn_state, ConnState::Live);
        assert_eq!(state.msg, "");

        // 3. quit while disconnected -> exits
        state.mark_disconnected(3);
        assert!(!state.quit);
        let action = key_to_action(Key::Char('q'));
        assert_eq!(action, Some(Action::Quit));
        if action == Some(Action::Quit) {
            state.quit = true;
        }
        assert!(
            state.quit,
            "quit while disconnected must set state.quit = true"
        );
    }
}
