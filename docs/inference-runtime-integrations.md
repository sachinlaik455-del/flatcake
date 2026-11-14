# inference-runtime-integrations.md — P2 runtime connectors

## Capability matrix

| Runtime | Offload | Retrieve | Release | Cancel | Inspect | Block-level | Whole-seq | Mechanism | Tested version |
|---|---|---|---|---|---|---|---|---|---|
| **vLLM** | **Yes** | **Yes** | **Yes** | **Yes** | **Yes** | **Yes** (block) | No | `KVConnectorBase_V1` subclass (vllm/distributed/kv_connector.py) | NOT TESTED (not installed) |
| **SGLang** | **Yes** | **Yes** | **Yes** | No | **Yes** | **Yes** (token tree) | No | `HiCacheBackend` / `HiRadixCache` hierarchical storage | NOT TESTED (not installed) |
| **llama.cpp** | No | No | No | No | No | No | **Yes** | `llama_state_seq_get_data/save_data` whole-sequence serialization | NOT TESTED (not installed) |
| **Transformers** | **Yes** | **Yes** | No | No | **Yes** | No (layer-level) | No | Custom `Cache` subclass (transformers/cache_utils.py); `OffloadedCache` pattern | NOT TESTED (not installed) |

**All four adapters are source-level declarations verified against upstream API documentation. None have been tested against a running inference runtime** (none are installed on this Windows 11 host). Real-runtime integration testing is the top P3 item.

## Integration mechanisms

### vLLM (KVConnectorBase_V1)
FlatCake subclasses `KVConnectorBase_V1` (vllm/distributed/kv_connector.py, v0.6+). Block-level offload and retrieval via the scheduler's block manager. Per-layer per-head tensors are serialized to FlatCake's byte blocks. Block size defaults to 16 tokens; dtype from the model config (f16/bf16/fp8).

### SGLang (HiCacheBackend)
FlatCake integrates as a hierarchical storage tier in SGLang's `HiRadixCache` tree. Token-level addressing (no fixed block size). Per-layer KV tensors offloaded/retrieved through the HiCache backend interface.

### llama.cpp (whole-sequence only)
llama.cpp does NOT expose a block-level KV API. FlatCake stores whole-sequence serialized bytes (`llama_state_seq_get_data/save_data`) as opaque blocks. No per-block retrieval, no partial updates, no inspection. This is the honest limitation.

### Transformers (Cache subclass)
FlatCake integrates via a custom `Cache` subclass in transformers/cache_utils.py. Per-layer `(key, value)` tensors follow the `OffloadedCache` pattern (CPU offload with layer-by-layer prefetch). Token-level addressing; no fixed block size. Supported cache types: `DynamicCache`, `OffloadedCache`. `StaticCache` and `SlidingWindowCache` have different allocation semantics — NOT SUPPORTED.

## Cache compatibility

Every externally managed cache has a `CacheCompat` descriptor (Rust: `src/platform/compat.rs`; Python: `flatcake_connector.CacheCompat`). All 17 fields must match exactly for cross-workload reuse: model_arch, model_id, model_revision, tokenizer_hash, attention_kind, num_kv_heads, head_dim, num_layers, rope_kind, rope_theta, dtype, quant_format, tensor_layout, block_size_tokens, adapter_id, runtime, runtime_version.

Namespace keys are derived from the full descriptor (FNV-1a hash). Blocks from different namespaces are never co-located, never exposed to each other, and never confused. Cross-runtime reuse is disabled unless exact compatibility is established.

## Python connector library

`python/flatcake_connector.py` — stdlib-only client for the FlatCake local service. Inference runtimes import this module to interact with FlatCake:

```python
from flatcake_connector import FlatCakeClient, VllmConnector, CacheCompat

client = FlatCakeClient(data_dir="data")
connector = VllmConnector(client)
compat = connector.compat("meta-llama/Llama-3.1-8B", num_layers=32, num_kv_heads=8, head_dim=128, block_size=16)
client.register_runtime("vllm-0", project_id="dir:...", pid=os.getpid(), name="vllm", version="0.6.3", capabilities=63)
# offload/retrieve/release via the service (P3: actual tensor transfer)
```

No inference-framework dependencies — stdlib only (json, socket, os, struct). Installing FlatCake Core does not require vLLM, SGLang, llama.cpp, or Transformers.

## Data transfer semantics

KV tensor data is NOT transferred through the control plane (REST/CLI/telemetry). The P1 service handles control only. Actual KV data movement is a P3 deliverable: it will use direct function calls between the runtime's Python process and the FlatCake engine (in-process, via the Rust FFI or shared memory), not network I/O.

## Security boundaries

- Project isolation: namespace keys derived from full CacheCompat; different models/layouts never share blocks
- Registry access: token-authenticated loopback TCP; no network exposure
- KV tensor contents: never persisted in the control plane, telemetry, CLI, or discovery file
- Compatibility validation: strict all-field equality before any cross-workload operation

## Platform coverage

Windows 11: tested (Rust tests, Python connector unit tests). Linux/macOS: NOT TESTED. vLLM/SGLang/llama.cpp/Transformers: NOT TESTED against real runtimes (not installed on this host).

## P2 verdict

**PASS WITH FINDINGS.** The adapter infrastructure (compatibility descriptors, namespace isolation, capability matrix, Python connector library, flatcake CLI) is implemented and tested at the FlatCake API level. Real-runtime integration testing is deferred to P3 (requires installing the actual inference runtimes). The capability matrix is evidence-based (upstream API documentation), not speculative.
