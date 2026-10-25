//! `--soak` — the companion soak harness (repeatability over time), built into
//! the product binary. Supersedes `tools/soak.mjs` (kept for one release).
//!
//! Spawns THIS executable as the server under test (loopback, token auth, an
//! isolated CHERNOBYL_HOME under the run dir, ephemeral ports read from the
//! boot log, an `--exit-after` failsafe), drives it through lifecycle stress
//! phases with the in-repo client, samples `/health`, and writes timestamped
//! JSONL events plus a machine-readable summary.json. Crash, graceful
//! shutdown and restart are judged across a real process boundary.
//!
//!   chernobyl-companion --soak smoke                       # one full pass (~80 s)
//!   chernobyl-companion --soak 86400                       # long run: 24 h of load
//!   chernobyl-companion --soak smoke --inject server-kill  # failure-detection demo
//!   --out <dir>   artifacts [services/companion-rs/target/soak/<ts>-<mode>]
//!
//! Long mode is the same pass with a load loop (churn + FIFO every cycle,
//! saturation every 3rd, dead peer every 10th) running until the duration has
//! elapsed, inserted before the closing phases so the quiesced RSS is measured
//! after the load. Throughout, one progress line every 30 s; while the loop
//! runs it replaces per-phase output (events.jsonl still gets everything).
//!
//! PASS/FAIL criteria — evaluated from observations, never from "the process
//! was still alive". Any failure ⇒ exit 1; exit 0 only when all ten pass:
//! ```text
//!   1. unexpected_server_exit  — server exited without a harness-requested stop
//!   2. ordering_violation      — a pong arrived out of order / missing / wrong id
//!   3. unauthorized_accepted   — a wrong-token client got any data (must get none)
//!   4. peer_leak               — /health clients != 0 after a phase that must
//!                                release every peer (churn, saturation, dead-peer,
//!                                writer-failure, final quiesce)
//!   5. d8_saturation_teardown  — a stopped-consuming client stayed registered after
//!                                the reply flood (bounded-queue regression)
//!   6. dead_peer_not_released  — a silent paused peer survived past the heartbeat
//!                                bound (HEARTBEAT_MS 15s x MISS_LIMIT 3 + slack)
//!   7. graceful_shutdown_failed— --exit-after instance exited non-zero or hung
//!   8. restart_recovery_failed — respawned server failed to serve reconnecting clients
//!   9. resource_growth         — quiesced RSS above the limits below
//!  10. connection_failure      — an expected connect/hello/pong that did not happen
//! ```
//!
//! Threshold provenance (stated in summary.thresholds):
//! ```text
//!   EXISTING LIMITS (the source constants themselves, not copies):
//!     OUTBOUND_QUEUE_MAX_MESSAGES=256, OUTBOUND_QUEUE_MAX_BYTES=4 MiB,
//!     AUTH_FAILURE_LIMIT=5 (server.rs), HEARTBEAT_MS=15s,
//!     HEARTBEAT_MISS_LIMIT=3 (protocol.rs), /health rssBytes (server self-report)
//!   PROPOSED (new for the soak; documented for review):
//!     rssGrowthLimitBytes = 64 MiB   (quiesced growth over baseline; absorbs
//!                                     allocator retention — a leak that matters
//!                                     shows far more over a long run)
//!     rssAbsoluteLimitBytes = 512 MiB (any time; catches runaway independent
//!                                     of baseline)
//!     releaseTimeouts: normal 30s, saturation 120s, dead-peer 150s (each above
//!                                     the source-derived teardown bound so a
//!                                     timeout means "stuck", not "slow")
//! ```
//!
//! Honesty notes: this is a loopback harness — it does NOT simulate a phone,
//! does NOT exercise E2E-sealed sessions (the Rust e2e matrix covers those),
//! does NOT bind beyond loopback (LAN exposure is forbidden), and mode labels
//! are literal: a long run ends only when its real duration has elapsed.

use chernobyl_companion::client::{WsClient, http_get};
use chernobyl_companion::json::{self, Json};
use chernobyl_companion::protocol::{HEARTBEAT_MISS_LIMIT, HEARTBEAT_MS, PROTOCOL_VERSION, now_ms};
use chernobyl_companion::server::{
    AUTH_FAILURE_LIMIT, OUTBOUND_QUEUE_MAX_BYTES, OUTBOUND_QUEUE_MAX_MESSAGES,
};
use chernobyl_companion::ws::Frame;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN: &str = "soak-secret";

// PROPOSED limits (see the header for their reasoning)
const RSS_GROWTH_LIMIT_BYTES: i64 = 64 * 1024 * 1024;
const RSS_ABSOLUTE_LIMIT_BYTES: i64 = 512 * 1024 * 1024;
const RELEASE_NORMAL_MS: u64 = 30_000;
const RELEASE_SATURATION_MS: u64 = 120_000;
const RELEASE_DEAD_PEER_MS: u64 = 150_000; // 4+ beats of 15 s + slack

/// Resource sample + progress line cadence.
const SAMPLE_EVERY_MS: u64 = 30_000;
/// Reply volume a saturation flood asks for: far beyond any socket buffer.
const FLOOD_TARGET_BYTES: usize = 40 * 1024 * 1024;
/// What a poll returns once an unexpected server exit aborted the run — the
/// exit itself is the failure, so a phase cut short by it records nothing.
const ABORTED: &str = "run aborted";

/// `--soak <mode> [--out <dir>] [--inject server-kill]`. Exit 0 = every
/// criterion passed, 1 = a failure (or, with `--inject`, the injected fault
/// WAS detected), 2 = bad arguments, 3 = an injected fault went unnoticed.
pub fn run(mode: &str, out: Option<&str>, inject: Option<&str>) -> i32 {
    let Some(duration_s) = parse_mode(mode) else {
        eprintln!("unknown --soak {mode} (smoke|<seconds>)");
        return 2;
    };
    let inject = match inject {
        None => false,
        Some("server-kill") => true,
        Some(other) => {
            eprintln!("unknown --inject {other} (server-kill)");
            return 2;
        }
    };
    let long = duration_s > 0;
    let started_ms = now_ms();
    let out = out.map(PathBuf::from).unwrap_or_else(|| {
        // a dev checkout keeps runs beside the crate; a moved binary uses temp
        crate_dir()
            .map_or_else(
                || std::env::temp_dir().join("chernobyl-soak"),
                |d| d.join("target").join("soak"),
            )
            .join(format!(
                "{}-{}",
                iso_utc(started_ms).replace([':', '.'], "-"),
                if long { "long" } else { "smoke" }
            ))
    });
    let out = std::path::absolute(&out).unwrap_or(out);
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[soak] cannot resolve own exe path: {e}");
            return 1;
        }
    };
    let events = std::fs::create_dir_all(&out).and_then(|()| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(out.join("events.jsonl"))
    });
    let events = match events {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[soak] cannot create {}: {e}", out.display());
            return 1;
        }
    };
    let soak = Arc::new(Soak {
        exe,
        out,
        duration_s,
        clients: if long { 10 } else { 5 },
        churn_cycles: if long { 40 } else { 10 },
        inject,
        t0: Instant::now(),
        started_ms,
        abort: AtomicBool::new(false),
        quiet: AtomicBool::new(false),
        sampling: AtomicBool::new(false),
        obs: Mutex::new(Obs::default()),
        events: Mutex::new(events),
    });
    soak.main()
}

/// `smoke` → 0 (one pass); `<seconds>` → a long run of that many seconds.
fn parse_mode(mode: &str) -> Option<u64> {
    if mode == "smoke" {
        return Some(0);
    }
    mode.parse().ok().filter(|&s| s > 0)
}

/* ------------------------------------------------------------------ */
/* observations → criteria → summary (pure)                            */
/* ------------------------------------------------------------------ */

/// Everything the criteria are judged on, filled by the phases, the server
/// watchers and the sampler.
#[derive(Default)]
struct Obs {
    /// current top-level phase and load-loop cycle (progress lines only)
    phase: String,
    cycle: u64,
    failures: Vec<Failure>,
    connects_completed: u64,
    connects_failed: u64,
    pongs_expected: u64,
    pongs_received: u64,
    ordering_violations: u64,
    unauthorized: BTreeMap<String, u64>,
    disconnect_reasons: BTreeMap<String, u64>,
    teardowns: Vec<Teardown>,
    unexpected_exits: Vec<Exited>,
    requested_stops: Vec<Exited>,
    graceful: Option<Graceful>,
    /// (served, attempted) reconnects after the restart
    restart: Option<(u64, u64)>,
    rss_baseline: Option<i64>,
    rss_peak: Option<i64>,
    rss_final: Option<i64>,
    samples: u64,
}

impl Obs {
    fn growth(&self) -> Option<i64> {
        self.rss_baseline.zip(self.rss_final).map(|(b, f)| f - b)
    }
}

struct Failure {
    phase: String,
    code: String,
    detail: String,
}

struct Teardown {
    label: String,
    n: usize,
    reply_bytes: usize,
    expected_bound: &'static str,
    teardown_ms: u64,
}

struct Exited {
    server: &'static str,
    code: Option<i32>,
    at_ms: u64,
}

struct Graceful {
    code: Option<i32>,
    has_shutting_down: bool,
    has_bye: bool,
    ms: u64,
}

struct Criterion {
    id: &'static str,
    requirement: String,
    observed: String,
    pass: bool,
}

fn criteria(o: &Obs) -> Vec<Criterion> {
    let has = |code: &str| o.failures.iter().any(|f| f.code == code);
    let crit = |id, requirement: &str, observed: String, pass| Criterion {
        id,
        requirement: requirement.into(),
        observed,
        pass,
    };
    let growth = o.growth();
    let teardowns: Vec<String> = o
        .teardowns
        .iter()
        .map(|t| format!("{} {}ms", t.label, t.teardown_ms))
        .collect();
    let conn_failures = o
        .failures
        .iter()
        .filter(|f| f.code == "connection_failure" || f.code == "exception")
        .count();
    vec![
        crit(
            "unexpected_server_exit",
            "server never exits without a harness-requested stop",
            format!("{} unexpected exit(s)", o.unexpected_exits.len()),
            o.unexpected_exits.is_empty(),
        ),
        crit(
            "ordering_violation",
            "every expected pong arrives in send order (per client)",
            format!(
                "{}/{} pongs, {} order violations",
                o.pongs_received, o.pongs_expected, o.ordering_violations
            ),
            o.ordering_violations == 0 && o.pongs_received == o.pongs_expected,
        ),
        crit(
            "unauthorized_accepted",
            "wrong-token clients get only 4001/429 and never protocol data",
            json::stringify(&tally_json(&o.unauthorized)),
            !has("unauthorized_accepted"),
        ),
        crit(
            "peer_leak",
            "clients gauge returns to 0 after churn / writer failure / quiesce",
            if has("peer_leak") {
                "see failures[]"
            } else {
                "all phases released to 0"
            }
            .into(),
            !has("peer_leak"),
        ),
        crit(
            "d8_saturation_teardown",
            "stopped-consuming clients are torn down (bounded queue), then reconnect fine",
            format!(
                "{} teardown(s): {}",
                teardowns.len(),
                if teardowns.is_empty() {
                    "none".into()
                } else {
                    teardowns.join(", ")
                }
            ),
            !has("d8_saturation_teardown"),
        ),
        crit(
            "dead_peer_not_released",
            "silent paused peer released within the heartbeat bound",
            if has("dead_peer_not_released") {
                "NOT released (see failures)"
            } else {
                "released"
            }
            .into(),
            !has("dead_peer_not_released"),
        ),
        crit(
            "graceful_shutdown_failed",
            "--exit-after shutdown with live clients exits 0 and logs shutdown",
            o.graceful
                .as_ref()
                .map_or("not run".into(), |g| json::stringify(&graceful_json(g))),
            o.graceful
                .as_ref()
                .is_some_and(|g| g.code == Some(0) && g.has_shutting_down)
                && !has("graceful_shutdown_failed"),
        ),
        crit(
            "restart_recovery_failed",
            "respawned server serves reconnecting clients on the same data dir",
            o.restart
                .map_or("not run".into(), |(ok, total)| format!("{ok}/{total}")),
            o.restart.is_some_and(|(ok, total)| ok == total) && !has("restart_recovery_failed"),
        ),
        crit(
            "resource_growth",
            &format!(
                "quiesced RSS growth ≤ {RSS_GROWTH_LIMIT_BYTES}B and absolute ≤ {RSS_ABSOLUTE_LIMIT_BYTES}B"
            ),
            format!(
                "baseline={} final={} growth={} peak={}",
                shown(o.rss_baseline),
                shown(o.rss_final),
                shown(growth),
                shown(o.rss_peak)
            ),
            growth.is_none_or(|g| g <= RSS_GROWTH_LIMIT_BYTES)
                && o.rss_peak.is_none_or(|p| p <= RSS_ABSOLUTE_LIMIT_BYTES)
                && o.rss_final.is_some(),
        ),
        crit(
            "connection_failure",
            "every expected connect/hello/pong/ack happens (failures listed)",
            format!("{conn_failures} failure(s)"),
            conn_failures == 0,
        ),
    ]
}

/// The summary.json document: `head` (run metadata) first, then thresholds,
/// counts, servers, resources, criteria, failures and the verdict, then
/// `extra`. A failure outside the ten criteria still fails the run.
fn summary_json(
    o: &Obs,
    crits: &[Criterion],
    head: Vec<(&str, Json)>,
    extra: Vec<(&str, Json)>,
) -> Json {
    let pass = o.failures.is_empty() && crits.iter().all(|c| c.pass);
    let exits = |list: &[Exited]| {
        Json::Arr(
            list.iter()
                .map(|e| {
                    json::obj(vec![
                        ("server", json::s(e.server)),
                        ("code", opt(e.code.map(i64::from))),
                        ("at", int(e.at_ms)),
                    ])
                })
                .collect(),
        )
    };
    let mut fields = head;
    fields.extend([
        ("thresholds", thresholds_json()),
        (
            "counts",
            json::obj(vec![
                ("connectsCompleted", int(o.connects_completed)),
                ("connectsFailed", int(o.connects_failed)),
                ("pongsExpected", int(o.pongs_expected)),
                ("pongsReceived", int(o.pongs_received)),
                ("orderingViolations", int(o.ordering_violations)),
                ("unauthorizedOutcomes", tally_json(&o.unauthorized)),
                ("disconnectReasons", tally_json(&o.disconnect_reasons)),
                (
                    "teardowns",
                    Json::Arr(
                        o.teardowns
                            .iter()
                            .map(|t| {
                                json::obj(vec![
                                    ("label", json::s(&t.label)),
                                    ("n", int(t.n)),
                                    ("replyBytes", int(t.reply_bytes)),
                                    ("expectedBound", json::s(t.expected_bound)),
                                    ("teardownMs", int(t.teardown_ms)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
        ),
        (
            "servers",
            json::obj(vec![
                ("unexpectedExits", exits(&o.unexpected_exits)),
                ("requestedStops", exits(&o.requested_stops)),
                (
                    "graceful",
                    o.graceful.as_ref().map_or(Json::Null, graceful_json),
                ),
                (
                    "restart",
                    o.restart.map_or(Json::Null, |(ok, total)| {
                        json::obj(vec![("ok", int(ok)), ("total", int(total))])
                    }),
                ),
            ]),
        ),
        (
            "resources",
            json::obj(vec![
                ("baselineRssBytes", opt(o.rss_baseline)),
                ("peakRssBytes", opt(o.rss_peak)),
                ("finalRssBytes", opt(o.rss_final)),
                ("growthBytes", opt(o.growth())),
                ("sampleCount", int(o.samples)),
                ("samplesEveryMs", int(SAMPLE_EVERY_MS)),
            ]),
        ),
        (
            "criteria",
            Json::Arr(
                crits
                    .iter()
                    .map(|c| {
                        json::obj(vec![
                            ("id", json::s(c.id)),
                            ("requirement", json::s(&c.requirement)),
                            ("observed", json::s(&c.observed)),
                            ("pass", Json::Bool(c.pass)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "failures",
            Json::Arr(
                o.failures
                    .iter()
                    .map(|f| {
                        json::obj(vec![
                            ("phase", json::s(&f.phase)),
                            ("code", json::s(&f.code)),
                            ("detail", json::s(&f.detail)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("pass", Json::Bool(pass)),
    ]);
    fields.extend(extra);
    json::obj(fields)
}

fn thresholds_json() -> Json {
    json::obj(vec![
        (
            "existingLimits",
            json::obj(vec![
                ("outboundMaxMessages", int(OUTBOUND_QUEUE_MAX_MESSAGES)),
                ("outboundMaxBytes", int(OUTBOUND_QUEUE_MAX_BYTES)),
                ("authFailureLimit", int(AUTH_FAILURE_LIMIT)),
                ("heartbeatMs", int(HEARTBEAT_MS)),
                ("heartbeatMissLimit", int(HEARTBEAT_MISS_LIMIT)),
                (
                    "source",
                    json::s("services/companion-rs/src/server.rs, src/protocol.rs"),
                ),
            ]),
        ),
        (
            "proposedLimits",
            json::obj(vec![
                ("rssGrowthLimitBytes", int(RSS_GROWTH_LIMIT_BYTES)),
                ("rssAbsoluteLimitBytes", int(RSS_ABSOLUTE_LIMIT_BYTES)),
                ("releaseNormalMs", int(RELEASE_NORMAL_MS)),
                ("releaseSaturationMs", int(RELEASE_SATURATION_MS)),
                ("releaseDeadPeerMs", int(RELEASE_DEAD_PEER_MS)),
                (
                    "note",
                    json::s(
                        "newly proposed by the soak harness; release timeouts sit above the source-derived teardown bounds so a timeout means stuck, not slow",
                    ),
                ),
            ]),
        ),
    ])
}

fn graceful_json(g: &Graceful) -> Json {
    json::obj(vec![
        ("code", opt(g.code.map(i64::from))),
        ("hasShuttingDown", Json::Bool(g.has_shutting_down)),
        ("hasBye", Json::Bool(g.has_bye)),
        ("ms", int(g.ms)),
    ])
}

fn tally_json(t: &BTreeMap<String, u64>) -> Json {
    Json::Obj(t.iter().map(|(k, v)| (k.clone(), int(*v))).collect())
}

fn int<T: TryInto<i64>>(n: T) -> Json {
    Json::Int(n.try_into().unwrap_or(i64::MAX))
}

fn opt(n: Option<i64>) -> Json {
    n.map_or(Json::Null, Json::Int)
}

/// An optional number as the Node harness printed it (`null` when absent).
fn shown(n: Option<i64>) -> String {
    n.map_or("null".into(), |v| v.to_string())
}

/* ------------------------------------------------------------------ */
/* harness state                                                       */
/* ------------------------------------------------------------------ */

struct Soak {
    exe: PathBuf,
    out: PathBuf,
    /// 0 = smoke (one pass); otherwise the long run's wall-clock target
    duration_s: u64,
    clients: usize,
    churn_cycles: usize,
    inject: bool,
    t0: Instant,
    started_ms: i64,
    /// set by a watcher when a server exits unasked: later phases are skipped
    abort: AtomicBool,
    /// mutes per-phase progress while the load loop runs (FAIL lines stay)
    quiet: AtomicBool,
    sampling: AtomicBool,
    obs: Mutex<Obs>,
    events: Mutex<File>,
}

/// One server under test: a child process of this exe, watched for exits.
struct Server {
    name: &'static str,
    child: Mutex<Child>,
    log: PathBuf,
    /// the harness asked for this exit (stop / expected --exit-after)
    requested: AtomicBool,
    exit: OnceLock<Option<i32>>,
}

impl Soak {
    fn long(&self) -> bool {
        self.duration_s > 0
    }

    fn obs(&self) -> MutexGuard<'_, Obs> {
        self.obs.lock().unwrap()
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::SeqCst)
    }

    fn secs(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    /// One events.jsonl line: ts/t/phase/event plus `fields` (an object).
    fn ev(&self, phase: &str, event: &str, fields: Json) {
        let now = now_ms();
        let mut line = vec![
            ("ts".to_string(), json::s(&iso_utc(now))),
            ("t".to_string(), Json::Int(now)),
            ("phase".to_string(), json::s(phase)),
            ("event".to_string(), json::s(event)),
        ];
        if let Json::Obj(pairs) = fields {
            line.extend(pairs);
        }
        let mut text = json::stringify(&Json::Obj(line));
        text.push('\n');
        let _ = self.events.lock().unwrap().write_all(text.as_bytes());
    }

    fn fail(&self, phase: &str, code: &str, detail: &str) {
        self.obs().failures.push(Failure {
            phase: phase.into(),
            code: code.into(),
            detail: detail.into(),
        });
        self.ev(
            phase,
            "FAIL",
            json::obj(vec![("code", json::s(code)), ("detail", json::s(detail))]),
        );
        eprintln!("  FAIL [{phase}] {code}: {detail}");
    }

    fn progress(&self, msg: &str) {
        if !self.quiet.load(Ordering::SeqCst) {
            println!("[t+{:.1}s] {msg}", self.secs());
        }
    }

    /// Runs one phase unless the run was aborted; an error is an `exception`
    /// failure (counted under connection_failure), except an abort.
    fn phase(&self, name: &str, f: impl FnOnce() -> Result<(), String>) {
        if self.aborted() {
            return;
        }
        self.obs().phase = name.into();
        self.ev(name, "phase_start", Json::Null);
        self.progress(&format!("phase {name} …"));
        let start = Instant::now();
        match f() {
            Ok(()) => self.ev(
                name,
                "phase_ok",
                json::obj(vec![("ms", int(start.elapsed().as_millis()))]),
            ),
            Err(e) if e == ABORTED => {}
            Err(e) => self.fail(name, "exception", &e),
        }
    }

    fn poll_until(
        &self,
        timeout_ms: u64,
        every_ms: u64,
        mut done: impl FnMut() -> Result<bool, String>,
    ) -> Result<bool, String> {
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.aborted() {
                return Err(ABORTED.into());
            }
            if done()? {
                return Ok(true);
            }
            if Instant::now() > end {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(every_ms));
        }
    }

    /// Polls `/health` until the clients gauge reads 0.
    fn released(&self, port: u16, timeout_ms: u64, every_ms: u64) -> Result<bool, String> {
        self.poll_until(timeout_ms, every_ms, || Ok(clients(port)? == 0))
    }

    fn connect(&self, port: u16, token: &str, label: &str) -> Result<WsClient, String> {
        let c = WsClient::connect(
            &format!("127.0.0.1:{port}"),
            &format!("/?token={token}"),
            None,
        )
        .map_err(|e| format!("{label}: {e}"))?;
        self.obs().connects_completed += 1;
        Ok(c)
    }

    /* -------------------------------------------------------------- */
    /* server children                                                 */
    /* -------------------------------------------------------------- */

    /// Spawns this exe as a server (loopback, token, ephemeral ports, own
    /// CHERNOBYL_HOME, stdout+stderr to `server-<name>.log`) and returns it
    /// with the main and diagnostics ports its boot log announced.
    fn spawn_server(
        self: &Arc<Self>,
        name: &'static str,
        home: &Path,
        exit_after_ms: Option<u64>,
    ) -> Result<(Arc<Server>, u16, u16), String> {
        std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
        let log = self.out.join(format!("server-{name}.log"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("{}: {e}", log.display()))?;
        let file2 = file.try_clone().map_err(|e| e.to_string())?;
        // failsafe: a forgotten long run still exits on its own
        let exit_after = exit_after_ms.unwrap_or((self.duration_s + 3600) * 1000);
        let child = Command::new(&self.exe)
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                "0",
                "--diagnostics-port",
                "0",
            ])
            .args(["--token", TOKEN, "--exit-after", &exit_after.to_string()])
            .env("CHERNOBYL_HOME", home)
            .env_remove("PORT")
            .env_remove("HOST_TOKEN")
            .stdin(Stdio::null())
            .stdout(file2)
            .stderr(file)
            .spawn()
            .map_err(|e| format!("spawn {name}: {e}"))?;
        let srv = Arc::new(Server {
            name,
            child: Mutex::new(child),
            log,
            requested: AtomicBool::new(false),
            exit: OnceLock::new(),
        });
        self.watch(&srv);
        // each boot line is written on its own: parse per field
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = read_lossy(&srv.log);
            if let (Some(port), Some(diag)) = (
                announced_port(&text, "listening on "),
                announced_port(&text, "diagnostics on "),
            ) {
                return Ok((srv, port, diag));
            }
            if srv.exit.get().is_some() || Instant::now() > deadline {
                let _ = self.stop(&srv, 5_000);
                return Err(format!(
                    "{name}: ports never announced; log: {}",
                    srv.log.display()
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Records the server's exit the moment it happens; an exit the harness
    /// did not ask for is criterion 1 and aborts the run.
    fn watch(self: &Arc<Self>, srv: &Arc<Server>) {
        let (soak, srv) = (Arc::clone(self), Arc::clone(srv));
        thread::spawn(move || {
            loop {
                let status = srv.child.lock().unwrap().try_wait();
                match status {
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Ok(Some(st)) => return soak.on_exit(&srv, st.code()),
                    Err(_) => return soak.on_exit(&srv, None),
                }
            }
        });
    }

    fn on_exit(&self, srv: &Server, code: Option<i32>) {
        let _ = srv.exit.set(code);
        let requested = srv.requested.load(Ordering::SeqCst);
        let rec = Exited {
            server: srv.name,
            code,
            at_ms: self.t0.elapsed().as_millis() as u64,
        };
        if requested {
            self.obs().requested_stops.push(rec);
        } else {
            // criterion 1: the server must never go away on its own
            self.obs().unexpected_exits.push(rec);
            self.fail(
                "server",
                "unexpected_server_exit",
                &format!(
                    "{} exited code={} without a harness request",
                    srv.name,
                    shown(code.map(i64::from))
                ),
            );
            self.abort.store(true, Ordering::SeqCst);
        }
        self.ev(
            "server",
            "exit",
            json::obj(vec![
                ("server", json::s(srv.name)),
                ("code", opt(code.map(i64::from))),
                ("requested", Json::Bool(requested)),
            ]),
        );
    }

    /// Requested stop (a hard kill, as the Node harness's `child.kill()`).
    fn stop(&self, srv: &Server, ms: u64) -> Result<Option<i32>, String> {
        if let Some(code) = srv.exit.get() {
            return Ok(*code);
        }
        srv.requested.store(true, Ordering::SeqCst);
        let _ = srv.child.lock().unwrap().kill();
        self.wait_exit(srv, ms)
    }

    /// The server's own exit (`--exit-after`) is the expected path.
    fn expect_exit(&self, srv: &Server, ms: u64) -> Result<Option<i32>, String> {
        srv.requested.store(true, Ordering::SeqCst);
        self.wait_exit(srv, ms)
    }

    fn wait_exit(&self, srv: &Server, ms: u64) -> Result<Option<i32>, String> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(code) = srv.exit.get() {
                return Ok(*code);
            }
            if Instant::now() > deadline {
                return Err(format!("stop {} timed out after {ms}ms", srv.name));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /* -------------------------------------------------------------- */
    /* resource sampler + progress                                     */
    /* -------------------------------------------------------------- */

    fn start_sampler(self: &Arc<Self>, port: u16) -> thread::JoinHandle<()> {
        self.sampling.store(true, Ordering::SeqCst);
        let soak = Arc::clone(self);
        thread::spawn(move || {
            let every = Duration::from_millis(SAMPLE_EVERY_MS);
            let mut next = Instant::now() + every;
            while soak.sampling.load(Ordering::SeqCst) {
                if Instant::now() < next {
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
                next += every;
                soak.sample(port);
            }
        })
    }

    /// One `/health` resource sample, then the fixed-cadence progress line.
    fn sample(&self, port: u16) {
        let h = health(port).ok();
        let rss = h.as_ref().and_then(|h| json::get_i64(h, "rssBytes"));
        if let Some(h) = &h {
            self.ev(
                "sample",
                "resource",
                json::obj(vec![
                    ("rss", opt(rss)),
                    ("clients", opt(json::get_i64(h, "clients"))),
                ]),
            );
        }
        let line = {
            let mut o = self.obs();
            o.samples += 1;
            if let Some(r) = rss {
                o.rss_peak = Some(o.rss_peak.map_or(r, |p| p.max(r)));
                o.rss_baseline.get_or_insert(r);
            }
            let mut line = format!(
                "progress: phase {}, pongs {}/{}, failures {}, rss {}",
                o.phase,
                o.pongs_received,
                o.pongs_expected,
                o.failures.len(),
                shown(rss)
            );
            if self.long() {
                let left = self.duration_s.saturating_sub(self.t0.elapsed().as_secs());
                line.push_str(&format!(", cycle {}, {left}s left", o.cycle));
            }
            line
        };
        println!("[t+{:.1}s] {line}", self.secs());
    }

    /* -------------------------------------------------------------- */
    /* run                                                             */
    /* -------------------------------------------------------------- */

    fn main(self: &Arc<Self>) -> i32 {
        self.progress(&format!(
            "run dir {} (server {})",
            self.out.display(),
            self.exe.display()
        ));
        let (main1, port, diag) = match self.spawn_server("main", &self.out.join("home-a"), None) {
            Ok(v) => v,
            Err(e) => return self.finish(Some(e)),
        };
        self.progress(&format!("server up on :{port} (diag :{diag})"));
        let h0 = match wait_health(port) {
            Ok(h) => h,
            Err(e) => {
                let _ = self.stop(&main1, 10_000);
                return self.finish(Some(e));
            }
        };
        self.obs().rss_baseline = json::get_i64(&h0, "rssBytes");
        if json::get_i64(&h0, "protocol") != Some(i64::from(PROTOCOL_VERSION)) {
            let got = json::get(&h0, "protocol").map_or("null".into(), json::stringify);
            self.fail(
                "boot",
                "connection_failure",
                &format!("unexpected protocol {got}"),
            );
        }
        self.ev("boot", "health", h0);
        let sampler = self.start_sampler(port);

        self.phase("auth", || self.phase_auth(port));
        self.phase("fifo", || {
            if !self.inject {
                return self.phase_fifo(port, self.clients, if self.long() { 80 } else { 40 });
            }
            // failure-injection demo: let real traffic flow first, then pull
            // the plug without announcing it
            self.phase_fifo(port, self.clients.min(3), 15)?;
            self.progress(
                "INJECT: killing server without a harness stop (expect unexpected_server_exit)",
            );
            let _ = main1.child.lock().unwrap().kill(); // deliberately NOT stop() → must be detected
            thread::sleep(Duration::from_millis(1500));
            Ok(())
        });
        self.phase("churn", || self.phase_churn(port, self.churn_cycles, 8));
        self.phase("reconnect", || self.phase_reconnect(port));
        if self.long() {
            self.phase("load-loop", || {
                self.quiet.store(true, Ordering::SeqCst);
                let r = self.load_loop(port);
                self.quiet.store(false, Ordering::SeqCst);
                r
            });
            self.phase("saturation", || {
                let l = self.probe_state_size(port, "probe-long-sat")?;
                self.phase_saturation(port, "sat-final", l)
            });
        } else {
            self.phase("saturation", || self.saturation_paths(port));
        }
        self.phase("dead-peer", || self.phase_dead_peer(port));
        self.phase("writer-failure", || self.phase_writer_failure(port));
        self.phase("quiesce", || self.phase_quiesce(port));

        if self.aborted() {
            self.progress(if self.inject {
                "injected fault detected — writing summary"
            } else {
                "ABORTED — writing summary"
            });
        } else if !self.inject {
            self.phase("graceful", || self.phase_graceful());
            self.phase("restart", || self.phase_restart(&main1));
        }

        self.sampling.store(false, Ordering::SeqCst);
        let _ = sampler.join();
        let _ = self.stop(&main1, 10_000); // no-op once it exited
        self.finish(None)
    }

    /// Writes summary.json, prints one PASS/FAIL line per criterion, and
    /// returns the exit code. `fatal` = the run could not start at all.
    fn finish(&self, fatal: Option<String>) -> i32 {
        if let Some(e) = &fatal {
            self.fail("boot", "exception", e);
        }
        let detected = self.inject.then(|| !self.obs().unexpected_exits.is_empty());
        let head_sha = git_head();
        let mode = if self.long() { "long" } else { "smoke" };
        let head = vec![
            ("harness", json::s("chernobyl-companion --soak")),
            ("mode", json::s(mode)),
            ("startedAt", json::s(&iso_utc(self.started_ms))),
            ("endedAt", json::s(&iso_utc(now_ms()))),
            (
                "durationS",
                Json::Float((self.secs() * 10.0).round() / 10.0),
            ),
            ("gitHead", head_sha.as_deref().map_or(Json::Null, json::s)),
            ("version", json::s(env!("CARGO_PKG_VERSION"))),
            (
                "platform",
                json::s(&format!(
                    "{} {}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )),
            ),
            (
                "config",
                json::obj(vec![
                    ("durationS", int(self.duration_s)),
                    ("clients", int(self.clients)),
                    ("churnCycles", int(self.churn_cycles)),
                    (
                        "inject",
                        json::s(if self.inject { "server-kill" } else { "" }),
                    ),
                    ("out", json::s(&self.out.display().to_string())),
                ]),
            ),
        ];
        let mut extra = vec![
            (
                "injected",
                if self.inject {
                    json::s("server-kill")
                } else {
                    Json::Null
                },
            ),
            (
                "injectedFaultDetected",
                detected.map_or(Json::Null, Json::Bool),
            ),
        ];
        if let Some(e) = &fatal {
            extra.push(("fatal", json::s(e)));
        }
        let o = self.obs();
        let crits = criteria(&o);
        let summary = summary_json(&o, &crits, head, extra);
        let pass = json::get_bool(&summary, "pass") == Some(true);
        let path = self.out.join("summary.json");
        if let Err(e) = std::fs::write(&path, json::stringify(&summary) + "\n") {
            eprintln!("[soak] cannot write {}: {e}", path.display());
        }
        println!("\n=== soak summary ===");
        println!(
            "mode={mode} duration={:.1}s head={}",
            self.secs(),
            head_sha
                .as_deref()
                .map_or("null", |h| &h[..h.len().min(12)])
        );
        println!(
            "rss baseline={} peak={} final={} growth={}",
            shown(o.rss_baseline),
            shown(o.rss_peak),
            shown(o.rss_final),
            shown(o.growth())
        );
        for c in &crits {
            let verdict = if c.pass { "PASS" } else { "FAIL" };
            println!("  {verdict}  {}: {}", c.id, c.observed);
        }
        println!("failures={} pass={pass}", o.failures.len());
        println!("artifacts: {}", self.out.display());
        if fatal.is_some() {
            1
        } else if self.inject {
            // demo run: exit 1 iff the fault WAS detected (detection is the point)
            if detected == Some(true) { 1 } else { 3 }
        } else if pass {
            0
        } else {
            1
        }
    }

    /* -------------------------------------------------------------- */
    /* phases                                                          */
    /* -------------------------------------------------------------- */

    /// Criterion 3: wrong tokens must yield outcomes by design (4001/429) and
    /// must never receive a single byte of protocol data.
    fn phase_auth(&self, port: u16) -> Result<(), String> {
        // failures 0→3: three wrong tokens close 4001, no data
        for i in 1..=3 {
            let what = format!("wrong token #{i}");
            self.auth_check(port, &format!("auth-wrong-{i}"), &what, "closed-4001")?;
        }
        // good token resets the counter
        let mut c = self.connect(port, TOKEN, "auth-good-1")?;
        hello(&mut c, "auth-good-1")?;
        bye(c);
        // failures 1..LIMIT → that many more 4001s, the next attempt → 429
        let limit = AUTH_FAILURE_LIMIT + 1;
        for i in 1..=limit {
            let expected = if i < limit { "closed-4001" } else { "http-429" };
            let what = format!("wrong token b#{i}");
            self.auth_check(port, &format!("auth-wrong-b-{i}"), &what, expected)?;
        }
        // recovery after the cooldown trip
        let mut c = self.connect(port, TOKEN, "auth-good-2")?;
        hello(&mut c, "auth-good-2")?;
        ping_wait(&mut c, "auth-good-2", "auth-recover-ping")?;
        bye(c);
        let outcomes = tally_json(&self.obs().unauthorized);
        self.ev("auth", "auth_outcomes", outcomes);
        Ok(())
    }

    fn auth_check(&self, port: u16, label: &str, what: &str, expected: &str) -> Result<(), String> {
        let (outcome, texts) = self.auth_attempt(port, label)?;
        *self.obs().unauthorized.entry(outcome.clone()).or_default() += 1;
        if outcome != expected {
            let detail = format!("{what} got {outcome} (expected {expected})");
            self.fail("auth", "unauthorized_accepted", &detail);
        }
        if texts != 0 {
            let detail = format!("{what} received {texts} messages");
            self.fail("auth", "unauthorized_accepted", &detail);
        }
        Ok(())
    }

    /// One wrong-token attempt: `closed-<code>` (upgraded, then closed) or
    /// `http-<status>` (refused before the upgrade), plus the number of text
    /// frames that arrived before the close (must be 0).
    fn auth_attempt(&self, port: u16, label: &str) -> Result<(String, usize), String> {
        match self.connect(port, "wrong-token", label) {
            Ok(mut c) => wait_close(&mut c, 8000)
                .map(|(code, texts)| (format!("closed-{code}"), texts))
                .map_err(|e| format!("{label}: {e}")),
            // WsClient reports a refused upgrade as "handshake rejected: <status line>"
            Err(e) => match e
                .split("handshake rejected: ")
                .nth(1)
                .and_then(|line| line.split_whitespace().nth(1))
            {
                Some(status) => Ok((format!("http-{status}"), 0)),
                None => Err(e),
            },
        }
    }

    /// Criterion 2: pipelined pings per client must come back in send order.
    fn phase_fifo(&self, port: u16, clients: usize, pings: usize) -> Result<(), String> {
        let mut cs = Vec::new();
        for i in 0..clients {
            let label = format!("fifo-{i}");
            let mut c = self.connect(port, TOKEN, &label)?;
            hello(&mut c, &label)?;
            cs.push((label, c));
        }
        let want = |i: usize, j: usize| format!("f{i}-{j}");
        for (i, (_, c)) in cs.iter_mut().enumerate() {
            for j in 0..pings {
                // a refused send surfaces below as missing pongs
                let _ = c.send_text(&cmd("ping", &want(i, j)));
            }
        }
        self.obs().pongs_expected += (clients * pings) as u64;
        for (i, (label, mut c)) in cs.into_iter().enumerate() {
            let got = pongs(&mut c, &label, pings, 15_000);
            if got.len() < pings {
                let detail = format!("{label}: got {}/{pings} pongs", got.len());
                self.fail("fifo", "connection_failure", &detail);
            }
            self.obs().pongs_received += got.len() as u64;
            if got.len() == pings
                && let Some(j) = got.iter().enumerate().position(|(j, id)| *id != want(i, j))
            {
                self.obs().ordering_violations += 1;
                let detail = format!("{label}: at {j} expected {} got {}", want(i, j), got[j]);
                self.fail("fifo", "ordering_violation", &detail);
            }
            bye(c);
        }
        Ok(())
    }

    /// Criterion 4 (churn slice): every peer must come back to 0 after churn.
    fn phase_churn(&self, port: u16, cycles: usize, per_cycle: usize) -> Result<(), String> {
        for cy in 0..cycles {
            for k in 0..per_cycle {
                let label = format!("churn-{cy}-{k}");
                let session = (|| {
                    let mut c = self.connect(port, TOKEN, &label)?;
                    hello(&mut c, &label)?;
                    ping_wait(&mut c, &label, &format!("{label}-p"))?;
                    Ok::<_, String>(c)
                })();
                let reason = match session {
                    Ok(mut c) if k % 2 == 0 => {
                        let _ = c.close(1000);
                        wait_close(&mut c, 5000)
                            .map_or("closed".into(), |(code, _)| format!("close-{code}"))
                    }
                    Ok(c) => {
                        drop(c); // abrupt mid-session disappearance
                        "terminated".into()
                    }
                    Err(e) => {
                        self.obs().connects_failed += 1;
                        self.fail("churn", "connection_failure", &e);
                        continue;
                    }
                };
                *self.obs().disconnect_reasons.entry(reason).or_default() += 1;
            }
        }
        if !self.released(port, RELEASE_NORMAL_MS, 250)? {
            let n = clients(port).map_or("?".into(), |n| n.to_string());
            let detail =
                format!("{n} clients still registered after churn (limit {RELEASE_NORMAL_MS}ms)");
            self.fail("churn", "peer_leak", &detail);
        }
        self.ev(
            "churn",
            "released",
            json::obj(vec![("cycles", int(cycles)), ("perCycle", int(per_cycle))]),
        );
        Ok(())
    }

    /// Recovery: a 4001'd client reconnects with the right token and works.
    fn phase_reconnect(&self, port: u16) -> Result<(), String> {
        let _ = self.connect(port, "wrong-token", "recon-wrong"); // 4001/429 expected
        let mut c = self.connect(port, TOKEN, "recon-good")?;
        hello(&mut c, "recon-good")?;
        ping_wait(&mut c, "recon-good", "recon-ping")?;
        bye(c);
        Ok(())
    }

    fn probe_state_size(&self, port: u16, label: &str) -> Result<usize, String> {
        let mut c = self.connect(port, TOKEN, label)?;
        hello(&mut c, label)?;
        send(&mut c, label, &cmd("list_state", &format!("{label}-probe")))?;
        let (_, bytes) =
            next_matching(&mut c, label, "state reply", 8000, |v| is_type(v, "state"))?;
        bye(c);
        Ok(bytes)
    }

    /// Smoke saturation: the count-bound path at the boot snapshot's reply
    /// size, then the byte-bound path once seeded sessions push one state
    /// reply past 4 MiB / 256 = 16 KiB (the byte bound then trips first).
    fn saturation_paths(&self, port: u16) -> Result<(), String> {
        let l0 = self.probe_state_size(port, "probe-a")?;
        self.phase_saturation(port, "sat-count", l0)?;
        match self.seed_sessions(port, 17_000, 1000)? {
            Ok(l2) => {
                self.phase_saturation(port, "sat-bytes", l2)?;
                let reachable = expected_bound(l2) == "bytes";
                self.ev(
                    "saturation",
                    "byte_path",
                    json::obj(vec![
                        ("stateBytes", int(l2)),
                        ("reachable", Json::Bool(reachable)),
                    ]),
                );
            }
            Err(reason) => {
                self.ev(
                    "saturation",
                    "byte_path",
                    json::obj(vec![
                        ("reachable", Json::Bool(false)),
                        ("reason", json::s(reason)),
                    ]),
                );
                println!(
                    "  note: byte-bound path not reachable ({reason}); the count path was exercised — byte-bound proof lives in the outbound_queue_bounds_total_bytes unit test"
                );
            }
        }
        Ok(())
    }

    /// Criterion 5: a stopped-consuming client + reply flood → the D8
    /// teardown must be observed via /health release, then a fresh client
    /// reconnects (recovery after a server-side disconnect).
    fn phase_saturation(&self, port: u16, label: &str, reply_bytes: usize) -> Result<(), String> {
        let (n, bound) = (flood_size(reply_bytes), expected_bound(reply_bytes));
        let mut c = self.connect(port, TOKEN, label)?;
        hello(&mut c, label)?;
        // Stop consuming BEFORE the flood: no reply is ever read. The flood
        // has its own thread so the poll below times the teardown from the
        // first request (the Node harness's sends were buffered), and that
        // thread holds the unread socket open until the poll is over — only
        // the server can have released it. A refused send is the teardown
        // arriving mid-flood.
        let (hold, held) = mpsc::channel::<()>();
        let id = label.to_string();
        let t0 = Instant::now();
        thread::spawn(move || {
            for j in 0..n {
                if c.send_text(&cmd("list_state", &format!("{id}-{j}")))
                    .is_err()
                {
                    break;
                }
            }
            let _ = held.recv();
        });
        let released = self.released(port, RELEASE_SATURATION_MS, 500);
        drop(hold); // the Node harness's resume() + kill(): the flood thread drops the client
        let teardown_ms = t0.elapsed().as_millis() as u64;
        if released? {
            self.obs().teardowns.push(Teardown {
                label: label.into(),
                n,
                reply_bytes,
                expected_bound: bound,
                teardown_ms,
            });
            self.ev(
                label,
                "saturation_teardown",
                json::obj(vec![
                    ("n", int(n)),
                    ("replyBytes", int(reply_bytes)),
                    ("expectedBound", json::s(bound)),
                    ("teardownMs", int(teardown_ms)),
                ]),
            );
            self.progress(&format!(
                "{label}: D8 teardown in {teardown_ms}ms (n={n}, reply={reply_bytes}B, expected bound: {bound})"
            ));
        } else {
            let detail = format!(
                "still registered {RELEASE_SATURATION_MS}ms after {n} list_states (server must tear down, not buffer)"
            );
            self.fail(label, "d8_saturation_teardown", &detail);
        }
        let rec = format!("{label}-recovery");
        let mut c2 = self.connect(port, TOKEN, &rec)?;
        hello(&mut c2, &rec)?;
        ping_wait(&mut c2, &rec, &format!("{rec}-ping"))?;
        bye(c2);
        Ok(())
    }

    /// Creates sessions until one state reply reaches `target_bytes`:
    /// `Ok(Ok(state_bytes))`, or `Ok(Err(reason))` when no agent takes one.
    fn seed_sessions(
        &self,
        port: u16,
        target_bytes: usize,
        max_sessions: usize,
    ) -> Result<Result<usize, &'static str>, String> {
        let not_seeded = |reason: &'static str| {
            self.ev(
                "saturation",
                "seed",
                json::obj(vec![
                    ("ok", Json::Bool(false)),
                    ("reason", json::s(reason)),
                    ("created", int(0)),
                    ("stateBytes", int(0)),
                ]),
            );
            Ok(Err(reason))
        };
        let mut c = self.connect(port, TOKEN, "seed")?;
        let ack = hello(&mut c, "seed")?;
        let agents: Vec<String> = json::get(&ack, "snapshot")
            .and_then(|s| json::get(s, "agents"))
            .and_then(json::as_arr)
            .unwrap_or_default()
            .iter()
            .filter_map(|a| json::get_str(a, "id").map(str::to_string))
            .collect();
        // find an agent that accepts new sessions (one probe per candidate)
        let mut chosen = None;
        for agent in &agents {
            let id = format!("seed-probe-{agent}");
            send(
                &mut c,
                "seed",
                &create_session(&id, agent, "Soak session probe"),
            )?;
            let (ack, _) = next_matching(&mut c, "seed", "create ack", 8000, |v| {
                is_reply(v, "ack", &id)
            })?;
            if json::get_bool(&ack, "ok") == Some(true) {
                chosen = Some(agent.clone());
                break;
            }
        }
        let Some(agent) = chosen else {
            bye(c);
            return not_seeded(if agents.is_empty() {
                "snapshot has no agents"
            } else {
                "no agent accepted a session"
            });
        };
        let (mut created, mut state_bytes) = (1, 0);
        while created < max_sessions {
            let batch = 50.min(max_sessions - created);
            for j in created..created + batch {
                let msg = create_session(
                    &format!("seed-{agent}-{j}"),
                    &agent,
                    &format!("Soak session {j}"),
                );
                send(&mut c, "seed", &msg)?;
            }
            // replies are FIFO per connection: wait for each ack in turn
            for j in created..created + batch {
                let id = format!("seed-{agent}-{j}");
                next_matching(&mut c, "seed", "create ack batch", 15_000, |v| {
                    is_reply(v, "ack", &id)
                })?;
            }
            created += batch;
            send(&mut c, "seed", &cmd("list_state", "seed-probe"))?;
            state_bytes = next_matching(&mut c, "seed", "state", 8000, |v| is_type(v, "state"))?.1;
            if state_bytes >= target_bytes {
                break;
            }
        }
        bye(c);
        self.ev(
            "saturation",
            "seed",
            json::obj(vec![
                ("ok", Json::Bool(true)),
                ("created", int(created)),
                ("stateBytes", int(state_bytes)),
                ("agentId", json::s(&agent)),
            ]),
        );
        Ok(Ok(state_bytes))
    }

    /// Criteria 4/6: a silent peer that never answers heartbeats must be
    /// released by the heartbeat path (HEARTBEAT_MS x MISS_LIMIT ≈ 45-60 s).
    fn phase_dead_peer(&self, port: u16) -> Result<(), String> {
        let mut c = self.connect(port, TOKEN, "dead-peer")?;
        hello(&mut c, "dead-peer")?;
        // no reads → no pongs → the server counts heartbeat misses
        let t0 = Instant::now();
        let released = self.released(port, RELEASE_DEAD_PEER_MS, 1000)?;
        let ms = t0.elapsed().as_millis() as u64;
        if released {
            self.ev("dead-peer", "released", json::obj(vec![("ms", int(ms))]));
            self.progress(&format!("dead-peer released in {}s", ms / 1000));
        } else {
            let detail = format!("still registered after {RELEASE_DEAD_PEER_MS}ms");
            self.fail("dead-peer", "dead_peer_not_released", &detail);
        }
        drop(c);
        Ok(())
    }

    /// Criterion 4: abrupt client death mid-reply-stream must not wedge the
    /// server or leak the registry entry.
    fn phase_writer_failure(&self, port: u16) -> Result<(), String> {
        let mut c = self.connect(port, TOKEN, "writer-fail")?;
        hello(&mut c, "writer-fail")?;
        for j in 0..500 {
            if c.send_text(&cmd("list_state", &format!("wf-{j}"))).is_err() {
                break;
            }
        }
        drop(c); // RST while the server is mid-reply (unread replies)
        if !self.released(port, RELEASE_NORMAL_MS, 250)? {
            let detail = "client still registered after abrupt death";
            self.fail("writer-fail", "peer_leak", detail);
        }
        // the server must still serve a fresh client afterwards
        let mut c2 = self.connect(port, TOKEN, "writer-fail-after")?;
        hello(&mut c2, "writer-fail-after")?;
        ping_wait(&mut c2, "writer-fail-after", "wf-after-ping")?;
        bye(c2);
        Ok(())
    }

    fn phase_quiesce(&self, port: u16) -> Result<(), String> {
        let released = self.released(port, RELEASE_NORMAL_MS, 250)?;
        let h = health(port).ok();
        if !released {
            let n = h
                .as_ref()
                .and_then(|h| json::get_i64(h, "clients"))
                .map_or("?".into(), |n| n.to_string());
            let detail = format!("{n} clients still registered at quiesce");
            self.fail("quiesce", "peer_leak", &detail);
        }
        let Some(h) = h else {
            self.fail(
                "quiesce",
                "connection_failure",
                "/health unavailable at quiesce",
            );
            return Ok(());
        };
        self.obs().rss_final = json::get_i64(&h, "rssBytes");
        let keys = [
            "clients",
            "rssBytes",
            "commands",
            "unresolved",
            "malformed",
            "events",
            "sessions",
        ];
        let fields = keys
            .iter()
            .map(|&k| (k, json::get(&h, k).cloned().unwrap_or(Json::Null)))
            .collect();
        self.ev("quiesce", "health", json::obj(fields));
        Ok(())
    }

    /// Criterion 7: `--exit-after` shutdown with live clients + queued work
    /// must exit 0 and log its shutdown sequence.
    fn phase_graceful(self: &Arc<Self>) -> Result<(), String> {
        let home = self.out.join("home-graceful");
        let (s, port, _) = self.spawn_server("graceful", &home, Some(8000))?;
        let clients = (|| {
            let mut cs = Vec::new();
            for i in 0..3 {
                let label = format!("graceful-{i}");
                let mut c = self.connect(port, TOKEN, &label)?;
                hello(&mut c, &label)?;
                for j in 0..30 {
                    let _ = c.send_text(&cmd("ping", &format!("g-{label}-{j}")));
                }
                cs.push(c);
            }
            Ok::<_, String>(cs)
        })();
        // queued work in flight; the timed shutdown must still be clean.
        // Awaited even when a client failed, so the instance's own exit is
        // never misread as an unexpected one.
        let exited = self.expect_exit(&s, 8000 + 20_000);
        if exited.is_err() {
            let _ = self.stop(&s, 5_000); // hung: do not leave it behind
        }
        let _clients = clients?;
        let code = exited?;
        let log = read_lossy(&s.log);
        let g = Graceful {
            code,
            has_shutting_down: log.contains("shutting down"),
            has_bye: log.contains("[companion] bye"),
            ms: self.t0.elapsed().as_millis() as u64,
        };
        if code != Some(0) {
            let detail = format!("exit code {} (expected 0)", shown(code.map(i64::from)));
            self.fail("graceful", "graceful_shutdown_failed", &detail);
        }
        if !g.has_shutting_down {
            let detail = "stderr never logged \"shutting down\"";
            self.fail("graceful", "graceful_shutdown_failed", detail);
        }
        self.obs().graceful = Some(g);
        Ok(())
    }

    /// Criterion 8: restart on the same data dir must serve reconnecting clients.
    fn phase_restart(self: &Arc<Self>, main: &Server) -> Result<(), String> {
        self.stop(main, 15_000)?;
        let (s, port, _) = self.spawn_server("restart", &self.out.join("home-a"), None)?;
        let mut ok = 0;
        for i in 0..2 {
            let label = format!("restart-{i}");
            let served = (|| {
                let mut c = self.connect(port, TOKEN, &label)?;
                hello(&mut c, &label)?;
                ping_wait(&mut c, &label, &format!("restart-ping-{i}"))?;
                bye(c);
                Ok::<_, String>(())
            })();
            match served {
                Ok(()) => ok += 1,
                Err(e) => {
                    let detail = format!("client {i}: {e}");
                    self.fail("restart", "restart_recovery_failed", &detail);
                }
            }
        }
        self.obs().restart = Some((ok, 2));
        self.stop(&s, 15_000).map(drop)
    }

    /// Long mode: churn + FIFO every cycle, saturation every 3rd, dead peer
    /// every 10th, until the requested duration (from harness start) has
    /// really elapsed.
    fn load_loop(&self, port: u16) -> Result<(), String> {
        let mut cycle = 0;
        while self.t0.elapsed().as_secs() < self.duration_s && !self.aborted() {
            cycle += 1;
            self.obs().cycle = cycle;
            self.phase_churn(port, 3, 8)?;
            self.phase_fifo(port, self.clients, 60)?;
            if cycle % 3 == 0 {
                let l = self.probe_state_size(port, &format!("probe-{cycle}"))?;
                self.phase_saturation(port, &format!("sat-cycle-{cycle}"), l)?;
            }
            if cycle % 10 == 0 {
                self.phase_dead_peer(port)?;
            }
            let left = self.duration_s.saturating_sub(self.t0.elapsed().as_secs());
            self.ev(
                "load-loop",
                "cycle",
                json::obj(vec![("cycle", int(cycle)), ("remainingS", int(left))]),
            );
        }
        let actual = self.t0.elapsed().as_secs();
        self.ev(
            "load-loop",
            "duration",
            json::obj(vec![
                ("targetS", int(self.duration_s)),
                ("actualS", int(actual)),
            ]),
        );
        println!("  load loop: target {}s, ran {actual}s", self.duration_s);
        Ok(())
    }
}

/* ------------------------------------------------------------------ */
/* client helpers (in-repo WsClient + http_get)                        */
/* ------------------------------------------------------------------ */

fn cmd(kind: &str, id: &str) -> String {
    json::stringify(&json::obj(vec![
        ("type", json::s(kind)),
        ("commandId", json::s(id)),
    ]))
}

fn create_session(id: &str, agent: &str, title: &str) -> String {
    json::stringify(&json::obj(vec![
        ("type", json::s("create_session")),
        ("commandId", json::s(id)),
        ("agentId", json::s(agent)),
        ("title", json::s(title)),
        ("prompt", json::s("soak seed")),
    ]))
}

fn is_type(v: &Json, kind: &str) -> bool {
    json::get_str(v, "type") == Some(kind)
}

fn is_reply(v: &Json, kind: &str, id: &str) -> bool {
    is_type(v, kind) && json::get_str(v, "commandId") == Some(id)
}

fn send(c: &mut WsClient, label: &str, text: &str) -> Result<(), String> {
    c.send_text(text).map_err(|e| format!("{label}: {e}"))
}

fn ms_left(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64
}

/// Reads until a text frame satisfies `pred` within `ms` overall; frames in
/// between (fanned-out engine events) are skipped. Returns the parsed frame
/// and its UTF-8 byte length.
fn next_matching(
    c: &mut WsClient,
    label: &str,
    what: &str,
    ms: u64,
    pred: impl Fn(&Json) -> bool,
) -> Result<(Json, usize), String> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let left = ms_left(deadline);
        if left == 0 {
            return Err(format!("{label}: no {what} within {ms}ms"));
        }
        c.set_read_timeout_ms(left)?;
        match c.next_text_timeoutable() {
            Ok(Some(text)) => {
                let v = json::parse(&text).unwrap_or(Json::Null);
                if pred(&v) {
                    return Ok((v, text.len()));
                }
            }
            Ok(None) => {}
            Err(e) => return Err(format!("{label}: {e} while waiting for {what}")),
        }
    }
}

fn hello(c: &mut WsClient, label: &str) -> Result<Json, String> {
    let msg = json::obj(vec![
        ("type", json::s("hello")),
        ("clientId", json::s(label)),
        ("protocol", int(PROTOCOL_VERSION)),
    ]);
    send(c, label, &json::stringify(&msg))?;
    next_matching(c, label, "hello_ack", 8000, |v| is_type(v, "hello_ack")).map(|(v, _)| v)
}

fn ping_wait(c: &mut WsClient, label: &str, id: &str) -> Result<(), String> {
    send(c, label, &cmd("ping", id))?;
    let what = format!("pong {id}");
    next_matching(c, label, &what, 8000, |v| is_reply(v, "pong", id)).map(drop)
}

/// Pong commandIds in arrival order until `n` arrived or `ms` passed.
fn pongs(c: &mut WsClient, label: &str, n: usize, ms: u64) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut got = Vec::new();
    while got.len() < n {
        match next_matching(c, label, "pong", ms_left(deadline), |v| is_type(v, "pong")) {
            Ok((v, _)) => got.push(json::get_str(&v, "commandId").unwrap_or_default().into()),
            Err(_) => break,
        }
    }
    got
}

/// Reads until the peer closes. Returns the close code — 1005 for a close
/// frame without one, 1006 when the socket ended without a close frame (the
/// codes the ws library reported) — and how many text frames came first.
fn wait_close(c: &mut WsClient, ms: u64) -> Result<(u16, usize), String> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut texts = 0;
    loop {
        let left = ms_left(deadline);
        if left == 0 {
            return Err(format!("no close within {ms}ms"));
        }
        c.set_read_timeout_ms(left)?;
        match c.read_frame() {
            Ok(Frame::Text(_)) => texts += 1,
            Ok(Frame::Close(code, _)) => return Ok((code.unwrap_or(1005), texts)),
            Ok(_) => {}
            // read_frame carries io errors as Debug text: the read timeout
            // means the deadline (checked above), anything else the socket ended
            Err(e) if e.contains("TimedOut") || e.contains("WouldBlock") => {}
            Err(_) => return Ok((1006, texts)),
        }
    }
}

/// Clean close (1000), then wait for the server to drop the connection.
fn bye(mut c: WsClient) {
    let _ = c.close(1000);
    let _ = wait_close(&mut c, 5000);
}

fn health(port: u16) -> Result<Json, String> {
    let raw = http_get(
        &format!("127.0.0.1:{port}"),
        &format!("/health?token={TOKEN}"),
    )?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or("health: response has no header terminator")?;
    match head.split_whitespace().nth(1) {
        Some("200") => json::parse(body).map_err(|e| format!("health: {e}")),
        status => Err(format!("health: HTTP {}", status.unwrap_or("?"))),
    }
}

fn clients(port: u16) -> Result<i64, String> {
    json::get_i64(&health(port)?, "clients").ok_or_else(|| "health: no clients gauge".into())
}

fn wait_health(port: u16) -> Result<Json, String> {
    let mut last = String::from("server /health never answered");
    for _ in 0..100 {
        match health(port) {
            Ok(h) => return Ok(h),
            Err(e) => last = e,
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

/* ------------------------------------------------------------------ */
/* small utilities                                                     */
/* ------------------------------------------------------------------ */

/// Saturation flood size for a measured reply size: enough requests for
/// FLOOD_TARGET_BYTES of replies, clamped to [512, 40 000].
fn flood_size(reply_bytes: usize) -> usize {
    (FLOOD_TARGET_BYTES.div_ceil(reply_bytes.max(512)) + 1024).clamp(512, 40_000)
}

/// Which outbound bound a stalled consumer of `reply_bytes`-sized replies
/// trips first.
fn expected_bound(reply_bytes: usize) -> &'static str {
    if OUTBOUND_QUEUE_MAX_MESSAGES * reply_bytes > OUTBOUND_QUEUE_MAX_BYTES {
        "bytes"
    } else {
        "count"
    }
}

/// The port a boot log announced after `marker` (`listening on 127.0.0.1:N`).
fn announced_port(log: &str, marker: &str) -> Option<u16> {
    let at = log.find(marker)? + marker.len();
    log[at..]
        .split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// A log the child may still be writing: never fails on a half-written char.
fn read_lossy(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
}

/// UTC `Date.toISOString()` shape (`2026-10-26T12:34:56.789Z`). std has no
/// calendar, so days → civil date uses Howard Hinnant's algorithm.
fn iso_utc(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// The crate directory baked in at build time, when it still exists.
fn crate_dir() -> Option<PathBuf> {
    Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))).filter(|d| d.is_dir())
}

/// Best effort: the checkout's HEAD (null outside a git checkout).
fn git_head() -> Option<String> {
    let mut git = Command::new("git");
    git.args(["rev-parse", "HEAD"]);
    if let Some(dir) = crate_dir() {
        git.current_dir(dir);
    }
    let out = git.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Observations of a clean smoke run.
    fn clean() -> Obs {
        Obs {
            pongs_expected: 245,
            pongs_received: 245,
            graceful: Some(Graceful {
                code: Some(0),
                has_shutting_down: true,
                has_bye: true,
                ms: 70_000,
            }),
            restart: Some((2, 2)),
            rss_baseline: Some(5_000_000),
            rss_peak: Some(7_000_000),
            rss_final: Some(6_000_000),
            ..Obs::default()
        }
    }

    fn failed(o: &Obs) -> Vec<&'static str> {
        criteria(o)
            .into_iter()
            .filter(|c| !c.pass)
            .map(|c| c.id)
            .collect()
    }

    fn failure(code: &str) -> Failure {
        Failure {
            phase: "p".into(),
            code: code.into(),
            detail: "d".into(),
        }
    }

    #[test]
    fn a_clean_run_passes_all_ten_criteria_in_order() {
        let ids: Vec<_> = criteria(&clean()).iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                "unexpected_server_exit",
                "ordering_violation",
                "unauthorized_accepted",
                "peer_leak",
                "d8_saturation_teardown",
                "dead_peer_not_released",
                "graceful_shutdown_failed",
                "restart_recovery_failed",
                "resource_growth",
                "connection_failure",
            ]
        );
        assert!(failed(&clean()).is_empty());
    }

    #[test]
    fn each_failure_code_fails_exactly_its_criterion() {
        for (code, id) in [
            ("unauthorized_accepted", "unauthorized_accepted"),
            ("peer_leak", "peer_leak"),
            ("d8_saturation_teardown", "d8_saturation_teardown"),
            ("dead_peer_not_released", "dead_peer_not_released"),
            ("graceful_shutdown_failed", "graceful_shutdown_failed"),
            ("restart_recovery_failed", "restart_recovery_failed"),
            ("connection_failure", "connection_failure"),
            ("exception", "connection_failure"),
        ] {
            let mut o = clean();
            o.failures.push(failure(code));
            assert_eq!(failed(&o), [id], "{code}");
        }
    }

    #[test]
    fn process_ordering_and_lifecycle_criteria_need_their_own_evidence() {
        let mut o = clean();
        o.unexpected_exits.push(Exited {
            server: "main",
            code: Some(1),
            at_ms: 1,
        });
        assert_eq!(failed(&o), ["unexpected_server_exit"]);
        // a missing pong fails ordering even with zero reorders
        let mut o = clean();
        o.pongs_received -= 1;
        assert_eq!(failed(&o), ["ordering_violation"]);
        let mut o = clean();
        o.ordering_violations = 1;
        assert_eq!(failed(&o), ["ordering_violation"]);
        // graceful + restart need positive evidence: not run, wrong exit
        // code, no shutdown line, partial recovery all fail
        let mut o = clean();
        o.graceful = None;
        assert_eq!(failed(&o), ["graceful_shutdown_failed"]);
        let mut o = clean();
        o.graceful.as_mut().unwrap().code = Some(1);
        assert_eq!(failed(&o), ["graceful_shutdown_failed"]);
        let mut o = clean();
        o.graceful.as_mut().unwrap().has_shutting_down = false;
        assert_eq!(failed(&o), ["graceful_shutdown_failed"]);
        let mut o = clean();
        o.restart = None;
        assert_eq!(failed(&o), ["restart_recovery_failed"]);
        let mut o = clean();
        o.restart = Some((1, 2));
        assert_eq!(failed(&o), ["restart_recovery_failed"]);
    }

    #[test]
    fn rss_limits_are_inclusive_and_a_quiesced_sample_is_required() {
        let mut o = clean();
        o.rss_final = Some(5_000_000 + RSS_GROWTH_LIMIT_BYTES);
        o.rss_peak = Some(RSS_ABSOLUTE_LIMIT_BYTES);
        assert!(failed(&o).is_empty());
        o.rss_final = Some(5_000_000 + RSS_GROWTH_LIMIT_BYTES + 1);
        assert_eq!(failed(&o), ["resource_growth"]);
        let mut o = clean();
        o.rss_peak = Some(RSS_ABSOLUTE_LIMIT_BYTES + 1);
        assert_eq!(failed(&o), ["resource_growth"]);
        // no baseline: growth unknown, still judged on peak + final
        let mut o = clean();
        o.rss_baseline = None;
        assert!(failed(&o).is_empty());
        let mut o = clean();
        o.rss_final = None;
        assert_eq!(failed(&o), ["resource_growth"]);
    }

    #[test]
    fn summary_json_carries_criteria_thresholds_failures_and_verdict() {
        let o = clean();
        let doc = summary_json(
            &o,
            &criteria(&o),
            vec![("mode", json::s("smoke"))],
            vec![("injected", Json::Null)],
        );
        let v = json::parse(&json::stringify(&doc)).unwrap();
        assert_eq!(json::get_str(&v, "mode"), Some("smoke"));
        assert_eq!(json::get(&v, "injected"), Some(&Json::Null));
        assert_eq!(json::get_bool(&v, "pass"), Some(true));
        let crits = json::get(&v, "criteria").and_then(json::as_arr).unwrap();
        assert_eq!(crits.len(), 10);
        for c in crits {
            for k in ["id", "requirement", "observed"] {
                assert!(json::get_str(c, k).is_some(), "{k}");
            }
            assert_eq!(json::get_bool(c, "pass"), Some(true));
        }
        let t = json::get(&v, "thresholds").unwrap();
        let existing = json::get(t, "existingLimits").unwrap();
        for k in [
            "outboundMaxMessages",
            "outboundMaxBytes",
            "authFailureLimit",
            "heartbeatMs",
            "heartbeatMissLimit",
        ] {
            assert!(json::get_i64(existing, k).is_some_and(|n| n > 0), "{k}");
        }
        assert!(
            json::get_str(existing, "source")
                .unwrap()
                .contains("server.rs")
        );
        let proposed = json::get(t, "proposedLimits").unwrap();
        assert_eq!(
            json::get_i64(proposed, "rssGrowthLimitBytes"),
            Some(64 << 20)
        );
        assert_eq!(
            json::get_i64(proposed, "rssAbsoluteLimitBytes"),
            Some(512 << 20)
        );
        assert_eq!(json::get_i64(proposed, "releaseDeadPeerMs"), Some(150_000));
        assert!(json::get_str(proposed, "note").is_some());
        for k in ["counts", "servers", "resources"] {
            assert!(matches!(json::get(&v, k), Some(Json::Obj(_))), "{k}");
        }
        assert_eq!(json::get(&v, "failures"), Some(&Json::Arr(vec![])));

        // a failure is listed with its phase and flips the verdict
        let mut o = clean();
        o.failures.push(Failure {
            phase: "fifo".into(),
            code: "connection_failure".into(),
            detail: "fifo-0: got 3/40 pongs".into(),
        });
        let v = summary_json(&o, &criteria(&o), vec![], vec![]);
        assert_eq!(json::get_bool(&v, "pass"), Some(false));
        let f = &json::get(&v, "failures").and_then(json::as_arr).unwrap()[0];
        assert_eq!(json::get_str(f, "phase"), Some("fifo"));
        assert_eq!(json::get_str(f, "code"), Some("connection_failure"));
        // a failure outside the ten criteria still fails the run
        let mut o = clean();
        o.failures.push(failure("spawn_error"));
        assert!(failed(&o).is_empty());
        let v = summary_json(&o, &criteria(&o), vec![], vec![]);
        assert_eq!(json::get_bool(&v, "pass"), Some(false));
    }

    #[test]
    fn helpers_match_the_node_harness() {
        // Date.toISOString() shape, leap day included
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            iso_utc(1_792_972_800_000 + 45_296_789),
            "2026-10-26T12:34:56.789Z"
        );
        // flood sizing (Node: min(40000, max(512, ceil(40 MiB / max(L, 512)) + 1024)))
        assert_eq!(flood_size(8107), 6198);
        assert_eq!(flood_size(38_334), 2119);
        assert_eq!(flood_size(0), 40_000);
        assert_eq!(flood_size(10_000_000), 1029);
        // 4 MiB / 256 = 16 KiB per reply is where the byte bound trips first
        assert_eq!(expected_bound(16 * 1024), "count");
        assert_eq!(expected_bound(16 * 1024 + 1), "bytes");
        // boot-log port announcements
        let log = "[companion] listening on 127.0.0.1:50123 (protocol v2, token auth on)\n\
                   [companion] diagnostics on 127.0.0.1:50124\n";
        assert_eq!(announced_port(log, "listening on "), Some(50123));
        assert_eq!(announced_port(log, "diagnostics on "), Some(50124));
        assert_eq!(
            announced_port("[companion] wrote default config", "listening on "),
            None
        );
        // --soak argument
        assert_eq!(parse_mode("smoke"), Some(0));
        assert_eq!(parse_mode("86400"), Some(86_400));
        for bad in ["", "0", "-5", "long", "1.5"] {
            assert_eq!(parse_mode(bad), None, "{bad}");
        }
    }
}
