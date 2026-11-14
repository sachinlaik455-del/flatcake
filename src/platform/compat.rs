//! Cache compatibility descriptor (P2): strict per-cache metadata that must
//! match exactly before any cross-workload cache reuse. Two workloads sharing
//! a model name is NEVER sufficient — every field must be equal.

/// Full compatibility descriptor for an externally managed KV cache.
/// All fields participate in equality; any mismatch disallows reuse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheCompat {
    /// Model architecture, e.g. "llama", "qwen2", "deepseek_v2".
    pub model_arch: String,
    /// Model identity: repo id or local path hash.
    pub model_id: String,
    /// Model revision (commit sha, tag, or "local").
    pub model_revision: String,
    /// Tokenizer/token identity hash (tokens must match for KV reuse).
    pub tokenizer_hash: u64,
    /// Attention configuration: "mha", "gqa", "mqa", "mla", "sliding_window".
    pub attention_kind: String,
    /// Number of KV heads (must match for per-head layout compatibility).
    pub num_kv_heads: u32,
    /// Head dimension.
    pub head_dim: u32,
    /// Number of layers.
    pub num_layers: u32,
    /// Positional encoding: "rope", "alibi", "none", "yarn", "longrope".
    pub rope_kind: String,
    /// RoPE base theta (must match for rotary position encoding).
    pub rope_theta: u64,
    /// KV tensor dtype string: "f16", "bf16", "f32", "q8_0", "q4_0", etc.
    pub dtype: String,
    /// Quantization format: "none", "awq", "gptq", "gguf", "fp8", etc.
    pub quant_format: String,
    /// Tensor layout: "vllm_block", "hf_tuple", "llama_seq", "sglang_tree".
    pub tensor_layout: String,
    /// Block size in tokens (0 for non-block-based layouts).
    pub block_size_tokens: u32,
    /// Adapter/LoRA identity (empty = base model).
    pub adapter_id: String,
    /// Runtime that produced the cache: "vllm", "sglang", "llama.cpp", "transformers".
    pub runtime: String,
    /// Runtime version string.
    pub runtime_version: String,
}

impl CacheCompat {
    /// Namespace key: a stable hash of all fields for registry lookup.
    pub fn namespace_key(&self) -> u64 {
        let mut bytes = Vec::new();
        for s in [
            &self.model_arch, &self.model_id, &self.model_revision,
            &self.attention_kind, &self.rope_kind, &self.dtype,
            &self.quant_format, &self.tensor_layout, &self.adapter_id,
            &self.runtime, &self.runtime_version,
        ] {
            bytes.extend_from_slice(s.as_bytes());
            bytes.push(0);
        }
        for v in [
            self.tokenizer_hash, self.num_kv_heads as u64, self.head_dim as u64,
            self.num_layers as u64, self.rope_theta, self.block_size_tokens as u64,
        ] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        crate::util::fnv1a64(&bytes)
    }

    /// Strict equality check with a human-readable mismatch report.
    pub fn check_compatible(&self, other: &CacheCompat) -> Result<(), Vec<String>> {
        let mut diffs = Vec::new();
        macro_rules! cmp {
            ($field:ident) => {
                if self.$field != other.$field {
                    diffs.push(format!("{}: {:?} != {:?}", stringify!($field), self.$field, other.$field));
                }
            };
        }
        cmp!(model_arch); cmp!(model_id); cmp!(model_revision);
        cmp!(tokenizer_hash); cmp!(attention_kind); cmp!(num_kv_heads);
        cmp!(head_dim); cmp!(num_layers); cmp!(rope_kind); cmp!(rope_theta);
        cmp!(dtype); cmp!(quant_format); cmp!(tensor_layout);
        cmp!(block_size_tokens); cmp!(adapter_id); cmp!(runtime); cmp!(runtime_version);
        if diffs.is_empty() { Ok(()) } else { Err(diffs) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(overrides: &[(&str, String)]) -> CacheCompat {
        let mut c = CacheCompat {
            model_arch: "llama".into(),
            model_id: "meta-llama/Llama-3.1-8B-Instruct".into(),
            model_revision: "abc123".into(),
            tokenizer_hash: 42,
            attention_kind: "gqa".into(),
            num_kv_heads: 8,
            head_dim: 128,
            num_layers: 32,
            rope_kind: "rope".into(),
            rope_theta: 500000,
            dtype: "bf16".into(),
            quant_format: "none".into(),
            tensor_layout: "vllm_block".into(),
            block_size_tokens: 16,
            adapter_id: "".into(),
            runtime: "vllm".into(),
            runtime_version: "0.6.3".into(),
        };
        for (field, val) in overrides {
            match *field {
                "model_arch" => c.model_arch = val.clone(),
                "model_id" => c.model_id = val.clone(),
                "dtype" => c.dtype = val.clone(),
                "block_size_tokens" => c.block_size_tokens = val.parse().unwrap(),
                _ => {}
            }
        }
        c
    }

    #[test]
    fn identical_descriptors_compatible() {
        let a = sample(&[]);
        let b = sample(&[]);
        assert!(a.check_compatible(&b).is_ok());
        assert_eq!(a.namespace_key(), b.namespace_key());
    }

    #[test]
    fn dtype_mismatch_rejects() {
        let a = sample(&[]);
        let b = sample(&[("dtype", "f16".into())]);
        let errs = a.check_compatible(&b).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("dtype")));
    }

    #[test]
    fn block_size_mismatch_rejects() {
        let a = sample(&[]);
        let b = sample(&[("block_size_tokens", "32".into())]);
        assert!(a.check_compatible(&b).is_err());
    }

    #[test]
    fn namespace_keys_differ_for_different_models() {
        let a = sample(&[]);
        let b = sample(&[("model_id", "different/model".into())]);
        assert_ne!(a.namespace_key(), b.namespace_key());
    }
}
