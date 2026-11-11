//! Engine API shared by the FlatCake engine and the tiered baseline (orchestrator-owned).
//! The replay driver talks to both systems exclusively through `KvEngine`.

use crate::types::{BlockKey, Metrics};
use std::io;

/// One put record = one turn's new blocks, layer-major (layer l block i is the
/// (l*n_blk + i)-th block). Payloads are generated deterministically inside engines:
/// `util::payload_fill(session, layer, start_blk + i*block_tokens, seed, buf)`.
#[derive(Clone, Debug)]
pub struct PutSpec {
    pub session: u64,
    pub turn: u64,
    pub layers: u32,
    pub start_blk: u64,
    pub n_blk: u32,
    pub shared: u32,
    pub reply_tokens: u64,
    pub ctx_tokens: u64,
    pub seed: u64,
}

impl PutSpec {
    pub fn block_key(&self, layer: u32, i: u32, block_tokens: u64) -> BlockKey {
        BlockKey {
            session: self.session,
            layer,
            token_start: (self.start_blk + i as u64) * block_tokens,
        }
    }
    pub fn n_blocks(&self) -> u64 {
        self.layers as u64 * self.n_blk as u64
    }
}

/// One get request: blocks in layer-major order (`seed_by_block` is parallel — the
/// seed of the put that created each block; used for recompute re-persist; replay
/// does integrity verification independently). `signals` are the requesting
/// session's write-time signals, used ONLY for recompute re-persist placement
/// when the block's scope was reclaimed (ARCHITECTURE §8 symmetric miss model).
#[derive(Clone, Debug)]
pub struct GetSpec {
    pub id: u64,
    pub arrival_ts_us: u64,
    pub blocks: Vec<BlockKey>,
    pub seed_by_block: Vec<u64>,
    pub signals: crate::types::PlacementSignals,
}

/// Uniform engine interface ( FlatCake engine + baseline).
pub trait KvEngine: Send + Sync {
    fn put_batch(&self, spec: &PutSpec) -> io::Result<()>;
    /// Deliver each requested block's bytes to `sink(idx, bytes)` (idx = position in
    /// spec.blocks). Hits/misses/latencies are recorded in engine Metrics.
    fn get_request(&self, spec: &GetSpec, sink: &mut dyn FnMut(usize, &[u8])) -> io::Result<()>;
    fn snapshot(&self) -> Metrics;
    /// Bytes currently held in engine-owned RAM (DRAM pool + hbm window + staging).
    fn mem_used(&self) -> u64;
    fn shutdown(&self);
}
