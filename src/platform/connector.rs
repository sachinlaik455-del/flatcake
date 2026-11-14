//! Connector contract (P1): a versioned, capability-based interface between
//! an external runtime and the platform. A connector declares what it can do;
//! the platform never assumes direct KV access unless the capability is
//! declared AND the underlying runtime actually provides it.

/// Wire protocol version. Connectors must match to register.
pub const PROTOCOL_VERSION: u32 = 1;

/// Declared connector capabilities (bit flags).
pub mod caps {
    pub const STATUS: u64 = 1 << 0;
    pub const CACHE_INSPECT: u64 = 1 << 1;
    pub const EXTERNAL_STORAGE: u64 = 1 << 2;
    pub const BLOCK_ACCESS: u64 = 1 << 3;
    pub const OFFLOAD: u64 = 1 << 4;
    pub const RESTORE: u64 = 1 << 5;
    pub const ASYNC_OPS: u64 = 1 << 6;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorInfo {
    /// Stable connector identity, e.g. "sglang", "llama-cpp", "custom-agent".
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    /// Bit mask of `caps::` constants.
    pub capabilities: u64,
}

impl ConnectorInfo {
    pub fn supports(&self, cap: u64) -> bool {
        self.capabilities & cap == cap
    }

    /// Registration check: protocol must match exactly; a connector must at
    /// minimum declare status reporting.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("connector name is empty".into());
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(format!(
                "protocol mismatch: connector {} != platform {}",
                self.protocol_version, PROTOCOL_VERSION
            ));
        }
        if self.capabilities & caps::STATUS == 0 {
            return Err("connector must declare the STATUS capability".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded,
    Unreachable,
}
