//! gva::arena — pre-allocated DRAM pool behind the frozen [`DramArena`] trait
//! (ARCHITECTURE §3: "pre-allocated, bump + session-extent free").
//!
//! Allocation strategy: **first-fit reuse of freed extents is preferred over
//! bump growth** so that `used_bytes` (the DRAM-watermark input, §5) always
//! reflects live data; the bump pointer only advances when no free extent
//! satisfies the request. Freed extents are kept as a disjoint, offset-sorted
//! list and coalesced with adjacent neighbors on free, so a fully freed pool
//! collapses back into one extent (and `reserve` of the full size succeeds
//! again).
//!
//! Payload copies happen inside the arena lock (EGM #12: ms-scale for MB
//! payloads, accepted for correctness at Level B, profiled at M10).
//!
//! Offsets returned by `reserve` fit the 48-bit GVA Offset field (asserted at
//! construction); encode with `Gva::encode(MEDIA_DRAM, node, off)`.

use crate::config::Config;
use crate::types::{ArenaFull, DramArena};
use std::sync::Mutex;

/// Concrete DRAM arena; handed out as `&dyn DramArena` to engine/reclaim.
pub struct Arena {
    inner: Mutex<Pool>,
    cap: u64,
}

struct Pool {
    buf: Vec<u8>,
    /// One past the highest byte ever committed by bump allocation.
    bump_high: u64,
    /// Disjoint freed extents `(off, len)` sorted by offset.
    /// Invariant: fully coalesced — no two adjacent extents.
    free: Vec<(u64, u64)>,
    /// Sum of all free-extent lengths (`== free.iter().map(|e| e.1).sum()`).
    freed_bytes: u64,
}

impl Arena {
    /// Pool sized from `cfg.dram_pool_bytes`.
    pub fn new(cfg: &Config) -> Arena {
        Arena::with_capacity(cfg.dram_pool_bytes)
    }

    /// Pool sized explicitly (tests, engine sizing runs).
    pub fn with_capacity(bytes: u64) -> Arena {
        assert!(
            bytes <= 1 << 48,
            "dram pool must fit the 48-bit GVA Offset field"
        );
        Arena {
            inner: Mutex::new(Pool {
                buf: vec![0u8; bytes as usize],
                bump_high: 0,
                free: Vec::new(),
                freed_bytes: 0,
            }),
            cap: bytes,
        }
    }
}

impl DramArena for Arena {
    fn reserve(&self, len: u64) -> Result<u64, ArenaFull> {
        let mut p = self.inner.lock().expect("dram arena poisoned");
        if len == 0 {
            // Zero-byte reservation: stable offset, no bytes ever touched.
            return Ok(p.bump_high);
        }
        // 1) first-fit over the coalesced free list (lowest offset that fits).
        for i in 0..p.free.len() {
            let (off, elen) = p.free[i];
            if elen >= len {
                p.free.remove(i);
                if elen > len {
                    // Split: keep the remainder as a hole at the same place
                    // (list stays offset-sorted; coalescing invariant holds
                    // because neighbors were non-adjacent before the split).
                    p.free.insert(i, (off + len, elen - len));
                    p.freed_bytes -= len;
                } else {
                    p.freed_bytes -= elen;
                }
                return Ok(off);
            }
        }
        // 2) bump allocation (no overflow: bump_high <= cap invariant).
        if self.cap - p.bump_high >= len {
            let off = p.bump_high;
            p.bump_high += len;
            return Ok(off);
        }
        // Neither the free list nor the bump region can satisfy: DRAM-full.
        // Caller implements the placement spill path (OQ-6: put → SSD, scope
        // flips to SSD, metrics.dram_spill_puts++).
        Err(ArenaFull)
    }

    fn write(&self, off: u64, buf: &[u8]) {
        let mut p = self.inner.lock().expect("dram arena poisoned");
        let end = off + buf.len() as u64;
        assert!(
            end <= self.cap,
            "arena write out of bounds: off={off} len={} cap={}",
            buf.len(),
            self.cap
        );
        p.buf[off as usize..end as usize].copy_from_slice(buf);
    }

    fn read(&self, off: u64, len: u64, out: &mut [u8]) {
        let p = self.inner.lock().expect("dram arena poisoned");
        let end = off + len;
        assert!(
            end <= self.cap,
            "arena read out of bounds: off={off} len={len} cap={}",
            self.cap
        );
        assert!(
            out.len() as u64 >= len,
            "arena read: output buffer smaller than requested length"
        );
        out[..len as usize].copy_from_slice(&p.buf[off as usize..end as usize]);
    }

    fn free_extent(&self, off: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut p = self.inner.lock().expect("dram arena poisoned");
        debug_assert!(
            off + len <= p.bump_high,
            "free_extent of a range never allocated (off={off} len={len} bump={})",
            p.bump_high
        );
        // Insert keeping offset order; neighbors must not overlap (guards a
        // double-free from corrupting the coalescing invariant).
        let idx = p.free.partition_point(|&(o, _)| o < off);
        debug_assert!(
            idx == 0 || p.free[idx - 1].0 + p.free[idx - 1].1 <= off,
            "free_extent overlaps/double-frees the previous extent"
        );
        debug_assert!(
            idx == p.free.len() || off + len <= p.free[idx].0,
            "free_extent overlaps/double-frees the next extent"
        );
        p.free.insert(idx, (off, len));
        p.freed_bytes += len;
        // Coalesce with the left neighbor(s) (cascade), then the right one(s).
        let mut i = idx;
        while i > 0 && p.free[i - 1].0 + p.free[i - 1].1 == p.free[i].0 {
            let (o, l0) = p.free[i - 1];
            let l1 = p.free[i].1;
            p.free[i - 1] = (o, l0 + l1);
            p.free.remove(i);
            i -= 1;
        }
        while i + 1 < p.free.len() && p.free[i].0 + p.free[i].1 == p.free[i + 1].0 {
            let o = p.free[i].0;
            let l0 = p.free[i].1;
            let l1 = p.free[i + 1].1;
            p.free[i] = (o, l0 + l1);
            p.free.remove(i + 1);
        }
    }

    fn used_bytes(&self) -> u64 {
        let p = self.inner.lock().expect("dram arena poisoned");
        debug_assert!(p.freed_bytes <= p.bump_high, "free-list accounting corrupt");
        p.bump_high.saturating_sub(p.freed_bytes)
    }

    fn capacity(&self) -> u64 {
        self.cap
    }
}
