//! FC-GVA — Global Virtual Addressing subsystem (Phase 1, ARCHITECTURE §1).
//! Submodules:
//! * [`arena`]     — pre-allocated DRAM pool (`DramArena` trait impl)
//! * [`index`]     — fixed-capacity lock-free Level-1 index (T2/T3/T21)
//! * [`placement`] — frozen write-time precedence (T4/T5, OQ-19)
//! * [`reclaim`]   — session-scope registry + watermark reclamation (T6)
//! LOCK ORDER (single source of truth, see `reclaim` module docs):
//! registry → {index, arena, layout}; never the reverse. No lock is held
//! across physical file I/O (ARCHITECTURE §3 invariant).
//! GVA encode/decode itself  is the frozen [`crate::types::Gva`].

pub mod arena;
pub mod index;
pub mod placement;
pub mod reclaim;
