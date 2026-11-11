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
}

pub struct FlatCakeEngine {
    core: Arc<EngineCore>,
    running: Arc<AtomicBool>,
    worker_handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl FlatCakeEngine {
    pub fn new(cfg: &Config) -> io::Result<FlatCakeEngine> {
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
        });
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
    fn scope_medium(&self, sig: &PlacementSignals) -> Medium {
        let sticky = self.core.registry.sticky_medium(sig.session);
        let medium = placement::decide(sig, sticky, &self.core.cfg);
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
                        for (s, g) in slots.iter().zip(gvas.iter()) {
                            if let Some(s) = s {
                                self.core.index.publish(*s, *g);
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
        {
            let used = self.mem_used();
            self.core.metrics.lock().unwrap().track_high_water(used);
        }
        result
    }

    fn get_request(&self, spec: &GetSpec, sink: &mut dyn FnMut(usize, &[u8])) -> io::Result<()> {
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
        for (idx, b, seed) in &recompute {
            payload_fill(b.session, b.layer, b.token_start, *seed, &mut payload);
            self.core.metrics.lock().unwrap().modeled_recompute_us +=
                self.core.cfg.recompute_us_per_token * self.core.cfg.block_tokens;
            // A persist failure must not fail THIS request: the recomputed payload
            // is deliverable regardless; the block's next read simply misses again
            // (honest accounting — never silently dropped).
            let _ = self.recompute_persist(b, &spec.signals, &payload);
            out[*idx] = Some(payload.clone());
        }

        // Hybrid rule (A2 R5 fix, paper §3.3): the DRAM-resident portion of an
        // SSD-classified request is delivered inline (pinned in place) while only
        // the SSD portion is enqueued. Pure-DRAM requests go through the
        // DRAM-Ready Queue (arrival-ordered dispatch by dram workers).
        let is_hybrid = !ssd_gvas.is_empty() && !dram_list.is_empty();
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
            let slot_arc = Arc::new((
                Mutex::new(Slot {
                    out: vec![None; n_blocks],
                    map: map.clone(),
                    err: None,
                    done: false,
                }),
                Condvar::new(),
            ));
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

/// Mark the slot done, remove it from the registry (single completion per id),
/// and wake the waiting caller.
fn finish_slot(core: &EngineCore, slot_arc: &Arc<(Mutex<Slot>, Condvar)>, id: u64) {
    {
        let (lock, cv) = &**slot_arc;
        let mut g = lock.lock().unwrap();
        g.done = true;
        cv.notify_all();
    }
    core.slots.lock().unwrap().remove(&id);
}
