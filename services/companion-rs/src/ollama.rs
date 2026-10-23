//! Ollama adapter (Phase 7 — PRD §7.7): attach-only HTTP client for a local
//! Ollama model server. Ollama is a **model-runtime provider**, not a session
//! manager: this adapter reports runtime health/version, the installed-model
//! inventory and per-model metadata, and which models are currently loaded.
//! Session creation, prompt submission and permission review are honestly
//! reported `unavailable` — they belong to the agent framework (OpenCode,
//! Hermes, …) that *backs* Ollama, never to the model server itself (PRD §7.7:
//! "Ollama alone does not represent the full session manager"; "do not create
//! an implicit chat-history store and claim it is an existing agent session").
//!
//! Probe rules carried from the OpenCode adapter (adapter-sdk G4):
//!  * **G4.3** attach-only — never start a second Ollama; a dead endpoint is
//!    `runtime-unavailable`, not a spawn request.
//!  * **G4.2** status codes prove nothing — every response is content-type +
//!    shape validated before it is trusted (an HTML page with HTTP 200 must
//!    never parse as success).
//!
//! Endpoints (all loopback HTTP, live-verified against Ollama 0.40.9 on
//! `127.0.0.1:11434`, 2026-10-23):
//!  * `GET /api/version` → `{"version":"0.40.9"}`
//!  * `GET /api/tags`    → installed models (`models[]` with `details{}`)
//!  * `GET /api/ps`      → currently loaded/running models

use crate::adapters::Capability;
use crate::client::{HttpResponse, http_request};
use crate::json::{self, Json};
use crate::opencode::AdapterError;
use std::sync::Mutex;

/* ------------------------------------------------------------------ */
/* Model inventory                                                     */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, PartialEq)]
pub struct OllamaModel {
    pub name: String,
    pub size: i64,
    pub digest: String,
    pub modified_at: String,
    pub family: String,
    pub parameter_size: String,
    pub quantization: String,
    pub capabilities: Vec<String>,
    /// Whether the model is currently loaded (present in `/api/ps`).
    pub loaded: bool,
}

impl OllamaModel {
    pub fn to_json(&self) -> Json {
        json::obj(vec![
            ("name", json::s(&self.name)),
            ("size", Json::Int(self.size)),
            ("digest", json::s(&self.digest)),
            ("modifiedAt", json::s(&self.modified_at)),
            ("family", json::s(&self.family)),
            ("parameterSize", json::s(&self.parameter_size)),
            ("quantization", json::s(&self.quantization)),
            (
                "capabilities",
                Json::Arr(self.capabilities.iter().map(|c| json::s(c)).collect()),
            ),
            ("loaded", Json::Bool(self.loaded)),
        ])
    }
}

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

#[derive(Default)]
struct OlState {
    healthy: bool,
    detail: Option<String>,
    version: String,
    models: Vec<OllamaModel>,
}

pub struct OllamaAdapter {
    pub id: String,
    endpoint: String,
    state: Mutex<OlState>,
}

/* ------------------------------------------------------------------ */
/* Construction + public surface                                       */
/* ------------------------------------------------------------------ */

impl OllamaAdapter {
    pub fn new(id: &str, endpoint: &str) -> OllamaAdapter {
        let ep = endpoint.trim();
        let ep = if ep.starts_with("http://") || ep.starts_with("https://") {
            ep.to_string()
        } else {
            format!("http://{ep}")
        };
        OllamaAdapter {
            id: id.to_string(),
            endpoint: ep.trim_end_matches('/').to_string(),
            state: Mutex::new(OlState::default()),
        }
    }

    /// PRD §7.3 honest capability declaration. Structural rules (tested):
    /// unique ids, every non-`available` entry carries a `detail`, and at
    /// least one entry is not `available`. Session/prompt/permission are
    /// `unavailable` because a model server is not a session manager.
    pub fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::new("server.discovery", "available", None),
            Capability::new("server.health", "available", None),
            Capability::new("model.list", "available", None),
            Capability::new("model.metadata", "available", None),
            Capability::new("model.running", "available", None),
            Capability::new(
                "model.generate",
                "unavailable",
                Some(
                    "only via an explicitly configured CHERNOBYL-managed agent workflow; \
                     Ollama alone is not a session manager (PRD §7.7)",
                ),
            ),
            Capability::new(
                "session.list",
                "unavailable",
                Some(
                    "Ollama is a model runtime, not a session manager — list sessions \
                     through the agent framework that backs it",
                ),
            ),
            Capability::new(
                "session.create",
                "unavailable",
                Some("same as session.list: sessions belong to the agent framework"),
            ),
            Capability::new(
                "session.prompt",
                "unavailable",
                Some("same as session.list: prompting belongs to the agent framework"),
            ),
            Capability::new(
                "permission.review",
                "unavailable",
                Some(
                    "a model server surfaces no permission approval surface; approvals \
                     are owned by the agent framework / engine (G2)",
                ),
            ),
            Capability::new(
                "server.spawn",
                "unavailable",
                Some(
                    "attach-only by design (G4.3): never start a second Ollama on a \
                     possibly-shared model store",
                ),
            ),
        ]
    }

    /// Health + inventory probe; latches failure into `healthy`/`detail`.
    pub fn refresh(&self) -> Result<(), AdapterError> {
        let outcome: Result<(), AdapterError> = (|| {
            let version = self.fetch_version()?;
            let mut models = self.fetch_models()?;
            let running = self.fetch_running()?;
            for m in models.iter_mut() {
                m.loaded = running.iter().any(|r| r == &m.name);
            }
            {
                let mut st = self.state.lock().unwrap();
                st.healthy = true;
                st.detail = None;
                st.version = version;
                st.models = models;
            }
            Ok(())
        })();
        if let Err(e) = &outcome {
            let mut st = self.state.lock().unwrap();
            st.healthy = false;
            st.detail = Some(e.to_string());
        }
        outcome
    }

    pub fn is_healthy(&self) -> bool {
        self.state.lock().unwrap().healthy
    }

    pub fn detail(&self) -> Option<String> {
        self.state.lock().unwrap().detail.clone()
    }

    pub fn version(&self) -> String {
        self.state.lock().unwrap().version.clone()
    }

    pub fn models(&self) -> Vec<OllamaModel> {
        self.state.lock().unwrap().models.clone()
    }

    /// Names of the currently loaded (running) models.
    pub fn running_models(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .models
            .iter()
            .filter(|m| m.loaded)
            .map(|m| m.name.clone())
            .collect()
    }

    /// The model inventory as JSON (diagnostics / Phase 8 TUI surface).
    pub fn inventory_json(&self) -> Json {
        Json::Arr(self.models().iter().map(|m| m.to_json()).collect())
    }
}

/* ------------------------------------------------------------------ */
/* HTTP path                                                           */
/* ------------------------------------------------------------------ */

impl OllamaAdapter {
    fn req(&self, path: &str) -> Result<HttpResponse, AdapterError> {
        let url = format!("{}{}", self.endpoint, path);
        http_request("GET", &url, None, None).map_err(|e| {
            if e.starts_with("connect") || e.starts_with("resolve") {
                AdapterError::RuntimeUnavailable(e)
            } else {
                AdapterError::Unknown(e)
            }
        })
    }

    fn fetch_version(&self) -> Result<String, AdapterError> {
        let resp = self.req("/api/version")?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "version answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        Ok(json::get_str(&v, "version")
            .unwrap_or("(unknown)")
            .to_string())
    }

    fn fetch_models(&self) -> Result<Vec<OllamaModel>, AdapterError> {
        let resp = self.req("/api/tags")?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "model list answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let Some(list) = json::get(&v, "models").and_then(json::as_arr) else {
            return Err(AdapterError::InvalidResponse(
                "model list response has no `models` array".into(),
            ));
        };
        list.iter().map(parse_model).collect()
    }

    fn fetch_running(&self) -> Result<Vec<String>, AdapterError> {
        let resp = self.req("/api/ps")?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "running models answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        // `/api/ps` may answer `{"models":[]}` when nothing is loaded.
        let Some(list) = json::get(&v, "models").and_then(json::as_arr) else {
            return Ok(Vec::new());
        };
        Ok(list
            .iter()
            .filter_map(|m| json::get_str(m, "name").map(str::to_string))
            .collect())
    }
}

/// Parse one `/api/tags` entry. `details{}` fields are optional across
/// Ollama versions, so each degrades to an empty string rather than failing
/// the whole inventory.
fn parse_model(m: &Json) -> Result<OllamaModel, AdapterError> {
    let name = json::get_str(m, "name")
        .or_else(|| json::get_str(m, "model"))
        .ok_or_else(|| AdapterError::InvalidResponse("model entry has no `name`".into()))?
        .to_string();
    let details = json::get(m, "details");
    let dstr = |k: &str| {
        details
            .and_then(|d| json::get_str(d, k))
            .unwrap_or("")
            .to_string()
    };
    let capabilities = json::get(m, "capabilities")
        .and_then(json::as_arr)
        .map(|a| {
            a.iter()
                .filter_map(|c| json::as_str(c).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(OllamaModel {
        name,
        size: json::get_i64(m, "size").unwrap_or(0),
        digest: json::get_str(m, "digest").unwrap_or("").to_string(),
        modified_at: json::get_str(m, "modified_at").unwrap_or("").to_string(),
        family: dstr("family"),
        parameter_size: dstr("parameter_size"),
        quantization: dstr("quantization_level"),
        capabilities,
        loaded: false,
    })
}

/// G4.2: content-type first, then shape — an HTML page with HTTP 200 must
/// never parse as success (status alone proves nothing).
fn validate_json(resp: &HttpResponse) -> Result<Json, AdapterError> {
    if !resp.content_type.contains("application/json") {
        let ct = if resp.content_type.is_empty() {
            "(no content-type)".to_string()
        } else {
            resp.content_type.clone()
        };
        return Err(AdapterError::InvalidResponse(format!(
            "expected application/json, got {ct} (HTTP {} — status alone proves nothing)",
            resp.status
        )));
    }
    json::parse(&resp.body)
        .map_err(|e| AdapterError::InvalidResponse(format!("body is not valid JSON: {e:?}")))
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn respond(stream: &mut TcpStream, status: &str, ct: &str, body: &str) {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }

    /// Fake Ollama: serves `/api/version`, `/api/tags`, `/api/ps`. `html`
    /// forces every route to answer the SPA-style HTML page (G4.2 probe).
    struct Fake {
        addr: String,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr.trim_start_matches("http://"));
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    const TAGS: &str = r#"{"models":[{"name":"qwen3.5:4b","model":"qwen3.5:4b","modified_at":"2026-03-24T03:36:46.9011567+05:30","size":3389983735,"digest":"2a654d98e6fba55d452b7043684e9b57a947e393bbffa62485a7aac05ee4eefd","details":{"parent_model":"","format":"gguf","family":"qwen35","families":["qwen35"],"parameter_size":"4.7B","quantization_level":"Q4_K_M","context_length":262144,"embedding_length":2560,"runner":"ggml"},"capabilities":["completion","vision","tools","thinking"]}]}"#;

    fn start(html: bool) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("http://127.0.0.1:{port}");
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (addr.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let s = s.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match stream.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                    }
                    let mut parts = req_line.split_whitespace();
                    let _method = parts.next().unwrap_or("GET").to_string();
                    let path = parts.next().unwrap_or("/").to_string();
                    if s.load(Ordering::SeqCst) {
                        return;
                    }
                    if html {
                        respond(&mut stream, "200 OK", "text/html", "<html>app shell</html>");
                        return;
                    }
                    match path.as_str() {
                        "/api/version" => respond(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            r#"{"version":"0.40.9"}"#,
                        ),
                        "/api/tags" => respond(&mut stream, "200 OK", "application/json", TAGS),
                        "/api/ps" => respond(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            r#"{"models":[{"name":"qwen3.5:4b"}]}"#,
                        ),
                        _ => respond(&mut stream, "404 Not Found", "text/plain", "nope"),
                    }
                });
            }
        });
        let _ = a;
        Fake {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn adapter(f: &Fake) -> OllamaAdapter {
        OllamaAdapter::new("ol-1", &f.addr)
    }

    #[test]
    fn parses_inventory_and_maps_loaded_flag() {
        let f = start(false);
        let ad = adapter(&f);
        ad.refresh().expect("healthy fake must probe");
        assert!(ad.is_healthy());
        assert_eq!(ad.version(), "0.40.9");
        let models = ad.models();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.name, "qwen3.5:4b");
        assert_eq!(m.size, 3389983735);
        assert_eq!(m.family, "qwen35");
        assert_eq!(m.parameter_size, "4.7B");
        assert_eq!(m.quantization, "Q4_K_M");
        assert_eq!(
            m.capabilities,
            vec!["completion", "vision", "tools", "thinking"]
        );
        // /api/ps listed it as loaded
        assert!(m.loaded);
        assert_eq!(ad.running_models(), vec!["qwen3.5:4b"]);
    }

    #[test]
    fn html_with_200_is_rejected_g4_2() {
        let f = start(true);
        let ad = adapter(&f);
        let err = ad.refresh().expect_err("HTML must not parse as success");
        assert_eq!(err.code(), "invalid-response");
        assert!(!ad.is_healthy());
        assert!(ad.detail().unwrap().contains("application/json"));
    }

    #[test]
    fn dead_endpoint_latches_runtime_unavailable() {
        // port 1 on loopback is never listening
        let ad = OllamaAdapter::new("ol-down", "http://127.0.0.1:1");
        let err = ad.refresh().expect_err("dead endpoint must fail");
        assert_eq!(err.code(), "runtime-unavailable");
        assert!(!ad.is_healthy());
        assert!(ad.detail().unwrap().contains("connect"));
    }

    #[test]
    fn capability_declaration_is_structurally_honest() {
        let caps = adapter(&start(false)).capabilities();
        let mut ids: Vec<&str> = caps.iter().map(|c| c.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "capability ids must be unique");
        // every non-available entry carries a detail; >=1 is not available
        for c in &caps {
            if c.status != "available" {
                assert!(
                    c.detail.as_deref().unwrap_or("").len() > 10,
                    "capability '{}' is '{}' but has no explanatory detail",
                    c.id,
                    c.status
                );
            }
        }
        assert!(caps.iter().any(|c| c.status != "available"));
        // a model server must never advertise session management as available
        for sid in ["session.list", "session.create", "session.prompt"] {
            let c = caps.iter().find(|c| c.id == sid).expect(sid);
            assert_eq!(c.status, "unavailable", "{sid} must not be faked available");
        }
    }

    #[test]
    fn inventory_json_is_an_array_of_models() {
        let f = start(false);
        let ad = adapter(&f);
        ad.refresh().unwrap();
        let text = crate::json::stringify(&ad.inventory_json());
        assert!(text.starts_with('['));
        assert!(text.contains("\"name\":\"qwen3.5:4b\""));
        assert!(text.contains("\"loaded\":true"));
    }

    /// Opt-in live probe against a real local Ollama (PRD §7.7). Only runs
    /// when `CHERNOBYL_LIVE_OLLAMA=1`; the endpoint defaults to the standard
    /// loopback port. Run: `CHERNOBYL_LIVE_OLLAMA=1 cargo test ollama_live --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ollama_live_probe() {
        if std::env::var("CHERNOBYL_LIVE_OLLAMA").as_deref() != Ok("1") {
            eprintln!("skipped: set CHERNOBYL_LIVE_OLLAMA=1 to probe a real Ollama");
            return;
        }
        let ep = std::env::var("CHERNOBYL_OLLAMA_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".into());
        let ad = OllamaAdapter::new("ol-live", &ep);
        ad.refresh().expect("real Ollama must probe healthy");
        assert!(ad.is_healthy());
        println!(
            "ollama {} — {} model(s), {} loaded",
            ad.version(),
            ad.models().len(),
            ad.running_models().len()
        );
    }
}
