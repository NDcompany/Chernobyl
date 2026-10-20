//! Adapter discovery + status (port of the Node companion's adapters.ts,
//! mock-only scope). Unknown types are recorded as discovery-failed and the
//! engine serves as the in-process mock adapter. Process adapters (OpenCode
//! etc.) arrive with Phase 3+; the manager shape is already here.

use crate::config::AdapterEntryConfig;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct AdapterStatus {
    pub id: String,
    pub kind: String,
    pub supported: bool,
    pub healthy: bool,
    pub last_error: Option<String>,
}

pub struct AdapterManager {
    statuses: HashMap<String, AdapterStatus>,
}

impl AdapterManager {
    /// Discovery: `mock` is served in-process by the engine; everything else
    /// is recorded as unsupported (never silently dropped).
    pub fn discover(entries: &[AdapterEntryConfig]) -> AdapterManager {
        let mut statuses = HashMap::new();
        for entry in entries.iter().filter(|e| e.enabled) {
            let (supported, healthy, last_error) = if entry.kind == "mock" {
                (true, true, None)
            } else {
                (
                    false,
                    false,
                    Some(format!(
                        "unsupported adapter type '{}' — discovery failed (recorded, not fatal)",
                        entry.kind
                    )),
                )
            };
            statuses.insert(
                entry.id.clone(),
                AdapterStatus {
                    id: entry.id.clone(),
                    kind: entry.kind.clone(),
                    supported,
                    healthy,
                    last_error,
                },
            );
        }
        AdapterManager { statuses }
    }

    /// Auto-routing (Phase 2 rule): exactly one healthy adapter → route to it.
    pub fn sole_healthy(&self) -> Option<&AdapterStatus> {
        let healthy: Vec<_> = self.statuses.values().filter(|s| s.healthy).collect();
        match healthy.len() {
            1 => Some(healthy[0]),
            _ => None,
        }
    }

    pub fn list(&self) -> Vec<AdapterStatus> {
        let mut out: Vec<_> = self.statuses.values().cloned().collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Re-attempt discovery for failed entries (kept for the probe loop of
    /// future process adapters; harmless now).
    pub fn reprobe(&mut self, entries: &[AdapterEntryConfig]) {
        let rediscovered = Self::discover(entries);
        for (id, status) in rediscovered.statuses {
            if let Some(existing) = self.statuses.get_mut(&id)
                && !existing.supported
            {
                *existing = status;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: &str) -> AdapterEntryConfig {
        AdapterEntryConfig {
            id: id.into(),
            kind: kind.into(),
            enabled: true,
        }
    }

    #[test]
    fn mock_is_healthy_and_unknown_is_recorded_not_fatal() {
        let m = AdapterManager::discover(&[
            entry("mock-1", "mock"),
            entry("oc-1", "opencode"),
            AdapterEntryConfig {
                id: "off-1".into(),
                kind: "mock".into(),
                enabled: false,
            },
        ]);
        let list = m.list();
        assert_eq!(list.len(), 2); // disabled entry skipped
        let oc = list.iter().find(|s| s.id == "oc-1").unwrap();
        assert!(!oc.supported);
        assert!(!oc.healthy);
        assert!(oc.last_error.as_deref().unwrap().contains("unsupported"));
        assert!(m.sole_healthy().is_some()); // only mock-1 is healthy
    }

    #[test]
    fn multiple_healthy_adapters_disable_auto_routing() {
        let m = AdapterManager::discover(&[entry("m1", "mock"), entry("m2", "mock")]);
        assert!(m.sole_healthy().is_none());
    }
}
