//! Companion configuration — exact port of `services/companion/src/config.ts`.
//! JSON file, versioned, fail-closed on anything a newer companion wrote.
//! `CHERNOBYL_HOME` overrides the default location (tests, portable installs).

use crate::json::{self, Json};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdapterEntryConfig {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    /// `opencode` entries: where the EXISTING server listens, required for
    /// that kind (attach-only — see ADR/G4.3, no port guessing).
    pub endpoint: Option<String>,
    /// `opencode` entries: `user:pass` for a server started with
    /// `OPENCODE_SERVER_PASSWORD` (sent as HTTP Basic on loopback).
    pub basic_auth: Option<String>,
    /// `opencode` entries: the NAME of an environment variable holding the
    /// server password, read at boot so no secret sits in the file. The user
    /// is `OPENCODE_SERVER_USERNAME` or `opencode`, as the server decides.
    /// Unset or empty: no auth. Exclusive with `basic_auth`.
    pub basic_auth_env: Option<String>,
    /// `codex` entries (ADR-015 §4.3): `workspaces`, `binary`, `args`,
    /// validated at load (workspace policy §4.2.3).
    pub codex: Option<crate::codex::LaunchConfig>,
    /// `claude` entries (ADR-015 §4.2.3): folders the phone may start Claude
    /// Code in. Empty = hook ingress only. Adding a folder is trusting it:
    /// print mode skips Claude's folder-trust dialog. Checked at boot
    /// (`claude_live::validate_workspace`).
    pub workspaces: Vec<String>,
    /// `claude` entries: an explicit `claude.exe` (never a `.cmd` shim).
    pub binary: Option<String>,
    /// `claude` entries: live Claude Code processes at once (default 3).
    pub max_sessions: Option<u32>,
    /// `claude` entries: the starting permission mode (default, acceptEdits
    /// or plan; never a bypass mode).
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompanionConfig {
    pub name: String,
    pub data_dir: PathBuf,
    pub adapters: Vec<AdapterEntryConfig>,
    pub events_cap: usize,
    pub commands_cap: usize,
    pub events_max_age_ms: i64,
    pub diagnostics_port: u16,
    pub probe_interval_ms: u64,
    /// Loopback WS/HTTP bind port for the protocol server (0 = ephemeral).
    /// Not part of the Node config (the Node host reads PORT env); added here
    /// so one file configures the whole binary. Default 8787, matching the
    /// Node host default.
    pub server_port: u16,
    pub server_bind: String,
    pub server_require_e2e: bool,
    /// ADR-020: the opt-in one-shot terminal (`terminal.enabled`, default
    /// off). Only when true is `terminal.v1` advertised and run.
    pub terminal_enabled: bool,
}

impl AdapterEntryConfig {
    /// HTTP Basic `user:pass` for an `opencode` entry: `basicAuth`, or the
    /// password from the `basicAuthEnv` variable at call time.
    pub fn opencode_auth(&self) -> Option<String> {
        if let Some(b) = &self.basic_auth {
            return Some(b.clone());
        }
        let pass = std::env::var(self.basic_auth_env.as_deref()?)
            .ok()
            .filter(|p| !p.is_empty())?;
        let user = std::env::var("OPENCODE_SERVER_USERNAME")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "opencode".into());
        Some(format!("{user}:{pass}"))
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("CHERNOBYL_HOME") {
        return PathBuf::from(home);
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("CHERNOBYL");
    }
    PathBuf::from(".").join(".chernobyl")
}

pub fn default_config_path() -> PathBuf {
    config_dir().join("companion.json")
}

pub fn default_config() -> CompanionConfig {
    CompanionConfig {
        name: "CHERNOBYL host".into(),
        data_dir: config_dir(),
        adapters: vec![AdapterEntryConfig {
            id: "mock-1".into(),
            kind: "mock".into(),
            enabled: true,
            ..Default::default()
        }],
        events_cap: 5_000,
        commands_cap: 5_000,
        events_max_age_ms: 7 * 24 * 60 * 60 * 1000,
        diagnostics_port: 9487,
        probe_interval_ms: 15_000,
        server_port: 8787,
        server_bind: "127.0.0.1".into(),
        server_require_e2e: false,
        terminal_enabled: false,
    }
}

fn as_i64(v: Option<&Json>, field: &str, min: i64, max: i64) -> Result<i64, String> {
    match v {
        Some(Json::Int(n)) if *n >= min && *n <= max => Ok(*n),
        _ => Err(format!(
            "'{field}' must be an integer between {min} and {max}"
        )),
    }
}

/// Validates untrusted config JSON into a full CompanionConfig.
pub fn validate_config(raw: &Json) -> Result<CompanionConfig, String> {
    let Json::Obj(_) = raw else {
        return Err("config root must be an object".into());
    };
    // a config from a FUTURE companion is refused before any range checks
    if let Some(Json::Int(v)) = json::get(raw, "version")
        && *v > CONFIG_VERSION
    {
        return Err(format!(
            "config was written by a newer companion (v{v} > v{CONFIG_VERSION}) — upgrade the companion, not the config"
        ));
    }
    let version = as_i64(json::get(raw, "version"), "version", 1, CONFIG_VERSION)?;
    if version < CONFIG_VERSION {
        return Err(format!(
            "config version {version} is older than supported {CONFIG_VERSION} — migration not defined"
        ));
    }

    let name = json::as_str(json::get(raw, "name").ok_or("'name' is required")?)
        .filter(|s| !s.is_empty())
        .ok_or("'name' must be a non-empty string")?
        .to_string();
    let data_dir = json::as_str(json::get(raw, "dataDir").ok_or("'dataDir' is required")?)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or("'dataDir' must be a non-empty string")?;

    // codex workspaces must never contain it (§4.2.3)
    let data_dir_ref = data_dir.clone();
    let Some(Json::Arr(entries)) = json::get(raw, "adapters") else {
        return Err("'adapters' must be an array".into());
    };
    let mut adapters = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let id = json::get_str(entry, "id")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("adapters[{i}].id must be a non-empty string"))?
            .to_string();
        let kind = json::get_str(entry, "type")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("adapters[{i}].type must be a non-empty string"))?
            .to_string();
        let enabled = json::get(entry, "enabled")
            .map(|v| {
                json::as_bool(v).ok_or_else(|| format!("adapters[{i}].enabled must be a boolean"))
            })
            .transpose()?
            .unwrap_or(true);
        let endpoint = json::get_str(entry, "endpoint").map(str::to_string);
        let basic_auth = json::get_str(entry, "basicAuth").map(str::to_string);
        let basic_auth_env = match json::get(entry, "basicAuthEnv") {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                json::as_str(v)
                    .filter(|n| {
                        !n.is_empty()
                            && n.len() <= 128
                            && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    .ok_or_else(|| {
                        format!("adapters[{i}].basicAuthEnv must be an environment variable name")
                    })?
                    .to_string(),
            ),
        };
        if basic_auth.is_some() && basic_auth_env.is_some() {
            return Err(format!(
                "adapters[{i}]: set basicAuth or basicAuthEnv, not both"
            ));
        }
        // Attach-only adapter: where the server is must be stated — no
        // implicit default port that could probe the wrong process (G4.3).
        if enabled && kind == "opencode" && endpoint.is_none() {
            return Err(format!(
                "adapters[{i}].endpoint is required for type 'opencode' \
                 (e.g. \"http://127.0.0.1:4096\" — attach to an existing serve)"
            ));
        }
        // Same attach-only rule for the Phase 7 Ollama model runtime.
        if enabled && kind == "ollama" && endpoint.is_none() {
            return Err(format!(
                "adapters[{i}].endpoint is required for type 'ollama' \
                 (e.g. \"http://127.0.0.1:11434\" — attach to an existing serve)"
            ));
        }
        // a spawning runtime: its folder allow-list is checked here, fail closed
        let codex = if enabled && kind == "codex" {
            Some(
                crate::codex::launch_config(entry, &data_dir_ref)
                    .map_err(|e| format!("adapters[{i}]: {e}"))?,
            )
        } else {
            None
        };
        let workspaces = match json::get(entry, "workspaces").filter(|_| kind == "claude") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Arr(list)) => list
                .iter()
                .map(|w| {
                    json::as_str(w)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("adapters[{i}].workspaces must be non-empty strings"))?,
            Some(_) => return Err(format!("adapters[{i}].workspaces must be an array")),
        };
        let binary = match json::get(entry, "binary").filter(|_| kind == "claude") {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                json::as_str(v)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("adapters[{i}].binary must be a non-empty string"))?
                    .to_string(),
            ),
        };
        let max_sessions = json::get(entry, "maxSessions")
            .filter(|_| kind == "claude")
            .filter(|v| **v != Json::Null)
            .map(|v| as_i64(Some(v), &format!("adapters[{i}].maxSessions"), 1, 16))
            .transpose()?
            .map(|n| n as u32);
        let permission_mode = match json::get(entry, "permissionMode").filter(|_| kind == "claude")
        {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                json::as_str(v)
                    .filter(|m| crate::claude_live::PERMISSION_MODES.contains(m))
                    .ok_or_else(|| {
                        format!("adapters[{i}].permissionMode must be default, acceptEdits or plan")
                    })?
                    .to_string(),
            ),
        };
        adapters.push(AdapterEntryConfig {
            id,
            kind,
            enabled,
            endpoint,
            basic_auth,
            basic_auth_env,
            codex,
            workspaces,
            binary,
            max_sessions,
            permission_mode,
        });
    }
    let ids: Vec<&str> = adapters
        .iter()
        .filter(|a| a.enabled)
        .map(|a| a.id.as_str())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != ids.len() {
        return Err("adapter ids must be unique".into());
    }

    let ret = json::get(raw, "retention").ok_or("'retention' is required")?;
    let events_cap = as_i64(json::get(ret, "events"), "retention.events", 100, 1_000_000)? as usize;
    let events_max_age_ms = as_i64(
        json::get(ret, "eventsMaxAgeMs"),
        "retention.eventsMaxAgeMs",
        60_000,
        365 * 24 * 60 * 60 * 1000,
    )?;
    // optional: default keeps configs written before command retention valid
    let commands_cap = json::get(ret, "commands")
        .map(|v| as_i64(Some(v), "retention.commands", 100, 1_000_000))
        .transpose()?
        .unwrap_or(5_000) as usize;

    let diag = json::get(raw, "diagnostics").ok_or("'diagnostics' is required")?;
    let diagnostics_port = as_i64(json::get(diag, "port"), "diagnostics.port", 0, 65_535)? as u16;
    let probe_interval_ms = as_i64(
        json::get(raw, "probeIntervalMs"),
        "probeIntervalMs",
        1_000,
        600_000,
    )? as u64;

    // optional additions (defaults keep configs written by the Node companion valid)
    let server_port = json::get(raw, "server")
        .and_then(|sv| json::get(sv, "port"))
        .map(|v| as_i64(Some(v), "server.port", 0, 65_535))
        .transpose()?
        .unwrap_or(8787) as u16;
    let server_bind = json::get(raw, "server")
        .and_then(|sv| json::get(sv, "bind"))
        .map(|v| {
            json::as_str(v)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| "'server.bind' must be a non-empty string".to_string())
        })
        .transpose()?
        .unwrap_or_else(|| "127.0.0.1".into());
    let server_require_e2e = json::get(raw, "server")
        .and_then(|sv| json::get(sv, "requireE2e"))
        .map(|v| {
            json::as_bool(v).ok_or_else(|| "'server.requireE2e' must be a boolean".to_string())
        })
        .transpose()?
        .unwrap_or(false);
    // ADR-020: off unless the file says exactly `true`
    let terminal_enabled = json::get(raw, "terminal")
        .and_then(|t| json::get(t, "enabled"))
        .map(|v| json::as_bool(v).ok_or_else(|| "'terminal.enabled' must be a boolean".to_string()))
        .transpose()?
        .unwrap_or(false);

    Ok(CompanionConfig {
        name,
        data_dir,
        adapters,
        events_cap,
        commands_cap,
        events_max_age_ms,
        diagnostics_port,
        probe_interval_ms,
        server_port,
        server_bind,
        server_require_e2e,
        terminal_enabled,
    })
}

pub fn config_to_json(c: &CompanionConfig) -> Json {
    json::obj(vec![
        ("version", Json::Int(CONFIG_VERSION)),
        ("name", json::s(&c.name)),
        ("dataDir", json::s(&c.data_dir.to_string_lossy())),
        (
            "adapters",
            Json::Arr(
                c.adapters
                    .iter()
                    .map(|a| {
                        let mut fields = vec![
                            ("id", json::s(&a.id)),
                            ("type", json::s(&a.kind)),
                            ("enabled", Json::Bool(a.enabled)),
                        ];
                        if let Some(e) = &a.endpoint {
                            fields.push(("endpoint", json::s(e)));
                        }
                        if let Some(b) = &a.basic_auth {
                            fields.push(("basicAuth", json::s(b)));
                        }
                        if let Some(n) = &a.basic_auth_env {
                            fields.push(("basicAuthEnv", json::s(n)));
                        }
                        if let Some(cx) = &a.codex {
                            let paths = cx.workspaces.iter().map(|w| json::s(&w.path));
                            fields.push(("workspaces", Json::Arr(paths.collect())));
                            fields.push(("binary", json::s(&cx.binary.to_string_lossy())));
                            let args = cx.args.iter().map(|a| json::s(a));
                            fields.push(("args", Json::Arr(args.collect())));
                        }
                        if !a.workspaces.is_empty() {
                            let list = a.workspaces.iter().map(|w| json::s(w)).collect();
                            fields.push(("workspaces", Json::Arr(list)));
                        }
                        if let Some(b) = &a.binary {
                            fields.push(("binary", json::s(b)));
                        }
                        if let Some(n) = a.max_sessions {
                            fields.push(("maxSessions", Json::Int(n.into())));
                        }
                        if let Some(m) = &a.permission_mode {
                            fields.push(("permissionMode", json::s(m)));
                        }
                        json::obj(fields)
                    })
                    .collect(),
            ),
        ),
        (
            "retention",
            json::obj(vec![
                ("events", Json::Int(c.events_cap as i64)),
                ("commands", Json::Int(c.commands_cap as i64)),
                ("eventsMaxAgeMs", Json::Int(c.events_max_age_ms)),
            ]),
        ),
        (
            "diagnostics",
            json::obj(vec![("port", Json::Int(c.diagnostics_port as i64))]),
        ),
        ("probeIntervalMs", Json::Int(c.probe_interval_ms as i64)),
        (
            "server",
            json::obj(vec![
                ("port", Json::Int(c.server_port as i64)),
                ("bind", json::s(&c.server_bind)),
                ("requireE2e", Json::Bool(c.server_require_e2e)),
            ]),
        ),
        (
            "terminal",
            json::obj(vec![("enabled", Json::Bool(c.terminal_enabled))]),
        ),
    ])
}

/// Loads the config at `path` (default location when omitted). Missing file
/// → defaults written to disk; invalid file → Err (fail closed).
/// ADR-018: the config as it is on disk, never writing a default (the
/// permission hook runs inside Claude Code and must not create files).
/// `None` when it is absent or invalid.
pub fn read_config(path: Option<&Path>) -> Option<CompanionConfig> {
    let file = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    let text = fs::read_to_string(file).ok()?;
    validate_config(&json::parse(text.trim_start_matches('\u{feff}')).ok()?).ok()
}

pub fn load_config(path: Option<&Path>) -> Result<(CompanionConfig, PathBuf, bool), String> {
    let file = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    if !file.exists() {
        let config = default_config();
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
        }
        let mut text = json::stringify(&config_to_json(&config));
        text.push('\n');
        fs::write(&file, text).map_err(|e| format!("write default config: {e}"))?;
        return Ok((config, file, true));
    }
    let text = fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
    let parsed = json::parse(&text)
        .map_err(|e| format!("config {} is not valid JSON: {e}", file.display()))?;
    let config = validate_config(&parsed)?;
    Ok((config, file, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let c = default_config();
        let v = config_to_json(&c);
        let parsed = validate_config(&v).unwrap();
        assert_eq!(parsed.name, c.name);
        assert_eq!(parsed.events_cap, 5_000);
        assert_eq!(parsed.commands_cap, 5_000);
        assert_eq!(parsed.server_port, 8787);
        assert!(!parsed.server_require_e2e);
    }

    #[test]
    fn opencode_entry_requires_endpoint_and_round_trips() {
        let mut v = config_to_json(&default_config());
        if let Json::Obj(pairs) = &mut v {
            for (_, val) in pairs.iter_mut() {
                if let Json::Arr(adapters) = val {
                    adapters.push(json::obj(vec![
                        ("id", json::s("oc-1")),
                        ("type", json::s("opencode")),
                        ("enabled", Json::Bool(true)),
                    ]));
                }
            }
        }
        // attach-only: no endpoint, no guessing a port (G4.3)
        let err = validate_config(&v).unwrap_err();
        assert!(err.contains("endpoint is required"), "{err}");

        if let Json::Obj(pairs) = &mut v {
            for (_, val) in pairs.iter_mut() {
                if let Json::Arr(adapters) = val
                    && let Some(Json::Obj(entry)) = adapters.last_mut()
                {
                    entry.push(("endpoint".into(), json::s("http://127.0.0.1:4096")));
                    entry.push(("basicAuth".into(), json::s("u:p")));
                }
            }
        }
        let parsed = validate_config(&v).unwrap();
        let oc = parsed.adapters.iter().find(|a| a.id == "oc-1").unwrap();
        assert_eq!(oc.endpoint.as_deref(), Some("http://127.0.0.1:4096"));
        assert_eq!(oc.basic_auth.as_deref(), Some("u:p"));
        let out = crate::json::stringify(&config_to_json(&parsed));
        assert!(out.contains("http://127.0.0.1:4096"), "{out}");
        assert!(out.contains("\"basicAuth\""), "{out}");
    }

    #[test]
    fn opencode_basic_auth_env_and_terminal_flag_parse_and_round_trip() {
        let cfg = |adapter: &str, extra: &str| {
            validate_config(&json::parse(&format!(
                r#"{{"version":1,"name":"x","dataDir":"x","adapters":[{adapter}],"retention":{{"events":5000,"eventsMaxAgeMs":600000}},"diagnostics":{{"port":9487}},"probeIntervalMs":15000{extra}}}"#
            )).unwrap())
        };
        let oc = |more: &str| {
            format!(
                r#"{{"id":"opencode-1","type":"opencode","endpoint":"http://127.0.0.1:4096"{more}}}"#
            )
        };
        let c = cfg(&oc(r#","basicAuthEnv":"OPENCODE_SERVER_PASSWORD""#), "").unwrap();
        let a = &c.adapters[0];
        assert_eq!(
            a.basic_auth_env.as_deref(),
            Some("OPENCODE_SERVER_PASSWORD")
        );
        assert_eq!(a.basic_auth, None);
        assert!(!c.terminal_enabled, "the terminal is off by default");
        assert_eq!(validate_config(&config_to_json(&c)).unwrap(), c);
        // the password is read from the variable at call time, never stored
        let named = |var: &str| AdapterEntryConfig {
            basic_auth_env: Some(var.into()),
            ..a.clone()
        };
        assert_eq!(named("CHB_SURELY_UNSET_7F3A").opencode_auth(), None);
        let path = std::env::var("PATH").unwrap();
        let auth = named("PATH").opencode_auth().unwrap();
        assert!(auth.ends_with(&format!(":{path}")), "{auth}");
        let inline = cfg(&oc(r#","basicAuth":"u:p""#), "").unwrap();
        assert_eq!(inline.adapters[0].opencode_auth().as_deref(), Some("u:p"));
        for (more, field) in [
            (r#","basicAuthEnv":"""#, "basicAuthEnv"),
            (r#","basicAuthEnv":"A B""#, "basicAuthEnv"),
            (r#","basicAuthEnv":7"#, "basicAuthEnv"),
            (r#","basicAuth":"u:p","basicAuthEnv":"P""#, "not both"),
        ] {
            let err = cfg(&oc(more), "").unwrap_err();
            assert!(err.contains(field), "{more}: {err}");
        }

        let on = cfg(&oc(""), r#","terminal":{"enabled":true}"#).unwrap();
        assert!(on.terminal_enabled);
        let text = json::stringify(&config_to_json(&on));
        assert!(text.contains(r#""terminal":{"enabled":true}"#), "{text}");
        assert_eq!(validate_config(&config_to_json(&on)).unwrap(), on);
        assert!(!cfg(&oc(""), r#","terminal":{}"#).unwrap().terminal_enabled);
        let err = cfg(&oc(""), r#","terminal":{"enabled":"yes"}"#).unwrap_err();
        assert!(err.contains("terminal.enabled"), "{err}");
    }

    #[test]
    fn codex_entry_is_checked_at_load_and_round_trips() {
        let root = std::env::temp_dir().join(format!("chernobyl-cfg-codex-{}", std::process::id()));
        let ws = root.join("ws");
        fs::create_dir_all(&ws).unwrap();
        let config = |dir: &Path, folder: &Path| {
            let text = format!(
                r#"{{"version":1,"name":"x","dataDir":{},"adapters":[{{"id":"codex-1","type":"codex","workspaces":[{}],"binary":"node","args":["fake.mjs"]}}],"retention":{{"events":5000,"eventsMaxAgeMs":600000}},"diagnostics":{{"port":9487}},"probeIntervalMs":15000}}"#,
                json::stringify(&json::s(&dir.to_string_lossy())),
                json::stringify(&json::s(&folder.to_string_lossy())),
            );
            validate_config(&json::parse(&text).unwrap())
        };
        // the data folder inside a workspace fails closed
        let err = config(&ws.join("data"), &ws).unwrap_err();
        assert!(
            err.contains("adapters[0]") && err.contains("data folder"),
            "{err}"
        );
        let parsed = config(&root.join("data"), &ws).unwrap();
        let cx = parsed.adapters[0].codex.clone().unwrap();
        assert_eq!(cx.argv(), ["fake.mjs", "app-server"]);
        // written back, it loads to the same launch config
        let again = validate_config(&config_to_json(&parsed)).unwrap();
        assert_eq!(again.adapters[0].codex, Some(cx));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_live_keys_parse_round_trip_and_fail_closed() {
        let base = r#"{"version":1,"name":"x","dataDir":"x","retention":{"events":5000,"eventsMaxAgeMs":600000},"diagnostics":{"port":9487},"probeIntervalMs":15000,"adapters":["#;
        let parse =
            |entry: &str| validate_config(&json::parse(&format!("{base}{entry}]}}")).unwrap());
        let c = parse(r#"{"id":"claude-1","type":"claude","workspaces":["C:/w"],"binary":"C:/c.exe","maxSessions":2,"permissionMode":"plan"}"#).unwrap();
        let a = &c.adapters[0];
        assert_eq!(a.workspaces, ["C:/w"]);
        assert_eq!(a.binary.as_deref(), Some("C:/c.exe"));
        assert_eq!(a.max_sessions, Some(2));
        assert_eq!(a.permission_mode.as_deref(), Some("plan"));
        assert_eq!(validate_config(&config_to_json(&c)).unwrap(), c);
        // without the keys: an ingress-only entry, as before
        let c = parse(r#"{"id":"claude-1","type":"claude","workspaces":null}"#).unwrap();
        assert!(c.adapters[0].workspaces.is_empty());
        for (entry, field) in [
            (
                r#"{"id":"c","type":"claude","workspaces":"C:/w"}"#,
                "workspaces",
            ),
            (
                r#"{"id":"c","type":"claude","workspaces":[""]}"#,
                "workspaces",
            ),
            (
                r#"{"id":"c","type":"claude","maxSessions":0}"#,
                "maxSessions",
            ),
            (
                r#"{"id":"c","type":"claude","permissionMode":"bypassPermissions"}"#,
                "permissionMode",
            ),
            (r#"{"id":"c","type":"claude","binary":7}"#, "binary"),
        ] {
            let err = parse(entry).unwrap_err();
            assert!(err.contains(field), "{entry}: {err}");
        }
    }

    #[test]
    fn future_version_is_refused() {
        let mut v = config_to_json(&default_config());
        if let Json::Obj(pairs) = &mut v {
            pairs[0].1 = Json::Int(999);
        }
        let err = validate_config(&v).unwrap_err();
        assert!(err.contains("newer companion"), "{err}");
    }

    #[test]
    fn invalid_shapes_are_refused() {
        assert!(validate_config(&Json::Null).is_err());
        let v = json::parse(r#"{"version":1,"name":"","dataDir":"x","adapters":[],"retention":{"events":5000,"eventsMaxAgeMs":600000},"diagnostics":{"port":9487},"probeIntervalMs":15000}"#).unwrap();
        assert!(validate_config(&v).unwrap_err().contains("name"));
        let v = json::parse(r#"{"version":1,"name":"x","dataDir":"x","adapters":[{"id":"a","type":"mock"},{"id":"a","type":"mock"}],"retention":{"events":5000,"eventsMaxAgeMs":600000},"diagnostics":{"port":9487},"probeIntervalMs":15000}"#).unwrap();
        assert!(validate_config(&v).unwrap_err().contains("unique"));
        let v = json::parse(r#"{"version":1,"name":"x","dataDir":"x","adapters":[],"retention":{"events":10,"eventsMaxAgeMs":600000},"diagnostics":{"port":9487},"probeIntervalMs":15000}"#).unwrap();
        assert!(
            validate_config(&v)
                .unwrap_err()
                .contains("retention.events")
        );
        // out-of-range command retention fails closed like events does
        let v = json::parse(r#"{"version":1,"name":"x","dataDir":"x","adapters":[],"retention":{"events":5000,"commands":10,"eventsMaxAgeMs":600000},"diagnostics":{"port":9487},"probeIntervalMs":15000}"#).unwrap();
        assert!(
            validate_config(&v)
                .unwrap_err()
                .contains("retention.commands")
        );
    }

    #[test]
    fn node_written_config_parses() {
        // a config exactly as the Node companion writes it (no server block)
        let v = json::parse(
            r#"{"version":1,"name":"CHERNOBYL host","dataDir":"C:\\x","adapters":[{"id":"mock-1","type":"mock","enabled":true}],"retention":{"events":5000,"eventsMaxAgeMs":604800000},"diagnostics":{"port":9487},"probeIntervalMs":15000}"#,
        )
        .unwrap();
        let c = validate_config(&v).unwrap();
        assert_eq!(c.server_port, 8787);
        assert_eq!(c.server_bind, "127.0.0.1");
        // written before command retention existed → default cap applies
        assert_eq!(c.commands_cap, 5_000);
    }

    #[test]
    fn load_creates_default_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("chernobyl-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("companion.json");
        let (c, _, created) = load_config(Some(&path)).unwrap();
        assert!(created);
        assert_eq!(c.name, "CHERNOBYL host");
        // second load reuses the file
        let (_, _, created2) = load_config(Some(&path)).unwrap();
        assert!(!created2);
        // garbage fails closed
        fs::write(&path, "not json").unwrap();
        assert!(load_config(Some(&path)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
