//! FlatCake platform layer: runtime registry, connector contract, telemetry,
//! and the local control service. The engine remains the sole owner of cache
//! storage and I/O; this layer only coordinates and observes.

pub mod connector;
pub mod project;
pub mod registry;
pub mod service;
pub use service::ServiceState;
pub mod compat;
pub mod adapters;
pub mod namespace;
pub mod http_api;
pub mod telemetry;

pub const PROTOCOL_VERSION: u32 = 1;
