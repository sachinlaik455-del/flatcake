//! FC-MAS — Medium-Aware Request Scheduler (paper §3.3, T7–T10 control plane):
//! dual queues (DRAM-Ready / SSD-Preparing), HRRN Eq. 1 prefetch priority
//! (OQ-4/15), arrival-ordered re-admission (T9), bounded backpressure.
//!
//! Pure control plane: no file I/O, no payload handling. The engine
//! (orchestrator) spawns worker threads that consume what
//! [`Scheduler::pop_dram`] / [`Scheduler::pick_ssd`] return — DRAM workers
//! drive DRAM-scope dispatch, SSD workers drive FC-CIO prefetch of
//! SSD-resident blocks and hand completed requests back via
//! [`Scheduler::readmit`].
#![allow(dead_code)]

pub mod scheduler;

pub use scheduler::{hrrn_priority, Clock, PoppedReq, Scheduler, SchedulerStats};

use crate::types::Request;

/// T7 request classification (paper §3.3, OQ-14): a request is SSD-bound iff
/// any block is SSD-resident — `max_j GVA_j >= 2^60` — including hybrid
/// requests that mix a DRAM-pinned prefix with SSD-resident history (their
/// DRAM blocks stay pinned in place). Returns `true` for the SSD-Preparing
/// queue, `false` for the DRAM-Ready queue. The caller (engine) classifies
/// once and passes the result as `Scheduler::enqueue`'s `ssd` flag.
pub fn classify(req: &Request) -> bool {
    req.is_ssd_bound()
}
