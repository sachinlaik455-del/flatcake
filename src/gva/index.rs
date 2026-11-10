//! gva::index — fixed-capacity lock-free open-addressed Level-1 index
//! (T2/T3/T21; DECISION D13, OQ-16).
//!
//! Fixed capacity, **no resize** (startup asserts unique blocks ≤ 0.7 ×
//! capacity — engine-side, OQ-16), linear probing (+1), four slot states
//! EMPTY/PENDING/PUBLISHED/TOMBSTONE. TOMBSTONE keeps probe chains intact
//! (EGM finding) and is reusable only by new inserts; lookups skip it.
//!
//! ## Publish-after-write (T3)
//! `insert_pending` claims a slot in PENDING state; `publish` stores the GVA
//! (Release) and only then flips PENDING → PUBLISHED (Release CAS). Readers
//! loading PUBLISHED (Acquire) synchronize with that CAS, so they can never
//! observe PUBLISHED with a stale GVA. A get racing an in-flight put observes
//! Miss or Pending — the engine counts Pending separately as
//! `pending_racing_gets` and both as a miss by design.
//!
//! ## Hash-store ordering (deliberate refinement of the tasking note)
//! The tasking note says "store key_hash first, then CAS state". Storing the
//! hash BEFORE the state CAS has a race a CAS loser cannot recover from: its
//! tentative hash store can land after the winner's store and permanently
//! clobber the winner's hash (the loser re-probes, but the winner's key
//! becomes unreachable — a lost publish). Therefore the hash is stored by the
//! CAS winner ONLY, immediately after winning the EMPTY|TOMBSTONE → PENDING
//! claim. Consequences, both permitted by the T3 contract:
//!   * a PENDING slot briefly carries a stale hash → lookups see Miss (not
//!     Pending) for the few instructions until the owner's store lands; Miss
//!     is a valid pre-publish outcome;
//!   * two threads inserting the SAME key inside that window can double-insert
//!     (the engine serializes same-key puts through the PENDING check + writer
//!     pins; the deterministic duplicate tests exercise the idempotent path
//!     that is reachable once the hash is visible).
//! An alternative protocol (hash-as-claim: CAS the hash array 0→h and zero it
//! on tombstone) was considered and rejected as a larger departure from the
//! specified state-CAS-claim protocol.
//!
//! ## Memory ordering
//! All state loads are Acquire; all state CASes are Release/AcqRel. The GVA is
//! stored with Release before the PENDING→PUBLISHED CAS, so an Acquire load of
//! PUBLISHED guarantees visibility of both the GVA and the owner's hash store.
//!
//! ## Known limitation (documented; closed at engine level)
//! Lookup re-checks the state after loading the GVA, closing the
//! publish↔invalidate race. A full invalidate→reinsert→publish ABA on the same
//! slot between a reader's two loads could still deliver a stale GVA; D15
//! closes this at the engine level (reader refs pinned at GVA-resolution time;
//! a pinned scope cannot be reclaimed). A per-slot version counter would close
//! it inside the index if ever needed.

use crate::config::Config;
use crate::types::{BlockKey, Gva, Lookup};
use crate::util::key_hash;
use std::sync::atomic::{AtomicU64, Ordering};

/// Slot states (D13). Stored in `states[i]`; zero-initialized slots are EMPTY.
pub const EMPTY: u64 = 0;
pub const PENDING: u64 = 1;
pub const PUBLISHED: u64 = 2;
pub const TOMBSTONE: u64 = 3;

/// Re-probe budget after losing a CAS race. Each loss implies concurrent
/// progress by another thread, so termination is guaranteed in practice; the
/// bound only guards against pathological livelock.
const RACE_RETRIES: usize = 64;

/// Returned by [`Index::insert_pending`] when no EMPTY/TOMBSTONE slot exists.
#[derive(Debug)]
pub struct IndexFull;

/// Fixed-capacity lock-free open-addressed hash table: `BlockKey` → `Gva`.
/// Wait-free reads (single Acquire loads along the probe), lock-free writes.
pub struct Index {
    states: Box<[AtomicU64]>,
    hashes: Box<[AtomicU64]>,
    gvas: Box<[AtomicU64]>,
    cap: u64,
}

impl Index {
    pub fn new(capacity: u64) -> Index {
        assert!(capacity > 0, "index capacity must be positive");
        let n = capacity as usize;
        Index {
            states: (0..n).map(|_| AtomicU64::new(EMPTY)).collect(),
            hashes: (0..n).map(|_| AtomicU64::new(0)).collect(),
            gvas: (0..n).map(|_| AtomicU64::new(0)).collect(),
            cap: capacity,
        }
    }

    /// Fixed capacity from config (no resize — OQ-16 / D13).
    pub fn from_config(cfg: &Config) -> Index {
        Index::new(cfg.index_capacity)
    }

    pub fn capacity(&self) -> u64 {
        self.cap
    }

    /// Claim a PENDING slot for `key` (T2). Idempotent for duplicate
    /// identities: if the key is already PENDING/PUBLISHED, its existing slot
    /// is returned. `Err(IndexFull)` when no EMPTY/TOMBSTONE slot exists.
    pub fn insert_pending(&self, key: &BlockKey) -> Result<usize, IndexFull> {
        let h = key_hash(key.session, key.layer, key.token_start);
        let cap = self.cap as usize;
        let start = (h % self.cap) as usize;
        for i in 0..cap {
            let idx = (start + i) % cap;
            let st = self.states[idx].load(Ordering::Acquire);
            match st {
                EMPTY | TOMBSTONE => {
                    // Claim by CAS; losers re-probe (D13).
                    if self.states[idx]
                        .compare_exchange(st, PENDING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        // Owner-exclusive hash store, AFTER winning the claim
                        // (see module doc for why it cannot precede the CAS).
                        self.hashes[idx].store(h, Ordering::Release);
                        return Ok(idx);
                    }
                    // Lost the race → re-probe from the next slot.
                }
                PENDING | PUBLISHED => {
                    if self.hashes[idx].load(Ordering::Acquire) == h {
                        // Duplicate identity → idempotent existing slot (T2).
                        return Ok(idx);
                    }
                    // Different key: keep probing.
                }
                _ => unreachable!("corrupt index state {st}"),
            }
        }
        Err(IndexFull)
    }

    /// Publish-after-write (T3): store the GVA with Release, then flip
    /// PENDING → PUBLISHED. Returns false if the slot is no longer PENDING
    /// (rolled back, or already PUBLISHED — double-put detection aid).
    ///
    /// The PENDING pre-check guards payload immutability: a late/double
    /// publish on a non-PENDING slot must not clobber the already-published
    /// GVA. The engine additionally serializes same-key puts via writer pins,
    /// so the check-then-act window is not exercised concurrently.
    pub fn publish(&self, slot: usize, gva: Gva) -> bool {
        debug_assert!(slot < self.cap as usize);
        if self.states[slot].load(Ordering::Acquire) != PENDING {
            return false;
        }
        self.gvas[slot].store(gva.0, Ordering::Release);
        self.states[slot]
            .compare_exchange(PENDING, PUBLISHED, Ordering::Release, Ordering::Acquire)
            .is_ok()
    }

    /// Abort an in-flight write: PENDING → TOMBSTONE (never EMPTY — the
    /// tombstone keeps probe chains intact, EGM finding; D13).
    pub fn rollback(&self, slot: usize) -> bool {
        debug_assert!(slot < self.cap as usize);
        self.states[slot]
            .compare_exchange(PENDING, TOMBSTONE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Tombstone-invalidate the entry for `key` (reclaim path, D15). Returns
    /// true iff this call's CAS won. EMPTY terminates the probe; TOMBSTONE
    /// slots are skipped so chains stay intact.
    pub fn invalidate(&self, key: &BlockKey) -> bool {
        let h = key_hash(key.session, key.layer, key.token_start);
        let cap = self.cap as usize;
        let start = (h % self.cap) as usize;
        for _ in 0..RACE_RETRIES {
            let mut raced = false;
            for i in 0..cap {
                let idx = (start + i) % cap;
                let st = self.states[idx].load(Ordering::Acquire);
                match st {
                    EMPTY => return false,
                    TOMBSTONE => {}
                    PENDING | PUBLISHED => {
                        if self.hashes[idx].load(Ordering::Acquire) != h {
                            continue;
                        }
                        if self.states[idx]
                            .compare_exchange(st, TOMBSTONE, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            return true;
                        }
                        // State moved under us (publish raced) → restart probe.
                        raced = true;
                        break;
                    }
                    _ => unreachable!("corrupt index state {st}"),
                }
            }
            if !raced {
                return false; // full cycle: no live entry for this key
            }
        }
        false
    }

    /// Lock-free lookup (T2/T21). EMPTY terminates → Miss; TOMBSTONE is
    /// skipped; PENDING with matching hash → Pending; PUBLISHED with matching
    /// hash → the GVA is loaded with Acquire and the state re-checked before
    /// Hit is returned (readers never observe PUBLISHED with a stale GVA, T3).
    pub fn lookup(&self, key: &BlockKey) -> Lookup {
        let h = key_hash(key.session, key.layer, key.token_start);
        let cap = self.cap as usize;
        let start = (h % self.cap) as usize;
        for _ in 0..RACE_RETRIES {
            let mut raced = false;
            for i in 0..cap {
                let idx = (start + i) % cap;
                let st = self.states[idx].load(Ordering::Acquire);
                match st {
                    EMPTY => return Lookup::Miss,
                    TOMBSTONE => {}
                    PENDING => {
                        if self.hashes[idx].load(Ordering::Acquire) == h {
                            return Lookup::Pending;
                        }
                    }
                    PUBLISHED => {
                        if self.hashes[idx].load(Ordering::Acquire) != h {
                            continue;
                        }
                        let g = self.gvas[idx].load(Ordering::Acquire);
                        if self.states[idx].load(Ordering::Acquire) == PUBLISHED {
                            return Lookup::Hit(Gva(g));
                        }
                        // PUBLISHED → TOMBSTONE raced under us: re-probe.
                        raced = true;
                        break;
                    }
                    _ => unreachable!("corrupt index state {st}"),
                }
            }
            if !raced {
                return Lookup::Miss;
            }
        }
        Lookup::Miss
    }
}
