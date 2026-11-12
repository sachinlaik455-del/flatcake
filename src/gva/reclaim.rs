//! gva::reclaim — session-scope registry and the watermark reclamation
//! protocol (T6, D15, ARCHITECTURE §5).
//!
//! ## Scope registry
//! One `ScopeState` per session scope: medium (placement stickiness),
//! creation timestamp (oldest-first reclaim order), reader/writer refcounts,
//! the DRAM/SSD extents the scope owns, and the index keys it published.
//!
//! ## D15 reclamation sequence (per reclaimed scope)
//! (1) pick the OLDEST scope with no reader and no writer refs
//!     (order key = `(creation_ts_us, session)`);
//! (2) **tombstone-invalidate its index entries FIRST** — kills stale
//!     GVA→freed-offset wrong-data hits before any byte is freed;
//! (3) return extents via the O(1) metadata free ops — zero payload-byte
//!     operations, asserted through `metrics.reclaim_payload_ops` (EGM #12c);
//! (4) remove the scope; `metrics.reclaim_events += 1`.
//!
//! The registry lock is held across (select → tombstone → free → remove),
//! making the reader/writer ref-check atomic against pins. This closes the
//! resolve→read TOCTOU and the in-flight-writer window (D15's three race
//! windows).
//!
//! ## LOCK ORDER (crate-wide)
//! registry → {index, arena, layout}; never the reverse. No lock is held
//! across physical file I/O — `free_extent` on both traits is a metadata-only
//! op, so holding the registry lock across it is legal (§3 invariant).
//!
//! ## Engine contract (required for the protocol to hold)
//! * a writer calls `pin_writer(session)` BEFORE `arena.reserve` / layout
//!   append, and `unpin_writer` only after extents and keys are registered —
//!   a PENDING write pins its scope;
//! * a reader pins `reader_refs` at GVA-resolution time and holds it until
//!   that request's I/O completes; `pin_reader` returning false means the
//!   scope was reclaimed concurrently = clean miss (recompute path);
//! * `create_scope` is idempotent (first creation wins, pinning the
//!   oldest-first reclaim order).
//!
//! ## Watermark semantics (ARCHITECTURE §5, OQ-12)
//! A pass triggers when `usage > high_watermark × capacity`; it then frees
//! oldest-unreferenced scopes until `usage ≤ low_watermark × capacity` or
//! until nothing is reclaimable (advisory watermark — writes continue, §5).
//! DRAM capacity = arena capacity over `arena.used_bytes()`; SSD capacity =
//! `gpu_file_pool × gpu_file_bytes` over logical bytes written.
//!
//! ## Mixed-medium scopes (spill flip, OQ-6)
//! A scope that flipped to SSD mid-life still owns already-placed DRAM
//! extents (write-once, never migrated). Each pass therefore classifies every
//! candidate key through the index at reclaim time: the DRAM pass invalidates
//! only keys resolving to DRAM GVAs and frees only `dram_extents`, keeping
//! the scope alive until the SSD pass retires the rest. A scope is removed
//! once it holds no extents of either medium and no unresolved keys.
//!
//! ## Decoupling
//! This module works only against the frozen [`DramArena`] / [`SsdLayout`]
//! traits (`types.rs`) — it never touches concrete `cio` types (the CIO
//! module is developed in parallel against the same frozen contract).

use crate::config::Config;
use crate::gva::index::Index;
use crate::types::{BlockKey, DramArena, Lookup, Medium, Metrics, SsdLayout};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Per-session-scope reclamation state (single source of truth, §3).
#[derive(Debug)]
pub struct ScopeState {
    pub medium: Medium,
    pub creation_ts_us: u64,
    pub reader_refs: AtomicU64,
    pub writer_refs: AtomicU64,
    /// `(arena offset, len)` extents owned in the DRAM pool.
    pub dram_extents: Vec<(u64, u64)>,
    /// `(file_seq, in-file offset, len)` extents owned in GPU Files.
    pub ssd_extents: Vec<(u32, u64, u64)>,
    /// Block keys published by this scope (tombstoned before extents free).
    pub index_keys: Vec<BlockKey>,
}

/// Session-scope registry (reader/writer refcounts + extent ownership).
#[derive(Debug)]
pub struct ScopeRegistry {
    scopes: Mutex<HashMap<u64, ScopeState>>,
}

impl ScopeRegistry {
    pub fn new() -> ScopeRegistry {
        ScopeRegistry {
            scopes: Mutex::new(HashMap::new()),
        }
    }

    /// Register a scope; idempotent — if it exists, the first creation (and
    /// its `creation_ts_us`) wins, pinning the oldest-first reclaim order.
    /// Returns true on new creation.
    pub fn create_scope(&self, session: u64, medium: Medium, creation_ts_us: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        if m.contains_key(&session) {
            return false;
        }
        m.insert(
            session,
            ScopeState {
                medium,
                creation_ts_us,
                reader_refs: AtomicU64::new(0),
                writer_refs: AtomicU64::new(0),
                dram_extents: Vec::new(),
                ssd_extents: Vec::new(),
                index_keys: Vec::new(),
            },
        );
        true
    }

    /// Register a DRAM extent (arena offset) with the scope.
    pub fn add_dram_extent(&self, session: u64, off: u64, len: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                s.dram_extents.push((off, len));
                true
            }
            None => false,
        }
    }

    /// Active reader count for a scope (0 = no in-flight request holds it).
    /// P3 migration uses this to close the read-vs-free race on extent moves.
    pub fn active_readers(&self, session: u64) -> u64 {
        let m = self.scopes.lock().expect("scope registry poisoned");
        m.get(&session)
            .map(|s| s.reader_refs.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Active writer count for a scope (0 = no PENDING write in flight).
    pub fn active_writers(&self, session: u64) -> u64 {
        let m = self.scopes.lock().expect("scope registry poisoned");
        m.get(&session)
            .map(|s| s.writer_refs.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Remove a DRAM extent from the scope's ownership (P3 migration: the extent
    /// was moved elsewhere; removing prevents double-free at reclaim).
    pub fn remove_dram_extent(&self, session: u64, off: u64, len: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                if let Some(pos) = s.dram_extents.iter().position(|&(o, l)| o == off && l == len) {
                    s.dram_extents.swap_remove(pos);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Remove an SSD extent from the scope's ownership (P3 migration).
    pub fn remove_ssd_extent(&self, session: u64, file_seq: u32, off: u64, len: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                if let Some(pos) = s
                    .ssd_extents
                    .iter()
                    .position(|&(f, o, l)| f == file_seq && o == off && l == len)
                {
                    s.ssd_extents.swap_remove(pos);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Register an SSD extent (GPU File segment) with the scope.
    pub fn add_ssd_extent(&self, session: u64, file_seq: u32, off: u64, len: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                s.ssd_extents.push((file_seq, off, len));
                true
            }
            None => false,
        }
    }

    /// Register a published index key with the scope (before `unpin_writer`).
    pub fn add_key(&self, session: u64, key: BlockKey) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                s.index_keys.push(key);
                true
            }
            None => false,
        }
    }

    /// Reader ref — taken at GVA-resolution time, held until that request's
    /// I/O completes (D15). False = scope unknown/gone → clean miss.
    pub fn pin_reader(&self, session: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                s.reader_refs.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn unpin_reader(&self, session: u64) {
        self.dec_ref(session, |s| &s.reader_refs);
    }

    /// Writer ref — a PENDING write pins its scope (D15).
    pub fn pin_writer(&self, session: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                s.writer_refs.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn unpin_writer(&self, session: u64) {
        self.dec_ref(session, |s| &s.writer_refs);
    }

    /// Current scope medium (placement stickiness input). None = unknown.
    pub fn sticky_medium(&self, session: u64) -> Option<Medium> {
        let m = self.scopes.lock().expect("scope registry poisoned");
        m.get(&session).map(|s| s.medium)
    }

    /// DRAM-full spill (OQ-6, EGM #5): the scope flips to SSD; already-placed
    /// DRAM extents are not migrated (write-once invariant) but stay
    /// reclaimable by the DRAM pass via extent ownership.
    pub fn flip_to_ssd(&self, session: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        match m.get_mut(&session) {
            Some(s) => {
                s.medium = Medium::Ssd;
                true
            }
            None => false,
        }
    }

    /// Remove the registry entry (engine teardown). Extent freeing / key
    /// tombstoning outside the reclamation protocol are the caller's job.
    pub fn drop_scope(&self, session: u64) -> bool {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        m.remove(&session).is_some()
    }

    pub fn scope_exists(&self, session: u64) -> bool {
        self.scopes
            .lock()
            .expect("scope registry poisoned")
            .contains_key(&session)
    }

    /// Saturating decrement (extra unpins are ignored). All ref access is
    /// serialized by the registry mutex, so plain load/store is exact.
    fn dec_ref(&self, session: u64, which: fn(&ScopeState) -> &AtomicU64) {
        let mut m = self.scopes.lock().expect("scope registry poisoned");
        if let Some(s) = m.get_mut(&session) {
            let r = which(s);
            let cur = r.load(Ordering::Relaxed);
            if cur > 0 {
                r.store(cur - 1, Ordering::Relaxed);
            }
        }
    }
}

/// The store whose watermark triggered the pass — reclaim works only against
/// the frozen traits (`arena_or_layout`): the DRAM arena for a DRAM pass, the
/// GPU-file layout for an SSD pass.
#[derive(Clone, Copy)]
pub enum ReclaimStore<'a> {
    Dram(&'a dyn DramArena),
    Ssd(&'a dyn SsdLayout),
}

/// One reclamation pass (T6). Returns the total number of payload bytes
/// freed. Triggers only when `usage > high × capacity`; drains to
/// `usage ≤ low × capacity` (OQ-12); returns early (0 freed) below the high
/// watermark and stops with the advisory-watermark break when nothing is
/// reclaimable (ARCHITECTURE §5).
pub fn reclaim_pass(
    medium: Medium,
    usage_bytes: u64,
    cfg: &Config,
    registry: &ScopeRegistry,
    index: &Index,
    store: ReclaimStore<'_>,
    metrics: &mut Metrics,
) -> u64 {
    debug_assert!(matches!(
        (medium, &store),
        (Medium::Dram, ReclaimStore::Dram(_)) | (Medium::Ssd, ReclaimStore::Ssd(_))
    ));
    let (high_wm, low_wm, capacity) = match store {
        ReclaimStore::Dram(a) => (cfg.dram_high_watermark, cfg.dram_low_watermark, a.capacity()),
        ReclaimStore::Ssd(l) => (
            cfg.ssd_high_watermark,
            cfg.ssd_low_watermark,
            l.file_capacity() * cfg.gpu_file_pool,
        ),
    };
    let high_limit = (high_wm * capacity as f64) as u64;
    let low_limit = (low_wm * capacity as f64) as u64;
    let reclaiming_ssd = medium == Medium::Ssd;

    if usage_bytes <= high_limit {
        return 0;
    }
    let mut usage = usage_bytes;
    let mut freed_total = 0u64;

    while usage > low_limit {
        // One registry-lock hold per scope: the ref-check, the tombstone
        // pass, the frees and the removal are atomic against every other
        // registry operation (pin/unpin/create/add) — this closes the
        // resolve→read TOCTOU and the in-flight-writer window (D15).
        let mut scopes = registry.scopes.lock().expect("scope registry poisoned");

        // (1) oldest unreferenced scope owning extents of this medium.
        let victim = scopes
            .iter()
            .filter(|(_, s)| {
                s.reader_refs.load(Ordering::Relaxed) == 0
                    && s.writer_refs.load(Ordering::Relaxed) == 0
            })
            .filter(|(_, s)| match medium {
                Medium::Dram => !s.dram_extents.is_empty(),
                Medium::Ssd => !s.ssd_extents.is_empty(),
            })
            .min_by_key(|(&session, s)| (s.creation_ts_us, session))
            .map(|(&session, _)| session);

        let session = match victim {
            Some(s) => s,
            None => break, // advisory watermark: nothing reclaimable (§5)
        };
        let state = scopes.get_mut(&session).expect("victim vanished under lock");

        // (2) tombstone-invalidate THIS medium's index keys FIRST (D15: no
        // stale GVA may survive into a freed offset). Keys resolving to the
        // other medium (mixed scopes after a spill flip) stay untouched.
        let mut kept_keys = Vec::new();
        for k in state.index_keys.iter() {
            match index.lookup(k) {
                Lookup::Hit(g) => {
                    if g.is_ssd() == reclaiming_ssd {
                        index.invalidate(k);
                    } else {
                        kept_keys.push(*k);
                    }
                }
                // In-flight leftover without a resolved medium: keep until it
                // resolves (the engine's rollback is the normal cleaner).
                Lookup::Pending => kept_keys.push(*k),
                Lookup::Miss => {}
            }
        }
        state.index_keys = kept_keys;

        // (3) free extents — O(1) metadata ops, ZERO payload-byte operations.
        // metrics.reclaim_payload_ops must remain 0 (EGM #12c): any future
        // non-metadata free implementation must increment it and be caught.
        let mut scope_bytes = 0u64;
        match store {
            ReclaimStore::Dram(arena) => {
                for (off, len) in std::mem::take(&mut state.dram_extents) {
                    arena.free_extent(off, len);
                    scope_bytes += len;
                }
            }
            ReclaimStore::Ssd(layout) => {
                for (file_seq, off, len) in std::mem::take(&mut state.ssd_extents) {
                    layout.free_extent(file_seq, off, len);
                    scope_bytes += len;
                }
            }
        }

        // (4) remove the scope once nothing of it remains.
        let other_medium_empty = match medium {
            Medium::Dram => state.ssd_extents.is_empty(),
            Medium::Ssd => state.dram_extents.is_empty(),
        };
        if other_medium_empty && state.index_keys.is_empty() {
            scopes.remove(&session);
            metrics.reclaim_events += 1;
        }
        drop(scopes);

        freed_total += scope_bytes;
        usage = usage.saturating_sub(scope_bytes);
    }

    freed_total
}
