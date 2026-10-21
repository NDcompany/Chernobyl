//! Companion configuration — exact port of `services/companion/src/config.ts`.
//! JSON file, versioned, fail-closed on anything a newer companion wrote.
//! `CHERNOBYL_HOME` overrides the default location (tests, portable installs).

use crate::json::{self, Json};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq)]
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
            endpoint: None,
            basic_auth: None,
        }],
        events_cap: 5_000,
        commands_cap: 5_000,
        events_max_age_ms: 7 * 24 * 60 * 60 * 1000,
        diagnostics_port: 9487,
        probe_interval_ms: 15_000,
        server_port: 8787,
        server_bind: "127.0.0.1".into(),
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
        // Attach-only adapter: where the server is must be stated — no
        // implicit default port that could probe the wrong process (G4.3).
        if enabled && kind == "opencode" && endpoint.is_none() {
            return Err(format!(
                "adapters[{i}].endpoint is required for type 'opencode' \
                 (e.g. \"http://127.0.0.1:4096\" — attach to an existing serve)"
            ));
        }
        adapters.push(AdapterEntryConfig {
            id,
            kind,
            enabled,
            endpoint,
            basic_auth,
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
            ]),
        ),
    ])
}

/// Loads the config at `path` (default location when omitted). Missing file
/// → defaults written to disk; invalid file → Err (fail closed).
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
