//! Runtime-specific adapters (P2): each runtime's actual upstream integration
//! mechanism, documented honestly. An adapter declares what the upstream API
//! supports; unsupported operations return `Err(AdapterError::Unsupported)`
//! rather than silently degrading.

/// What the upstream runtime's API actually permits FlatCake to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeOp {
    /// Offload KV blocks to FlatCake-managed storage.
    Offload,
    /// Retrieve previously offloaded KV blocks.
    Retrieve,
    /// Release blocks (FlatCake reclaims storage).
    Release,
    /// Cancel an in-flight offload/retrieve.
    Cancel,
    /// Inspect cache contents (block ids, metadata) without transferring data.
    Inspect,
    /// Read whole-sequence cache state (llama.cpp style, not block-level).
    WholeSeqSerialize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdapterError {
    /// The runtime's API does not expose this operation at all.
    Unsupported(&'static str),
    /// The operation exists but FlatCake cannot integrate with this variant
    /// (e.g. quantized cache, sliding-window attention with incompatible layout).
    Incompatible(String),
    /// The runtime process is not reachable or has crashed.
    Unreachable(String),
    /// A data transfer failed mid-flight.
    TransferError(String),
    /// Compatibility check failed (different model, dtype, etc.).
    IncompatibleCache(String),
    /// The operation timed out.
    Timeout(String),
}

/// Per-runtime capability and integration-mechanism declaration.
/// This is documentation-as-code: each adapter's flags are verified against
/// the upstream API's actual extension points (see docs/inference-runtime-integrations.md
/// for the evidence and tested version).
#[derive(Debug, Clone)]
pub struct RuntimeAdapter {
    pub info: super::connector::ConnectorInfo,
    pub compat_template: super::compat::CacheCompat,
    ops: std::collections::HashSet<RuntimeOp>,
    /// The actual upstream API mechanism used for integration.
    pub mechanism: &'static str,
    /// Tested upstream version (empty = not tested against a real runtime).
    pub tested_version: String,
}

impl RuntimeAdapter {
    pub fn supports(&self, op: RuntimeOp) -> bool {
        self.ops.contains(&op)
    }

    pub fn reject(&self, op: RuntimeOp) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported(match op {
            RuntimeOp::Offload => "runtime does not expose a block-level offload API",
            RuntimeOp::Retrieve => "runtime does not expose a block-level retrieve API",
            RuntimeOp::Release => "runtime does not expose per-block release",
            RuntimeOp::Cancel => "runtime does not expose cancellation",
            RuntimeOp::Inspect => "runtime does not expose cache inspection",
            RuntimeOp::WholeSeqSerialize => "runtime does not expose whole-sequence serialization",
        }))
    }
}

/// vLLM: KVConnectorBase (v0.6+), block-level KV offload, per-layer tensors.
/// Upstream: vllm/distributed/kv_connector.py, KVConnectorBase_V1.
pub fn vllm_adapter(version: &str) -> RuntimeAdapter {
    RuntimeAdapter {
        info: super::connector::ConnectorInfo {
            name: "vllm".into(),
            version: version.into(),
            protocol_version: super::PROTOCOL_VERSION,
            capabilities: super::connector::caps::STATUS
                | super::connector::caps::CACHE_INSPECT
                | super::connector::caps::EXTERNAL_STORAGE
                | super::connector::caps::BLOCK_ACCESS
                | super::connector::caps::OFFLOAD
                | super::connector::caps::RESTORE,
        },
        compat_template: super::compat::CacheCompat {
            model_arch: "".into(), // filled by caller
            model_id: "".into(),
            model_revision: "".into(),
            tokenizer_hash: 0,
            attention_kind: "".into(),
            num_kv_heads: 0,
            head_dim: 0,
            num_layers: 0,
            rope_kind: "".into(),
            rope_theta: 0,
            dtype: "".into(), // f16/bf16/fp8 depending on config
            quant_format: "".into(),
            tensor_layout: "vllm_block".into(),
            block_size_tokens: 16, // vLLM default block_size=16
            adapter_id: "".into(),
            runtime: "vllm".into(),
            runtime_version: version.into(),
        },
        ops: [
            RuntimeOp::Offload,
            RuntimeOp::Retrieve,
            RuntimeOp::Release,
            RuntimeOp::Cancel,
            RuntimeOp::Inspect,
        ]
        .into(),
        mechanism: "KVConnectorBase_V1 subclass (vllm/distributed/kv_connector.py); block-level get/save via KVCacheFormat; per-layer per-head tensors",
        tested_version: String::new(), // NOT TESTED: vLLM not installed on this host
    }
}

/// SGLang: HiCacheBackend (hierarchical cache), token-level tree, per-layer tensors.
/// Upstream: sglang/srt/mem_cache/, HiRadixCache + hierarchical storage.
pub fn sglang_adapter(version: &str) -> RuntimeAdapter {
    RuntimeAdapter {
        info: super::connector::ConnectorInfo {
            name: "sglang".into(),
            version: version.into(),
            protocol_version: super::PROTOCOL_VERSION,
            capabilities: super::connector::caps::STATUS
                | super::connector::caps::CACHE_INSPECT
                | super::connector::caps::EXTERNAL_STORAGE
                | super::connector::caps::OFFLOAD
                | super::connector::caps::RESTORE,
        },
        compat_template: super::compat::CacheCompat {
            model_arch: "".into(),
            model_id: "".into(),
            model_revision: "".into(),
            tokenizer_hash: 0,
            attention_kind: "".into(),
            num_kv_heads: 0,
            head_dim: 0,
            num_layers: 0,
            rope_kind: "".into(),
            rope_theta: 0,
            dtype: "".into(),
            quant_format: "".into(),
            tensor_layout: "sglang_tree".into(),
            block_size_tokens: 0, // token-level tree, not fixed blocks
            adapter_id: "".into(),
            runtime: "sglang".into(),
            runtime_version: version.into(),
        },
        ops: [
            RuntimeOp::Offload,
            RuntimeOp::Retrieve,
            RuntimeOp::Release,
            RuntimeOp::Inspect,
        ]
        .into(),
        mechanism: "HiCacheBackend / HiRadixCache hierarchical storage; token-level tree with per-layer KV tensors",
        tested_version: String::new(),
    }
}

/// llama.cpp: NO block-level KV API. Only whole-sequence serialization via
/// llama_state_seq_get_data/save_data (LLAMA_TYPE_K or prompt-cache files).
/// Upstream: llama.h, llama_kv_cache functions.
pub fn llama_cpp_adapter(version: &str) -> RuntimeAdapter {
    RuntimeAdapter {
        info: super::connector::ConnectorInfo {
            name: "llama.cpp".into(),
            version: version.into(),
            protocol_version: super::PROTOCOL_VERSION,
            capabilities: super::connector::caps::STATUS,
        },
        compat_template: super::compat::CacheCompat {
            model_arch: "".into(),
            model_id: "".into(),
            model_revision: "".into(),
            tokenizer_hash: 0,
            attention_kind: "".into(),
            num_kv_heads: 0,
            head_dim: 0,
            num_layers: 0,
            rope_kind: "".into(),
            rope_theta: 0,
            dtype: "f16".into(), // llama.cpp KV cache is always f16
            quant_format: "gguf".into(),
            tensor_layout: "llama_seq".into(),
            block_size_tokens: 0, // whole-sequence, not block-level
            adapter_id: "".into(),
            runtime: "llama.cpp".into(),
            runtime_version: version.into(),
        },
        ops: [RuntimeOp::WholeSeqSerialize].into(),
        mechanism: "llama_state_seq_get_data/save_data for whole-sequence serialization; no block-level KV API",
        tested_version: String::new(),
    }
}

/// Hugging Face Transformers: Cache API (DynamicCache, OffloadedCache).
/// OffloadedCache already supports CPU offloading with layer-by-layer prefetch.
/// Upstream: transformers/cache_utils.py.
pub fn transformers_adapter(version: &str) -> RuntimeAdapter {
    RuntimeAdapter {
        info: super::connector::ConnectorInfo {
            name: "transformers".into(),
            version: version.into(),
            protocol_version: super::PROTOCOL_VERSION,
            capabilities: super::connector::caps::STATUS
                | super::connector::caps::EXTERNAL_STORAGE
                | super::connector::caps::OFFLOAD,
        },
        compat_template: super::compat::CacheCompat {
            model_arch: "".into(),
            model_id: "".into(),
            model_revision: "".into(),
            tokenizer_hash: 0,
            attention_kind: "".into(),
            num_kv_heads: 0,
            head_dim: 0,
            num_layers: 0,
            rope_kind: "".into(),
            rope_theta: 0,
            dtype: "".into(),
            quant_format: "".into(),
            tensor_layout: "hf_tuple".into(), // (key, value) tuples per layer
            block_size_tokens: 0,             // token-level, not block-based
            adapter_id: "".into(),
            runtime: "transformers".into(),
            runtime_version: version.into(),
        },
        ops: [RuntimeOp::Offload, RuntimeOp::Retrieve, RuntimeOp::Inspect].into(),
        mechanism: "Custom Cache subclass (transformers/cache_utils.py); per-layer (key, value) tensors; OffloadedCache pattern for CPU offload",
        tested_version: String::new(),
    }
}

/// All built-in adapter constructors.
pub fn all_adapters() -> Vec<(&'static str, RuntimeAdapter)> {
    vec![
        ("vllm", vllm_adapter("untested")),
        ("sglang", sglang_adapter("untested")),
        ("llama.cpp", llama_cpp_adapter("untested")),
        ("transformers", transformers_adapter("untested")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vllm_supports_block_offload_and_retrieve() {
        let a = vllm_adapter("0.6.3");
        assert!(a.supports(RuntimeOp::Offload));
        assert!(a.supports(RuntimeOp::Retrieve));
        assert!(a.supports(RuntimeOp::Inspect));
        assert!(!a.supports(RuntimeOp::WholeSeqSerialize));
        assert!(a.info.supports(super::super::connector::caps::BLOCK_ACCESS));
    }

    #[test]
    fn llama_cpp_only_supports_whole_seq() {
        let a = llama_cpp_adapter("b1234");
        assert!(a.supports(RuntimeOp::WholeSeqSerialize));
        assert!(!a.supports(RuntimeOp::Offload));
        assert!(!a.supports(RuntimeOp::Retrieve));
        assert!(!a.info.supports(super::super::connector::caps::BLOCK_ACCESS));
        // Attempting unsupported ops returns a clear rejection.
        let err = a.reject(RuntimeOp::Offload).unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[test]
    fn sglang_has_no_block_size_token_tree_layout() {
        let a = sglang_adapter("0.3");
        assert_eq!(a.compat_template.block_size_tokens, 0);
        assert_eq!(a.compat_template.tensor_layout, "sglang_tree");
    }

    #[test]
    fn all_adapters_declare_status_capability() {
        for (name, a) in all_adapters() {
            assert!(a.info.supports(super::super::connector::caps::STATUS), "{name}");
            assert!(a.info.validate().is_ok(), "{name}");
        }
    }

    #[test]
    fn tested_version_is_empty_for_all_untested_runtimes() {
        for (name, a) in all_adapters() {
            assert!(a.tested_version.is_empty(), "{name} claims tested but no runtime is installed");
        }
    }
}
