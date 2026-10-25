//! Durable companion store — crash-safe append-only journal.
//!
//! Format: one JSON record per line (`\n`-terminated). A record is durable
//! only once its newline is written; a torn tail (power loss mid-append) is
//! detected at load (invalid JSON / missing terminator) and truncated.
//!
//! PRD §16.2 mandates SQLite; the Rust companion uses this journal instead
//! (conflict register C15: zero external modules → no C toolchain, no C code
//! in-process). Logical tables map 1:1: commands, events, approvals, audit,
//! config KV, adapter status, schema meta. Same durability guarantees:
//! persist-before-execute, idempotency by commandId, boot reconciliation,
//! latched fail-closed on write failure.

use crate::json::{self, Json};
use crate::protocol::now_ms;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;

/// Once this many bytes were appended since the last compaction (or boot),
/// the journal is rewritten down to the live set — bounds file growth inside
/// one long-running process instead of only across restarts.
const JOURNAL_COMPACT_TRIGGER_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct CommandRecord {
    pub command_id: String,
    pub kind: String,
    pub payload: String, // raw client message JSON
    pub status: String,  // unresolved | executed | refused | unknown | ...
    pub created_at: i64,
    pub updated_at: i64,
    /// The original ack's `ok` once recorded; `None` = in flight. Replays
    /// must return this verbatim (the Node host replays its engine-cached
    /// ack exactly — refusals replay as refusals, never as success).
    pub ok: Option<bool>,
    pub session_id: Option<String>,
    pub permission_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    pub seq: i64,
    pub kind: String,
    pub payload: String, // full ServerMessage JSON (already enveloped)
    pub occurred_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRecord {
    pub permission_id: String,
    pub session_id: String,
    pub native_request_id: String,
    pub status: String, // pending | approved | rejected | stale | expired
    pub revision: i64,
    pub requested_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    pub ts: i64,
    pub kind: String,
    pub text: String,
}

/// A pending pairing offer (ADR-011): single-use, short-TTL. The PSK is stored
/// DPAPI-protected (base64) so the on-disk journal never holds a plaintext key.
#[derive(Debug, Clone, PartialEq)]
pub struct PairingRecord {
    pub device_id: String,
    pub psk_protected: String, // base64 of DPAPI-protected 32-byte PSK
    pub challenge: String,     // base64 of the 16-byte challenge
    pub created_at: i64,
    pub expires_at: i64,
    pub consumed: bool,
}

/// A paired device's registry entry (ADR-002/011).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRecord {
    pub device_id: String,
    pub name: String,
    pub psk_protected: String, // base64 of DPAPI-protected 32-byte PSK
    pub paired_at: i64,
    pub revoked: bool,
}

pub struct Storage {
    dir: PathBuf,
    journal: File,
    pub commands: HashMap<String, CommandRecord>,
    pub events: Vec<EventRecord>,
    pub approvals: HashMap<String, ApprovalRecord>,
    pub audit: Vec<AuditRecord>,
    pub kv: HashMap<String, String>,
    pub pairings: HashMap<String, PairingRecord>,
    pub devices: HashMap<String, DeviceRecord>,
    pub last_seq: i64,
    /// Retention caps (config `retention.events` / `retention.commands`).
    event_cap: usize,
    command_cap: usize,
    /// Bytes appended since the last compaction (boot: on-disk size), for
    /// `maybe_compact`.
    bytes_since_compact: u64,
    compact_trigger: u64,
    /// Latched after any write failure: the store refuses further writes and
    /// the companion fails closed (PRD §19.4).
    failed: bool,
    /// Latched count of mid-file corruption events seen at boot (0 = clean).
    /// A torn tail does NOT count: nothing follows it, so no record is lost
    /// (the documented ADR-010 policy). Mandate §5: corruption is never
    /// silent when records are actually discarded.
    pub corrupt_events: u64,
    /// Details of the corruption event(s) for `/health` consumers and the
    /// audit record `open` writes after truncation.
    pub corrupt: Option<CorruptLoss>,
    #[cfg(test)]
    pub fail_after_appends: Option<usize>,
    #[cfg(test)]
    pub appends_count: usize,
}

/// Where the journal was corrupt and what the truncation discarded
/// (mandate §5): byte offset of the corrupt line, its 1-based line number,
/// and how many bytes — including the corrupt line itself — were dropped.
#[derive(Clone, Copy, Debug)]
pub struct CorruptLoss {
    pub line: u64,
    pub offset: u64,
    pub discarded_bytes: u64,
}

fn record_line(kind: &str, body: &Json) -> String {
    let mut line = String::with_capacity(128);
    line.push_str("{\"kind\":");
    json::write_str(&mut line, kind);
    line.push(',');
    // body is an object: splice its fields after "kind"
    match body {
        Json::Obj(pairs) => {
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    line.push(',');
                }
                json::write_str(&mut line, k);
                line.push(':');
                json::write(&mut line, v);
            }
        }
        other => json::write(&mut line, other),
    }
    line.push_str("}\n");
    line
}

impl Storage {
    /// Opens (creating if absent) the journal at `<dir>/companion.journal`.
    pub fn open(dir: &Path, event_cap: usize, command_cap: usize) -> Result<Storage, String> {
        fs::create_dir_all(dir).map_err(|e| format!("create data dir: {e}"))?;
        let path = dir.join("companion.journal");

        let mut store = Storage {
            dir: dir.to_path_buf(),
            journal: OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("open journal: {e}"))?,
            commands: HashMap::new(),
            events: Vec::new(),
            approvals: HashMap::new(),
            audit: Vec::new(),
            kv: HashMap::new(),
            pairings: HashMap::new(),
            devices: HashMap::new(),
            last_seq: 0,
            event_cap,
            command_cap,
            bytes_since_compact: 0,
            compact_trigger: JOURNAL_COMPACT_TRIGGER_BYTES,
            failed: false,
            corrupt_events: 0,
            corrupt: None,
            #[cfg(test)]
            fail_after_appends: None,
            #[cfg(test)]
            appends_count: 0,
        };

        // Load, then truncate any torn tail so future appends stay aligned.
        let valid_bytes = store.load(&path)?;
        let file_len = fs::metadata(&path)
            .map_err(|e| format!("stat journal: {e}"))?
            .len();
        if valid_bytes < file_len {
            // torn tail from an unclean shutdown — truncate to last full record
            let f = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|e| format!("reopen for truncate: {e}"))?;
            f.set_len(valid_bytes)
                .map_err(|e| format!("truncate torn tail: {e}"))?;
        }

        // Mid-file corruption (mandate §5): `load` logged it to stderr and
        // latched it; now that the file is truncated, record the loss in the
        // audit log so the discarding is visible in the durable trail too.
        if let Some(loss) = store.corrupt {
            store.append_audit(
                "storage",
                &format!(
                    "journal corrupt at line {}, byte offset {}: {} bytes discarded (truncated per ADR-010)",
                    loss.line, loss.offset, loss.discarded_bytes
                ),
            )?;
        }

        // first compaction trigger must consider the whole file, not only
        // writes made in this process (a journal grown for weeks trips it
        // on the first appends after boot)
        store.bytes_since_compact = valid_bytes;

        // schema marker
        if !store.kv.contains_key("schema") {
            store.put_kv("schema", &SCHEMA_VERSION.to_string())?;
        }

        // retention: prune records over their caps, then compact so dropped
        // records cannot be resurrected by the next journal replay
        let mut pruned = false;
        if store.events.len() > event_cap {
            let drop_n = store.events.len() - event_cap;
            store.events.drain(..drop_n);
            pruned = true;
        }
        if store.enforce_command_cap() {
            pruned = true;
        }
        if pruned {
            store.compact()?;
            store.bytes_since_compact = 0;
        }

        Ok(store)
    }

    /// Evicts the oldest completed command records while over `command_cap`;
    /// returns whether anything was evicted. `unresolved` records are never
    /// evicted: boot reconciliation and verbatim replay must find them.
    /// shortcut: if unresolved alone reaches the cap, completed records yield
    /// entirely — unresolved is bounded by in-flight work in practice, and
    /// becomes evictable once boot reconciliation marks it `unknown`.
    fn enforce_command_cap(&mut self) -> bool {
        let mut evicted = false;
        // evict in batches: one scan selects the oldest `k` completed records
        // instead of rescanning the whole map per eviction. At cap 5 000 a
        // per-eviction scan cost ~100 µs/command and turned the 19k/s
        // saturation bench into 6.5k/s; one scan per ~78 commands is ~1 µs.
        // Small caps (tests) keep exact cap+1 behavior: batch = 1.
        let batch = (self.command_cap / 64).max(1);
        while self.commands.len() > self.command_cap {
            let want = self.commands.len() - self.command_cap;
            let mut completed: Vec<(i64, &String)> = self
                .commands
                .iter()
                .filter(|(_, c)| c.status != "unresolved")
                .map(|(id, c)| (c.created_at, id))
                .collect();
            if completed.is_empty() {
                return evicted; // all remaining are in flight — over cap is legal
            }
            let k = want.max(batch).min(completed.len());
            completed.select_nth_unstable_by(k - 1, |a, b| a.cmp(b));
            let victims: Vec<String> = completed[..k].iter().map(|(_, id)| (*id).clone()).collect();
            drop(completed); // borrows end — removal can start
            for id in &victims {
                self.commands.remove(id);
            }
            evicted = true;
        }
        evicted
    }

    /// Bounds journal growth inside one process: past `compact_trigger` bytes
    /// of appends, rewrite the journal down to the live set. Must only run
    /// where the in-memory maps already match the journal (after inserts), or
    /// a rewrite would drop a record the map still holds. Failure is ignored
    /// on purpose: the preceding append succeeded, so the record is durable —
    /// shortcut: a persistently failing compact leaves the journal growing
    /// until a successful retry or restart, without ever losing data.
    fn maybe_compact(&mut self) {
        if self.bytes_since_compact >= self.compact_trigger && self.compact().is_ok() {
            self.bytes_since_compact = 0;
        }
    }

    fn load(&mut self, path: &Path) -> Result<u64, String> {
        let file = File::open(path).map_err(|e| format!("read journal: {e}"))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut valid_bytes = 0u64;
        let mut line_no = 0u64;
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .map_err(|e| format!("read journal: {e}"))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let Some(text) = line.strip_suffix('\n') else {
                // torn tail: last line has no newline terminator
                break;
            };
            match json::parse(text) {
                Ok(rec) => {
                    self.apply_record(&rec);
                    valid_bytes += n as u64;
                }
                Err(_) => {
                    // Corrupt line. If nothing follows it, this is the torn
                    // tail ADR-010 documents (no record lost — stay quiet).
                    // Valid bytes AFTER it mean real loss, which mandate §5
                    // forbids hiding: log offset + line to stderr, latch the
                    // counter, and `open` writes the audit record once the
                    // truncation below has happened. The truncate-and-continue
                    // policy itself is unchanged.
                    let mut rest = Vec::new();
                    // read error while probing: assume data was lost (visible)
                    let trailing = reader.read_to_end(&mut rest).map_or(true, |b| b > 0);
                    if trailing {
                        let discarded = n as u64 + rest.len() as u64;
                        self.corrupt_events += 1;
                        self.corrupt = Some(CorruptLoss {
                            line: line_no,
                            offset: valid_bytes,
                            discarded_bytes: discarded,
                        });
                        eprintln!(
                            "[companion] journal corrupt at line {line_no}, byte offset {valid_bytes}: {discarded} bytes discarded (records after the corrupt line are lost; truncating per ADR-010)"
                        );
                    }
                    break;
                }
            }
        }
        Ok(valid_bytes)
    }

    fn apply_record(&mut self, rec: &Json) {
        let Some(kind) = json::as_str(json::get(rec, "kind").unwrap_or(&Json::Null)) else {
            return;
        };
        let g = |k: &str| json::get(rec, k).cloned().unwrap_or(Json::Null);
        let gs = |k: &str| json::as_str(&g(k)).map(str::to_string);
        let gi = |k: &str| json::as_i64(&g(k)).unwrap_or(0);
        match kind {
            "command" => {
                let id = gs("id").unwrap_or_default();
                if !id.is_empty() {
                    self.commands.insert(
                        id.clone(),
                        CommandRecord {
                            command_id: id,
                            kind: gs("cmdKind").unwrap_or_default(),
                            payload: gs("payload").unwrap_or_default(),
                            status: gs("status").unwrap_or_else(|| "unresolved".into()),
                            created_at: gi("createdAt"),
                            updated_at: gi("updatedAt"),
                            ok: json::as_bool(&g("ok")),
                            session_id: gs("sessionId"),
                            permission_id: gs("permissionId"),
                            error: gs("error"),
                        },
                    );
                }
            }
            "event" => {
                let seq = gi("seq");
                self.events.push(EventRecord {
                    seq,
                    kind: gs("eventKind").unwrap_or_default(),
                    payload: gs("payload").unwrap_or_default(),
                    occurred_at: gi("occurredAt"),
                });
                if seq > self.last_seq {
                    self.last_seq = seq;
                }
            }
            "approval" => {
                let id = gs("id").unwrap_or_default();
                if !id.is_empty() {
                    self.approvals.insert(
                        id.clone(),
                        ApprovalRecord {
                            permission_id: id,
                            session_id: gs("sessionId").unwrap_or_default(),
                            native_request_id: gs("nativeRequestId").unwrap_or_default(),
                            status: gs("status").unwrap_or_default(),
                            revision: gi("revision"),
                            requested_at: gi("requestedAt"),
                            expires_at: gi("expiresAt"),
                        },
                    );
                }
            }
            "audit" => {
                self.audit.push(AuditRecord {
                    ts: gi("ts"),
                    kind: gs("auditKind").unwrap_or_default(),
                    text: gs("text").unwrap_or_default(),
                });
            }
            "kv" => {
                if let (Some(k), v) = (gs("k"), g("v"))
                    && let Json::Str(v) = v
                {
                    self.kv.insert(k, v);
                }
            }
            "pairing" => {
                let id = gs("deviceId").unwrap_or_default();
                if !id.is_empty() {
                    self.pairings.insert(
                        id.clone(),
                        PairingRecord {
                            device_id: id,
                            psk_protected: gs("psk").unwrap_or_default(),
                            challenge: gs("challenge").unwrap_or_default(),
                            created_at: gi("createdAt"),
                            expires_at: gi("expiresAt"),
                            consumed: json::as_bool(&g("consumed")).unwrap_or(false),
                        },
                    );
                }
            }
            "device" => {
                let id = gs("deviceId").unwrap_or_default();
                if !id.is_empty() {
                    self.devices.insert(
                        id.clone(),
                        DeviceRecord {
                            device_id: id,
                            name: gs("name").unwrap_or_default(),
                            psk_protected: gs("psk").unwrap_or_default(),
                            paired_at: gi("pairedAt"),
                            revoked: json::as_bool(&g("revoked")).unwrap_or(false),
                        },
                    );
                }
            }
            _ => {}
        }
    }

    fn append(&mut self, line: &str) -> Result<(), String> {
        if self.failed {
            return Err("storage latched failed — refusing writes (fail-closed)".into());
        }
        #[cfg(test)]
        if let Some(limit) = self.fail_after_appends {
            if self.appends_count >= limit {
                self.failed = true;
                return Err("simulated journal write failure".into());
            }
            self.appends_count += 1;
        }
        match self
            .journal
            .write_all(line.as_bytes())
            .and_then(|_| self.journal.flush())
        {
            Ok(()) => {
                self.bytes_since_compact += line.len() as u64;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                Err(format!("journal append failed (latched): {e}"))
            }
        }
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    #[cfg(test)]
    pub fn latch_failed_for_test(&mut self) {
        self.failed = true;
    }

    /* ---- writes ------------------------------------------------- */

    /// The single command serializer — put_command and compact() both use it,
    /// so a new field can never be dropped by one of the two writers again
    /// (compaction once silently lost `ok`, turning refusals into in-flight
    /// answers after restart; found by the A/B differential).
    fn command_body(rec: &CommandRecord) -> Json {
        let mut fields = vec![
            ("id", json::s(&rec.command_id)),
            ("cmdKind", json::s(&rec.kind)),
            ("payload", json::s(&rec.payload)),
            ("status", json::s(&rec.status)),
            ("createdAt", Json::Int(rec.created_at)),
            ("updatedAt", Json::Int(rec.updated_at)),
        ];
        if let Some(ok) = rec.ok {
            fields.push(("ok", Json::Bool(ok)));
        }
        if let Some(s) = &rec.session_id {
            fields.push(("sessionId", json::s(s)));
        }
        if let Some(p) = &rec.permission_id {
            fields.push(("permissionId", json::s(p)));
        }
        if let Some(e) = &rec.error {
            fields.push(("error", json::s(e)));
        }
        json::obj(fields)
    }

    pub fn put_command(&mut self, rec: &CommandRecord) -> Result<(), String> {
        let body = Self::command_body(rec);
        let line = record_line("command", &body);
        self.append(&line)?;
        self.commands.insert(rec.command_id.clone(), rec.clone());
        self.enforce_command_cap();
        // only after the map matches the journal: a compaction rewrites both
        // consistently and can never drop the record just inserted
        self.maybe_compact();
        Ok(())
    }

    /// Records the outcome of a command so replays return it verbatim.
    /// `ok: None` leaves the recorded ack outcome untouched (used by the
    /// async command_result closer, which updates status only — replays
    /// return the original acceptance ack, exactly like the Node host).
    pub fn update_command_outcome(
        &mut self,
        command_id: &str,
        status: &str,
        ok: Option<bool>,
        session_id: Option<&str>,
        permission_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let Some(mut rec) = self.commands.get(command_id).cloned() else {
            return Ok(());
        };
        rec.status = status.to_string();
        if ok.is_some() {
            rec.ok = ok;
        }
        rec.updated_at = now_ms();
        if session_id.is_some() {
            rec.session_id = session_id.map(str::to_string);
        }
        if permission_id.is_some() {
            rec.permission_id = permission_id.map(str::to_string);
        }
        rec.error = error.map(str::to_string);
        self.put_command(&rec)
    }

    /// Persists an enveloped event; returns the assigned seq.
    pub fn append_event(&mut self, kind: &str, payload: &str, seq: i64) -> Result<(), String> {
        let body = json::obj(vec![
            ("seq", Json::Int(seq)),
            ("eventKind", json::s(kind)),
            ("payload", json::s(payload)),
            ("occurredAt", Json::Int(now_ms())),
        ]);
        self.append(&record_line("event", &body))?;
        self.events.push(EventRecord {
            seq,
            kind: kind.to_string(),
            payload: payload.to_string(),
            occurred_at: now_ms(),
        });
        // runtime retention — the cap used to apply only at boot, so a long
        // session could grow the event set without bound
        if self.events.len() > self.event_cap {
            let drop_n = self.events.len() - self.event_cap;
            self.events.drain(..drop_n);
        }
        self.maybe_compact();
        Ok(())
    }

    pub fn put_approval(&mut self, rec: &ApprovalRecord) -> Result<(), String> {
        let body = json::obj(vec![
            ("id", json::s(&rec.permission_id)),
            ("sessionId", json::s(&rec.session_id)),
            ("nativeRequestId", json::s(&rec.native_request_id)),
            ("status", json::s(&rec.status)),
            ("revision", Json::Int(rec.revision)),
            ("requestedAt", Json::Int(rec.requested_at)),
            ("expiresAt", Json::Int(rec.expires_at)),
        ]);
        self.append(&record_line("approval", &body))?;
        self.approvals
            .insert(rec.permission_id.clone(), rec.clone());
        Ok(())
    }

    fn pairing_body(rec: &PairingRecord) -> Json {
        json::obj(vec![
            ("deviceId", json::s(&rec.device_id)),
            ("psk", json::s(&rec.psk_protected)),
            ("challenge", json::s(&rec.challenge)),
            ("createdAt", Json::Int(rec.created_at)),
            ("expiresAt", Json::Int(rec.expires_at)),
            ("consumed", Json::Bool(rec.consumed)),
        ])
    }

    /// Persists a pending (or consumed) pairing offer. The PSK field must
    /// already be DPAPI-protected base64 — the journal never holds plaintext.
    pub fn put_pairing(&mut self, rec: &PairingRecord) -> Result<(), String> {
        self.append(&record_line("pairing", &Self::pairing_body(rec)))?;
        self.pairings.insert(rec.device_id.clone(), rec.clone());
        self.maybe_compact();
        Ok(())
    }

    fn device_body(rec: &DeviceRecord) -> Json {
        json::obj(vec![
            ("deviceId", json::s(&rec.device_id)),
            ("name", json::s(&rec.name)),
            ("psk", json::s(&rec.psk_protected)),
            ("pairedAt", Json::Int(rec.paired_at)),
            ("revoked", Json::Bool(rec.revoked)),
        ])
    }

    /// Persists a paired-device registry entry (PSK already DPAPI-protected).
    pub fn put_device(&mut self, rec: &DeviceRecord) -> Result<(), String> {
        self.append(&record_line("device", &Self::device_body(rec)))?;
        self.devices.insert(rec.device_id.clone(), rec.clone());
        self.maybe_compact();
        Ok(())
    }

    pub fn append_audit(&mut self, kind: &str, text: &str) -> Result<(), String> {
        let body = json::obj(vec![
            ("ts", Json::Int(now_ms())),
            ("auditKind", json::s(kind)),
            ("text", json::s(text)),
        ]);
        self.append(&record_line("audit", &body))?;
        self.audit.push(AuditRecord {
            ts: now_ms(),
            kind: kind.to_string(),
            text: text.to_string(),
        });
        Ok(())
    }

    pub fn put_kv(&mut self, k: &str, v: &str) -> Result<(), String> {
        let body = json::obj(vec![("k", json::s(k)), ("v", json::s(v))]);
        self.append(&record_line("kv", &body))?;
        self.kv.insert(k.to_string(), v.to_string());
        Ok(())
    }

    /* ---- reconciliation (boot) ---------------------------------- */

    /// PRD §10.4/§12.4: commands that never reached a terminal state are
    /// reconciled to `unknown` — never re-executed, never silently dropped.
    pub fn reconcile_unresolved(&mut self) -> Result<Vec<String>, String> {
        let unresolved: Vec<String> = self
            .commands
            .values()
            .filter(|c| c.status == "unresolved")
            .map(|c| c.command_id.clone())
            .collect();
        for id in &unresolved {
            // a command interrupted mid-flight is answered as unknown, never
            // re-executed (PRD §10.4); replays keep this honest refusal
            self.update_command_outcome(
                id,
                "unknown",
                Some(false),
                None,
                None,
                Some("reconciled at boot"),
            )?;
        }
        Ok(unresolved)
    }

    /// Pending approvals past their deadline become `stale` at boot.
    pub fn reconcile_expired_approvals(&mut self, now: i64) -> Result<Vec<String>, String> {
        let expired: Vec<String> = self
            .approvals
            .values()
            .filter(|a| a.status == "pending" && now >= a.expires_at)
            .map(|a| a.permission_id.clone())
            .collect();
        for id in &expired {
            if let Some(mut rec) = self.approvals.get(id).cloned() {
                rec.status = "stale".into();
                rec.revision += 1;
                self.put_approval(&rec)?;
            }
        }
        Ok(expired)
    }

    /* ---- compaction --------------------------------------------- */

    /// Rewrites the journal with only live records (atomic: tmp + rename).
    pub fn compact(&mut self) -> Result<(), String> {
        let tmp = self.dir.join("companion.journal.tmp");
        {
            let mut f = File::create(&tmp).map_err(|e| format!("compact create: {e}"))?;
            let mut write_rec = |kind: &str, body: &Json| -> std::io::Result<()> {
                f.write_all(record_line(kind, body).as_bytes())
            };
            write_rec(
                "kv",
                &json::obj(vec![
                    ("k", json::s("schema")),
                    ("v", json::s(&SCHEMA_VERSION.to_string())),
                ]),
            )
            .map_err(|e| format!("compact write: {e}"))?;
            for (k, v) in &self.kv {
                if k == "schema" {
                    continue;
                }
                write_rec("kv", &json::obj(vec![("k", json::s(k)), ("v", json::s(v))]))
                    .map_err(|e| format!("compact write: {e}"))?;
            }
            for c in self.commands.values() {
                write_rec("command", &Self::command_body(c))
                    .map_err(|e| format!("compact write: {e}"))?;
            }
            for e in &self.events {
                write_rec(
                    "event",
                    &json::obj(vec![
                        ("seq", Json::Int(e.seq)),
                        ("eventKind", json::s(&e.kind)),
                        ("payload", json::s(&e.payload)),
                        ("occurredAt", Json::Int(e.occurred_at)),
                    ]),
                )
                .map_err(|e| format!("compact write: {e}"))?;
            }
            for a in self.approvals.values() {
                write_rec(
                    "approval",
                    &json::obj(vec![
                        ("id", json::s(&a.permission_id)),
                        ("sessionId", json::s(&a.session_id)),
                        ("nativeRequestId", json::s(&a.native_request_id)),
                        ("status", json::s(&a.status)),
                        ("revision", Json::Int(a.revision)),
                        ("requestedAt", Json::Int(a.requested_at)),
                        ("expiresAt", Json::Int(a.expires_at)),
                    ]),
                )
                .map_err(|e| format!("compact write: {e}"))?;
            }
            for a in &self.audit {
                write_rec(
                    "audit",
                    &json::obj(vec![
                        ("ts", Json::Int(a.ts)),
                        ("auditKind", json::s(&a.kind)),
                        ("text", json::s(&a.text)),
                    ]),
                )
                .map_err(|e| format!("compact write: {e}"))?;
            }
            for p in self.pairings.values() {
                write_rec("pairing", &Self::pairing_body(p))
                    .map_err(|e| format!("compact write: {e}"))?;
            }
            for d in self.devices.values() {
                write_rec("device", &Self::device_body(d))
                    .map_err(|e| format!("compact write: {e}"))?;
            }
            f.sync_all().map_err(|e| format!("compact sync: {e}"))?;
        }
        // swap: std::fs::rename replaces the destination on Windows too
        fs::rename(&tmp, self.dir.join("companion.journal"))
            .map_err(|e| format!("compact rename: {e}"))?;
        self.journal = OpenOptions::new()
            .append(true)
            .open(self.dir.join("companion.journal"))
            .map_err(|e| format!("compact reopen: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chernobyl-rs-test-{}-{}-{}",
            tag,
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn command(id: &str, status: &str) -> CommandRecord {
        CommandRecord {
            command_id: id.into(),
            kind: "create_session".into(),
            payload: "{}".into(),
            status: status.into(),
            created_at: 1,
            updated_at: 1,
            ok: None,
            session_id: None,
            permission_id: None,
            error: None,
        }
    }

    #[test]
    fn command_outcome_round_trips() {
        let dir = temp_dir("outcome");
        {
            let mut s = Storage::open(&dir, 500, 500).unwrap();
            s.put_command(&command("c1", "unresolved")).unwrap();
            s.update_command_outcome(
                "c1",
                "refused",
                Some(false),
                None,
                None,
                Some("session is already running"),
            )
            .unwrap();
            let rec = s.commands.get("c1").unwrap();
            assert_eq!(rec.ok, Some(false));
            assert_eq!(rec.error.as_deref(), Some("session is already running"));
            // compaction rewrites every record — it must not drop fields
            // (the exact path that once lost `ok`)
            s.compact().unwrap();
        }
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        let rec = s2.commands.get("c1").unwrap();
        assert_eq!(rec.ok, Some(false), "ok outcome must survive the journal");
        assert_eq!(rec.error.as_deref(), Some("session is already running"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "micro-benchmark — run: cargo test --release microbench -- --ignored --nocapture"]
    fn microbench_storage_put_update_pair() {
        // mirrors the per-command wire cost on Rust: put_command (unresolved)
        // + update_command_outcome (refused) = two journal appends per command
        const N: usize = 100_000;
        let dir = temp_dir("microbench-store");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        let mut rec = command("mb-0", "unresolved");
        rec.kind = "permission_decision".into();
        rec.payload = r#"{"type":"permission_decision","commandId":"mb-0","permissionId":"p1","decision":"reject","revision":2}"#.into();
        let t0 = std::time::Instant::now();
        for i in 0..N {
            rec.command_id = format!("mb-{i}");
            s.put_command(&rec).unwrap();
            s.update_command_outcome(
                &rec.command_id,
                "refused",
                Some(false),
                None,
                None,
                Some("request changed — review it again"),
            )
            .unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "storage put+update pair (two appends + map inserts): {} ns/op ({} ops in {:?})",
            dt.as_nanos() / N as u128,
            N,
            dt
        );
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_reopens_with_contiguous_seq() {
        let dir = temp_dir("reopen");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        s.append_event("session_created", "{}", 1).unwrap();
        s.append_event("activity", "{}", 2).unwrap();
        drop(s);
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.last_seq, 2);
        assert_eq!(s2.events.len(), 2);
        assert_eq!(s2.kv.get("schema").map(String::as_str), Some("1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_are_idempotent_by_id() {
        let dir = temp_dir("idem");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        s.put_command(&command("c1", "unresolved")).unwrap();
        // same id again replaces in place (last write wins, single record)
        s.put_command(&command("c1", "unresolved")).unwrap();
        assert_eq!(s.commands.len(), 1);
        drop(s);
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.commands.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_marks_unresolved_unknown_and_expires_approvals() {
        let dir = temp_dir("recon");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        s.put_command(&command("c1", "unresolved")).unwrap();
        s.put_command(&command("c2", "completed")).unwrap();
        s.put_approval(&ApprovalRecord {
            permission_id: "p1".into(),
            session_id: "s1".into(),
            native_request_id: "p1".into(),
            status: "pending".into(),
            revision: 1,
            requested_at: 0,
            expires_at: 100,
        })
        .unwrap();
        s.put_approval(&ApprovalRecord {
            permission_id: "p2".into(),
            session_id: "s2".into(),
            native_request_id: "p2".into(),
            status: "pending".into(),
            revision: 1,
            requested_at: 0,
            expires_at: 99_999,
        })
        .unwrap();

        let unresolved = s.reconcile_unresolved().unwrap();
        assert_eq!(unresolved, vec!["c1".to_string()]);
        assert_eq!(s.commands["c1"].status, "unknown");
        assert_eq!(s.commands["c2"].status, "completed");

        let expired = s.reconcile_expired_approvals(1_000).unwrap();
        assert_eq!(expired, vec!["p1".to_string()]);
        assert_eq!(s.approvals["p1"].status, "stale");
        assert_eq!(s.approvals["p1"].revision, 2);
        assert_eq!(s.approvals["p2"].status, "pending");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_is_truncated_and_store_still_works() {
        let dir = temp_dir("torn");
        {
            let mut s = Storage::open(&dir, 500, 500).unwrap();
            s.append_event("e1", "{}", 1).unwrap();
            // simulate torn write: half a record, no newline
            s.append("{\"kind\":\"event\",\"seq\":2,\"p").unwrap();
        }
        let mut s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.last_seq, 1);
        assert_eq!(s2.events.len(), 1);
        // a torn tail loses nothing after it: NOT counted as corruption
        assert_eq!(s2.corrupt_events, 0);
        assert!(s2.corrupt.is_none());
        // appends after recovery stay valid
        s2.append_event("e3", "{}", 2).unwrap();
        drop(s2);
        let s3 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s3.events.len(), 2);
        assert_eq!(s3.last_seq, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Mandate §5: mid-file corruption with valid records after it must be
    /// visible — stderr log with offset/line, a latched counter for /health,
    /// and an audit record — while the truncate-and-continue policy itself
    /// (ADR-010) is unchanged. The torn-tail test above proves the silent
    /// path stays silent only when nothing is lost.
    #[test]
    fn mid_file_corruption_is_latched_and_audited_not_silent() {
        let dir = temp_dir("corrupt");
        let path = dir.join("companion.journal");
        fs::create_dir_all(&dir).unwrap();
        let event = |seq: i64| {
            record_line(
                "event",
                &json::obj(vec![
                    ("seq", Json::Int(seq)),
                    ("eventKind", json::s("e")),
                    ("payload", json::s("{}")),
                    ("occurredAt", Json::Int(1)),
                ]),
            )
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(event(1).as_bytes());
        bytes.extend_from_slice(event(2).as_bytes());
        let corrupt_offset = bytes.len() as u64;
        bytes.extend_from_slice(b"{not json at all\n");
        bytes.extend_from_slice(event(3).as_bytes());
        fs::write(&path, &bytes).unwrap();

        let s = Storage::open(&dir, 500, 500).unwrap();
        // records before the corrupt line survive; the valid record after it
        // is discarded — policy unchanged (ADR-010), but now observable
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.last_seq, 2);
        assert_eq!(s.corrupt_events, 1);
        let loss = s.corrupt.expect("corruption must be latched");
        assert_eq!(loss.offset, corrupt_offset);
        assert_eq!(loss.line, 3);
        assert_eq!(loss.discarded_bytes, bytes.len() as u64 - corrupt_offset);
        // the loss is in the durable audit trail, not only on stderr
        assert!(
            s.audit
                .iter()
                .any(|a| a.kind == "storage" && a.text.contains("journal corrupt at line 3"))
        );
        drop(s);

        // next boot: garbage gone, audit record survives, counter back to 0
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.corrupt_events, 0);
        assert_eq!(s2.events.len(), 2);
        assert!(
            s2.audit
                .iter()
                .any(|a| a.kind == "storage" && a.text.contains("journal corrupt"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_cap_prunes_oldest_events() {
        let dir = temp_dir("retention");
        let mut s = Storage::open(&dir, 3, 500).unwrap();
        for seq in 1..=10 {
            s.append_event("e", "{}", seq).unwrap();
        }
        drop(s);
        let s2 = Storage::open(&dir, 3, 500).unwrap();
        assert_eq!(s2.events.len(), 3);
        assert_eq!(s2.last_seq, 10);
        let seqs: Vec<i64> = s2.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![8, 9, 10]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_shrinks_journal_and_preserves_state() {
        let dir = temp_dir("compact");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        for seq in 1..=50 {
            s.append_event("e", "{\"x\":1}", seq).unwrap();
        }
        s.put_command(&command("c1", "completed")).unwrap();
        s.compact().unwrap();
        assert_eq!(s.events.len(), 50);
        assert_eq!(s.commands.len(), 1);
        drop(s);
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.events.len(), 50);
        assert_eq!(s2.last_seq, 50);
        assert_eq!(s2.commands["c1"].status, "completed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kv_survives_reopen() {
        let dir = temp_dir("kv");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        s.put_kv("bootCount", "3").unwrap();
        drop(s);
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.kv.get("bootCount").map(String::as_str), Some("3"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_and_device_records_round_trip() {
        let dir = temp_dir("pairing");
        {
            let mut s = Storage::open(&dir, 500, 500).unwrap();
            s.put_pairing(&PairingRecord {
                device_id: "d1".into(),
                psk_protected: "PROTECTED_PSK".into(),
                challenge: "Q0hBTA".into(),
                created_at: 100,
                expires_at: 400_000,
                consumed: false,
            })
            .unwrap();
            // redemption flips single-use; the update must persist too
            let mut used = s.pairings.get("d1").unwrap().clone();
            used.consumed = true;
            s.put_pairing(&used).unwrap();
            s.put_device(&DeviceRecord {
                device_id: "d1".into(),
                name: "phone".into(),
                psk_protected: "PROTECTED_PSK".into(),
                paired_at: 200,
                revoked: false,
            })
            .unwrap();
            // compaction must not drop either record kind
            s.compact().unwrap();
        }
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        let p = s2
            .pairings
            .get("d1")
            .expect("pairing survives reopen+compact");
        assert!(p.consumed, "single-use flag survives");
        assert_eq!(p.psk_protected, "PROTECTED_PSK");
        assert_eq!(p.expires_at, 400_000);
        let d = s2
            .devices
            .get("d1")
            .expect("device survives reopen+compact");
        assert_eq!(d.name, "phone");
        assert!(!d.revoked);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_retention_cap_evicts_oldest_completed_keeps_unresolved() {
        let dir = temp_dir("cmd-retention");
        let mut s = Storage::open(&dir, 500, 3).unwrap();
        for i in 1..=3 {
            let mut c = command(&format!("done-{i}"), "refused");
            c.created_at = i;
            c.ok = Some(false);
            s.put_command(&c).unwrap();
        }
        // in-flight commands are never evicted: they yield the oldest completed
        for i in 4..=5 {
            let mut c = command(&format!("open-{i}"), "unresolved");
            c.created_at = i;
            s.put_command(&c).unwrap();
        }
        let mut c = command("done-6", "refused");
        c.created_at = 6;
        s.put_command(&c).unwrap();
        assert_eq!(s.commands.len(), 3, "cap holds for the evictable class");
        assert!(!s.commands.contains_key("done-1"));
        assert!(!s.commands.contains_key("done-3"));
        assert!(s.commands.contains_key("open-4") && s.commands.contains_key("open-5"));
        assert!(s.commands.contains_key("done-6"));
        // another in-flight command: the newest completed yields to in-flight
        // (shortcut documented on enforce_command_cap) — unresolved all kept
        let mut c = command("open-7", "unresolved");
        c.created_at = 7;
        s.put_command(&c).unwrap();
        assert_eq!(s.commands.len(), 3);
        assert!(!s.commands.contains_key("done-6"));
        assert!(s.commands.contains_key("open-4") && s.commands.contains_key("open-5"));
        assert!(s.commands.contains_key("open-7"));
        drop(s);
        // restart: the journal replay cannot resurrect the evicted records
        // (open prunes again after load, then compacts)
        let s2 = Storage::open(&dir, 500, 3).unwrap();
        assert!(!s2.commands.contains_key("done-1"));
        assert!(s2.commands.contains_key("open-4"));
        assert!(s2.commands.contains_key("open-7"));
        let _ = fs::remove_dir_all(&dir);

        // all-in-flight legally exceeds the cap — dropping them would break
        // boot reconciliation and verbatim replay
        let dir = temp_dir("cmd-retention-flight");
        let mut s = Storage::open(&dir, 500, 2).unwrap();
        for i in 1..=4 {
            let mut c = command(&format!("open-{i}"), "unresolved");
            c.created_at = i;
            s.put_command(&c).unwrap();
        }
        assert_eq!(
            s.commands.len(),
            4,
            "no evictable records → over cap is legal"
        );
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_compact_trigger_bounds_file_within_one_run() {
        let dir = temp_dir("compact-trigger");
        let mut s = Storage::open(&dir, 500, 500).unwrap();
        s.compact_trigger = 4_096; // tiny for the test; default is 64 MB
        for i in 0..50 {
            let mut c = command(&format!("t-{i}"), "refused");
            c.payload = "x".repeat(300); // ~400 B/record so the trigger trips often
            c.created_at = i;
            s.put_command(&c).unwrap();
        }
        let len = fs::metadata(dir.join("companion.journal")).unwrap().len();
        // without the trigger this would be 100 appends ≈ 40 KB+ of raw growth
        // per run compounding forever; with it the file stays near the trigger
        assert!(len < 16 * 4_096, "journal grew to {len} bytes");
        assert_eq!(s.commands.len(), 50, "compaction never drops live records");
        drop(s);
        let s2 = Storage::open(&dir, 500, 500).unwrap();
        assert_eq!(s2.commands.len(), 50);
        let _ = fs::remove_dir_all(&dir);
    }
}
