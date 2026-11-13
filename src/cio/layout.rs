//! cio::layout — T11/T20: SSD scope-file (GPU File) layout, ARCHITECTURE §6.
//! Frozen byte formula (ARCHITECTURE §6): the SSD GVA offset packs the file as
//! `off[47:0] = file_seq[47:28] | in_file_off[27:0]` (2^28 = 256 MiB per file).
//! Segments are appended layer-major per turn batch:
//! `for layer l { segment(l) at align_up(cur, 4096); cur += blocks_per_layer × block_bytes }`
//! so every segment start — and therefore every block offset when
//! `block_bytes` is a 4 KiB multiple (default config) — satisfies the T20
//! alignment discipline.
//! All scope-file writes use `FILE_FLAG_NO_BUFFERING` handles (ARCHITECTURE §2,
//! OQ-21) with 4 KiB-aligned offsets, 4 KiB-aligned buffers (see
//! [`AlignedBuf`]) and lengths padded up to 4 KiB multiples (pad bytes zero).
//! Multi-file chaining is deferred (OQ-11): a session scope must fit one
//! pre-allocated `gpu_file_bytes` file; a batch exceeding it is a hard error
//! (the workload never triggers it — the guard is the assert).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::types::{FileRange, Gva, SsdLayout, MEDIA_SSD, SSD_FILE_MAX, SSD_IN_FILE_BITS};

/// DMA alignment discipline (paper §4 / T20): all scope-file I/O offsets,
/// lengths and buffer addresses are multiples of this.
pub const IO_ALIGN: u64 = 4096;

/// `FILE_FLAG_NO_BUFFERING` (winbase.h) — unbuffered scope-file I/O (OQ-21: NTFS page cache would make every `measured:` number a
/// cache artifact).
pub const FILE_FLAG_NO_BUFFERING: u32 = 0x0200_0000;

/// Align `v` up to a power-of-two multiple of `a`.
pub fn align_up(v: u64, a: u64) -> u64 {
    debug_assert!(a > 0 && a.count_ones() == 1);
    ((v + a - 1) / a) * a
}

/// Scope-file naming — single source shared with `cio::stream`.
pub fn scope_file_name(file_seq: u32) -> String {
    format!("gf_{:06}.bin", file_seq)
}

/// Sector-aligned heap buffer for NO_BUFFERING I/O (Windows requires the
/// buffer *address* to be sector-aligned, not just offset and length).
/// Zero-filled at allocation so write-padding bytes are always zero, including
/// when the buffer is reused across segments. Movable across threads (the
/// prefetch reader thread owns its buffer exclusively); never shared.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: plain unshared byte storage; ownership transfers exclusively.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> AlignedBuf {
        let len = len.max(1);
        let layout = std::alloc::Layout::from_size_align(len, IO_ALIGN as usize)
            .expect("AlignedBuf layout (size,4096)");
        // SAFETY: layout has non-zero size and a power-of-two align.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        AlignedBuf { ptr, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    fn layout(&self) -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(self.len, IO_ALIGN as usize)
            .expect("AlignedBuf layout (size,4096)")
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr valid for self.len bytes (allocated + zeroed in `new`).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, with exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated with the identical layout in `new`.
        unsafe { std::alloc::dealloc(self.ptr, self.layout()) }
    }
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AlignedBuf(len={})", self.len)
    }
}

/// Pre-allocate a scope file on disk: `File::create` + `set_len` (metadata op;
/// idempotent when the file already exists, e.g. from a previous run).
fn preallocate_file(dir: &Path, file_seq: u32, file_bytes: u64) -> io::Result<()> {
    let path = dir.join(scope_file_name(file_seq));
    if path.exists() {
        return Ok(());
    }
    let f = File::create(&path)?;
    f.set_len(file_bytes)?;
    Ok(())
}

/// Per-file mutable metadata. One mutex serializes appends AND free-list
/// updates per file (writes must be serialized per file, ARCHITECTURE §3).
#[derive(Default)]
struct FileMeta {
    /// Free extents `(off, len)` — reclamation bookkeeping , metadata only.
    free: Vec<(u64, u64)>,
    /// Logical bytes appended to this file (padding excluded).
    appended: u64,
    /// Logical bytes returned via `free_extent`.
    freed: u64,
    /// Physical append high-water (padded lengths included).
    cursor: u64,
    /// Lazily-opened NO_BUFFERING read+write handle.
    handle: Option<File>,
}

impl FileMeta {
    /// Open (once) and return the NO_BUFFERING scope-file handle.
    fn handle(&mut self, dir: &Path, file_seq: u32) -> io::Result<&File> {
        if self.handle.is_none() {
            let path = dir.join(scope_file_name(file_seq));
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_NO_BUFFERING)
                .open(path)?;
            self.handle = Some(f);
        }
        Ok(self.handle.as_ref().expect("handle just opened"))
    }
}

struct FileState {
    meta: Mutex<FileMeta>,
}

struct SessionScope {
    file_seq: u32,
    append_off: u64,
}

struct Inner {
    sessions: HashMap<u64, SessionScope>,
    files: HashMap<u32, Arc<FileState>>,
    next_seq: u32,
    /// Pre-allocated pool size (files 0..prealloc_seqs exist on disk already).
    prealloc_seqs: u32,
    /// Fully-reclaimed files available for a new scope (recycled, OQ-11/D20
    /// one-file-per-scope discipline).
    free_files: Vec<u32>,
}

impl Inner {
    /// Allocate a file for a NEW scope: recycle a fully-freed file, else take
    /// the next sequence number. Called with the outer lock held; only
    /// allocator metadata ops plus (rare, beyond-pool) file pre-allocation
    /// happen here — never payload data I/O (ARCHITECTURE §3 invariant).
    fn alloc_file(&mut self, dir: &Path, file_bytes: u64) -> io::Result<u32> {
        if let Some(seq) = self.free_files.pop() {
            let st = self.files.get(&seq).expect("recycled file has state");
            let mut m = st.meta.lock().unwrap();
            // Recycled file is overwritten from offset 0: stale free-list
            // entries and counters belong to the previous tenant.
            m.free.clear();
            m.appended = 0;
            m.freed = 0;
            m.cursor = 0;
            drop(m);
            return Ok(seq);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.files
            .insert(seq, Arc::new(FileState { meta: Mutex::new(FileMeta::default()) }));
        if seq >= self.prealloc_seqs {
            preallocate_file(dir, seq, file_bytes)?;
        }
        Ok(seq)
    }
}

/// SSD scope-file layout (T11/T20). One pre-allocated `gpu_file_bytes` file
/// per SSD scope; layer-major segment appends (ARCHITECTURE §6).
pub struct SsdLayoutImpl {
    dir: PathBuf,
    file_bytes: u64,
    block_bytes: u64,
    inner: Mutex<Inner>,
    logical: AtomicU64,
}

impl SsdLayoutImpl {
    /// Creates `cfg.ssd_dir` and pre-allocates the GPU File pool
    /// (`gf_000000.bin` .. `gf_{gpu_file_pool-1}.bin`) via `File::create` +
    /// `set_len(cfg.gpu_file_bytes)` (file_seq = index).
    pub fn new(cfg: &Config) -> io::Result<Self> {
        debug_assert!(
            cfg.gpu_file_bytes <= SSD_FILE_MAX && cfg.gpu_file_bytes.count_ones() == 1,
            "config bounds enforce gpu_file_bytes <= 2^28, power of two"
        );
        let dir = PathBuf::from(&cfg.ssd_dir);
        std::fs::create_dir_all(&dir)?;
        for seq in 0..cfg.gpu_file_pool as u32 {
            preallocate_file(&dir, seq, cfg.gpu_file_bytes)?;
        }
        Ok(SsdLayoutImpl {
            dir,
            file_bytes: cfg.gpu_file_bytes,
            block_bytes: cfg.block_bytes(),
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                files: HashMap::new(),
                next_seq: 0,
                prealloc_seqs: cfg.gpu_file_pool as u32,
                free_files: Vec::new(),
            }),
            logical: AtomicU64::new(0),
        })
    }

    /// Append one turn batch: `layers` segments of `blocks_per_layer` blocks,
    /// layer-major, each segment start 4 KiB-aligned (ARCHITECTURE §6 frozen
    /// formula). Each block's bytes come from `provider(layer, idx)`.
    ///
    /// Returns one GVA per block in layer-major order (layer `l` block `i` =>
    /// position `l * blocks_per_layer + i`).
    ///
    /// Lock discipline (ARCHITECTURE §3): the outer lock is held only for the
    /// allocator decision (pure metadata); the provider runs unlocked; the
    /// per-file mutex is held only across the (serialized) NO_BUFFERING
    /// segment writes of this batch.
    pub fn append_scope_batch(
        &self,
        session: u64,
        layers: u32,
        blocks_per_layer: u32,
        mut provider: impl FnMut(u32 /*layer*/, u32 /*idx*/) -> Vec<u8>,
    ) -> io::Result<Vec<Gva>> {
        let block_bytes = self.block_bytes;
        let seg_len = blocks_per_layer as u64 * block_bytes;
        if layers == 0 || blocks_per_layer == 0 || seg_len == 0 {
            return Ok(Vec::new());
        }
        let seg_pad = align_up(seg_len, IO_ALIGN);
        let batch_logical = layers as u64 * seg_len;
        let total_blocks = layers as u64 * blocks_per_layer as u64;

        // ---- allocator decision + extent reservation (metadata only) ----
        let (file_seq, seg_starts, final_cur) = {
            let mut inner = self.inner.lock().unwrap();
            let existing = inner
                .sessions
                .get(&session)
                .map(|s| (s.file_seq, s.append_off));
            let (file_seq, cur, is_new) = match existing {
                Some((fs, off)) => (fs, off, false),
                None => (inner.alloc_file(&self.dir, self.file_bytes)?, 0, true),
            };
            let mut p = cur;
            let mut seg_starts = Vec::with_capacity(layers as usize);
            for _ in 0..layers {
                p = align_up(p, IO_ALIGN);
                seg_starts.push(p);
                p += seg_len;
            }
            // Session-size guard (OQ-11): a scope must fit one file. The
            // last segment's write is padded, hence the padded end is checked.
            let physical_end = seg_starts[layers as usize - 1] + seg_pad;
            if physical_end > self.file_bytes {
                if is_new {
                    // Roll back the just-created allocation so a retry starts
                    // clean; the file carries no data yet.
                    inner.sessions.remove(&session);
                    inner.free_files.push(file_seq);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "session {session}: batch would grow scope to {physical_end} bytes \
                         > gpu_file_bytes {} (multi-file chaining deferred: OQ-11/D20)",
                        self.file_bytes
                    ),
                ));
            }
            // Reserve the whole batch extent atomically (no lock is held
            // across the provider calls or the file I/O below).
            inner
                .sessions
                .insert(session, SessionScope { file_seq, append_off: p });
            (file_seq, seg_starts, p)
        };
        let _ = final_cur;

        // ---- gather + validate all block payloads before any I/O ----
        // (a provider error aborts before the first byte is written)
        let mut blocks = Vec::with_capacity(total_blocks as usize);
        for l in 0..layers {
            for i in 0..blocks_per_layer {
                let v = provider(l, i);
                if v.len() as u64 != block_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "provider returned {} bytes for (layer {l}, idx {i}), expected block_bytes {block_bytes}",
                            v.len()
                        ),
                    ));
                }
                blocks.push(v);
            }
        }

        // ---- write segments (serialized per file, NO_BUFFERING) ----
        let st = {
            self.inner
                .lock()
                .unwrap()
                .files
                .get(&file_seq)
                .cloned()
                .expect("file state registered at allocation")
        };
        let mut m = st.meta.lock().unwrap();
        let handle = m.handle(&self.dir, file_seq)?;
        // Zero-filled buffer: pad regions stay zero across segment reuse.
        let mut buf = AlignedBuf::new(seg_pad as usize);
        for l in 0..layers {
            for i in 0..blocks_per_layer {
                let v = &blocks[(l as u64 * blocks_per_layer as u64 + i as u64) as usize];
                let at = (i as u64 * block_bytes) as usize;
                buf[at..at + block_bytes as usize].copy_from_slice(v);
            }
            let start = seg_starts[l as usize];
            let n = handle.seek_write(&buf[..seg_pad as usize], start)?;
            if n as u64 != seg_pad {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short NO_BUFFERING write on scope file",
                ));
            }
        }
        m.cursor = m.cursor.max(seg_starts[layers as usize - 1] + seg_pad);
        m.appended += batch_logical;
        drop(m);
        self.logical.fetch_add(batch_logical, Ordering::Relaxed);

        // ---- GVAs, layer-major ----
        let mut gvas = Vec::with_capacity(total_blocks as usize);
        for l in 0..layers {
            for i in 0..blocks_per_layer {
                let off = seg_starts[l as usize] + i as u64 * block_bytes;
                let packed = (file_seq as u64) << SSD_IN_FILE_BITS | off;
                let g = Gva::encode(MEDIA_SSD, 0, packed).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "SSD offset does not fit the GVA offset field",
                    )
                })?;
                gvas.push(g);
            }
        }
        Ok(gvas)
    }

    /// P4 recovery: re-register a session at a known append cursor after a
    /// crash (the replayed log gives the highest published in-file offset).
    /// The file state (lazily-opened NO_BUFFERING handle) is created if absent.
    pub fn adopt_session(&self, session: u64, file_seq: u32, append_off: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.sessions.contains_key(&session) {
            return; // idempotent
        }
        if !inner.files.contains_key(&file_seq) {
            inner.files.insert(
                file_seq,
                Arc::new(FileState {
                    meta: Mutex::new(FileMeta::default()),
                }),
            );
        }
        inner.sessions.insert(
            session,
            SessionScope {
                file_seq,
                append_off: align_up(append_off, IO_ALIGN),
            },
        );
    }


    /// Detach a session's scope entry. The engine calls this at reclamation
    /// (after freeing the scope's extents) so a stale session can never append
    /// into a recycled file. Contract gap: the frozen `SsdLayout` trait has no
    /// session detach — provided as an inherent method.
    pub fn release_session(&self, session: u64) -> bool {
        self.inner.lock().unwrap().sessions.remove(&session).is_some()
    }
}

impl SsdLayout for SsdLayoutImpl {
    /// Map an SSD GVA to a direct-I/O range (ARCHITECTURE §6 frozen formula):
    /// `file_seq = offset >> 28`, `in_file_off = offset & (2^28 - 1)`.
    fn resolve(&self, gva: Gva, len: u64) -> FileRange {
        debug_assert!(gva.is_ssd(), "resolve is for SSD GVAs");
        let off = gva.offset();
        FileRange {
            file_seq: (off >> SSD_IN_FILE_BITS) as u32,
            in_file_off: off & (SSD_FILE_MAX - 1),
            len,
        }
    }

    /// O(1) metadata free: pushes the extent onto the file's
    /// free list and adjusts counters. ZERO payload-byte operations (no file
    /// reads, no writes, no zeroing) — asserted by the reclamation tests.
    fn free_extent(&self, file_seq: u32, off: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let st = match inner.files.get(&file_seq) {
            Some(s) => s.clone(),
            None => {
                // Defensive: unknown file (engine frees what it appended, so
                // this never happens in the happy path). Register fresh state;
                // it can never recycle (appended stays 0 < freed).
                let s = Arc::new(FileState { meta: Mutex::new(FileMeta::default()) });
                inner.files.insert(file_seq, s.clone());
                s
            }
        };
        {
            let mut m = st.meta.lock().unwrap();
            m.free.push((off, len));
            m.freed += len;
            // Fully reclaimed → recyclable for a new scope (O(1) counter check;
            // when a file recycles, its free list is reset by `alloc_file`).
            if m.appended > 0 && m.freed == m.appended && !inner.free_files.contains(&file_seq) {
                inner.free_files.push(file_seq);
            }
        }
        self.logical.fetch_sub(len, Ordering::Relaxed);
    }

    /// Logical bytes written minus freed (watermark denominator, §5).
    fn logical_bytes(&self) -> u64 {
        self.logical.load(Ordering::Relaxed)
    }

    fn file_capacity(&self) -> u64 {
        self.file_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_edges() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
        assert_eq!(align_up(16896, 4096), 20480);
    }

    #[test]
    fn scope_file_naming() {
        assert_eq!(scope_file_name(0), "gf_000000.bin");
        assert_eq!(scope_file_name(7), "gf_000007.bin");
        assert_eq!(scope_file_name(123456), "gf_123456.bin");
    }

    #[test]
    fn aligned_buf_is_aligned_and_zeroed() {
        let mut b = AlignedBuf::new(1000);
        assert_eq!(b.len(), 1000);
        assert_eq!(b.as_ptr() as usize % 4096, 0, "address sector-aligned");
        assert!(b.iter().all(|&x| x == 0), "zero-filled (pad bytes zero)");
        b[7] = 9;
        assert_eq!(b[7], 9);
        let s: &[u8] = &b;
        assert_eq!(s.len(), 1000);
    }
}
