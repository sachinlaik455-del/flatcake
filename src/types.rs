//! FROZEN shared contract (shared contract).
//! Modules implemented by separate agents build against these types.

/// MediaType field values (paper §3.2: 0=DRAM, 1=SSD, encodings >=2 reserved).
pub const MEDIA_DRAM: u64 = 0;
pub const MEDIA_SSD: u64 = 1;

/// 64-bit Global Virtual Address (paper §3.2, verified verbatim):
/// MediaType [63:60] | NodeID [59:48] | Offset [47:0].
/// DRAM addresses: GVA < 2^60. SSD addresses: GVA >= 2^60.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Gva(pub u64);

/// SSD offset packing (ARCHITECTURE §6): off[47:0] = file_seq[47:28] | in_file_off[27:0].
/// gpu_file_bytes must therefore be <= 2^28 (256 MiB) and a power of two.
pub const SSD_IN_FILE_BITS: u32 = 28;
pub const SSD_FILE_MAX: u64 = 1 << SSD_IN_FILE_BITS;

impl Gva {
    /// Returns None on invalid field values (media >= 2, node > 0xFFF, offset > 2^48-1).
    pub fn encode(media: u64, node: u64, offset: u64) -> Option<Gva> {
        if media > MEDIA_SSD || node > 0xFFF || offset > 0x0000_FFFF_FFFF_FFFF {
            return None;
        }
        Some(Gva(media << 60 | node << 48 | offset))
    }
    pub fn media(self) -> u64 {
        self.0 >> 60
    }
    pub fn node(self) -> u64 {
        (self.0 >> 48) & 0xFFF
    }
    pub fn offset(self) -> u64 {
        self.0 & 0x0000_FFFF_FFFF_FFFF
    }
    /// Single-integer-comparison medium classification (paper §3.2).
    pub fn is_ssd(self) -> bool {
        self.0 >= 1 << 60
    }
}

/// Logical block identity (paper §3.2: session ID, layer, token range).
/// token_start = start_blk * block_tokens .
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlockKey {
    pub session: u64,
    pub layer: u32,
    pub token_start: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Medium {
    Dram,
    Ssd,
}

/// Level-1 index lookup outcome. Miss|Pending are both counted as misses
/// by the engine (publish-after-write, T3); Pending is counted separately.
#[derive(Clone, Debug)]
pub enum Lookup {
    Miss,
    Pending,
    Hit(Gva),
}

/// Write-time placement signals (paper Table 2 / ARCHITECTURE §4 precedence).
#[derive(Clone, Copy, Debug)]
pub struct PlacementSignals {
    pub session: u64,
    pub refcount: u32,
    pub turn: u64,
    pub reply_tokens: u64,
    pub ctx_tokens: u64,
}

/// A scheduler request (paper §3.3): classified by max_j GVA_j.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: u64,
    pub session: u64,
    pub arrival_ts_us: u64,
    pub blocks: Vec<Gva>,
    pub size_bytes: u64,
}

impl Request {
    /// Paper §3.3: classify by slowest required medium (single integer comparison).
    pub fn is_ssd_bound(&self) -> bool {
        self.blocks.iter().any(|g| g.is_ssd())
    }
}

/// DRAM arena interface implemented by `gva::arena`, consumed by engine/baseline.
/// Offsets are 48-bit-safe byte offsets (fit GVA Offset field).
pub trait DramArena: Send + Sync {
    /// Reserve `len` bytes; Err(ArenaFull) triggers the DRAM-full spill path .
    fn reserve(&self, len: u64) -> Result<u64, ArenaFull>;
    fn write(&self, off: u64, buf: &[u8]);
    fn read(&self, off: u64, len: u64, out: &mut [u8]);
    /// Session-extent free (reclamation, T6). Zero payload ops on reclaim.
    fn free_extent(&self, off: u64, len: u64);
    fn used_bytes(&self) -> u64;
    fn capacity(&self) -> u64;
}

#[derive(Debug)]
pub struct ArenaFull;

/// SSD scope-file layout interface implemented by `cio::layout`.
pub trait SsdLayout: Send + Sync {
    /// Map an SSD GVA to (file_seq, in_file_offset, len) for direct I/O.
    fn resolve(&self, gva: Gva, len: u64) -> FileRange;
    /// O(1) metadata free . Must perform zero payload-byte operations.
    fn free_extent(&self, file_seq: u32, off: u64, len: u64);
    /// Logical bytes written (watermark denominator, ARCHITECTURE §5).
    fn logical_bytes(&self) -> u64;
    fn file_capacity(&self) -> u64;
}

#[derive(Clone, Copy, Debug)]
pub struct FileRange {
    pub file_seq: u32,
    pub in_file_off: u64,
    pub len: u64,
}

/// All benchmark counters (field names = results JSON keys, ARCHITECTURE §11).
#[derive(Default, Clone)]
pub struct Metrics {
    pub hit_hbm: u64,
    pub hit_dram: u64,
    pub hit_ssd: u64,
    pub misses: u64,
    pub pending_racing_gets: u64,
    pub requested_blocks: u64,
    pub logical_block_gets: u64,
    pub physical_read_ops: u64,
    pub physical_read_bytes: u64,
    pub logical_read_bytes: u64,
    pub physical_write_bytes: u64,
    pub logical_write_bytes: u64,
    pub merged_ranges: u64,
    pub bridged_gaps: u64,
    pub overread_bytes: u64,
    pub dram_spill_puts: u64,
    pub ssd_over_high_watermark_us: u64,
    pub modeled_recompute_us: u64,
    pub reclaim_events: u64,
    pub reclaim_payload_ops: u64,
    pub delivered_requests: u64,
    pub mem_high_water: u64,
    // P3 medium-aware migration counters (zero when migration_enabled = 0).
    pub migr_promotions: u64,
    pub migr_demotions: u64,
    pub migr_bytes_moved: u64,
    pub migr_cooldown_skips: u64,
    // P4 durability + coalescing counters.
    pub dedup_requests: u64,
    pub recovery_replayed: u64,
    pub recovery_truncated: u64,
    /// samples in microseconds (latency includes queue wait, ARCHITECTURE §9)
    pub lat_samples: Vec<u64>,
    pub qwait_samples: Vec<u64>,
    pub prep_samples: Vec<u64>,
}

impl Metrics {
    /// Delivered hit rate : delivered blocks / requested blocks.
    pub fn delivered_hit_rate(&self) -> f64 {
        let hits = self.hit_hbm + self.hit_dram + self.hit_ssd;
        let total = hits + self.misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }
    /// Physical bytes read / logical bytes requested (normative definition).
    pub fn read_amp(&self) -> f64 {
        if self.logical_read_bytes == 0 {
            0.0
        } else {
            self.physical_read_bytes as f64 / self.logical_read_bytes as f64
        }
    }
    /// Physical bytes written / logical bytes placed (normative definition).
    pub fn write_amp(&self) -> f64 {
        if self.logical_write_bytes == 0 {
            0.0
        } else {
            self.physical_write_bytes as f64 / self.logical_write_bytes as f64
        }
    }
    pub fn throughput_blocks_per_s(&self, elapsed_s: f64) -> f64 {
        if elapsed_s <= 0.0 {
            0.0
        } else {
            self.logical_block_gets as f64 / elapsed_s
        }
    }
    pub fn mem_high_water_bytes(&self) -> u64 {
        // set by engine/baseline via track_high_water
        self.mem_high_water
    }
    pub fn track_high_water(&mut self, bytes: u64) {
        if bytes > self.mem_high_water {
            self.mem_high_water = bytes;
        }
    }
}
