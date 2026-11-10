//! Conventional tiered baseline (T17): HBM window → DRAM LRU → SSD, host-staged SSD
//! restores, LRU-eviction-ordered SSD placement in FIFO-recycled buckets.
//! Faithfully reproduces the mechanisms the paper criticizes (Observations 1 & 2):
//! - every put enters DRAM first; eviction order (not session order) decides SSD layout;
//! - SSD restores stage through a bounded host staging pool: O(context × concurrency);
//!   pool exhaustion → prefetch abort → recompute + re-persist (write amplification);
//! - SSD space is FIFO-recycled buckets reclaimed whole, invalidating blocks across
//!   sessions (the cross-session prefix invalidation of paper §3.2);
//! - restored blocks are promoted back into DRAM (churn → more eviction writes);
//! - no read coalescing: one physical read per SSD block (O(layers × blocks)).
//! NO_BUFFERING file I/O on both systems (D12) so `measured:` numbers are honest.

use crate::engine_api::{GetSpec, KvEngine, PutSpec};
use crate::hbm::HbmWindow;
use crate::types::{BlockKey, Metrics};
use crate::util::payload_fill;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const NO_BUFFERING: u32 = 0x0200_0000; // FILE_FLAG_NO_BUFFERING
const SECTOR: u64 = 4096;

fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) / a * a
}

/// Sector-aligned byte buffer (required by FILE_FLAG_NO_BUFFERING on Windows).
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}
impl AlignedBuf {
    pub fn new(len: usize) -> AlignedBuf {
        let len = len.max(SECTOR as usize);
        let layout = std::alloc::Layout::from_size_align(len, SECTOR as usize).unwrap();
        unsafe {
            AlignedBuf {
                ptr: std::alloc::alloc(layout),
                len,
            }
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.len, SECTOR as usize).unwrap();
        unsafe { std::alloc::dealloc(self.ptr, layout) }
    }
}

// ---------------------------------------------------------------- DRAM LRU cache

struct DramEntry {
    off: u64,
    len: u64,
    stamp: u64,
}

enum Inserted {
    Ok(Vec<(BlockKey, Vec<u8>)>),      // evicted blocks (caller writes them to SSD)
    DoesNotFit(Vec<(BlockKey, Vec<u8>)>), // even an empty cache cannot hold it
}

struct DramCache {
    arena: Vec<u8>,
    bump: u64,
    free: Vec<(u64, u64)>,
    map: HashMap<BlockKey, DramEntry>,
    by_stamp: BTreeMap<u64, BlockKey>,
    stamp: u64,
    used: u64,
}

impl DramCache {
    fn new(cap: u64) -> DramCache {
        DramCache {
            arena: vec![0u8; cap as usize],
            bump: 0,
            free: Vec::new(),
            map: HashMap::new(),
            by_stamp: BTreeMap::new(),
            stamp: 0,
            used: 0,
        }
    }
    fn used_bytes(&self) -> u64 {
        self.used
    }
    /// First-fit reservation from free list or bump; None if nothing fits.
    fn reserve_internal(&mut self, len: u64) -> Option<u64> {
        for i in 0..self.free.len() {
            if self.free[i].1 >= len {
                let (off, flen) = self.free.swap_remove(i);
                if flen > len {
                    self.free.push((off + len, flen - len));
                }
                return Some(off);
            }
        }
        if self.bump + len <= self.arena.len() as u64 {
            let off = self.bump;
            self.bump += len;
            return Some(off);
        }
        None
    }
    /// Insert with LRU eviction until it fits. Refreshes stamp if key already present.
    fn insert(&mut self, key: BlockKey, payload: &[u8]) -> Inserted {
        let len = payload.len() as u64;
        let mut evicted = Vec::new();
        if let Some(old) = self.map.remove(&key) {
            self.used -= old.len;
            self.by_stamp.remove(&old.stamp);
            self.free.push((old.off, old.len));
        }
        loop {
            if let Some(off) = self.reserve_internal(len) {
                self.arena[off as usize..(off + len) as usize].copy_from_slice(payload);
                self.stamp += 1;
                self.map.insert(key, DramEntry { off, len, stamp: self.stamp });
                self.by_stamp.insert(self.stamp, key);
                self.used += len;
                return Inserted::Ok(evicted);
            }
            match self.evict_one() {
                Some(kv) => evicted.push(kv),
                None => return Inserted::DoesNotFit(evicted), // cache empty, still too small
            }
        }
    }
    fn evict_one(&mut self) -> Option<(BlockKey, Vec<u8>)> {
        let (&stamp, &key) = self.by_stamp.iter().next()?;
        self.by_stamp.remove(&stamp);
        let e = self.map.remove(&key)?;
        self.used -= e.len;
        let bytes = self.arena[e.off as usize..(e.off + e.len) as usize].to_vec();
        self.free.push((e.off, e.len));
        Some((key, bytes))
    }
    fn touch(&mut self, key: &BlockKey) {
        if let Some(e) = self.map.get_mut(key) {
            self.by_stamp.remove(&e.stamp);
            self.stamp += 1;
            e.stamp = self.stamp;
            self.by_stamp.insert(e.stamp, *key);
        }
    }
    fn get_bytes(&self, key: &BlockKey) -> Option<&[u8]> {
        let e = self.map.get(key)?;
        Some(&self.arena[e.off as usize..(e.off + e.len) as usize])
    }
}

// ---------------------------------------------------------------- SSD FIFO buckets

struct Bucket {
    file: std::fs::File,
    used: u64,
}

struct SsdBuckets {
    dir: String,
    buckets: Vec<Bucket>,
    current: usize,
    blocks: HashMap<BlockKey, (usize, u64, u64)>, // key -> (bucket, off, len)
    bucket_bytes: u64,
    logical_written: u64,
    physical_written: u64,
}

impl SsdBuckets {
    fn new(dir: &str, bucket_bytes: u64, bucket_count: u64) -> io::Result<SsdBuckets> {
        std::fs::create_dir_all(dir)?;
        let mut buckets = Vec::with_capacity(bucket_count as usize);
        for i in 0..bucket_count {
            let path = format!("{dir}/bucket_{i:04}.bin");
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(NO_BUFFERING)
                .open(&path)?;
            f.set_len(bucket_bytes)?;
            buckets.push(Bucket { file: f, used: 0 });
        }
        Ok(SsdBuckets {
            dir: dir.to_string(),
            buckets,
            current: 0,
            blocks: HashMap::new(),
            bucket_bytes,
            logical_written: 0,
            physical_written: 0,
        })
    }

    /// Append a block at the current bucket cursor; rotate/reclaim on exhaustion.
    /// Returns keys invalidated by FIFO bucket reclamation (cross-session loss).
    fn write_block(&mut self, key: BlockKey, payload: &[u8]) -> io::Result<Vec<BlockKey>> {
        let padded = align_up(payload.len() as u64, SECTOR) as usize;
        let mut invalidated = Vec::new();
        loop {
            let b = &mut self.buckets[self.current];
            if b.used + padded as u64 <= self.bucket_bytes {
                let off = b.used;
                let mut buf = AlignedBuf::new(padded);
                buf.as_mut_slice()[..payload.len()].copy_from_slice(payload);
                b.file.seek_write(buf.as_slice(), off)?;
                b.used += padded as u64;
                self.blocks.insert(key, (self.current, off, payload.len() as u64));
                self.logical_written += payload.len() as u64;
                self.physical_written += padded as u64;
                return Ok(invalidated);
            }
            self.advance_or_reclaim(&mut invalidated)?;
        }
    }

    /// Advance to a bucket with space; when all are full, reclaim FIFO-next and
    /// invalidate every block it held (they may belong to many sessions).
    fn advance_or_reclaim(&mut self, invalidated: &mut Vec<BlockKey>) -> io::Result<()> {
        for _ in 0..self.buckets.len() {
            self.current = (self.current + 1) % self.buckets.len();
            if self.buckets[self.current].used < self.bucket_bytes {
                return Ok(());
            }
        }
        let victim = (self.current + 1) % self.buckets.len();
        let keys: Vec<BlockKey> = self
            .blocks
            .iter()
            .filter(|(_, (bidx, _, _))| *bidx == victim)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.blocks.remove(&k);
            invalidated.push(k);
        }
        let path = format!("{}/bucket_{:04}.bin", self.dir, victim);
        self.buckets[victim] = Bucket {
            file: std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(NO_BUFFERING)
                .open(&path)?,
            used: 0,
        };
        self.current = victim;
        Ok(())
    }

    fn read_block(&self, key: &BlockKey) -> io::Result<Vec<u8>> {
        let (bidx, off, len) = *self
            .blocks
            .get(key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "block not on SSD"))?;
        let padded = align_up(len, SECTOR) as usize;
        let mut buf = AlignedBuf::new(padded);
        let n = self.buckets[bidx].file.seek_read(buf.as_mut_slice(), off)?;
        if (n as u64) < align_up(len, SECTOR) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        Ok(buf.as_slice()[..len as usize].to_vec())
    }

    fn contains(&self, key: &BlockKey) -> bool {
        self.blocks.contains_key(key)
    }
}

// ---------------------------------------------------------------- staging pool

struct StagingPool {
    used: Mutex<u64>,
    cap: u64,
    cv: Condvar,
}

impl StagingPool {
    fn new(cap: u64) -> StagingPool {
        StagingPool {
            used: Mutex::new(0),
            cap,
            cv: Condvar::new(),
        }
    }
    /// Reserve with bounded waiting; returns waited µs, or None after `retries`
    /// timeouts (prefetch abort — Observation 2 mechanism).
    fn reserve(&self, len: u64, timeout: Duration, mut retries: u32) -> Option<u64> {
        let started = Instant::now();
        let mut used = self.used.lock().unwrap();
        loop {
            if *used + len <= self.cap {
                *used += len;
                return Some(started.elapsed().as_micros() as u64);
            }
            if retries == 0 {
                return None;
            }
            let (g, _) = self.cv.wait_timeout(used, timeout).unwrap();
            used = g;
            retries -= 1;
        }
    }
    fn release(&self, len: u64) {
        let mut used = self.used.lock().unwrap();
        *used -= len;
        self.cv.notify_all();
    }
    fn used(&self) -> u64 {
        *self.used.lock().unwrap()
    }
}

// ---------------------------------------------------------------- engine

pub struct BaselineEngine {
    cfg: crate::config::Config,
    hbm: Mutex<HbmWindow>,
    dram: Mutex<DramCache>,
    ssd: Mutex<SsdBuckets>,
    staging: StagingPool,
    metrics: Mutex<Metrics>,
    block_bytes: u64,
}

impl BaselineEngine {
    pub fn new(cfg: &crate::config::Config) -> io::Result<BaselineEngine> {
        let block_bytes = cfg.block_bytes();
        Ok(BaselineEngine {
            cfg: cfg.clone(),
            hbm: Mutex::new(HbmWindow::new(cfg.hbm_window_bytes)),
            dram: Mutex::new(DramCache::new(cfg.dram_pool_bytes)),
            ssd: Mutex::new(SsdBuckets::new(
                &cfg.baseline_ssd_dir,
                cfg.bucket_bytes,
                cfg.bucket_count,
            )?),
            staging: StagingPool::new(cfg.staging_pool_bytes),
            metrics: Mutex::new(Metrics::default()),
            block_bytes,
        })
    }

    /// Write evicted blocks to SSD (churn path shared by put/promote/recompute).
    fn spill(&self, evicted: Vec<(BlockKey, Vec<u8>)>, m_bytes: &mut Metrics) -> io::Result<()> {
        if evicted.is_empty() {
            return Ok(());
        }
        let mut ssd = self.ssd.lock().unwrap();
        let mut invalidated = Vec::new();
        for (k, bytes) in evicted {
            m_bytes.physical_write_bytes += align_up(bytes.len() as u64, SECTOR);
            invalidated.extend(ssd.write_block(k, &bytes)?);
        }
        drop(ssd);
        let mut hbm = self.hbm.lock().unwrap();
        for k in &invalidated {
            hbm.invalidate(k.session, k.layer, k.token_start);
        }
        Ok(())
    }

    /// Recompute path (shared by missing blocks and staging-aborted restores):
    /// modeled compute cost + re-persist + deliver (paper §5.2/§5.3 mechanism).
    fn recompute_one(
        &self,
        b: &BlockKey,
        seed: u64,
        payload: &mut Vec<u8>,
        sink: &mut dyn FnMut(usize, &[u8]),
        idx: usize,
    ) -> io::Result<()> {
        payload_fill(b.session, b.layer, b.token_start, seed, payload);
        {
            let mut m = self.metrics.lock().unwrap();
            m.modeled_recompute_us += self.cfg.recompute_us_per_token * self.cfg.block_tokens;
            m.logical_write_bytes += self.block_bytes;
        }
        let evicted = {
            let mut dram = self.dram.lock().unwrap();
            match dram.insert(*b, payload) {
                Inserted::Ok(ev) => ev,
                Inserted::DoesNotFit(ev) => ev, // huge block: SSD-only
            }
        };
        self.spill(evicted, &mut self.metrics.lock().unwrap())?;
        // DoesNotFit blocks live only on SSD: write them now.
        let mut on_ssd = {
            let mut dram = self.dram.lock().unwrap();
            if dram.map.contains_key(b) {
                None
            } else {
                Some(payload.clone())
            }
        };
        if let Some(bytes) = on_ssd.take() {
            let mut ssd = self.ssd.lock().unwrap();
            let mut m = self.metrics.lock().unwrap();
            m.physical_write_bytes += align_up(self.block_bytes, SECTOR);
            drop(m);
            let inv = ssd.write_block(*b, &bytes)?;
            drop(ssd);
            let mut hbm = self.hbm.lock().unwrap();
            for k in &inv {
                hbm.invalidate(k.session, k.layer, k.token_start);
            }
        }
        self.hbm
            .lock()
            .unwrap()
            .record_delivery(b.session, b.layer, b.token_start, payload);
        sink(idx, payload);
        Ok(())
    }
}

impl KvEngine for BaselineEngine {
    fn put_batch(&self, spec: &PutSpec) -> io::Result<()> {
        let bb = self.block_bytes;
        let mut payload = vec![0u8; bb as usize];
        {
            let mut m = self.metrics.lock().unwrap();
            m.logical_write_bytes += spec.n_blocks() * bb;
        }
        for l in 0..spec.layers {
            for i in 0..spec.n_blk {
                let key = spec.block_key(l, i, self.cfg.block_tokens);
                payload_fill(spec.session, l, key.token_start, spec.seed, &mut payload);
                // Tiered premise: every block is admitted into DRAM first (paper §2.1).
                let evicted = {
                    let mut dram = self.dram.lock().unwrap();
                    match dram.insert(key, &payload) {
                        Inserted::Ok(ev) => ev,
                        Inserted::DoesNotFit(ev) => {
                            // Oversized-for-DRAM block: straight to SSD (still DRAM-first
                            // attempted, matching the tiered design).
                            ev
                        }
                    }
                };
                self.spill(evicted, &mut self.metrics.lock().unwrap())?;
                if !self.dram.lock().unwrap().map.contains_key(&key) {
                    // Landed on SSD directly (DoesNotFit).
                    let mut m = self.metrics.lock().unwrap();
                    m.physical_write_bytes += align_up(bb, SECTOR);
                    drop(m);
                    let mut ssd = self.ssd.lock().unwrap();
                    let inv = ssd.write_block(key, &payload)?;
                    drop(ssd);
                    let mut hbm = self.hbm.lock().unwrap();
                    for k in &inv {
                        hbm.invalidate(k.session, k.layer, k.token_start);
                    }
                }
            }
        }
        Ok(())
    }

    fn get_request(&self, spec: &GetSpec, sink: &mut dyn FnMut(usize, &[u8])) -> io::Result<()> {
        let t0 = Instant::now();
        let bb = self.block_bytes;
        let mut payload = vec![0u8; bb as usize];
        let mut staging_held: u64 = 0;
        let mut ssd_read_us: u64 = 0;

        // Phase 1: classify per block (hbm / dram / ssd / missing) — OQ-14 semantics.
        #[derive(Clone, Copy)]
        enum Src {
            Hbm,
            Dram,
            Ssd,
            Missing,
        }
        let mut srcs: Vec<Src> = Vec::with_capacity(spec.blocks.len());
        let mut ssd_len: u64 = 0;
        {
            let hbm = self.hbm.lock().unwrap();
            let dram = self.dram.lock().unwrap();
            let ssd = self.ssd.lock().unwrap();
            for b in &spec.blocks {
                let src = if hbm.get(b.session, b.layer, b.token_start).is_some() {
                    Src::Hbm
                } else if dram.map.contains_key(b) {
                    Src::Dram
                } else if ssd.contains(b) {
                    Src::Ssd
                } else {
                    Src::Missing
                };
                if matches!(src, Src::Ssd) {
                    ssd_len += align_up(bb, SECTOR);
                }
                srcs.push(src);
            }
        }

        // Phase 2: reserve staging for the whole SSD portion of the in-flight hit
        // (Observation 2). Exhaustion → abort prefetch → recompute.
        let mut aborted = false;
        if ssd_len > 0 {
            match self
                .staging
                .reserve(ssd_len, Duration::from_millis(100), 2)
            {
                Some(waited) => {
                    staging_held = ssd_len;
                    self.metrics
                        .lock()
                        .unwrap()
                        .qwait_samples
                        .push(waited);
                }
                None => {
                    aborted = true;
                }
            }
        }

        // Phase 3: deliver.
        let mut recompute: Vec<(usize, BlockKey, u64)> = Vec::new();
        for (idx, b) in spec.blocks.iter().enumerate() {
            match (srcs[idx], aborted) {
                (Src::Hbm, _) => {
                    let bytes = {
                        let hbm = self.hbm.lock().unwrap();
                        hbm.get(b.session, b.layer, b.token_start).unwrap().to_vec()
                    };
                    let mut m = self.metrics.lock().unwrap();
                    m.hit_hbm += 1;
                    m.requested_blocks += 1;
                    drop(m);
                    sink(idx, &bytes);
                }
                (Src::Dram, _) => {
                    let bytes = {
                        let mut dram = self.dram.lock().unwrap();
                        dram.touch(b);
                        dram.get_bytes(b).unwrap().to_vec()
                    };
                    let mut m = self.metrics.lock().unwrap();
                    m.hit_dram += 1;
                    m.requested_blocks += 1;
                    drop(m);
                    self.hbm
                        .lock()
                        .unwrap()
                        .record_delivery(b.session, b.layer, b.token_start, &bytes);
                    sink(idx, &bytes);
                }
                (Src::Ssd, false) => {
                    // No coalescing in tiered systems: one physical read per block
                    // (O(layers × blocks) fragmented reads, paper §3.4).
                    let t_read = Instant::now();
                    let bytes = {
                        let ssd = self.ssd.lock().unwrap();
                        let mut m = self.metrics.lock().unwrap();
                        m.physical_read_ops += 1;
                        m.physical_read_bytes += align_up(bb, SECTOR);
                        m.logical_read_bytes += bb;
                        drop(m);
                        ssd.read_block(b)?
                    };
                    ssd_read_us += t_read.elapsed().as_micros() as u64;
                    let mut m = self.metrics.lock().unwrap();
                    m.hit_ssd += 1;
                    m.requested_blocks += 1;
                    drop(m);
                    // Promote into the DRAM tier (paper §5.1) — may evict others (churn).
                    let evicted = {
                        let mut dram = self.dram.lock().unwrap();
                        match dram.insert(*b, &bytes) {
                            Inserted::Ok(ev) => ev,
                            Inserted::DoesNotFit(ev) => ev,
                        }
                    };
                    self.spill(evicted, &mut self.metrics.lock().unwrap())?;
                    self.hbm
                        .lock()
                        .unwrap()
                        .record_delivery(b.session, b.layer, b.token_start, &bytes);
                    sink(idx, &bytes);
                }
                (_, _) => {
                    recompute.push((idx, *b, spec.seed_by_block[idx]));
                    let mut m = self.metrics.lock().unwrap();
                    m.misses += 1;
                    m.requested_blocks += 1;
                }
            }
        }
        if staging_held > 0 {
            self.staging.release(staging_held);
        }
        for (idx, b, seed) in &recompute {
            self.recompute_one(b, *seed, &mut payload, sink, *idx)?;
        }

        let mut m = self.metrics.lock().unwrap();
        m.logical_block_gets += spec.blocks.len() as u64;
        m.prep_samples.push(ssd_read_us);
        m.lat_samples.push(t0.elapsed().as_micros() as u64);
        m.delivered_requests += 1;
        Ok(())
    }

    fn snapshot(&self) -> Metrics {
        self.metrics.lock().unwrap().clone()
    }

    fn mem_used(&self) -> u64 {
        self.dram.lock().unwrap().used_bytes()
            + self.hbm.lock().unwrap().used_bytes()
            + self.staging.used()
    }

    fn shutdown(&self) {}
}
