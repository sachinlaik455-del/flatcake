//! Namespace isolation (P2): cache data is keyed by (namespace_key, block_id)
//! so that two workloads sharing a runtime can never read each other's blocks.
//! Cross-runtime reuse requires an exact CacheCompat match; namespace keys
//! derived from the full compatibility descriptor enforce this.

use super::compat::CacheCompat;
use std::collections::HashMap;
use std::sync::Mutex;

/// Per-namespace block registry. Each namespace is a separate FlatCake
/// project_id (derived from the CacheCompat namespace_key), ensuring that
/// blocks from different models/layouts are never co-located or confused.
pub struct NamespaceRegistry {
    namespaces: Mutex<HashMap<u64, NamespaceMeta>>,
}

#[derive(Debug, Clone)]
pub struct NamespaceMeta {
    pub compat: CacheCompat,
    pub block_count: u64,
    pub created_at: u64,
}

impl NamespaceRegistry {
    pub fn new() -> Self {
        NamespaceRegistry {
            namespaces: Mutex::new(HashMap::new()),
        }
    }

    /// Register a namespace. Fails if an existing namespace has the same
    /// namespace_key but a different CacheCompat (hash collision guard).
    pub fn register(&self, compat: &CacheCompat) -> Result<u64, String> {
        let key = compat.namespace_key();
        let mut m = self.namespaces.lock().unwrap();
        if let Some(existing) = m.get(&key) {
            if existing.compat != *compat {
                return Err(format!(
                    "namespace collision: hash {} maps to a different CacheCompat (possible FNV collision)",
                    key
                ));
            }
            return Ok(key); // idempotent
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        m.insert(key, NamespaceMeta {
            compat: compat.clone(),
            block_count: 0,
            created_at: now,
        });
        Ok(key)
    }

    /// Look up a namespace by key; returns the CacheCompat for validation.
    pub fn lookup(&self, key: u64) -> Option<CacheCompat> {
        self.namespaces.lock().unwrap().get(&key).map(|m| m.compat.clone())
    }

    /// Increment the block count for a namespace (called on placement).
    pub fn add_block(&self, key: u64) -> bool {
        let mut m = self.namespaces.lock().unwrap();
        match m.get_mut(&key) {
            Some(meta) => { meta.block_count += 1; true }
            None => false,
        }
    }

    pub fn block_count(&self, key: u64) -> u64 {
        self.namespaces.lock().unwrap().get(&key).map(|m| m.block_count).unwrap_or(0)
    }

    pub fn namespace_count(&self) -> usize {
        self.namespaces.lock().unwrap().len()
    }
}

use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::compat::CacheCompat;

    fn compat(model: &str, dtype: &str) -> CacheCompat {
        CacheCompat {
            model_arch: "llama".into(),
            model_id: model.into(),
            model_revision: "abc".into(),
            tokenizer_hash: 1,
            attention_kind: "gqa".into(),
            num_kv_heads: 8,
            head_dim: 128,
            num_layers: 32,
            rope_kind: "rope".into(),
            rope_theta: 500000,
            dtype: dtype.into(),
            quant_format: "none".into(),
            tensor_layout: "vllm_block".into(),
            block_size_tokens: 16,
            adapter_id: "".into(),
            runtime: "vllm".into(),
            runtime_version: "0.6.3".into(),
        }
    }

    #[test]
    fn same_model_same_namespace() {
        let r = NamespaceRegistry::new();
        let k1 = r.register(&compat("model/a", "bf16")).unwrap();
        let k2 = r.register(&compat("model/a", "bf16")).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(r.namespace_count(), 1);
    }

    #[test]
    fn different_dtype_different_namespace() {
        let r = NamespaceRegistry::new();
        let k1 = r.register(&compat("model/a", "bf16")).unwrap();
        let k2 = r.register(&compat("model/a", "f16")).unwrap();
        assert_ne!(k1, k2);
        assert_eq!(r.namespace_count(), 2);
    }

    #[test]
    fn block_count_tracks_per_namespace() {
        let r = NamespaceRegistry::new();
        let k1 = r.register(&compat("model/a", "bf16")).unwrap();
        let k2 = r.register(&compat("model/b", "bf16")).unwrap();
        r.add_block(k1);
        r.add_block(k1);
        r.add_block(k2);
        assert_eq!(r.block_count(k1), 2);
        assert_eq!(r.block_count(k2), 1);
    }
}
