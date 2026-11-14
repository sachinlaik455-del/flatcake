"""P4 audit fixes part 2: F2 trace bounds, F3 no-panic workers, F4 file cap, F5 product bound."""
import re

# ---- F5: config product bound ----
c = open('src/config.rs', encoding='utf-8').read()
old_c = """    if cfg.gpu_file_bytes.count_ones() != 1 {
        return Err("gpu_file_bytes must be a power of two (SSD offset packing)".into());
    }"""
new_c = """    if cfg.gpu_file_bytes.count_ones() != 1 {
        return Err("gpu_file_bytes must be a power of two (SSD offset packing)".into());
    }
    // Product bound (audit F5): block_tokens x bytes_per_token are independently
    // bounded but their product can reach 4 GiB; cap it at 1 MiB per block.
    if cfg.block_tokens * cfg.bytes_per_token > 1 << 20 {
        return Err(format!(
            "block_tokens x bytes_per_token = {} exceeds the 1 MiB block cap",
            cfg.block_tokens * cfg.bytes_per_token
        ));
    }"""
assert old_c in c, "F5 config"
c = c.replace(old_c, new_c, 1)
# config knob: max_scope_files (audit F4)
old_k = """    /// P4: duplicate-request coalescing. DEFAULT OFF: a concurrency bug was
    /// found by the multi-round mixed-medium stress test (a follower can
    /// deliver another block's bytes under interleaving); see P4_REPORT.
    pub dedup_enabled: u64,
}"""
new_k = """    /// P4: duplicate-request coalescing. DEFAULT OFF: a concurrency bug was
    /// found by the multi-round mixed-medium stress test (a follower can
    /// deliver another block's bytes under interleaving); see P4_REPORT.
    pub dedup_enabled: u64,
    /// P4 audit F4: cap on total scope files (each reserves gpu_file_bytes of
    /// disk). Clean StorageFull error beyond the cap instead of silent fill.
    pub max_scope_files: u64,
}"""
assert old_k in c, "F4 knob decl"
c = c.replace(old_k, new_k, 1)
old_d = """            dedup_enabled: 0,
        }
    }
}"""
new_d = """            dedup_enabled: 0,
            max_scope_files: 4096,
        }
    }
}"""
assert old_d in c, "F4 default"
c = c.replace(old_d, new_d, 1)
old_b = """    u("dedup_enabled", &mut cfg.dedup_enabled, 0, 1)?;"""
new_b = """    u("dedup_enabled", &mut cfg.dedup_enabled, 0, 1)?;
    u("max_scope_files", &mut cfg.max_scope_files, 1, 1 << 16)?;"""
assert old_b in c, "F4 bound"
c = c.replace(old_b, new_b, 1)
old_u = """                "dedup_enabled","""
new_u = """                "dedup_enabled", "max_scope_files","""
assert old_u in c, "F4 unknown"
c = c.replace(old_u, new_u, 1)
open('src/config.rs', 'w', encoding='utf-8', newline='\n').write(c)
print('config.rs: F4/F5 applied')

# ---- F2: parse_line expansion bounds ----
w = open('src/workload.rs', encoding='utf-8').read()
old_p = """        Ok(TraceRecord {
            ts_us: get("ts_us")?,"""
new_p = """        // Hostile-trace bound (audit F2): a single record expands to
        // layers x n_blk blocks in memory; cap the expansion.
        let layers_n = get("layers")?;
        let nblk_n = get("n_blk")?;
        if layers_n == 0 || nblk_n == 0 || layers_n * nblk_n > 1 << 16 {
            return Err(format!(
                "record expands to {} blocks (layers={layers_n}, n_blk={nblk_n}); cap is {}",
                layers_n * nblk_n,
                1 << 16
            ));
        }
        Ok(TraceRecord {
            ts_us: get("ts_us")?,"""
assert old_p in w, "F2 parse"
w = w.replace(old_p, new_p, 1)
old_f = """            layers: get("layers")? as u32,
            start_blk: get("start_blk")?,
            n_blk: get("n_blk")? as u32,"""
new_f = """            layers: layers_n as u32,
            start_blk: get("start_blk")?,
            n_blk: nblk_n as u32,"""
assert old_f in w, "F2 fields"
w = w.replace(old_f, new_f, 1)
open('src/workload.rs', 'w', encoding='utf-8', newline='\n').write(w)
print('workload.rs: F2 applied')

# ---- F3: replay workers never panic on engine errors ----
r = open('src/replay.rs', encoding='utf-8').read()
old_1 = """                Job::Put(spec) => {
                    engine.put_batch(&spec).expect("put_batch failed");"""
new_1 = """                Job::Put(spec) => {
                    if let Err(e) = engine.put_batch(&spec) {
                        errors.lock().unwrap().push(format!("put: {e}"));
                    }"""
assert old_1 in r, "F3 put"
r = r.replace(old_1, new_1, 1)
old_2 = """                    engine
                        .get_request(&spec, &mut |idx, bytes| {
                            let b = &spec.blocks[idx];
                            let seed = spec.seed_by_block[idx];
                            payload_fill(b.session, b.layer, b.token_start, seed, &mut payload);
                            if bytes != payload.as_slice() {
                                integrity.fetch_add(1, Ordering::SeqCst);
                            }
                        })
                        .expect("get_request failed");"""
new_2 = """                    if let Err(e) = engine.get_request(&spec, &mut |idx, bytes| {
                        let b = &spec.blocks[idx];
                        let seed = spec.seed_by_block[idx];
                        payload_fill(b.session, b.layer, b.token_start, seed, &mut payload);
                        if bytes != payload.as_slice() {
                            integrity.fetch_add(1, Ordering::SeqCst);
                        }
                    }) {
                        errors.lock().unwrap().push(format!("get: {e}"));
                    }"""
assert old_2 in r, "F3 get"
r = r.replace(old_2, new_2, 1)
old_3 = """    let lat: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));"""
new_3 = """    let lat: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));"""
assert old_3 in r, "F3 errors decl"
r = r.replace(old_3, new_3, 1)
old_4 = """        let integrity = integrity.clone();
        let lat = lat.clone();"""
new_4 = """        let integrity = integrity.clone();
        let lat = lat.clone();
        let errors = errors.clone();"""
assert old_4 in r, "F3 errors clone"
r = r.replace(old_4, new_4, 1)
old_5 = """pub struct ReplayOutput {
    pub metrics: Metrics,
    pub elapsed_s: f64,
    pub integrity_failures: u64,
    pub requests_issued: u64,
}"""
new_5 = """pub struct ReplayOutput {
    pub metrics: Metrics,
    pub elapsed_s: f64,
    pub integrity_failures: u64,
    pub requests_issued: u64,
    pub op_errors: Vec<String>,
}"""
assert old_5 in r, "F3 output"
r = r.replace(old_5, new_5, 1)
old_6 = """    let integrity_failures = integrity.load(Ordering::SeqCst);
    if let Some(w) = csv.as_mut() {"""
new_6 = """    let integrity_failures = integrity.load(Ordering::SeqCst);
    let op_errors = std::mem::take(&mut *errors.lock().unwrap());
    if let Some(w) = csv.as_mut() {"""
assert old_6 in r, "F3 collect"
r = r.replace(old_6, new_6, 1)
old_7 = """    ReplayOutput {
        metrics: m,
        elapsed_s: t0.elapsed().as_secs_f64(),
        integrity_failures,
        requests_issued: issued.load(Ordering::SeqCst),
    }"""
new_7 = """    ReplayOutput {
        metrics: m,
        elapsed_s: t0.elapsed().as_secs_f64(),
        integrity_failures,
        requests_issued: issued.load(Ordering::SeqCst),
        op_errors,
    }"""
assert old_7 in r, "F3 output ctor"
r = r.replace(old_7, new_7, 1)
# thread closure clone for errors
old_8 = """        let integrity = integrity.clone();
        let lat = lat.clone();"""
new_8 = """        let integrity = integrity.clone();
        let lat = lat.clone();
        let errors = errors.clone();"""
assert old_8 in r, "F3 clone"
r = r.replace(old_8, new_8, 1)
# worker uses errors (both arms)
old_9 = """                Job::Put(spec) => {
                    if let Err(e) = engine.put_batch(&spec) {
                        errors.lock().unwrap().push(format!("put: {e}"));
                    }"""
assert old_9 in r, "F3 worker put uses errors"
open('src/replay.rs', 'w', encoding='utf-8', newline='\n').write(r)
print('replay.rs: F2/F3 applied')

# ---- F4: layout max_files cap ----
l = open('src/cio/layout.rs', encoding='utf-8').read()
old_s = """pub struct SsdLayoutImpl {
    dir: PathBuf,
    file_bytes: u64,
    block_bytes: u64,
    inner: Mutex<Inner>,
    logical: AtomicU64,
}"""
new_s = """pub struct SsdLayoutImpl {
    dir: PathBuf,
    file_bytes: u64,
    block_bytes: u64,
    inner: Mutex<Inner>,
    logical: AtomicU64,
    /// Cap on total scope files (audit F4): each pre-allocates gpu_file_bytes
    /// of reserved disk; unbounded creation would exhaust the volume.
    max_files: u64,
}"""
assert old_s in l, "F4 struct"
l = l.replace(old_s, new_s, 1)
old_n = """        Ok(SsdLayoutImpl {
            dir,
            file_bytes: cfg.gpu_file_bytes,
            block_bytes: cfg.block_bytes(),
            inner: Mutex::new(Inner {"""
new_n = """        Ok(SsdLayoutImpl {
            dir,
            file_bytes: cfg.gpu_file_bytes,
            block_bytes: cfg.block_bytes(),
            max_files: cfg.max_scope_files.max(cfg.gpu_file_pool),
            inner: Mutex::new(Inner {"""
assert old_n in l, "F4 ctor"
l = l.replace(old_n, new_n, 1)
old_a = """    fn alloc_file(&mut self, dir: &Path, file_bytes: u64) -> io::Result<u32> {
        if let Some(seq) = self.free_files.pop() {"""
new_a = """    fn alloc_file(&mut self, dir: &Path, file_bytes: u64) -> io::Result<u32> {
        if self.next_seq >= self.max_files {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "scope-file cap reached ({} files x {file_bytes} B); free scopes or raise max_scope_files",
                    self.max_files
                ),
            ));
        }
        if let Some(seq) = self.free_files.pop() {"""
assert old_a in l, "F4 alloc"
l = l.replace(old_a, new_a, 1)
open('src/cio/layout.rs', 'w', encoding='utf-8', newline='\n').write(l)
print('cio/layout.rs: F4 applied')

# ---- config field for SsdLayoutImpl: also need max_scope_files default present ----
cfgc = open('src/config.rs', encoding='utf-8').read()
assert 'max_scope_files' in cfgc, "config knob missing"
print('config knob present')
