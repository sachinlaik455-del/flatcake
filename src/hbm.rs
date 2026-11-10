//! GPU-HBM window analog (ARCHITECTURE §7): byte-resident LRU-by-delivery cache of
//! recently delivered blocks. Identical budget and policy for BOTH systems (fairness,
//! EGM #6/#8). Owned by engines behind a Mutex; not Sync itself.

use std::collections::{BTreeMap, HashMap};

pub struct HbmWindow {
    cap: u64,
    used: u64,
    stamp: u64,
    map: HashMap<[u64; 3], (Vec<u8>, u64)>, // key -> (bytes, last_delivery_stamp)
    by_stamp: BTreeMap<u64, [u64; 3]>,      // stamp -> key (eviction order)
}

fn k2a(session: u64, layer: u32, token_start: u64) -> [u64; 3] {
    [session, layer as u64, token_start]
}

impl HbmWindow {
    pub fn new(cap_bytes: u64) -> HbmWindow {
        HbmWindow {
            cap: cap_bytes,
            used: 0,
            stamp: 0,
            map: HashMap::new(),
            by_stamp: BTreeMap::new(),
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    /// Resident payload for a block, if delivered recently. Does NOT refresh recency
    /// (recency = delivery time only).
    pub fn get(&self, session: u64, layer: u32, token_start: u64) -> Option<&[u8]> {
        self.map
            .get(&k2a(session, layer, token_start))
            .map(|(b, _)| b.as_slice())
    }

    /// Record a delivered block (population path). Evicts least-recently-delivered
    /// entries while over budget. Blocks larger than the window are never resident.
    pub fn record_delivery(&mut self, session: u64, layer: u32, token_start: u64, bytes: &[u8]) {
        let key = k2a(session, layer, token_start);
        if bytes.len() as u64 > self.cap {
            return;
        }
        if let Some(old) = self.map.remove(&key) {
            self.used -= old.0.len() as u64;
            self.by_stamp.remove(&old.1);
        }
        self.stamp += 1;
        while self.used + bytes.len() as u64 > self.cap {
            let Some((&oldest, _)) = self.by_stamp.iter().next() else {
                break;
            };
            let victim = self.by_stamp.remove(&oldest).unwrap();
            if let Some((b, _)) = self.map.remove(&victim) {
                self.used -= b.len() as u64;
            }
        }
        self.used += bytes.len() as u64;
        self.map.insert(key, (bytes.to_vec(), self.stamp));
        self.by_stamp.insert(self.stamp, key);
    }

    /// Remove a block (used by baseline reclamation so the window never serves
    /// bytes whose backing store was reclaimed while the trace may re-put them).
    pub fn invalidate(&mut self, session: u64, layer: u32, token_start: u64) {
        if let Some((b, s)) = self.map.remove(&k2a(session, layer, token_start)) {
            self.used -= b.len() as u64;
            self.by_stamp.remove(&s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_by_delivery_and_bound() {
        let mut w = HbmWindow::new(1000);
        w.record_delivery(1, 0, 0, &vec![1u8; 400]);
        w.record_delivery(1, 0, 8192, &vec![2u8; 400]);
        w.record_delivery(1, 0, 16384, &vec![3u8; 400]); // evicts first (400+400+400>1000)
        assert!(w.get(1, 0, 0).is_none());
        assert!(w.get(1, 0, 8192).is_some());
        assert!(w.get(1, 0, 16384).is_some());
        assert!(w.used_bytes() <= 1000);
        w.record_delivery(1, 0, 1 << 20, &vec![9u8; 2000]); // oversize: never resident
        assert!(w.get(1, 0, 1 << 20).is_none());
        w.invalidate(1, 0, 8192);
        assert!(w.get(1, 0, 8192).is_none());
    }
}
