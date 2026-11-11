//! FlatCakeEngine (T10/T15, ARCHITECTURE §2): integrates FC-GVA (placement/index/
//! arena/reclaim), FC-CIO (layout/coalesce/stream), FC-MAS (dual-queue scheduler)
//! behind the `KvEngine` interface. Mirrors paper Fig. 3:
//! - write path: scope-direct placement → GVA allocate → physical write → publish;
//! - control path: dual queues; DRAM-ready dispatched immediately, SSD-preparing
//!   prefetched by HRRN and RE-ADMITTED by original arrival_ts (completion order
//!   follows arrival order);
//! - data path: DRAM hits via arena copies; SSD hits via coalesced bounded-window
//!   reads (per-ssd-worker `Reader`).
//!
//! Contract notes honored from module docs:
//! - registry lock order registry → {index, arena, layout} (gva/mod.rs);
//! - pin_writer before arena.reserve; add extents+keys before unpin_writer;
//! - pin_reader taken at resolution time, released when the request completes (D15);
//! - reclaim trigger usage > high_wm × capacity, drain to ≤ low_wm (OQ-12).

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
    BlockKey, Gva, Lookup, Medium, Metrics, PlacementSignals, Request, MEDIA_DRAM,
};
use crate::util::payload_fill;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Bounded read window per SSD worker — the Level-B analog of the paper's
/// double-buffered GPU HBM staging window sized to two transformer layers (§3.4):
/// O(1) per worker, independent of request size.
const SSD_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// Per-request completion slot: workers fill `out[idx]`; the caller delivers to
/// its sink on its own thread after completion (order preserved).
struct Slot {
    out: Vec<Option<Vec<u8>>>,
    /// (original block idx, GVA) for blocks the worker must serve.
    map: Vec<(usize, Gva)>,
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
            scheduler: Arc::new(Scheduler::new(cfg.queue_cap)),
            slots: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
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

    /// Advisory watermark reclamation (ARCHITECTURE §5).
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
            reclaim_pass(
                Medium::Ssd,
                ssd_usage,
                &self.core.cfg,
                &self.core.registry,
                &self.core.index,
                ReclaimStore::Ssd(&self.core.layout),
                &mut m,
            );
            self.core.layout.release_stale_sessions();
        }
    }

    /// Re-persist a recomputed block (symmetric miss model, ARCHITECTURE §8).
    fn recompute_persist(&self, key: &BlockKey, sig: &PlacementSignals, payload: &[u8]) {
        let medium = self.scope_medium(sig);
        self.core.registry.pin_writer(sig.session);
        let slot = self.core.index.insert_pending(key).ok();
        match medium {
            Medium::Dram => match self.core.arena.reserve(payload.len() as u64) {
                Ok(off) => {
                    self.core.arena.write(off, payload);
                    if let Some(s) = slot {
                        let gva = Gva::encode(0, 0, off).unwrap();
                        self.core.index.publish(s, gva);
                    }
                    self.core
                        .registry
                        .add_dram_extent(sig.session, off, payload.len() as u64);
                }
                Err(_) => {
                    // DRAM-full spill on re-persist (OQ-6): flip scope, write to SSD.
                    self.core.registry.flip_to_ssd(sig.session);
                    let gvas = self
                        .core
                        .layout
                        .append_scope_batch(sig.session, 1, 1, |_, _| payload.to_vec())
                        .expect("ssd append of recomputed block");
                    if let (Some(s), Some(g)) = (slot, gvas.first()) {
                        self.core.index.publish(s, *g);
                    }
                    if let Some(g) = gvas.first() {
                        let fr = self.core.layout.resolve(*g, payload.len() as u64);
                        self.core.registry.add_ssd_extent(
                            sig.session,
                            fr.file_seq,
                            fr.in_file_off,
                            payload.len() as u64,
                        );
                    }
                    let mut m = self.core.metrics.lock().unwrap();
                    m.dram_spill_puts += 1;
                    m.physical_write_bytes += payload.len() as u64;
                }
            },
            Medium::Ssd => {
                let gvas = self
                    .core
                    .layout
                    .append_scope_batch(sig.session, 1, 1, |_, _| payload.to_vec())
                    .expect("ssd append of recomputed block");
                if let (Some(s), Some(g)) = (slot, gvas.first()) {
                    self.core.index.publish(s, *g);
                }
                if let Some(g) = gvas.first() {
                    let fr = self.core.layout.resolve(*g, payload.len() as u64);
                    self.core.registry.add_ssd_extent(
                        sig.session,
                        fr.file_seq,
                        fr.in_file_off,
                        payload.len() as u64,
                    );
                }
                let mut m = self.core.metrics.lock().unwrap();
                m.physical_write_bytes += payload.len() as u64;
            }
        }
        self.core.registry.add_key(sig.session, *key);
        self.core.registry.unpin_writer(sig.session);
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
        self.core.registry.pin_writer(spec.session);
        self.core.metrics.lock().unwrap().logical_write_bytes += n * bb;
        let mut payload = vec![0u8; bb as usize];
        let mut result = Ok(());

        match medium {
            Medium::Dram => {
                'outer: for l in 0..spec.layers {
                    for i in 0..spec.n_blk {
                        let key = spec.block_key(l, i, self.core.cfg.block_tokens);
                        payload_fill(spec.session, l, key.token_start, spec.seed, &mut payload);
                        let slot = self.core.index.insert_pending(&key);
                        let slot = match slot {
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
                                self.core
                                    .registry
                                    .add_dram_extent(spec.session, off, bb);
                                self.core.registry.add_key(spec.session, key);
                            }
                            Err(_) => {
                                // DRAM-full spill (OQ-6): flip scope, this block to SSD.
                                self.core.index.rollback(slot);
                                self.core.registry.flip_to_ssd(spec.session);
                                self.core.metrics.lock().unwrap().dram_spill_puts += 1;
                                let gvas = self.core.layout.append_scope_batch(
                                    spec.session,
                                    1,
                                    1,
                                    |_, _| payload.clone(),
                                );
                                match gvas {
                                    Ok(gv) => {
                                        let s2 = self.core.index.insert_pending(&key);
                                        if let (Ok(s2), Some(g)) = (s2, gv.first()) {
                                            self.core.index.publish(s2, *g);
                                        }
                                        if let Some(g) = gv.first() {
                                            let fr =
                                                self.core.layout.resolve(*g, bb);
                                            self.core.registry.add_ssd_extent(
                                                spec.session,
                                                fr.file_seq,
                                                fr.in_file_off,
                                                bb,
                                            );
                                        }
                                        self.core.registry.add_key(spec.session, key);
                                        self.core.metrics.lock().unwrap().physical_write_bytes += bb;
                                    }
                                    Err(e) => {
                                        result = Err(e);
                                        break 'outer;
                                    }
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
                for l in 0..spec.layers {
                    for i in 0..spec.n_blk {
                        let key = spec.block_key(l, i, bt);
                        keys.push(key);
                        slots.push(self.core.index.insert_pending(&key).ok());
                    }
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
        result
    }

    fn get_request(&self, spec: &GetSpec, sink: &mut dyn FnMut(usize, &[u8])) -> io::Result<()> {
        let id = self.core.next_id.fetch_add(1, Ordering::SeqCst);
        let mut out: Vec<Option<Vec<u8>>> = vec![None; spec.blocks.len()];
        let mut dram_gvas: Vec<(usize, Gva)> = Vec::new();
        let mut ssd_gvas: Vec<(usize, Gva)> = Vec::new();
        let mut recompute: Vec<(usize, BlockKey, u64)> = Vec::new();
        let mut payload = vec![0u8; self.block_bytes_of() as usize];

        // Reader pin at resolution time; released at request end (D15).
        let pinned_reader = self.core.registry.pin_reader(spec.signals.session);

        // hbm first (ARCHITECTURE §7), then Level-1 index resolution.
        for (idx, b) in spec.blocks.iter().enumerate() {
            {
                let hbm = self.core.hbm.lock().unwrap();
                if let Some(bytes) = hbm.get(b.session, b.layer, b.token_start) {
                    out[idx] = Some(bytes.to_vec());
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
                        dram_gvas.push((idx, gva));
                    }
                }
                Lookup::Pending => {
                    let mut m = self.core.metrics.lock().unwrap();
                    m.pending_racing_gets += 1;
                    m.misses += 1;
                    m.requested_blocks += 1;
                    recompute.push((idx, *b, spec.seed_by_block[idx]));
                }
                Lookup::Miss => {
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
            self.recompute_persist(b, &spec.signals, &payload);
            out[*idx] = Some(payload.clone());
        }

        // Route remaining blocks through the dual-queue scheduler (paper Fig. 3a ❷).
        let ssd_count = ssd_gvas.len();
        if ssd_count > 0 || !dram_gvas.is_empty() {
            let is_ssd = ssd_count > 0;
            let blocks: Vec<Gva> = if is_ssd {
                ssd_gvas.iter().map(|(_, g)| *g).collect()
            } else {
                dram_gvas.iter().map(|(_, g)| *g).collect()
            };
            let map: Vec<(usize, Gva)> = if is_ssd {
                ssd_gvas.clone()
            } else {
                dram_gvas.clone()
            };
            let req = Request {
                id,
                session: spec.signals.session,
                arrival_ts_us: spec.arrival_ts_us,
                blocks,
                size_bytes: if is_ssd {
                    ssd_count as u64 * self.block_bytes_of()
                } else {
                    dram_gvas.len() as u64 * self.block_bytes_of()
                },
            };
            let slot_arc = Arc::new((
                Mutex::new(Slot {
                    out: vec![None; spec.blocks.len()],
                    map,
                    done: false,
                }),
                Condvar::new(),
            ));
            self.core.slots.lock().unwrap().insert(id, slot_arc.clone());
            self.core.scheduler.enqueue(req, is_ssd);
            let (slot_lock, cv) = &*slot_arc;
            let mut guard = slot_lock.lock().unwrap();
            while !guard.done {
                guard = cv.wait(guard).unwrap();
            }
            // Merge worker-delivered bytes into `out`.
            for idx in 0..spec.blocks.len() {
                if out[idx].is_none() {
                    if let Some(b) = guard.out[idx].take() {
                        out[idx] = Some(b);
                    }
                }
            }
        }

        if pinned_reader {
            self.core.registry.unpin_reader(spec.signals.session);
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
        m.logical_block_gets += spec.blocks.len() as u64;
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

impl FlatCakeEngine {
    fn block_bytes_of(&self) -> u64 {
        self.core.cfg.block_bytes()
    }
}

// ---------------------------------------------------------------- workers

/// DRAM-Ready dispatcher (paper Fig. 3(a)❷: DRAM hits dispatched immediately).
fn dram_worker(core: Arc<EngineCore>, running: Arc<AtomicBool>) {
    while running.load(Ordering::SeqCst) {
        match core.scheduler.pop_dram() {
            None => break,
            Some(PoppedReq { req, .. }) => {
                let slot_arc = core.slots.lock().unwrap().get(&req.id).cloned();
                let Some(slot_arc) = slot_arc else { continue };
                let bb = core.cfg.block_bytes();
                {
                    let (slot_lock, _) = &*slot_arc;
                    let mut slot = slot_lock.lock().unwrap();
                    for (orig_idx, gva) in &slot.map {
                        if gva.is_ssd() {
                            continue; // re-admitted marker: bytes already delivered
                        }
                        let off = gva.offset();
                        let mut buf = vec![0u8; bb as usize];
                        core.arena.read(off, bb, &mut buf);
                        slot.out[*orig_idx] = Some(buf);
                        let mut m = core.metrics.lock().unwrap();
                        m.hit_dram += 1;
                        m.requested_blocks += 1;
                    }
                }
                finish_slot(&core, &slot_arc, req.id);
            }
        }
    }
}

/// SSD prefetch worker (paper §3.3): HRRN-selected; coalesced bounded-window reads;
/// completion marker re-admitted by ORIGINAL arrival_ts (T9) before delivery.
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
                // Resolve GVAs to file ranges (desc idx = position in req.blocks).
                let bb = core.cfg.block_bytes();
                let mut orig: Vec<usize> = Vec::with_capacity(req.blocks.len());
                let mut descs: Vec<crate::types::FileRange> = Vec::with_capacity(req.blocks.len());
                {
                    let (slot_lock, _) = &*slot_arc;
                    let slot = slot_lock.lock().unwrap();
                    for (pos, gva) in req.blocks.iter().enumerate() {
                        // map[pos] = orig idx (position in slot.map matches req.blocks
                        // because the engine built map from the same gva list order).
                        if let Some((oi, _)) = slot.map.get(pos) {
                            orig.push(*oi);
                        } else {
                            orig.push(pos);
                        }
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
                let _ = read_ranges(reader, &merged, SSD_WINDOW_BYTES, |desc_idx, bytes| {
                    let (slot_lock, _) = &*slot_arc;
                    let mut slot = slot_lock.lock().unwrap();
                    let oi = orig[desc_idx];
                    slot.out[oi] = Some(bytes.to_vec());
                    drop(slot);
                    let mut m = core.metrics.lock().unwrap();
                    m.hit_ssd += 1;
                    m.requested_blocks += 1;
                })
                .ok();
                {
                    let mut m = core.metrics.lock().unwrap();
                    m.prep_samples.push(t0.elapsed().as_micros() as u64);
                }
                // Re-admit completion marker by ORIGINAL arrival_ts (T9): a DRAM
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
