//! FlatCakeEngine (T10/T15, ARCHITECTURE §2): integrates FC-GVA (placement/index/
//! arena/reclaim), FC-CIO (layout/coalesce/stream), FC-MAS (dual-queue scheduler)
//! behind the `KvEngine` interface. Mirrors paper Fig. 3:
//! - write path: scope-direct placement → GVA allocate → physical write → publish;
//! - control path: dual queues — pure-DRAM requests enter the DRAM-Ready Queue and
//!   are dispatched by dram workers in arrival order; the DRAM-resident portion of
//!   hybrid requests is delivered inline (paper §3.3: DRAM blocks are "pinned in
//!   place" while FC-CIO prefetches the SSD blocks); SSD requests are prefetched
//!   under HRRN and RE-ADMITTED by original arrival_ts (completion order follows
//!   arrival order);
//! - data path: DRAM hits via arena copies; SSD hits via coalesced bounded-window
//!   reads (per-ssd-worker `Reader`).
//!
//! Contract notes honored from module docs:
//! - registry lock order registry → {index, arena, layout} (gva/mod.rs);
//! - pin_writer before arena.reserve; add extents+keys before unpin_writer;
//! - pin_reader taken at resolution time, released when the request completes (D15);
//! - reclaim trigger usage > high_wm × capacity, drain to ≤ low_wm (OQ-12);
//! - reclaimed SSD scopes are detached from the layout (release_session) so no
//!   stale scope can append into a recycled file (A2 finding M2).
//!
//! A2 (docs/PHASE_2_ARCHITECTURE.md) findings addressed in this revision:
//! M1 trait imports, M2 stale-scope detach, M3 read_ranges 5-arg + local-metrics
//! merge, M4 no double-Arc, R5 hybrid DRAM-block delivery, error-swallowing
//! hardening (reconcile-undelivered-as-recompute), shutdown-bounded waits.

use crate::cio::{coalesce, read_ranges, Reader, SsdLayoutImpl};
use crate::config::Config;
use crate::engine_api::{GetSpec, KvEngine, PutSpec};
use crate::gva::arena::Arena;
use crate::gva::index::Index;
use crate::gva::placement;
use crate::gva::reclaim::{reclaim_pass, ReclaimStore, ScopeRegistry};
use crate::hbm::HbmWindow;
use crate::mas::scheduler::{PoppedReq, Scheduler};
use crate::types::{
    BlockKey, DramArena, Gva, Lookup, Medium, Metrics, PlacementSignals, Request, SsdLayout,
};
use crate::util::payload_fill;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Bounded read window per SSD worker — the Level-B analog of the paper's
/// double-buffered GPU HBM staging window sized to two transformer layers (§3.4):
/// O(1) per worker, independent of request size. Code constant (documented;
/// config keys stay limited to measured, experiment-needed knobs).
const SSD_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// Per-request completion slot: workers fill `out[idx]` (indexed by the ORIGINAL
/// block position); the caller delivers to its sink on its own thread after
/// completion, preserving order.
struct Slot {
    out: Vec<Option<Vec<u8>>>,
    /// (original block idx, GVA) the worker must serve; empty = completion marker.
    map: Vec<(usize, Gva)>,
    err: Option<String>,
    done: bool,
    /// Duplicate-request coalescing: leader owns the fingerprint; followers
    /// wait on this slot and merge `out` by GVA match (audit item 12).
    fp: u64,
}

struct EngineCore {
    cfg: Config,
    hbm: Mutex<HbmWindow>,
    index: Index,
    arena: Arena,
    registry: ScopeRegistry,
    layout: SsdLayoutImpl,
    metrics: Mutex<Metrics>,
    scheduler: Arc<Scheduler>,
    slots: Mutex<HashMap<u64, Arc<(Mutex<Slot>, Condvar)>>>,
    next_id: AtomicU64,
    /// Sessions this engine has placed on SSD — used to detach reclaimed scopes
    /// from the layout (A2 finding M2: a stale scope entry must never append
    /// into a recycled file).
    ssd_sessions: Mutex<HashSet<u64>>,
    /// M11 adaptive evidence: served gets per session (ADAPTIVE_EXTENSION.md).
    gets_served: Mutex<HashMap<u64, u64>>,
    // -------- P3 medium-aware migration state (inert while migration_enabled=0)
    /// Observed reuse heat per delivered block: (count, last-served us). Bounded:
    /// cold entries dropped at the cap, then cleared if still over.
    heat: Mutex<HashMap<BlockKey, (u64, u64)>>,
    /// DRAM-resident blocks by arena offset: (key, len, last-served us). Bounded
    /// by arena capacity / block size; purged when scopes are reclaimed.
    dram_blocks: Mutex<HashMap<u64, (BlockKey, u64, u64)>>,
    /// Extents detached by migration but not yet returned to their pool: the
    /// bytes stay readable until a flush sees no in-flight reader/writer.
    pending_frees: Mutex<Vec<PendingFree>>,
    /// Per-block last-migration time (anti-thrash cooldown).
    last_migrated: Mutex<HashMap<BlockKey, u64>>,
    /// Serializes migration state transitions (publish swaps + bookkeeping).
    migration_lock: Mutex<()>,
    // -------- P4 durability + request coalescing -----------------------------
    /// Recovery log writer (audit item 2: SSD publishes + invalidates only;
    /// DRAM publishes are process-volatile and never logged).
    recovery_log: Option<Mutex<std::io::BufWriter<std::fs::File>>>,
    /// In-flight SSD request dedup: fingerprint -> leader slot (audit item 11).
    inflight: Mutex<HashMap<u64, Arc<(Mutex<Slot>, Condvar)>>>,
}

/// One deferred extent free: bytes stay valid until the owning scope has no
/// in-flight readers/writers (closes the read-vs-free race; D15 analog).
#[derive(Debug)]
enum PendingFree {
    Dram { off: u64, len: u64, session: u64 },
    Ssd { file_seq: u32, off: u64, len: u64, session: u64 },
}

const HEAT_CAP: usize = 65_536;
const PENDING_FREE_CAP: usize = 65_536;

pub struct FlatCakeEngine {
    core: Arc<EngineCore>,
    running: Arc<AtomicBool>,
    worker_handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl FlatCakeEngine {
    pub fn new(cfg: &Config) -> io::Result<FlatCakeEngine> {
        // P4 recovery log (audit items 2/4/5): opened in append mode. Replay is
        // OPT-IN (recovery_log = 2) and assumes a same-workload restart — a
        // different trace reusing session ids must NOT see stale blocks as
        // valid (integrity would catch it, but correctness first). The log is
        // always written when enabled so a crashed run's state is recoverable.
        let recovery_log = if cfg.recovery_log >= 1 {
            std::fs::create_dir_all(&cfg.ssd_dir)?;
            let path = format!("{}/index_log.jsonl", cfg.ssd_dir);
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            Some(Mutex::new(std::io::BufWriter::new(f)))
        } else {
            None
        };
        let core = Arc::new(EngineCore {
            cfg: cfg.clone(),
            hbm: Mutex::new(HbmWindow::new(cfg.hbm_window_bytes)),
            index: Index::from_config(cfg),
            arena: Arena::new(cfg),
            registry: ScopeRegistry::new(),
            layout: SsdLayoutImpl::new(cfg)?,
            metrics: Mutex::new(Metrics::default()),
            scheduler: Scheduler::new(cfg.queue_cap),
            slots: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ssd_sessions: Mutex::new(HashSet::new()),
            gets_served: Mutex::new(HashMap::new()),
            heat: Mutex::new(HashMap::new()),
            dram_blocks: Mutex::new(HashMap::new()),
            pending_frees: Mutex::new(Vec::new()),
            last_migrated: Mutex::new(HashMap::new()),
            migration_lock: Mutex::new(()),
            recovery_log,
            inflight: Mutex::new(HashMap::new()),
        });
        if cfg.recovery_log == 2 {
            // Replay: last record per key wins; corrupt/partial tail lines are
            // dropped (crash mid-log-write). Only SSD-backed records recover.
            let path = format!("{}/index_log.jsonl", cfg.ssd_dir);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let mut live: HashMap<BlockKey, Gva> = HashMap::new();
                let mut dead: HashSet<BlockKey> = HashSet::new();
                let mut truncated = 0u64;
                for line in text.lines() {
                    match parse_log_line(line) {
                        Some(LogRec::Pub(k, g)) => {
                            dead.remove(&k);
                            live.insert(k, g);
                        }
                        Some(LogRec::Invalidate(k)) => {
                            live.remove(&k);
                            dead.insert(k);
                        }
                        None => truncated += 1,
                    }
                }
                let mut replayed = 0u64;
                let mut session_cursor: HashMap<u64, (u32, u64)> = HashMap::new();
                let bb = cfg.block_bytes();
                for (k, g) in live {
                    if let Ok(slot) = core.index.insert_pending(&k) {
                        core.index.publish(slot, g);
                        replayed += 1;
                        let fr = core.layout.resolve(g, bb);
                        let cur = session_cursor
                            .entry(k.session)
                            .or_insert((fr.file_seq, 0));
                        cur.0 = fr.file_seq;
                        cur.1 = cur.1.max(fr.in_file_off + bb);
                    }
                }
                for (session, (fs, cur)) in session_cursor {
                    core.layout.adopt_session(session, fs, cur);
                    core.registry.create_scope(session, Medium::Ssd, 0);
                    core.ssd_sessions.lock().unwrap().insert(session);
                }
                let mut m = core.metrics.lock().unwrap();
                m.recovery_replayed = replayed;
                m.recovery_truncated = truncated;
            }
        }
        let running = Arc::new(AtomicBool::new(true));
        let mut handles = Vec::new();
        for _ in 0..cfg.dram_workers {
            let core = core.clone();
            let running = running.clone();
            handles.push(std::thread::spawn(move || dram_worker(core, running)));
        }
        for _ in 0..cfg.ssd_workers {
            let core = core.clone();
            let running = running.clone();
            let ssd_dir = cfg.ssd_dir.clone();
            handles.push(std::thread::spawn(move || {
                let mut reader = Reader::new(&ssd_dir);
                ssd_worker(core, running, &mut reader);
            }));
        }
        Ok(FlatCakeEngine {
            core,
            running,
            worker_handles: Mutex::new(handles),
        })
    }

    fn now_us(&self) -> u64 {
        use std::time::SystemTime;
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }

    /// Resolve/create the session scope; returns its (possibly flipped) medium.
    /// M11: the configured policy (static/fixed/adaptive) wraps the paper's
    /// Table-2 precedence; adaptive uses observed reuse evidence + DRAM headroom.
    fn scope_medium(&self, sig: &PlacementSignals) -> Medium {
        let sticky = self.core.registry.sticky_medium(sig.session);
        let gets = self
            .core
            .gets_served
            .lock()
            .unwrap()
            .get(&sig.session)
            .copied();
        let headroom = 1.0 - (self.core.arena.used_bytes() as f64
            / self.core.arena.capacity() as f64);
        let medium = placement::decide_policy(sig, sticky, &self.core.cfg, gets, headroom);
        if sticky.is_none() {
            self.core.registry.create_scope(sig.session, medium, self.now_us());
        } else if medium == Medium::Ssd && sticky == Some(Medium::Dram) {
            // DRAM scope re-evaluation flipped to SSD (D14; old blocks never migrate).
            self.core.registry.flip_to_ssd(sig.session);
        }
        self.core.registry.sticky_medium(sig.session).unwrap_or(medium)
    }

    /// Writer pin with scope-recreation retry: a concurrent reclamation may have
    /// dropped the scope between creation and pinning (D15 race window).
    fn pin_writer_retry(&self, sig: &PlacementSignals) -> io::Result<()> {
        for _ in 0..3 {
            if self.core.registry.pin_writer(sig.session) {
                return Ok(());
            }
            self.scope_medium(sig);
        }
        Err(io::Error::new(
            io::ErrorKind::StaleNetworkFileHandle,
            "scope repeatedly reclaimed during writer pin",
        ))
    }

    /// Advisory watermark reclamation (ARCHITECTURE §5) + reclaimed-scope detach.
    fn maybe_reclaim(&self) {
        // P3 order (medium_aware): flush deferred frees -> demote cold DRAM
        // blocks (cheaper than losing whole scopes) -> session reclaims only if
        // STILL over their watermarks -> flush again + purge dead bookkeeping.
        // With eviction_policy = session_extents (paper default), demote_pass
        // no-ops and the behavior is identical to P1/P2.
        self.flush_pending_frees();
        self.demote_pass(self.now_us());
        self.flush_pending_frees();
        let mut m = self.core.metrics.lock().unwrap();
        let dram_usage = self.core.arena.used_bytes();
        if (dram_usage as f64)
            > self.core.cfg.dram_high_watermark * self.core.arena.capacity() as f64
        {
            reclaim_pass(
                Medium::Dram,
                dram_usage,
                &self.core.cfg,
                &self.core.registry,
                &self.core.index,
                ReclaimStore::Dram(&self.core.arena),
                &mut m,
            );
        }
        let ssd_usage = self.core.layout.logical_bytes();
        let ssd_cap = self.core.cfg.gpu_file_pool * self.core.cfg.gpu_file_bytes;
        if (ssd_usage as f64) > self.core.cfg.ssd_high_watermark * ssd_cap as f64 {
            let t_reclaim = std::time::Instant::now();
            reclaim_pass(
                Medium::Ssd,
                ssd_usage,
                &self.core.cfg,
                &self.core.registry,
                &self.core.index,
                ReclaimStore::Ssd(&self.core.layout),
                &mut m,
            );
            m.ssd_over_high_watermark_us += t_reclaim.elapsed().as_micros() as u64;
            drop(m);
            // Detach reclaimed scopes from the layout: any session we placed on SSD
            // that no longer exists in the registry must be released, or its stale
            // layout entry could append into a recycled file (A2 M2).
            let mut ssd_sessions = self.core.ssd_sessions.lock().unwrap();
            for session in ssd_sessions.clone().iter() {
                if !self.core.registry.scope_exists(*session) {
                    self.core.layout.release_session(*session);
                    ssd_sessions.remove(session);
                }
            }
        }
        self.flush_pending_frees();
        self.purge_dead_scope_bookkeeping();
    }

    /// Re-persist a recomputed block (symmetric miss model, ARCHITECTURE §8).
    fn recompute_persist(
        &self,
        key: &BlockKey,
        sig: &PlacementSignals,
        payload: &[u8],
    ) -> io::Result<()> {
        let medium = self.scope_medium(sig);
        self.pin_writer_retry(sig)?;
        let slot = self.core.index.insert_pending(key).ok();
        let bb = payload.len() as u64;
        let persist = match medium {
            Medium::Dram => match self.core.arena.reserve(bb) {
                Ok(off) => {
                    self.core.arena.write(off, payload);
                    if let Some(s) = slot {
                        let gva = Gva::encode(0, 0, off).unwrap();
                        self.core.index.publish(s, gva);
                    }
                    self.core.registry.add_dram_extent(sig.session, off, bb);
                    self.core.dram_blocks.lock().unwrap().insert(
                        off,
                        (*key, bb, self.now_us()),
                    );
                    Ok(())
                }
                Err(_) => {
                    // DRAM-full spill on re-persist (OQ-6): flip scope, write to SSD.
                    self.core.registry.flip_to_ssd(sig.session);
                    self.core.metrics.lock().unwrap().dram_spill_puts += 1;
                    self.append_one_ssd(sig.session, key, slot, payload)
                }
            },
            Medium::Ssd => self.append_one_ssd(sig.session, key, slot, payload),
        };
        self.core.registry.add_key(sig.session, *key);
        self.core.registry.unpin_writer(sig.session);
        persist
    }

    /// One batched SSD append for a recomputed (session, layer) chunk, then a
    /// publish + recovery-log record per block (payload durable first).
    fn flush_recompute_chunk(
        &self,
        session: u64,
        layer: u32,
        chunk: &mut Vec<(usize, BlockKey, u64)>,
        payload: &mut Vec<u8>,
        out: &mut Vec<Option<Vec<u8>>>,
    ) {
        if chunk.is_empty() {
            return;
        }
        chunk.sort_by_key(|(idx, _, _)| *idx);
        let n = chunk.len() as u32;
        let bb = self.block_bytes_of();
        let first_ts = chunk[0].1.token_start;
        let seeds: Vec<u64> = chunk.iter().map(|(_, _, s)| *s).collect();
        let gvas = self.core.layout.append_scope_batch(session, 1, n, |_, i| {
            let ts = first_ts + i as u64 * self.core.cfg.block_tokens;
            let mut buf = vec![0u8; bb as usize];
            payload_fill(session, layer, ts, seeds[i as usize], &mut buf);
            buf
        });
        if let Ok(gv) = gvas {
            for ((idx, b, seed), g) in chunk.iter().zip(gv.iter()) {
                if let Ok(slot) = self.core.index.insert_pending(b) {
                    self.core.index.publish(slot, *g);
                    self.log_publish(b, *g);
                }
                self.core
                    .metrics
                    .lock()
                    .unwrap()
                    .modeled_recompute_us +=
                    self.core.cfg.recompute_us_per_token * self.core.cfg.block_tokens;
                payload_fill(b.session, b.layer, b.token_start, *seed, payload);
                out[*idx] = Some(payload.clone());
                self.core.metrics.lock().unwrap().physical_write_bytes += bb;
                self.core.metrics.lock().unwrap().logical_write_bytes += bb;
                self.core.registry.add_key(session, *b);
                self.core.ssd_sessions.lock().unwrap().insert(session);
            }
        }
        chunk.clear();
    }

    /// Shared tail for dedup followers: accounting (audit item 13), unpin,
    /// delivery.
    fn finish_deduped_get(
        &self,
        spec: &GetSpec,
        mut out: Vec<Option<Vec<u8>>>,
        mut counted: Vec<bool>,
        pinned_scopes: Vec<u64>,
        sink: &mut dyn FnMut(usize, &[u8]),
        enqueued: Option<&(bool, Vec<(usize, Gva)>)>,
    ) -> io::Result<()> {
        if let Some((is_ssd, map)) = enqueued.as_ref() {
            for (idx, _) in map.iter() {
                if !counted[*idx] {
                    counted[*idx] = true;
                    let mut m = self.core.metrics.lock().unwrap();
                    m.requested_blocks += 1;
                    if out[*idx].is_some() {
                        if *is_ssd {
                            m.hit_ssd += 1;
                            m.logical_read_bytes += self.block_bytes_of();
                        } else {
                            m.hit_dram += 1;
                        }
                    } else {
                        m.misses += 1;
                    }
                }
            }
        }
        for s in pinned_scopes {
            self.core.registry.unpin_reader(s);
        }
        for (idx, b) in spec.blocks.iter().enumerate() {
            if let Some(bytes) = out[idx].as_ref() {
                self.core
                    .hbm
                    .lock()
                    .unwrap()
                    .record_delivery(b.session, b.layer, b.token_start, bytes);
                sink(idx, bytes);
            }
        }
        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += spec.blocks.len() as u64;
        m.delivered_requests += 1;
        Ok(())
    }

    /// Append one block to the session's SSD scope, publish, register extent.
    fn append_one_ssd(
        &self,
        session: u64,
        key: &BlockKey,
        slot: Option<usize>,
        payload: &[u8],
    ) -> io::Result<()> {
        let bb = payload.len() as u64;
        let gvas = self
            .core
            .layout
            .append_scope_batch(session, 1, 1, |_, _| payload.to_vec())?;
        if let (Some(s), Some(g)) = (slot, gvas.first()) {
            self.core.index.publish(s, *g);
            self.log_publish(key, *g);
        }
        if let Some(g) = gvas.first() {
            let fr = self.core.layout.resolve(*g, bb);
            self.core
                .registry
                .add_ssd_extent(session, fr.file_seq, fr.in_file_off, bb);
        }
        self.core.registry.add_key(session, *key);
        self.core.ssd_sessions.lock().unwrap().insert(session);
        self.core.metrics.lock().unwrap().physical_write_bytes += bb;
        Ok(())
    }

    fn block_bytes_of(&self) -> u64 {
        self.core.cfg.block_bytes()
    }

    /// Log an SSD publish record (audit item 2: after the payload write; DRAM
    /// publishes are never logged).
    fn log_publish(&self, key: &BlockKey, gva: Gva) {
        if let Some(w) = self.core.recovery_log.as_ref() {
            let mut w = w.lock().unwrap();
            let _ = std::io::Write::write_fmt(
                &mut *w,
                format_args!(
                    "{{\"p\":[{}, {}, {}],\"g\":{}}}\n",
                    key.session, key.layer, key.token_start, gva.0
                ),
            );
        }
    }

    /// Log an invalidate record (audit item 10: promotion/migration swaps must
    /// dead-log the previous SSD entry or replay resurrects freed extents).
    fn log_invalidate(&self, key: &BlockKey) {
        if let Some(w) = self.core.recovery_log.as_ref() {
            let mut w = w.lock().unwrap();
            let _ = std::io::Write::write_fmt(
                &mut *w,
                format_args!("{{\"i\":[{}, {}, {}]}}\n", key.session, key.layer, key.token_start),
            );
        }
    }

    /// Flush the recovery log (batch end / shutdown).
    fn flush_log(&self) {
        if let Some(w) = self.core.recovery_log.as_ref() {
            let _ = std::io::Write::flush(&mut *w.lock().unwrap());
        }
    }

    /// Record delivery heat; bounded (drop cold entries at the cap).
    fn record_heat(&self, key: &BlockKey, now: u64) {
        let mut heat = self.core.heat.lock().unwrap();
        let e = heat.entry(*key).or_insert((0, now));
        e.0 += 1;
        e.1 = now;
        if heat.len() > HEAT_CAP {
            heat.retain(|_, (c, _)| *c >= 2);
            if heat.len() > HEAT_CAP {
                heat.clear(); // pathological: all hot; lose state rather than grow
            }
        }
    }

    /// Cooldown gate (anti-thrash). Records the migration time on pass.
    fn cooldown_ok(&self, key: &BlockKey, now: u64, metrics: &mut Metrics) -> bool {
        let mut lm = self.core.last_migrated.lock().unwrap();
        if let Some(t) = lm.get(key) {
            if now.saturating_sub(*t) / 1000 < self.core.cfg.migration_cooldown_ms {
                metrics.migr_cooldown_skips += 1;
                return false;
            }
        }
        if lm.len() > HEAT_CAP {
            lm.clear();
        }
        lm.insert(*key, now);
        true
    }

    /// Swap a published block's GVA to `new_gva` (P3 migration protocol):
    /// invalidate old -> insert fresh PENDING -> publish new. Readers inside the
    /// window see Miss and recompute (immutable bytes stay correct); after the
    /// swap every lookup resolves to the new location.
    fn publish_swap(&self, key: &BlockKey, new_gva: Gva) -> bool {
        self.log_invalidate(key);
        self.core.index.invalidate(key);
        match self.core.index.insert_pending(key) {
            Ok(slot) => self.core.index.publish(slot, new_gva),
            Err(_) => false,
        }
    }

    /// Promote one delivered SSD block into DRAM using the bytes already in
    /// hand (zero extra SSD reads). Bytes are immutable so old/new copies are
    /// identical; the SSD extent is freed via the deferred-free list.
    fn promote_one(&self, key: &BlockKey, expected: Gva, bytes: &[u8], now: u64) -> bool {
        let bb = bytes.len() as u64;
        match self.core.index.lookup(key) {
            Lookup::Hit(g) if g.0 == expected.0 && g.is_ssd() => {}
            _ => return false,
        }
        if self.core.registry.active_writers(key.session) != 0 {
            return false;
        }
        {
            let mut m = self.core.metrics.lock().unwrap();
            if !self.cooldown_ok(key, now, &mut m) {
                return false;
            }
        }
        if (1.0 - self.core.arena.used_bytes() as f64 / self.core.arena.capacity() as f64)
            < self.core.cfg.adaptive_h_min
        {
            return false;
        }
        let Ok(off) = self.core.arena.reserve(bb) else {
            return false;
        };
        let _mig = self.core.migration_lock.lock().unwrap();
        if !matches!(self.core.index.lookup(key), Lookup::Hit(g) if g.0 == expected.0) {
            self.core.arena.free_extent(off, bb);
            return false;
        }
        self.core.arena.write(off, bytes);
        let new_gva = Gva::encode(0, 0, off).unwrap();
        if !self.publish_swap(key, new_gva) {
            self.core.arena.free_extent(off, bb);
            return false;
        }
        self.core.registry.add_dram_extent(key.session, off, bb);
        let fr = self.core.layout.resolve(expected, bb);
        if self
            .core
            .registry
            .remove_ssd_extent(key.session, fr.file_seq, fr.in_file_off, bb)
        {
            self.core.pending_frees.lock().unwrap().push(PendingFree::Ssd {
                file_seq: fr.file_seq,
                off: fr.in_file_off,
                len: bb,
                session: key.session,
            });
        }
        self.core
            .dram_blocks
            .lock()
            .unwrap()
            .insert(off, (*key, bb, now));
        let mut m = self.core.metrics.lock().unwrap();
        m.migr_promotions += 1;
        m.migr_bytes_moved += bb;
        true
    }

    /// Demote cold DRAM blocks to their session's SSD scope under pressure
    /// (eviction_policy = medium_aware). Victims: least-recently-served first;
    /// blocks with heat >= K are hot and stay. All frees are deferred.
    fn demote_pass(&self, now: u64) -> u64 {
        if self.core.cfg.migration_enabled == 0
            || self.core.cfg.eviction_policy != "medium_aware"
        {
            return 0;
        }
        let usage = self.core.arena.used_bytes();
        let cap = self.core.arena.capacity();
        if (usage as f64) <= self.core.cfg.dram_high_watermark * cap as f64 {
            return 0;
        }
        // SSD-pressure guard: demotion writes to SSD; skip when SSD is itself
        // over watermark (trading one pressure for another helps nobody).
        let ssd_cap = self.core.cfg.gpu_file_pool * self.core.cfg.gpu_file_bytes;
        if (self.core.layout.logical_bytes() as f64)
            > self.core.cfg.ssd_high_watermark * ssd_cap as f64
        {
            return 0;
        }
        let target = (self.core.cfg.dram_low_watermark * cap as f64) as u64;
        let mut victims: Vec<(u64, BlockKey, u64, u64)> = {
            let db = self.core.dram_blocks.lock().unwrap();
            let heat = self.core.heat.lock().unwrap();
            let mut v: Vec<(u64, BlockKey, u64, u64)> = db
                .iter()
                .filter(|(_, (k, len, last))| {
                    let (c, hlast) = heat.get(k).copied().unwrap_or((0, 0));
                    let recent = now.saturating_sub(hlast.max(*last)) / 1000
                        < self.core.cfg.migration_cooldown_ms;
                    let _ = len;
                    c < self.core.cfg.adaptive_k || !recent
                })
                .map(|(&off, (k, len, last))| (*last, *k, off, *len))
                .collect();
            v.sort_by_key(|&(last, _, _, _)| last);
            v
        };
        let mut db = self.core.dram_blocks.lock().unwrap();
        let mut freed = 0u64;
        let mut budget = self.core.cfg.migration_max_per_get;
        for (_, key, off, len) in victims.drain(..) {
            if usage.saturating_sub(freed) <= target || budget == 0 {
                break;
            }
            if !db.contains_key(&off) {
                continue;
            }
            if self.core.registry.active_readers(key.session) != 0
                || self.core.registry.active_writers(key.session) != 0
            {
                continue;
            }
            let mut payload = vec![0u8; len as usize];
            self.core.arena.read(off, len, &mut payload);
            let _mig = self.core.migration_lock.lock().unwrap();
            match self.core.index.lookup(&key) {
                Lookup::Hit(g) if !g.is_ssd() && g.offset() == off => {}
                _ => {
                    db.remove(&off);
                    continue;
                }
            }
            let gvas = match self
                .core
                .layout
                .append_scope_batch(key.session, 1, 1, |_, _| payload.clone())
            {
                Ok(g) => g,
                Err(_) => continue,
            };
            let Some(g) = gvas.first().copied() else { continue };
            if !self.publish_swap(&key, g) {
                continue;
            }
            self.log_publish(&key, g);
            let fr = self.core.layout.resolve(g, len);
            self.core.registry.remove_dram_extent(key.session, off, len);
            self.core
                .registry
                .add_ssd_extent(key.session, fr.file_seq, fr.in_file_off, len);
            self.core.ssd_sessions.lock().unwrap().insert(key.session);
            self.core
                .pending_frees
                .lock()
                .unwrap()
                .push(PendingFree::Dram { off, len, session: key.session });
            db.remove(&off);
            self.core
                .last_migrated
                .lock()
                .unwrap()
                .insert(key, now);
            freed += len;
            budget -= 1;
            let mut m = self.core.metrics.lock().unwrap();
            m.migr_demotions += 1;
            m.migr_bytes_moved += len;
            m.physical_write_bytes += len; // demotion is a real SSD write
        }
        freed
    }

    /// Return deferred-freed extents to their pools once no in-flight reader or
    /// writer can still be touching the bytes.
    fn flush_pending_frees(&self) {
        let mut list = self.core.pending_frees.lock().unwrap();
        if list.is_empty() {
            return;
        }
        let mut keep = Vec::new();
        for f in list.drain(..) {
            let session = match &f {
                PendingFree::Dram { session, .. } => *session,
                PendingFree::Ssd { session, .. } => *session,
            };
            if self.core.registry.active_readers(session) != 0
                || self.core.registry.active_writers(session) != 0
            {
                keep.push(f);
                continue;
            }
            match f {
                PendingFree::Dram { off, len, .. } => self.core.arena.free_extent(off, len),
                PendingFree::Ssd { file_seq, off, len, .. } => {
                    self.core.layout.free_extent(file_seq, off, len)
                }
            }
        }
        if keep.len() > PENDING_FREE_CAP {
            // Pathological (everything pinned forever): free all rather than
            // grow unbounded. Documented fallback; bytes are immutable so the
            // only exposure is pool reuse overlapping a straggler read.
            for f in keep.drain(..) {
                match f {
                    PendingFree::Dram { off, len, .. } => {
                        self.core.arena.free_extent(off, len)
                    }
                    PendingFree::Ssd { file_seq, off, len, .. } => {
                        self.core.layout.free_extent(file_seq, off, len)
                    }
                }
            }
        }
        *list = keep;
    }

    /// Purge migration bookkeeping for scopes reclaimed by session reclamation.
    fn purge_dead_scope_bookkeeping(&self) {
        let mut db = self.core.dram_blocks.lock().unwrap();
        if !db.is_empty() {
            db.retain(|_, (k, _, _)| self.core.registry.scope_exists(k.session));
        }
        let mut heat = self.core.heat.lock().unwrap();
        if !heat.is_empty() {
            heat.retain(|k, _| self.core.registry.scope_exists(k.session));
        }
    }
}

impl KvEngine for FlatCakeEngine {
    fn put_batch(&self, spec: &PutSpec) -> io::Result<()> {
        let bb = self.block_bytes_of();
        let n = spec.n_blocks();
        let sig = PlacementSignals {
            session: spec.session,
            refcount: if spec.shared == 1 { 2 } else { 1 },
            turn: spec.turn,
            reply_tokens: spec.reply_tokens,
            ctx_tokens: spec.ctx_tokens,
        };
        let medium = self.scope_medium(&sig);
        // Writer pin held for the whole batch (D15: PENDING writes pin their scope).
        self.pin_writer_retry(&sig)?;
        self.core.metrics.lock().unwrap().logical_write_bytes += n * bb;
        let mut payload = vec![0u8; bb as usize];
        let mut result = Ok(());

        match medium {
            Medium::Dram => {
                'outer: for l in 0..spec.layers {
                    for i in 0..spec.n_blk {
                        let key = spec.block_key(l, i, self.core.cfg.block_tokens);
                        payload_fill(spec.session, l, key.token_start, spec.seed, &mut payload);
                        let slot = match self.core.index.insert_pending(&key) {
                            Ok(s) => s,
                            Err(_) => {
                                result = Err(io::Error::new(
                                    io::ErrorKind::ResourceBusy,
                                    "index full — workload exceeds index_capacity",
                                ));
                                break 'outer;
                            }
                        };
                        match self.core.arena.reserve(bb) {
                            Ok(off) => {
                                self.core.arena.write(off, &payload);
                                let gva = Gva::encode(0, 0, off).unwrap();
                                self.core.index.publish(slot, gva);
                                self.core.registry.add_dram_extent(spec.session, off, bb);
                                self.core.registry.add_key(spec.session, key);
                                self.core.dram_blocks.lock().unwrap().insert(
                                    off,
                                    (key, bb, self.now_us()),
                                );
                            }
                            Err(_) => {
                                // DRAM-full spill (OQ-6): flip scope, block to SSD.
                                // The block MUST still be published (paper 3.2
                                // publish-after-write applies to every medium --
                                // A1 finding: an unpublished spill is invisible).
                                self.core.index.rollback(slot);
                                self.core.registry.flip_to_ssd(spec.session);
                                self.core.metrics.lock().unwrap().dram_spill_puts += 1;
                                let s2 = match self.core.index.insert_pending(&key) {
                                    Ok(s2) => Some(s2),
                                    Err(_) => None,
                                };
                                if let Err(e) =
                                    self.append_one_ssd(spec.session, &key, s2, &payload)
                                {
                                    result = Err(e);
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            Medium::Ssd => {
                let bt = self.core.cfg.block_tokens;
                let layer_span = spec.n_blk as usize;
                let total = spec.layers as usize * layer_span;
                // Reserve index slots BEFORE the write; publish after (T3).
                let mut slots: Vec<Option<usize>> = Vec::with_capacity(total);
                let mut keys: Vec<BlockKey> = Vec::with_capacity(total);
                let mut index_full = false;
                for l in 0..spec.layers {
                    for i in 0..spec.n_blk {
                        let key = spec.block_key(l, i, bt);
                        keys.push(key);
                        match self.core.index.insert_pending(&key) {
                            Ok(s) => slots.push(Some(s)),
                            Err(_) => {
                                slots.push(None);
                                index_full = true;
                            }
                        }
                    }
                }
                if index_full {
                    for s in slots.into_iter().flatten() {
                        self.core.index.rollback(s);
                    }
                    self.core.registry.unpin_writer(spec.session);
                    return Err(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        "index full — workload exceeds index_capacity",
                    ));
                }
                let session = spec.session;
                let seed = spec.seed;
                match self.core.layout.append_scope_batch(
                    session,
                    spec.layers,
                    spec.n_blk,
                    |l, i| {
                        let ts = (spec.start_blk + i as u64) * bt;
                        let mut buf = vec![0u8; bb as usize];
                        payload_fill(session, l, ts, seed, &mut buf);
                        buf
                    },
                ) {
                    Ok(gvas) if gvas.len() == total => {
                        for ((s, g), k) in slots.iter().zip(gvas.iter()).zip(keys.iter()) {
                            if let Some(s) = s {
                                self.core.index.publish(*s, *g);
                                self.log_publish(k, *g);
                            }
                        }
                        for k in &keys {
                            self.core.registry.add_key(spec.session, *k);
                        }
                        if let Some(g0) = gvas.first() {
                            let fr = self.core.layout.resolve(*g0, n * bb);
                            self.core.registry.add_ssd_extent(
                                spec.session,
                                fr.file_seq,
                                fr.in_file_off,
                                n * bb,
                            );
                        }
                        self.core.ssd_sessions.lock().unwrap().insert(spec.session);
                        self.core.metrics.lock().unwrap().physical_write_bytes += n * bb;
                    }
                    Ok(_) => {
                        result = Err(io::Error::new(
                            io::ErrorKind::Other,
                            "scope batch size mismatch",
                        ));
                    }
                    Err(e) => {
                        result = Err(e);
                    }
                }
                if result.is_err() {
                    for s in slots.into_iter().flatten() {
                        self.core.index.rollback(s);
                    }
                }
            }
        }
        self.core.registry.unpin_writer(spec.session);
        self.maybe_reclaim();
        self.flush_log();
        {
            let used = self.mem_used();
            self.core.metrics.lock().unwrap().track_high_water(used);
        }
        result
    }

    fn get_request(&self, spec: &GetSpec, sink: &mut dyn FnMut(usize, &[u8])) -> io::Result<()> {
        // M11 adaptive evidence: count served gets per session (before routing).
        *self
            .core
            .gets_served
            .lock()
            .unwrap()
            .entry(spec.signals.session)
            .or_insert(0) += 1;
        let id = self.core.next_id.fetch_add(1, Ordering::SeqCst);
        let n_blocks = spec.blocks.len();
        let mut out: Vec<Option<Vec<u8>>> = vec![None; n_blocks];
        let mut counted: Vec<bool> = vec![false; n_blocks];
        let mut ssd_gvas: Vec<(usize, Gva)> = Vec::new();
        let mut dram_list: Vec<(usize, Gva)> = Vec::new();
        let mut recompute: Vec<(usize, BlockKey, u64)> = Vec::new();
        let mut payload = vec![0u8; self.block_bytes_of() as usize];

        // Reader pins at resolution time, released at request end (D15). Hybrid
        // requests span multiple owning scopes (e.g., a DRAM-pinned shared prompt
        // plus an SSD-resident session history); EVERY owning scope is pinned so
        // reclamation cannot free extents between lookup and read (A1 finding 2).
        let mut pinned_scopes: Vec<u64> = Vec::new();
        {
            let mut owners: HashSet<u64> = HashSet::new();
            for b in &spec.blocks {
                owners.insert(b.session);
            }
            for s in owners {
                if self.core.registry.pin_reader(s) {
                    pinned_scopes.push(s);
                }
                // pin false = scope already reclaimed: its blocks will lookup-Miss
                // and recompute below (clean, accounted path).
            }
        }

        // hbm first (ARCHITECTURE §7), then Level-1 index resolution.
        for (idx, b) in spec.blocks.iter().enumerate() {
            {
                let hbm = self.core.hbm.lock().unwrap();
                if let Some(bytes) = hbm.get(b.session, b.layer, b.token_start) {
                    out[idx] = Some(bytes.to_vec());
                    counted[idx] = true;
                    let mut m = self.core.metrics.lock().unwrap();
                    m.hit_hbm += 1;
                    m.requested_blocks += 1;
                    continue;
                }
            }
            match self.core.index.lookup(b) {
                Lookup::Hit(gva) => {
                    if gva.is_ssd() {
                        ssd_gvas.push((idx, gva));
                    } else {
                        dram_list.push((idx, gva));
                    }
                }
                Lookup::Pending => {
                    counted[idx] = true;
                    let mut m = self.core.metrics.lock().unwrap();
                    m.pending_racing_gets += 1;
                    m.misses += 1;
                    m.requested_blocks += 1;
                    recompute.push((idx, *b, spec.seed_by_block[idx]));
                }
                Lookup::Miss => {
                    counted[idx] = true;
                    let mut m = self.core.metrics.lock().unwrap();
                    m.misses += 1;
                    m.requested_blocks += 1;
                    recompute.push((idx, *b, spec.seed_by_block[idx]));
                }
            }
        }

        // Recompute path (inline on the calling thread; modeled cost + re-persist).
        // P4 write coalescing: batch per (owning session, layer) — one SSD
        // append per group instead of one per small block. Ownership follows
        // the KEY's session: a recomputed shared-prompt block belongs to
        // session 1, never to the requester (P4 ownership fix).
        {
            let mut groups: HashMap<(u64, u32), Vec<(usize, BlockKey, u64)>> = HashMap::new();
            for (idx, b, seed) in &recompute {
                groups
                    .entry((b.session, b.layer))
                    .or_default()
                    .push((*idx, *b, *seed));
            }
            let mut group_keys: Vec<(u64, u32)> = groups.keys().copied().collect();
            group_keys.sort();
            for (session, layer) in group_keys {
                let mut group = groups.remove(&(session, layer)).unwrap();
                group.sort_by_key(|(idx, _, _)| *idx);
                let owner_sig = PlacementSignals {
                    session,
                    refcount: if session == 1 { 2 } else { 1 },
                    turn: 1,
                    reply_tokens: 0,
                    ctx_tokens: group.len() as u64 * self.core.cfg.block_tokens,
                };
                let medium = self.scope_medium(&owner_sig);
                let bb = self.block_bytes_of();
                match medium {
                    Medium::Dram => {
                        for (idx, b, seed) in &group {
                            payload_fill(b.session, b.layer, b.token_start, *seed, &mut payload);
                            self.core.metrics.lock().unwrap().modeled_recompute_us +=
                                self.core.cfg.recompute_us_per_token * self.core.cfg.block_tokens;
                            let _ = self.recompute_persist(b, &owner_sig, &payload);
                            out[*idx] = Some(payload.clone());
                        }
                    }
                    Medium::Ssd => {
                        let max_chunk_blocks =
                            (self.core.cfg.write_coalesce_bytes.max(bb) / bb) as u32;
                        let mut chunk: Vec<(usize, BlockKey, u64)> = Vec::new();
                        for item in group {
                            chunk.push(item);
                            if chunk.len() as u32 >= max_chunk_blocks {
                                self.flush_recompute_chunk(
                                    session, layer, &mut chunk, &mut payload, &mut out,
                                );
                            }
                        }
                        self.flush_recompute_chunk(session, layer, &mut chunk, &mut payload, &mut out);
                    }
                }
            }
        }

        // Hybrid rule (A2 R5 fix, paper §3.3): the DRAM-resident portion of an
        // SSD-classified request is delivered inline (pinned in place) while only
        // the SSD portion is enqueued. Pure-DRAM requests go through the
        // DRAM-Ready Queue (arrival-ordered dispatch by dram workers).
        let is_hybrid = !ssd_gvas.is_empty() && !dram_list.is_empty();
        let hybrid_dram: Vec<(usize, Gva)> = if is_hybrid { dram_list.clone() } else { Vec::new() };
        if is_hybrid {
            for (idx, gva) in dram_list.drain(..) {
                let bb = self.block_bytes_of();
                let mut buf = vec![0u8; bb as usize];
                self.core.arena.read(gva.offset(), bb, &mut buf);
                out[idx] = Some(buf);
                counted[idx] = true;
                let mut m = self.core.metrics.lock().unwrap();
                m.hit_dram += 1;
                m.requested_blocks += 1;
            }
        }

        // Route through the scheduler: SSD portion (hybrid or pure), or pure-DRAM.
        let enqueued: Option<(bool, Vec<(usize, Gva)>)> = if !ssd_gvas.is_empty() {
            Some((true, ssd_gvas.clone()))
        } else if !is_hybrid && !dram_list.is_empty() {
            Some((false, dram_list.clone()))
        } else {
            None
        };

        if let Some((is_ssd, map)) = enqueued.as_ref() {
            let req = Request {
                id,
                session: spec.signals.session,
                arrival_ts_us: spec.arrival_ts_us,
                blocks: map.iter().map(|(_, g)| *g).collect(),
                size_bytes: map.len() as u64 * self.block_bytes_of(),
            };
            // P4 dedup (audit items 11-13): fingerprint = SORTED SSD GVA list
            // (order-insensitive exact match; equal sets => identical byte
            // spans => one read serves both). Leader slot holds the map;
            // followers wait on the leader's condvar and merge their own out[]
            // by GVA match, then account exactly like the leader path.
            let fp = {
                let mut sorted: Vec<u64> = req.blocks.iter().map(|g| g.0).collect();
                sorted.sort_unstable();
                let mut bytes = Vec::with_capacity(sorted.len() * 8 + 8);
                for g in &sorted {
                    bytes.extend_from_slice(&g.to_le_bytes());
                }
                bytes.extend_from_slice(&req.blocks.len().to_le_bytes());
                crate::util::fnv1a64(&bytes)
            };
            let slot_arc = Arc::new((
                Mutex::new(Slot {
                    out: vec![None; n_blocks],
                    map: map.clone(),
                    err: None,
                    done: false,
                    fp: if *is_ssd { fp } else { 0 },
                }),
                Condvar::new(),
            ));
            if *is_ssd {
                let mut inflight = self.core.inflight.lock().unwrap();
                if let Some(leader) = inflight.get(&fp).cloned() {
                    drop(inflight);
                    let (slot_lock, cv) = &*leader;
                    let mut guard = slot_lock.lock().unwrap();
                    while !guard.done && self.running.load(Ordering::SeqCst) {
                        let (g, _) =
                            cv.wait_timeout(guard, Duration::from_millis(200)).unwrap();
                        guard = g;
                    }
                    let worker_err = guard.err.clone();
                    // Merge by GVA match (audit item 12): the follower's block
                    // order differs whenever its hbm/DRAM/recompute prefix
                    // differs; positional reuse would be wrong.
                    let leader_map: Vec<(usize, u64)> = {
                        let (slot_lock2, _) = &*leader;
                        slot_lock2
                            .lock()
                            .unwrap()
                            .map
                            .iter()
                            .map(|(oi, g)| (*oi, g.0))
                            .collect()
                    };
                    for (idx, gva) in map.iter() {
                        let want = gva.0;
                        if let Some(&(lidx, _)) =
                            leader_map.iter().find(|&(_, g)| *g == want)
                        {
                            let b = {
                                let (slot_lock2, _) = &*leader;
                                slot_lock2.lock().unwrap().out[lidx].take()
                            };
                            if let Some(bytes) = b {
                                out[*idx] = Some(bytes);
                            }
                        }
                    }
                    drop(guard);
                    if worker_err.is_some() || !self.running.load(Ordering::SeqCst) {
                        for (idx, _) in map.iter() {
                            if out[*idx].is_none() && !counted[*idx] {
                                counted[*idx] = true;
                                let mut m = self.core.metrics.lock().unwrap();
                                m.misses += 1;
                                m.requested_blocks += 1;
                                drop(m);
                                let b = spec.blocks[*idx];
                                let seed = spec.seed_by_block[*idx];
                                payload_fill(
                                    b.session,
                                    b.layer,
                                    b.token_start,
                                    seed,
                                    &mut payload,
                                );
                                self.core.metrics.lock().unwrap().modeled_recompute_us +=
                                    self.core.cfg.recompute_us_per_token
                                        * self.core.cfg.block_tokens;
                                let _ = self.recompute_persist(&b, &spec.signals, &payload);
                                out[*idx] = Some(payload.clone());
                            }
                        }
                    }
                    self.core.metrics.lock().unwrap().dedup_requests += 1;
                    return self.finish_deduped_get(
                        spec,
                        out,
                        counted,
                        pinned_scopes,
                        sink,
                        enqueued.as_ref(),
                    );
                }
                inflight.insert(fp, slot_arc.clone());
            }
            self.core.slots.lock().unwrap().insert(id, slot_arc.clone());
            self.core.scheduler.enqueue(req, *is_ssd);
            let (slot_lock, cv) = &*slot_arc;
            let mut guard = slot_lock.lock().unwrap();
            // Bounded wait: shutdown must never hang in-flight requests (A2).
            while !guard.done && self.running.load(Ordering::SeqCst) {
                let (g, _) = cv.wait_timeout(guard, Duration::from_millis(200)).unwrap();
                guard = g;
            }
            let worker_err = guard.err.clone();
            for idx in 0..n_blocks {
                if out[idx].is_none() {
                    if let Some(b) = guard.out[idx].take() {
                        out[idx] = Some(b);
                    }
                }
            }
            drop(guard);
            self.core.slots.lock().unwrap().remove(&id);
            // Reconcile: blocks the worker could not deliver (I/O error or shutdown
            // race) fall back to recompute — never silently missing (A2 finding).
            if worker_err.is_some() || !self.running.load(Ordering::SeqCst) {
                for (idx, _) in map.iter() {
                    if out[*idx].is_none() && !counted[*idx] {
                        counted[*idx] = true;
                        let mut m = self.core.metrics.lock().unwrap();
                        m.misses += 1;
                        m.requested_blocks += 1;
                        drop(m);
                        let b = spec.blocks[*idx];
                        let seed = spec.seed_by_block[*idx];
                        payload_fill(b.session, b.layer, b.token_start, seed, &mut payload);
                        self.core.metrics.lock().unwrap().modeled_recompute_us +=
                            self.core.cfg.recompute_us_per_token * self.core.cfg.block_tokens;
                        let _ = self.recompute_persist(&b, &spec.signals, &payload);
                        out[*idx] = Some(payload.clone());
                    }
                }
            }
        }

        // Final accounting for scheduler-served blocks (workers never count).
        if let Some((is_ssd, map)) = enqueued.as_ref() {
            for (idx, _) in map.iter() {
                if !counted[*idx] {
                    counted[*idx] = true;
                    let mut m = self.core.metrics.lock().unwrap();
                    m.requested_blocks += 1;
                    if out[*idx].is_some() {
                        if *is_ssd {
                            m.hit_ssd += 1;
                            m.logical_read_bytes += self.block_bytes_of();
                        } else {
                            m.hit_dram += 1;
                        }
                    } else {
                        m.misses += 1;
                    }
                }
            }
        }

        for s in pinned_scopes {
            self.core.registry.unpin_reader(s);
        }

        // Deliver in idx order + hbm population (identical policy both systems).
        for (idx, b) in spec.blocks.iter().enumerate() {
            if let Some(bytes) = out[idx].as_ref() {
                self.core
                    .hbm
                    .lock()
                    .unwrap()
                    .record_delivery(b.session, b.layer, b.token_start, bytes);
                sink(idx, bytes);
            }
        }

        // ---- P3 medium-aware migration (inert while migration_enabled = 0) ----
        if self.core.cfg.migration_enabled == 1 {
            let now = self.now_us();
            // Heat for every delivered block (reuse evidence).
            for (idx, b) in spec.blocks.iter().enumerate() {
                if out[idx].is_some() {
                    self.record_heat(b, now);
                }
            }
            // DRAM residency bookkeeping (demotion victims come from here).
            {
                let mut db = self.core.dram_blocks.lock().unwrap();
                let queued_dram: Vec<(usize, Gva)> = match enqueued.as_ref() {
                    Some((false, m)) => m.clone(),
                    _ => Vec::new(),
                };
                for (idx, gva) in hybrid_dram.iter().chain(queued_dram.iter()) {
                    if let (Some(bytes), Some(b)) =
                        (out[*idx].as_ref(), spec.blocks.get(*idx))
                    {
                        db.insert(gva.offset(), (*b, bytes.len() as u64, now));
                    }
                }
            }
            // Promotion: hottest blocks with SSD backing, budget-capped. Bytes
            // come from wherever they already are — this request's delivery or
            // the hbm window — so promotion costs zero extra SSD reads.
            let mut done = 0u64;
            let candidates: Vec<(u64, BlockKey)> = {
                let heat = self.core.heat.lock().unwrap();
                let mut v: Vec<(u64, BlockKey)> = heat
                    .iter()
                    .filter(|(k, _)| {
                        matches!(self.core.index.lookup(k), Lookup::Hit(ref g) if g.is_ssd())
                    })
                    .filter(|(_, (c, _))| *c >= self.core.cfg.adaptive_k)
                    .map(|(k, (c, _))| (*c, *k))
                    .collect();
                v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| format!("{:?}", a.1).cmp(&format!("{:?}", b.1))));
                v
            };
            for (_c, k) in candidates {
                if done >= self.core.cfg.migration_max_per_get {
                    break;
                }
                // Source 1: bytes delivered in this very request.
                let from_req = enqueued.as_ref().and_then(|(is_ssd, m)| {
                    if !*is_ssd {
                        return None;
                    }
                    m.iter()
                        .find(|(idx, _)| spec.blocks[*idx] == k)
                        .and_then(|(idx, gva)| out[*idx].as_ref().map(|b| (*gva, b.clone())))
                });
                if let Some((gva, bytes)) = from_req {
                    if self.promote_one(&k, gva, &bytes, now) {
                        done += 1;
                    }
                    continue;
                }
                // Source 2: hbm window (same bytes; backing store still SSD).
                let from_hbm = {
                    let hbm = self.core.hbm.lock().unwrap();
                    hbm.get(k.session, k.layer, k.token_start)
                        .map(|b| b.to_vec())
                };
                if let Some(bytes) = from_hbm {
                    if let Lookup::Hit(g) = self.core.index.lookup(&k) {
                        if self.promote_one(&k, g, &bytes, now) {
                            done += 1;
                        }
                    }
                }
            }
        }

        self.flush_log();
        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += n_blocks as u64;
        m.delivered_requests += 1;
        Ok(())
    }

    fn snapshot(&self) -> Metrics {
        let mut m = self.core.metrics.lock().unwrap().clone();
        let s = self.core.scheduler.stats();
        m.qwait_samples.extend(s.wait_samples.iter().copied());
        m
    }

    fn mem_used(&self) -> u64 {
        self.core.arena.used_bytes() + self.core.hbm.lock().unwrap().used_bytes()
    }

    fn shutdown(&self) {
        if self.running.swap(false, Ordering::SeqCst) {
            self.core.scheduler.shutdown();
            let handles: Vec<_> = self.worker_handles.lock().unwrap().drain(..).collect();
            for h in handles {
                let _ = h.join();
            }
            self.flush_pending_frees();
            self.flush_log();
        }
    }
}

// ---------------------------------------------------------------- workers

/// DRAM-Ready dispatcher (paper Fig. 3(a)❷): serves pure-DRAM requests in arrival
/// order and completes re-admitted SSD requests (markers) in their ORIGINAL
/// arrival order (T9).
fn dram_worker(core: Arc<EngineCore>, running: Arc<AtomicBool>) {
    while running.load(Ordering::SeqCst) {
        match core.scheduler.pop_dram() {
            None => break,
            Some(PoppedReq { req, .. }) => {
                let slot_arc = core.slots.lock().unwrap().get(&req.id).cloned();
                let Some(slot_arc) = slot_arc else { continue };
                {
                    let (slot_lock, _) = &*slot_arc;
                    let bb = core.cfg.block_bytes();
                    // Collect DRAM reads first (map borrowed immutably), then write
                    // into the slot (mutable) — avoids an E0502 self-borrow.
                    let reads: Vec<(usize, u64)> = slot_lock
                        .lock()
                        .unwrap()
                        .map
                        .iter()
                        .filter(|(_, gva)| !gva.is_ssd())
                        .map(|(oi, gva)| (*oi, gva.offset()))
                        .collect();
                    let mut slot = slot_lock.lock().unwrap();
                    for (orig_idx, off) in reads {
                        let mut buf = vec![0u8; bb as usize];
                        core.arena.read(off, bb, &mut buf);
                        slot.out[orig_idx] = Some(buf);
                    }
                }
                finish_slot(&core, &slot_arc, req.id);
            }
        }
    }
}

/// SSD prefetch worker (paper §3.3): HRRN-selected; coalesced bounded-window reads;
/// metrics merged from a LOCAL counter set (the callback must not re-lock core
/// metrics while read_ranges holds its `&mut Metrics` — A2 finding M3 deadlock);
/// completion marker re-admitted by ORIGINAL arrival_ts.
fn ssd_worker(core: Arc<EngineCore>, running: Arc<AtomicBool>, reader: &mut Reader) {
    while running.load(Ordering::SeqCst) {
        match core.scheduler.pick_ssd() {
            None => break,
            Some(PoppedReq { req, .. }) => {
                let t0 = Instant::now();
                let slot_arc = match core.slots.lock().unwrap().get(&req.id).cloned() {
                    Some(s) => s,
                    None => continue,
                };
                let bb = core.cfg.block_bytes();
                let mut orig: Vec<usize> = Vec::with_capacity(req.blocks.len());
                let mut descs: Vec<crate::types::FileRange> = Vec::with_capacity(req.blocks.len());
                {
                    let (slot_lock, _) = &*slot_arc;
                    let slot = slot_lock.lock().unwrap();
                    for (pos, gva) in req.blocks.iter().enumerate() {
                        orig.push(slot.map.get(pos).map(|(oi, _)| *oi).unwrap_or(pos));
                        descs.push(core.layout.resolve(*gva, bb));
                    }
                }
                let (merged, stats) = coalesce(descs, core.cfg.gap_bridge_bytes);
                {
                    let mut m = core.metrics.lock().unwrap();
                    m.merged_ranges += stats.merged_ranges;
                    m.bridged_gaps += stats.bridged_gaps;
                    m.overread_bytes += stats.overread_bytes;
                }
                let mut io_metrics = Metrics::default();
                let read_result = read_ranges(
                    reader,
                    &merged,
                    SSD_WINDOW_BYTES,
                    |desc_idx, bytes| {
                        let (slot_lock, _) = &*slot_arc;
                        let mut slot = slot_lock.lock().unwrap();
                        let oi = orig[desc_idx];
                        slot.out[oi] = Some(bytes.to_vec());
                    },
                    &mut io_metrics,
                );
                if let Err(e) = read_result {
                    let (slot_lock, _) = &*slot_arc;
                    slot_lock.lock().unwrap().err = Some(e.to_string());
                }
                {
                    let mut m = core.metrics.lock().unwrap();
                    m.physical_read_ops += io_metrics.physical_read_ops;
                    m.physical_read_bytes += io_metrics.physical_read_bytes;
                    // logical_read_bytes is accounted by get_request (read_amp's
                    // denominator has a single owner to avoid double counting --
                    // A1 finding 3).
                    m.prep_samples.push(t0.elapsed().as_micros() as u64);
                }
                // Re-admit completion marker by ORIGINAL arrival_ts (T9): a dram
                // worker completes this request in arrival order.
                core.scheduler.readmit(Request {
                    id: req.id,
                    session: req.session,
                    arrival_ts_us: req.arrival_ts_us,
                    blocks: Vec::new(),
                    size_bytes: 0,
                });
            }
        }
    }
}

/// One recovery-log record (audit item 2/10): SSD publish or invalidate.
enum LogRec {
    Pub(BlockKey, Gva),
    Invalidate(BlockKey),
}

/// Parse one recovery-log record. Returns None for corrupt/partial lines
/// (crash mid-log-write tail).
fn parse_log_line(line: &str) -> Option<LogRec> {
    let line = line.trim();
    if !line.starts_with('{') || !line.ends_with('}') {
        return None;
    }
    let parse_key = |seg: &str| -> Option<BlockKey> {
        let mut parts = seg.split(',');
        let session = parts.next()?.trim().parse::<u64>().ok()?;
        let layer = parts.next()?.trim().parse::<u32>().ok()?;
        let token_start = parts.next()?.trim().parse::<u64>().ok()?;
        Some(BlockKey {
            session,
            layer,
            token_start,
        })
    };
    if let Some(i) = line.find("\"p\":[") {
        let kpos = i + 5;
        let kend = line[kpos..].find(']').map(|j| kpos + j)?;
        let k = parse_key(&line[kpos..kend])?;
        let gpos = line.find("\"g\":").map(|j| j + 5)?;
        let gend = line[gpos..].find('}').map(|j| gpos + j)?;
        let g = line[gpos..gend].trim().parse::<u64>().ok()?;
        return Some(LogRec::Pub(k, Gva(g)));
    }
    if let Some(i) = line.find("\"i\":[") {
        let kpos = i + 5;
        let kend = line[kpos..].find(']').map(|j| kpos + j)?;
        let k = parse_key(&line[kpos..kend])?;
        return Some(LogRec::Invalidate(k));
    }
    None
}

/// Mark the slot done, remove it from the registry (single completion per id),
/// and wake the waiting caller.
fn finish_slot(core: &EngineCore, slot_arc: &Arc<(Mutex<Slot>, Condvar)>, id: u64) {
    let fp = {
        let (lock, cv) = &**slot_arc;
        let mut g = lock.lock().unwrap();
        g.done = true;
        let fp = g.fp;
        cv.notify_all();
        fp
    };
    core.slots.lock().unwrap().remove(&id);
    if fp != 0 {
        // Leader removes the dedup entry; followers already hold the Arc and
        // merge from the (now done) slot.
        core.inflight.lock().unwrap().remove(&fp);
    }
}
