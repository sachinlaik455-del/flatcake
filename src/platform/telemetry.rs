//! Bounded telemetry ring: engine events recorded by the service, exposed as
//! counters + a recent-events snapshot. Read-only; never carries KV contents.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const RING_CAP: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    BlockPlaced,
    BlockRetrieved,
    Migration,
    Eviction,
    SsdRead,
    SsdWrite,
    Error,
    Pressure,
}

impl EventKind {
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::BlockPlaced => "block_placed",
            EventKind::BlockRetrieved => "block_retrieved",
            EventKind::Migration => "migration",
            EventKind::Eviction => "eviction",
            EventKind::SsdRead => "ssd_read",
            EventKind::SsdWrite => "ssd_write",
            EventKind::Error => "error",
            EventKind::Pressure => "pressure",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub at_ms: u64,
    /// Count of blocks involved (1 for errors/pressure).
    pub blocks: u64,
}

#[derive(Default)]
pub struct Telemetry {
    ring: Mutex<Vec<Event>>,
    counters: Mutex<[u64; 8]>,
}

impl Telemetry {
    pub fn new() -> Telemetry {
        Telemetry {
            ring: Mutex::new(Vec::with_capacity(256)),
            counters: Mutex::new([0; 8]),
        }
    }

    pub fn record(&self, kind: EventKind, blocks: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut ring = self.ring.lock().unwrap();
        ring.push(Event { kind, at_ms: now, blocks });
        if ring.len() > RING_CAP {
            let over = ring.len() - RING_CAP;
            ring.drain(..over);
        }
        drop(ring);
        self.counters.lock().unwrap()[kind as usize] += blocks.max(1);
    }

    /// Cumulative per-kind counters (index = EventKind as usize).
    pub fn counters(&self) -> [u64; 8] {
        *self.counters.lock().unwrap()
    }

    /// The most recent `n` events, oldest first.
    pub fn recent(&self, n: usize) -> Vec<Event> {
        let ring = self.ring.lock().unwrap();
        let start = ring.len().saturating_sub(n);
        ring[start..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_counters_accumulate() {
        let t = Telemetry::new();
        for _ in 0..(RING_CAP + 100) {
            t.record(EventKind::BlockRetrieved, 1);
        }
        assert_eq!(t.recent(usize::MAX).len(), RING_CAP);
        let c = t.counters();
        assert!(c[EventKind::BlockRetrieved as usize] >= RING_CAP as u64);
    }
}
