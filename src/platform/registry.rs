//! Runtime registry: tracks connected runtimes and their project identity.
//! Entries are keyed by a unique runtime id; project ids separate concurrent
//! workloads so one project's metadata is never exposed as another's.

use super::connector::{Health, ConnectorInfo};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Heartbeats older than this are eligible for stale cleanup.
pub const STALE_AFTER: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct RuntimeEntry {
    pub runtime_id: String,
    pub project_id: String,
    pub pid: u32,
    pub connector: ConnectorInfo,
    pub health: Health,
    pub registered_at: Instant,
    pub last_heartbeat: Instant,
}

#[derive(Default)]
pub struct RuntimeRegistry {
    entries: Mutex<HashMap<String, RuntimeEntry>>,
}

impl RuntimeRegistry {
    pub fn new() -> Self {
        RuntimeRegistry {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Register a runtime. Idempotent per runtime_id: a re-registration with
    /// the same id refreshes the entry (heartbeat semantics).
    pub fn register(
        &self,
        runtime_id: &str,
        project_id: &str,
        pid: u32,
        connector: ConnectorInfo,
    ) -> Result<(), String> {
        connector.validate()?;
        if runtime_id.is_empty() || project_id.is_empty() {
            return Err("runtime_id and project_id must not be empty".into());
        }
        let mut m = self.entries.lock().unwrap();
        let now = Instant::now();
        m.insert(
            runtime_id.to_string(),
            RuntimeEntry {
                runtime_id: runtime_id.to_string(),
                project_id: project_id.to_string(),
                pid,
                connector,
                health: Health::Healthy,
                registered_at: now,
                last_heartbeat: now,
            },
        );
        Ok(())
    }

    pub fn heartbeat(&self, runtime_id: &str) -> bool {
        let mut m = self.entries.lock().unwrap();
        match m.get_mut(runtime_id) {
            Some(e) => {
                e.last_heartbeat = Instant::now();
                e.health = Health::Healthy;
                true
            }
            None => false,
        }
    }

    pub fn disconnect(&self, runtime_id: &str) -> bool {
        self.entries.lock().unwrap().remove(runtime_id).is_some()
    }

    /// Entries for one project, oldest registration first.
    pub fn list_project(&self, project_id: &str) -> Vec<RuntimeEntry> {
        let mut m = self.entries.lock().unwrap();
        Self::drop_stale(&mut m);
        let mut v: Vec<RuntimeEntry> = m
            .values()
            .filter(|e| e.project_id == project_id)
            .cloned()
            .collect();
        v.sort_by_key(|e| e.registered_at);
        v
    }

    pub fn list_all(&self) -> Vec<RuntimeEntry> {
        let mut m = self.entries.lock().unwrap();
        Self::drop_stale(&mut m);
        let mut v: Vec<RuntimeEntry> = m.values().cloned().collect();
        v.sort_by_key(|e| e.registered_at);
        v
    }

    pub fn count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    fn drop_stale(m: &mut HashMap<String, RuntimeEntry>) {
        m.retain(|_, e| e.last_heartbeat.elapsed() < STALE_AFTER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::connector::{caps, PROTOCOL_VERSION};

    fn info(name: &str) -> ConnectorInfo {
        ConnectorInfo {
            name: name.into(),
            version: "1.0".into(),
            protocol_version: PROTOCOL_VERSION,
            capabilities: caps::STATUS,
        }
    }

    #[test]
    fn register_heartbeat_disconnect_lifecycle() {
        let r = RuntimeRegistry::new();
        r.register("rt-1", "proj-a", 42, info("test")).unwrap();
        assert_eq!(r.count(), 1);
        assert!(r.heartbeat("rt-1"));
        assert_eq!(r.list_project("proj-a").len(), 1);
        assert!(r.disconnect("rt-1"));
        assert_eq!(r.count(), 0);
        assert!(!r.heartbeat("rt-1"), "heartbeat after disconnect fails");
    }

    #[test]
    fn project_isolation() {
        let r = RuntimeRegistry::new();
        r.register("rt-a", "proj-1", 1, info("a")).unwrap();
        r.register("rt-b", "proj-2", 2, info("b")).unwrap();
        assert_eq!(r.list_project("proj-1").len(), 1);
        assert_eq!(r.list_project("proj-2").len(), 1);
        assert!(r.list_project("proj-3").is_empty());
    }

    #[test]
    fn capability_negotiation_rejects_bad_connector() {
        let r = RuntimeRegistry::new();
        let mut bad = info("bad");
        bad.protocol_version = 99;
        assert!(r.register("rt-x", "p", 1, bad).is_err());
        let mut nostatus = info("nostatus");
        nostatus.capabilities = 0;
        assert!(r.register("rt-y", "p", 1, nostatus).is_err());
    }
}
