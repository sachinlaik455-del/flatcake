"""P4 Track B (audit-aligned): recovery log (SSD publishes + invalidates only,
opt-in replay), recompute write-batching + ownership fix, duplicate-request
coalescing (GVA-matched merge), metrics. Implements docs/P4_INTEGRATION_AUDIT.md
items 1-14 within the no-buffer design (recompute batching instead of a dirty
buffer: items 1/3/6/8/9 are satisfied structurally — every append stays a
synchronous NO_BUFFERING write, so flush-before-publish is automatic).
"""
p = 'src/engine.rs'
s = open(p, encoding='utf-8').read()

# ---- 1. Slot gains the dedup fingerprint owner marker ------------------------
old = """struct Slot {
    out: Vec<Option<Vec<u8>>>,
    /// (original block idx, GVA) the worker must serve; empty = completion marker.
    map: Vec<(usize, Gva)>,
    err: Option<String>,
    done: bool,
}"""
new = """struct Slot {
    out: Vec<Option<Vec<u8>>>,
    /// (original block idx, GVA) the worker must serve; empty = completion marker.
    map: Vec<(usize, Gva)>,
    err: Option<String>,
    done: bool,
    /// Duplicate-request coalescing: leader owns the fingerprint; followers
    /// wait on this slot and merge `out` by GVA match (audit item 12).
    fp: u64,
}"""
assert old in s, "1 slot fp"
s = s.replace(old, new, 1)

# ---- 2. EngineCore fields -----------------------------------------------------
old = """    /// Serializes migration state transitions (publish swaps + bookkeeping).
    migration_lock: Mutex<()>,
}"""
new = """    /// Serializes migration state transitions (publish swaps + bookkeeping).
    migration_lock: Mutex<()>,
    // -------- P4 durability + request coalescing -----------------------------
    /// Recovery log writer (audit item 2: SSD publishes + invalidates only;
    /// DRAM publishes are process-volatile and never logged).
    recovery_log: Option<Mutex<std::io::BufWriter<std::fs::File>>>,
    /// In-flight SSD request dedup: fingerprint -> leader slot (audit item 11).
    inflight: Mutex<HashMap<u64, Arc<(Mutex<Slot>, Condvar)>>>,
}"""
assert old in s, "2 core fields"
s = s.replace(old, new, 1)

old = """            last_migrated: Mutex::new(HashMap::new()),
            migration_lock: Mutex::new(()),
        });"""
new = """            last_migrated: Mutex::new(HashMap::new()),
            migration_lock: Mutex::new(()),
            recovery_log: None,
            inflight: Mutex::new(HashMap::new()),
        });"""
assert old in s, "2b core init"
s = s.replace(old, new, 1)

# ---- 3. new(): log writer + opt-in replay BEFORE workers ----------------------
old = """        let core = Arc::new(EngineCore {
            cfg: cfg.clone(),"""
new = """        // P4 recovery log (audit items 2/4/5): opened in append mode. Replay is
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
            cfg: cfg.clone(),"""
assert old in s, "3a new prelude"
s = s.replace(old, new, 1)

old = """            last_migrated: Mutex::new(HashMap::new()),
            migration_lock: Mutex::new(()),
            recovery_log: None,
            inflight: Mutex::new(HashMap::new()),
        });"""
new = """            last_migrated: Mutex::new(HashMap::new()),
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
        }"""
assert old in s, "3b replay"
s = s.replace(old, new, 1)

# ---- 4. log helpers + record type + parse -------------------------------------
old = """    /// Record delivery heat; bounded (drop cold entries at the cap)."""
new = """    /// Log an SSD publish record (audit item 2: after the payload write; DRAM
    /// publishes are never logged).
    fn log_publish(&self, key: &BlockKey, gva: Gva) {
        if let Some(w) = self.core.recovery_log.as_ref() {
            let mut w = w.lock().unwrap();
            let _ = std::io::Write::write_fmt(
                &mut w,
                format_args!(
                    "{{\\"p\\":[{}, {}, {}],\\"g\\":{}}}\\n",
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
                &mut w,
                format_args!("{{\\"i\\":[{}, {}, {}]}}\\n", key.session, key.layer, key.token_start),
            );
        }
    }

    /// Flush the recovery log (batch end / shutdown).
    fn flush_log(&self) {
        if let Some(w) = self.core.recovery_log.as_ref() {
            let _ = std::io::Write::flush(&mut w.lock().unwrap());
        }
    }

    /// Record delivery heat; bounded (drop cold entries at the cap)."""
assert old in s, "4 helpers"
s = s.replace(old, new, 1)

# ---- 5. publish_swap logs the invalidate (audit item 10) ----------------------
old = """    fn publish_swap(&self, key: &BlockKey, new_gva: Gva) -> bool {
        self.core.index.invalidate(key);"""
new = """    fn publish_swap(&self, key: &BlockKey, new_gva: Gva) -> bool {
        self.log_invalidate(key);
        self.core.index.invalidate(key);"""
assert old in s, "5 swap invalidate log"
s = s.replace(old, new, 1)

# ---- 6. SSD publish sites log (DRAM sites deliberately do NOT) ----------------
# 6a. put_batch SSD loop
old = """                    Ok(gvas) if gvas.len() == total => {
                        for (s, g) in slots.iter().zip(gvas.iter()) {
                            if let Some(s) = s {
                                self.core.index.publish(*s, *g);
                            }
                        }
                        for k in &keys {
                            self.core.registry.add_key(spec.session, *k);
                        }"""
new = """                    Ok(gvas) if gvas.len() == total => {
                        for ((s, g), k) in slots.iter().zip(gvas.iter()).zip(keys.iter()) {
                            if let Some(s) = s {
                                self.core.index.publish(*s, *g);
                                self.log_publish(k, *g);
                            }
                        }
                        for k in &keys {
                            self.core.registry.add_key(spec.session, *k);
                        }"""
assert old in s, "6a put ssd"
s = s.replace(old, new, 1)

# 6b. append_one_ssd
old = """        if let (Some(s), Some(g)) = (slot, gvas.first()) {
            self.core.index.publish(s, *g);
        }
        if let Some(g) = gvas.first() {
            let fr = self.core.layout.resolve(*g, bb);"""
new = """        if let (Some(s), Some(g)) = (slot, gvas.first()) {
            self.core.index.publish(s, *g);
            self.log_publish(key, *g);
        }
        if let Some(g) = gvas.first() {
            let fr = self.core.layout.resolve(*g, bb);"""
assert old in s, "6b append_one_ssd"
s = s.replace(old, new, 1)

# 6c. demote_pass
old = """            let Some(g) = gvas.first().copied() else { continue };
            if !self.publish_swap(&key, g) {
                continue;
            }
            let fr = self.core.layout.resolve(g, len);"""
new = """            let Some(g) = gvas.first().copied() else { continue };
            if !self.publish_swap(&key, g) {
                continue;
            }
            self.log_publish(&key, g);
            let fr = self.core.layout.resolve(g, len);"""
assert old in s, "6c demote"
s = s.replace(old, new, 1)

# NOTE: put_batch Dram branch, recompute_persist Dram branch, and promote_one
# publish DRAM GVAs — intentionally NOT logged (audit item 2).

# ---- 7. recompute batching + ownership fix ------------------------------------
old = """        // Recompute path (inline on the calling thread; modeled cost + re-persist).
        for (idx, b, seed) in &recompute {
            payload_fill(b.session, b.layer, b.token_start, *seed, &mut payload);
            self.core.metrics.lock().unwrap().modeled_recompute_us +=
                self.core.cfg.recompute_us_per_token * self.core.cfg.block_tokens;
            // A persist failure must not fail THIS request: the recomputed payload
            // is deliverable regardless; the block's next read simply misses again
            // (honest accounting — never silently dropped).
            let _ = self.recompute_persist(b, &spec.signals, &payload);
            out[*idx] = Some(payload.clone());
        }"""
new = """        // Recompute path (inline on the calling thread; modeled cost + re-persist).
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
        }"""
assert old in s, "7 recompute batching"
s = s.replace(old, new, 1)

# ---- 8. flush_recompute_chunk helper ------------------------------------------
old = """    /// Append one block to the session's SSD scope, publish, register extent."""
new = """    /// One batched SSD append for a recomputed (session, layer) chunk, then a
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
        let start_blk = first_ts / self.core.cfg.block_tokens;
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

    /// Append one block to the session's SSD scope, publish, register extent."""
assert old in s, "8 chunk helper"
s = s.replace(old, new, 1)

# ---- 9. dedup (audit items 11-13): sorted-GVA fp, GVA-matched merge ------------
old = """            let slot_arc = Arc::new((
                Mutex::new(Slot {
                    out: vec![None; n_blocks],
                    map: map.clone(),
                    err: None,
                    done: false,
                }),
                Condvar::new(),
            ));
            self.core.slots.lock().unwrap().insert(id, slot_arc.clone());
            self.core.scheduler.enqueue(req, *is_ssd);"""
new = """            // P4 dedup (audit items 11-13): fingerprint = SORTED SSD GVA list
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
                    fp: if is_ssd { fp } else { 0 },
                }),
                Condvar::new(),
            ));
            if is_ssd {
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
                    let (slot_lock2, _) = &*leader;
                    let leader_map: Vec<(usize, u64)> = slot_lock2
                        .lock()
                        .unwrap()
                        .map
                        .iter()
                        .map(|(oi, g)| (*oi, g.0))
                        .collect();
                    drop(leader_map);
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
            self.core.scheduler.enqueue(req, *is_ssd);"""
assert old in s, "9 dedup"
s = s.replace(old, new, 1)

# ---- 10. finish_slot removes the dedup entry (owner side) ----------------------
old = """fn finish_slot(core: &EngineCore, slot_arc: &Arc<(Mutex<Slot>, Condvar)>, id: u64) {
    {
        let (lock, cv) = &**slot_arc;
        let mut g = lock.lock().unwrap();
        g.done = true;
        cv.notify_all();
    }
    core.slots.lock().unwrap().remove(&id);
}"""
new = """fn finish_slot(core: &EngineCore, slot_arc: &Arc<(Mutex<Slot>, Condvar)>, id: u64) {
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
        let mut inflight = core.inflight.lock().unwrap();
        if let Some(v) = inflight.get(&fp) {
            if v.len() == 1 {
                inflight.remove(&fp);
            }
        }
    }
}"""
assert old in s, "10 finish_slot"
s = s.replace(old, new, 1)

# ---- 11. finish_deduped_get helper ---------------------------------------------
old = """    /// Append one block to the session's SSD scope, publish, register extent."""
new = """    /// Shared tail for dedup followers: accounting (audit item 13), unpin,
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

    /// Append one block to the session's SSD scope, publish, register extent."""
assert old in s, "11 helper"
s = s.replace(old, new, 1)

# ---- 12. shutdown: flush log (audit item 4) ------------------------------------
old = """    fn shutdown(&self) {
        if self.running.swap(false, Ordering::SeqCst) {
            self.core.scheduler.shutdown();
            let handles: Vec<_> = self.worker_handles.lock().unwrap().drain(..).collect();
            for h in handles {
                let _ = h.join();
            }
            self.flush_pending_frees();
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
            self.flush_log();
        }
    }"""
assert old in s, "12 shutdown"
s = s.replace(old, new, 1)

# ---- 13. batch-end log flushes (audit item 2 ordering: log durable per batch) --
old = """        self.core.registry.unpin_writer(spec.session);
        self.maybe_reclaim();
        {
            let used = self.mem_used();
            self.core.metrics.lock().unwrap().track_high_water(used);
        }
        result
    }"""
new = """        self.core.registry.unpin_writer(spec.session);
        self.maybe_reclaim();
        self.flush_log();
        {
            let used = self.mem_used();
            self.core.metrics.lock().unwrap().track_high_water(used);
        }
        result
    }"""
assert old in s, "13 put flush log"
s = s.replace(old, new, 1)

old = """        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += n_blocks as u64;
        m.delivered_requests += 1;
        Ok(())
    }"""
new = """        self.flush_log();
        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += n_blocks as u64;
        m.delivered_requests += 1;
        Ok(())
    }"""
assert old in s, "13b get flush log"
s = s.replace(old, new, 1)

# ---- 14. parse_log_line + record enum (file tail) ------------------------------
old = """/// Mark the slot done, remove it from the registry (single completion per id),
/// and wake the waiting caller."""
new = """/// One recovery-log record (audit item 2/10): SSD publish or invalidate.
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
    if let Some(i) = line.find("\\"p\\":[") {
        let kpos = i + 5;
        let kend = line[kpos..].find(']').map(|j| kpos + j)?;
        let k = parse_key(&line[kpos..kend])?;
        let gpos = line.find("\\"g\\":").map(|j| j + 5)?;
        let gend = line[gpos..].find('}').map(|j| gpos + j)?;
        let g = line[gpos..gend].trim().parse::<u64>().ok()?;
        return Some(LogRec::Pub(k, Gva(g)));
    }
    if let Some(i) = line.find("\\"i\\":[") {
        let kpos = i + 5;
        let kend = line[kpos..].find(']').map(|j| kpos + j)?;
        let k = parse_key(&line[kpos..kend])?;
        return Some(LogRec::Invalidate(k));
    }
    None
}

/// Mark the slot done, remove it from the registry (single completion per id),
/// and wake the waiting caller."""
assert old in s, "14 parse"
s = s.replace(old, new, 1)

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('engine.rs: P4 patch applied (audit-aligned, 14 sections)')
