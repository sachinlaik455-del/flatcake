"""D-A5/D-A2/minor fixes - anchors verified against the CURRENT tree
(which does NOT have log_publish in the put_batch DRAM branch - correct per
audit item 2: DRAM publishes are never logged)."""
e = open('src/engine.rs', encoding='utf-8').read()

# ---- D-A5: detach after BOTH reclaim passes ---------------------------------
old = """            m.ssd_over_high_watermark_us += t_reclaim.elapsed().as_micros() as u64;
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
    }"""
new = """            m.ssd_over_high_watermark_us += t_reclaim.elapsed().as_micros() as u64;
        }
        // Detach reclaimed scopes from the layout (audit D-A5): runs after BOTH
        // reclaim passes - a mixed DRAM+SSD scope removed by the DRAM pass must
        // not leave a stale layout entry that a later put could append into.
        {
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
    }"""
assert old in e, "D-A5"
e = e.replace(old, new, 1)

# ---- D-A2: capture recompute flush errors -----------------------------------
old = """                    Medium::Ssd => {
                        let max_chunk_blocks =
                            (self.core.cfg.write_coalesce_bytes.max(bb) / bb) as u32;
                        let mut chunk: Vec<(usize, BlockKey, u64)> = Vec::new();
                        for item in group {
                            chunk.push(item);
                            if chunk.len() as u32 >= max_chunk_blocks {
                                self.flush_recompute_chunk(
                                    session, layer, &mut chunk, &mut payload, &mut out,
                                )?;
                            }
                        }
                        self.flush_recompute_chunk(session, layer, &mut chunk, &mut payload, &mut out)?;
                    }"""
new = """                    Medium::Ssd => {
                        let max_chunk_blocks =
                            (self.core.cfg.write_coalesce_bytes.max(bb) / bb) as u32;
                        let mut chunk: Vec<(usize, BlockKey, u64)> = Vec::new();
                        // A re-persist error must not skip the reader-pin
                        // release (audit D-A2): capture, deliver what the
                        // flush produced (failed blocks stay None -> counted
                        // as misses), then report after delivery.
                        let mut chunk_err: Option<io::Error> = None;
                        for item in group {
                            chunk.push(item);
                            if chunk.len() as u32 >= max_chunk_blocks {
                                if let Err(e) = self.flush_recompute_chunk(
                                    session, layer, &mut chunk, &mut payload, &mut out,
                                ) {
                                    chunk_err.get_or_insert(e);
                                }
                            }
                        }
                        if let Err(e) =
                            self.flush_recompute_chunk(session, layer, &mut chunk, &mut payload, &mut out)
                        {
                            chunk_err.get_or_insert(e);
                        }
                        if let Some(e) = chunk_err {
                            first_recompute_err.get_or_insert(e);
                        }
                    }"""
assert old in e, "D-A2 capture"
e = e.replace(old, new, 1)

old = """        // Recompute path (inline on the calling thread; modeled cost + re-persist).
        // P4 write coalescing: batch per (owning session, layer) — one SSD
        // append per group instead of one per small block. Ownership follows
        // the KEY's session: a recomputed shared-prompt block belongs to
        // session 1, never to the requester (P4 ownership fix).
        {"""
new = """        // Recompute path (inline on the calling thread; modeled cost + re-persist).
        // P4 write coalescing: batch per (owning session, layer) - one SSD
        // append per group instead of one per small block. Ownership follows
        // the KEY's session: a recomputed shared-prompt block belongs to
        // session 1, never to the requester (ownership fix).
        let mut first_recompute_err: Option<io::Error> = None;
        {"""
assert old in e, "D-A2 decl"
e = e.replace(old, new, 1)

old = """        self.flush_log();
        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += n_blocks as u64;
        m.delivered_requests += 1;
        Ok(())
    }"""
new = """        self.flush_log();
        if let Some(e) = first_recompute_err {
            return Err(e);
        }
        let mut m = self.core.metrics.lock().unwrap();
        m.logical_block_gets += n_blocks as u64;
        m.delivered_requests += 1;
        Ok(())
    }"""
assert old in e, "D-A2 report"
e = e.replace(old, new, 1)

# ---- minor metrics: logical_write_bytes only on successful placement --------
old = """        self.pin_writer_retry(&sig)?;
        self.core.metrics.lock().unwrap().logical_write_bytes += n * bb;
        let mut payload = vec![0u8; bb as usize];"""
new = """        self.pin_writer_retry(&sig)?;
        let mut payload = vec![0u8; bb as usize];"""
assert old in e, "minor pre-remove"
e = e.replace(old, new, 1)
old = """                                self.core.index.publish(slot, gva);
                                self.core.registry.add_dram_extent(spec.session, off, bb);"""
new = """                                self.core.index.publish(slot, gva);
                                self.core.metrics.lock().unwrap().logical_write_bytes += bb;
                                self.core.registry.add_dram_extent(spec.session, off, bb);"""
assert old in e, "minor dram"
e = e.replace(old, new, 1)
old = """                        self.core.metrics.lock().unwrap().physical_write_bytes += n * bb;
                    }
                    Ok(_) => {"""
new = """                        self.core.metrics.lock().unwrap().physical_write_bytes += n * bb;
                        self.core.metrics.lock().unwrap().logical_write_bytes += n * bb;
                    }
                    Ok(_) => {"""
assert old in e, "minor ssd"
e = e.replace(old, new, 1)
old = """        self.core.metrics.lock().unwrap().physical_write_bytes += bb;
        Ok(())"""
new = """        self.core.metrics.lock().unwrap().physical_write_bytes += bb;
        self.core.metrics.lock().unwrap().logical_write_bytes += bb;
        Ok(())"""
assert old in e, "minor spill"
e = e.replace(old, new, 1)
open('src/engine.rs', 'w', encoding='utf-8', newline='\n').write(e)
print('engine.rs: D-A5 + D-A2 + minor metrics applied')

# ---- D-A3: config alignment validation ---------------------------------------
c = open('src/config.rs', encoding='utf-8').read()
old = """    if cfg.block_tokens * cfg.bytes_per_token > 1 << 20 {"""
new = """    // Audit D-A3: unaligned block sizes trip NO_BUFFERING alignment asserts
    // at read time; reject at load with a clear error instead.
    if (cfg.block_tokens * cfg.bytes_per_token) % 4096 != 0 {
        return Err(format!(
            "block_tokens x bytes_per_token = {} is not a multiple of 4096 (NO_BUFFERING alignment)",
            cfg.block_tokens * cfg.bytes_per_token
        ));
    }
    if cfg.block_tokens * cfg.bytes_per_token > 1 << 20 {"""
assert old in c, "D-A3"
c = c.replace(old, new, 1)
open('src/config.rs', 'w', encoding='utf-8', newline='\n').write(c)
print('config.rs: D-A3 applied')
