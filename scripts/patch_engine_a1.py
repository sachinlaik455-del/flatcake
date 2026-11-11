import io

p = 'src/engine.rs'
s = open(p, encoding='utf-8').read()

# --- A1 finding 1: DRAM-spill blocks must be published (insert_pending before append)
old1 = """                            Err(_) => {
                                // DRAM-full spill (OQ-6): flip scope, block to SSD.
                                self.core.index.rollback(slot);
                                self.core.registry.flip_to_ssd(spec.session);
                                self.core.metrics.lock().unwrap().dram_spill_puts += 1;
                                if let Err(e) =
                                    self.append_one_ssd(spec.session, &key, None, &payload)
                                {
                                    result = Err(e);
                                    break 'outer;
                                }
                            }"""
new1 = """                            Err(_) => {
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
                            }"""
assert old1 in s, "old1"
s = s.replace(old1, new1)

# --- A1 finding 2: pin EVERY owning session (cross-scope TOCTOU, D15)
old2 = """        // Reader pin at resolution time; released at request end (D15).
        let pinned_reader = self.core.registry.pin_reader(spec.signals.session);

        // hbm first (ARCHITECTURE §7), then Level-1 index resolution."""
new2 = """        // Reader pins at resolution time, released at request end (D15). Hybrid
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

        // hbm first (ARCHITECTURE §7), then Level-1 index resolution."""
assert old2 in s, "old2"
s = s.replace(old2, new2)

old2b = """        if pinned_reader {
            self.core.registry.unpin_reader(spec.signals.session);
        }"""
new2b = """        for s in pinned_scopes {
            self.core.registry.unpin_reader(s);
        }"""
assert old2b in s, "old2b"
s = s.replace(old2b, new2b)

# --- A1 finding 3a: logical_read_bytes owned by engine, not cio merge
old3 = """                {
                    let mut m = core.metrics.lock().unwrap();
                    m.physical_read_ops += io_metrics.physical_read_ops;
                    m.physical_read_bytes += io_metrics.physical_read_bytes;
                    m.logical_read_bytes += io_metrics.logical_read_bytes;
                    m.prep_samples.push(t0.elapsed().as_micros() as u64);
                }"""
new3 = """                {
                    let mut m = core.metrics.lock().unwrap();
                    m.physical_read_ops += io_metrics.physical_read_ops;
                    m.physical_read_bytes += io_metrics.physical_read_bytes;
                    // logical_read_bytes is accounted by get_request (read_amp's
                    // denominator has a single owner to avoid double counting --
                    // A1 finding 3).
                    m.prep_samples.push(t0.elapsed().as_micros() as u64);
                }"""
assert old3 in s, "old3"
s = s.replace(old3, new3)

old3b = """                    if out[*idx].is_some() {
                        if *is_ssd {
                            m.hit_ssd += 1;
                        } else {
                            m.hit_dram += 1;
                        }
                    } else {
                        m.misses += 1;
                    }"""
new3b = """                    if out[*idx].is_some() {
                        if *is_ssd {
                            m.hit_ssd += 1;
                            m.logical_read_bytes += self.block_bytes_of();
                        } else {
                            m.hit_dram += 1;
                        }
                    } else {
                        m.misses += 1;
                    }"""
assert old3b in s, "old3b"
s = s.replace(old3b, new3b)

# --- A1 finding 3c: mem_high_water + ssd_over_high_watermark_us writers
old4 = """        self.core.registry.unpin_writer(spec.session);
        self.maybe_reclaim();
        result
    }"""
new4 = """        self.core.registry.unpin_writer(spec.session);
        self.maybe_reclaim();
        {
            let used = self.mem_used();
            self.core.metrics.lock().unwrap().track_high_water(used);
        }
        result
    }"""
assert old4 in s, "old4"
s = s.replace(old4, new4)

old5 = """        let ssd_usage = self.core.layout.logical_bytes();
        let ssd_cap = self.core.cfg.gpu_file_pool * self.core.cfg.gpu_file_bytes;
        if (ssd_usage as f64) > self.core.cfg.ssd_high_watermark * ssd_cap as f64 {
            reclaim_pass("""
new5 = """        let ssd_usage = self.core.layout.logical_bytes();
        let ssd_cap = self.core.cfg.gpu_file_pool * self.core.cfg.gpu_file_bytes;
        if (ssd_usage as f64) > self.core.cfg.ssd_high_watermark * ssd_cap as f64 {
            let t_reclaim = std::time::Instant::now();
            reclaim_pass("""
assert old5 in s, "old5"
s = s.replace(old5, new5)

old6 = """                ReclaimStore::Ssd(&self.core.layout),
                &mut m,
            );
            drop(m);"""
new6 = """                ReclaimStore::Ssd(&self.core.layout),
                &mut m,
            );
            m.ssd_over_high_watermark_us += t_reclaim.elapsed().as_micros() as u64;
            drop(m);"""
assert old6 in s, "old6"
s = s.replace(old6, new6)

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('engine.rs: A1 fixes applied')

# --- load-factor assert in replay (OQ-16 startup guard)
p2 = 'src/replay.rs'
s = open(p2, encoding='utf-8').read()
old7 = """    // (session, start_blk, layers) -> seed of the creating put (integrity + recompute).
    let mut session_seeds: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new(); // session -> (start_blk, n_blk_per_layer, seed)"""
new7 = """    // Startup guard (OQ-16 / ARCHITECTURE 3): unique blocks must fit the
    // fixed-capacity Level-1 index at its documented 0.7 load factor.
    {
        let mut unique: std::collections::HashSet<(u64, u32, u64)> = Default::default();
        for r in records {
            if r.op == Op::Put {
                for l in 0..r.layers {
                    for i in 0..r.n_blk {
                        unique.insert((
                            r.session,
                            l,
                            (r.start_blk + i as u64) * cfg.block_tokens,
                        ));
                    }
                }
            }
        }
        let cap = cfg.index_capacity;
        if (unique.len() as f64) > 0.7 * cap as f64 {
            panic!(
                "trace needs {} unique blocks > 0.7 x index_capacity ({}); raise index_capacity or shrink the workload",
                unique.len(),
                cap
            );
        }
    }

    // (session, start_blk, layers) -> seed of the creating put (integrity + recompute).
    let mut session_seeds: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new(); // session -> (start_blk, n_blk_per_layer, seed)"""
assert old7 in s, "old7"
s = s.replace(old7, new7)
open(p2, 'w', encoding='utf-8', newline='\n').write(s)
print('replay.rs: load-factor guard added')
