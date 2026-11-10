//! fckv library — FlatCake Level-B prototype.
//! Module map in docs/ARCHITECTURE.md §1. Frozen contracts: types.rs, config.rs, util.rs.

pub mod baseline;
pub mod config;
pub mod engine_api;
pub mod hbm;
pub mod types;
pub mod util;
pub mod workload;
pub mod replay;

// Component modules (added as their owning agents complete them):
// pub mod gva;
// pub mod mas;
// pub mod cio;
