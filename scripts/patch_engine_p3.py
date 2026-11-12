p = 'src/engine.rs'
s = open(p, encoding='utf-8').read()

# ---- 1. core fields -------------------------------------------------------
old = """    ssd_sessions: Mutex<HashSet<u64>>,
    /// M11 adaptive evidence: served gets per session (ADAPTIVE_EXTENSION.md).
    gets_served: Mutex<HashMap<u64, u64>>,
}"""
new = """    ssd_sessions: Mutex<HashSet<u64>>,
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
}

/// One deferred extent free: bytes stay valid until the owning scope has no
/// in-flight readers/writers (closes the read-vs-free race; D15 analog).
#[derive(Debug)]
enum PendingFree {
    Dram { off: u64, len: u64, session: u64 },
    Ssd { file_seq: u32, off: u64, len: u64, session: u64 },
}

const HEAT_CAP: usize = 65_536;
const PENDING_FREE_CAP: usize = 65_536;"""
assert old in s, "1 core fields"
s = s.replace(old, new, 1)

# ---- 2. core init ---------------------------------------------------------
old = """            ssd_sessions: Mutex::new(HashSet::new()),
            gets_served: Mutex::new(HashMap::new()),
        });"""
new = """            ssd_sessions: Mutex::new(HashSet::new()),
            gets_served: Mutex::new(HashMap::new()),
            heat: Mutex::new(HashMap::new()),
            dram_blocks: Mutex::new(HashMap::new()),
            pending_frees: Mutex::new(Vec::new()),
            last_migrated: Mutex::new(HashMap::new()),
            migration_lock: Mutex::new(()),
        });"""
assert old in s, "2 core init"
s = s.replace(old, new, 1)

# ---- 3. helpers after block_bytes_of --------------------------------------
old = """    fn block_bytes_of(&self) -> u64 {
        self.core.cfg.block_bytes()
    }
}"""
new = """    fn block_bytes_of(&self) -> u64 {
        self.core.cfg.block_bytes()
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
        let target = (self.core.cfg.dram_low_watermark * cap) as u64;
        let mut victims: Vec<(u64, BlockKey, u64, u64)> = {
            let db = self.core.dram_blocks.lock().unwrap();
            let heat = self.core.heat.lock().unwrap();
            let mut v: Vec<(u64, BlockKey, u64, u64)> = db
                .iter()
                .filter(|(_, (k, _, _))| {
                    heat.get(k)
                        .map(|(c, _)| *c < self.core.cfg.adaptive_k)
                        .unwrap_or(true)
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
            freed += len;
            budget -= 1;
            let _ = now;
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
}"""
assert old in s, "3 helpers"
s = s.replace(old, new, 1)

# ---- 4. capture dram_touched before hybrid drain ---------------------------
old = """        let is_hybrid = !ssd_gvas.is_empty() && !dram_list.is_empty();
        if is_hybrid {
            for (idx, gva) in dram_list.drain(..) {"""
new = """        let is_hybrid = !ssd_gvas.is_empty() && !dram_list.is_empty();
        let hybrid_dram: Vec<(usize, Gva)> = if is_hybrid { dram_list.clone() } else { Vec::new() };
        if is_hybrid {
            for (idx, gva) in dram_list.drain(..) {"""
assert old in s, "4 hybrid capture"
s = s.replace(old, new, 1)

# ---- 5. post-delivery P3 block (heat, dram bookkeeping, promotion) ---------
old = """        // Deliver in idx order + hbm population (identical policy both systems).
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
    }"""
new = """        // Deliver in idx order + hbm population (identical policy both systems).
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
                let queued_dram: &Vec<(usize, Gva)> = match enqueued.as_ref() {
                    Some((false, m)) => m,
                    _ => &[],
                };
                for (idx, gva) in hybrid_dram.iter().chain(queued_dram.iter()) {
                    if let (Some(bytes), Some(b)) =
                        (out[*idx].as_ref(), spec.blocks.get(*idx))
                    {
                        db.insert(gva.offset(), (*b, bytes.len() as u64, now));
                    }
                }
            }
            // Promotion: hottest delivered SSD blocks first, budget-capped,
            // using the bytes already in hand (zero extra SSD reads).
            let mut done = 0u64;
            let candidates: Vec<(u64, usize, Gva, BlockKey)> = {
                let heat = self.core.heat.lock().unwrap();
                let mut v: Vec<(u64, usize, Gva, BlockKey)> = match enqueued.as_ref() {
                    Some((true, m)) => m
                        .iter()
                        .filter_map(|(idx, gva)| {
                            let k = spec.blocks[*idx];
                            heat.get(&k).map(|(c, _)| (*c, *idx, *gva, k))
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                v.sort_by(|a, b| b.0.cmp(&a.0).then(a.3.cmp(&b.3)));
                v
            };
            for (_c, idx, gva, k) in candidates {
                if done >= self.core.cfg.migration_max_per_get {
                    break;
                }
                let Some(bytes) = out[idx].as_ref() else { continue };
                if self.promote_one(&k, gva, bytes, now) {
                    done += 1;
                }
            }
        }

        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += n_blocks as u64;
        m.delivered_requests += 1;
        Ok(())
    }"""
assert old in s, "5 post-delivery"
s = s.replace(old, new, 1)

# ---- 6. put_batch dram_blocks bookkeeping ----------------------------------
old = """                            Ok(off) => {
                                self.core.arena.write(off, &payload);
                                let gva = Gva::encode(0, 0, off).unwrap();
                                self.core.index.publish(slot, gva);
                                self.core.registry.add_dram_extent(spec.session, off, bb);
                                self.core.registry.add_key(spec.session, key);
                            }"""
new = """                            Ok(off) => {
                                self.core.arena.write(off, &payload);
                                let gva = Gva::encode(0, 0, off).unwrap();
                                self.core.index.publish(slot, gva);
                                self.core.registry.add_dram_extent(spec.session, off, bb);
                                self.core.registry.add_key(spec.session, key);
                                self.core.dram_blocks.lock().unwrap().insert(
                                    off,
                                    (key, bb, self.now_us()),
                                );
                            }"""
assert old in s, "6 put dram bookkeeping"
s = s.replace(old, new, 1)

# ---- 7. recompute_persist dram_blocks bookkeeping --------------------------
old = """                Ok(off) => {
                    self.core.arena.write(off, payload);
                    if let Some(s) = slot {
                        let gva = Gva::encode(0, 0, off).unwrap();
                        self.core.index.publish(s, gva);
                    }
                    self.core.registry.add_dram_extent(sig.session, off, bb);
                    Ok(())
                }"""
new = """                Ok(off) => {
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
                }"""
assert old in s, "7 recompute bookkeeping"
s = s.replace(old, new, 1)

# ---- 8. maybe_reclaim: flush + demote + purge ------------------------------
old = """            let mut ssd_sessions = self.core.ssd_sessions.lock().unwrap();
            for session in ssd_sessions.clone().iter() {
                if !self.core.registry.scope_exists(*session) {
                    self.core.layout.release_session(*session);
                    ssd_sessions.remove(session);
                }
            }
        }
    }"""
new = """            let mut ssd_sessions = self.core.ssd_sessions.lock().unwrap();
            for session in ssd_sessions.clone().iter() {
                if !self.core.registry.scope_exists(*session) {
                    self.core.layout.release_session(*session);
                    ssd_sessions.remove(session);
                }
            }
        }
        // P3: return migration-deferred extents, then optionally demote cold
        // DRAM blocks before falling back to whole-session reclamation.
        self.flush_pending_frees();
        self.demote_pass(self.now_us());
        self.flush_pending_frees();
        self.purge_dead_scope_bookkeeping();
    }"""
assert old in s, "8 maybe_reclaim"
s = s.replace(old, new, 1)

# ---- 9. shutdown: final flush ----------------------------------------------
old = """    fn shutdown(&self) {
        if self.running.swap(false, Ordering::SeqCst) {
            self.core.scheduler.shutdown();
            let handles: Vec<_> = self.worker_handles.lock().unwrap().drain(..).collect();
            for h in handles {
                let _ = h.join();
            }
        }
    }"""
new = """    fn shutdown(&self) {
        if self.running.swap(false, Ordering::SeqCst) {
            self.core.scheduler.shutdown();
            let handles: Vec<_> = self.worker_handles.lock().unwrap().drain(..).collect();
            for h in handles {
                let _ = h.join();
            }
            self.flush_pending_frees();
        }
    }"""
assert old in s, "9 shutdown"
s = s.replace(old, new, 1)

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('engine.rs: P3 migration core applied (9 patches)')
