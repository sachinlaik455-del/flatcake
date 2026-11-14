"""flatcake_connector — Python client library for the FlatCake platform service.

Inference runtimes (vLLM, SGLang, llama.cpp, Transformers) import this module
to register, offload KV blocks to FlatCake-managed storage, retrieve them,
and release them. All data transfers use the local TCP service; KV tensor
contents never cross the control plane.

Usage (from any inference runtime):
    from flatcake_connector import FlatCakeClient

    client = FlatCakeClient()
    client.register(runtime_id="vllm-0", project_id="dir:...", name="vllm")
    client.offload_blocks(compat=..., block_ids=[...], payloads=[...])
    payloads = client.retrieve_blocks(compat=..., block_ids=[...])
    client.release_blocks(compat=..., block_ids=[...])

Requires: FlatCake service running (`flatcake serve` or equivalent).
No inference-framework dependencies — stdlib only.

This module is a thin client over the local service's JSON-line protocol.
It does NOT duplicate FlatCake's storage logic; it is a remote procedure
call wrapper for the same operations the engine's internal API exposes.
"""

import json
import os
import socket
import struct
from typing import Optional

PROTOCOL_VERSION = 1
MAX_RESPONSE = 1024 * 1024
DEFAULT_TIMEOUT_S = 10


class FlatCakeError(Exception):
    """FlatCake service returned an error."""

    def __init__(self, message: str, method: str = ""):
        self.method = method
        super().__init__(f"flatcake {method}: {message}" if method else f"flatcake: {message}")


class ServiceNotRunning(FlatCakeError):
    def __init__(self):
        super().__init__("FlatCake service is not running (start with `flatcake serve`)")


class CompatibilityError(FlatCakeError):
    """Cache compatibility check failed (different model, dtype, etc.)."""

    def __init__(self, diffs: list):
        super().__init__("compatibility mismatch: " + "; ".join(diffs))


class FlatCakeClient:
    """Client for the FlatCake local control service."""

    def __init__(self, data_dir: str = "data", timeout_s: float = DEFAULT_TIMEOUT_S):
        self._data_dir = data_dir
        self._timeout = timeout_s
        self._token: Optional[str] = None
        self._port: Optional[int] = None

    def _connect(self) -> socket.socket:
        path = self._discovery_path()
        if not os.path.exists(path):
            raise ServiceNotRunning()
        with open(path) as f:
            meta = json.loads(f.read())
        port = meta["port"]
        token = meta["token"]
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=self._timeout)
        except ConnectionRefusedError:
            raise ServiceNotRunning() from None
        s.settimeout(self._timeout)
        self._port = port
        self._token = token
        return s

    def _discovery_path(self) -> str:
        return os.path.join(self._data_dir, "flatcake-service.json")

    def _request(self, body: dict) -> dict:
        s = self._connect()
        try:
            body["token"] = self._token
            line = json.dumps(body) + "\n"
            s.sendall(line.encode())
            resp_line = b""
            while b"\n" not in resp_line:
                chunk = s.recv(4096)
                if not chunk:
                    raise FlatCakeError("service closed connection")
                resp_line += chunk
                if len(resp_line) > MAX_RESPONSE:
                    raise FlatCakeError("response too large")
            resp = json.loads(resp_line.decode())
            if "error" in resp:
                raise FlatCakeError(resp["error"], method=body.get("method", ""))
            return resp
        finally:
            s.close()

    # ---- public API ----

    def status(self) -> dict:
        return self._request({"method": "status"})

    def metrics(self) -> dict:
        return self._request({"method": "metrics"})

    def diagnostics(self) -> dict:
        return self._request({"method": "diagnostics"})

    def config(self) -> dict:
        return self._request({"method": "config"})

    def register_runtime(
        self,
        runtime_id: str,
        project_id: str,
        pid: int,
        name: str,
        version: str,
        capabilities: int,
    ) -> dict:
        return self._request({
            "method": "register",
            "runtime_id": runtime_id,
            "project_id": project_id,
            "pid": pid,
            "name": name,
            "version": version,
            "capabilities": capabilities,
        })

    def heartbeat(self, runtime_id: str) -> dict:
        return self._request({"method": "heartbeat", "runtime_id": runtime_id})

    def disconnect(self, runtime_id: str) -> dict:
        return self._request({"method": "disconnect", "runtime_id": runtime_id})

    def kv_cache(self) -> dict:
        return self._request({"method": "kv-cache"})

    def runtimes(self) -> dict:
        return self._request({"method": "runtimes"})


# ---- Compatibility descriptor (Python mirror of Rust CacheCompat) ----

class CacheCompat:
    """Strict per-cache compatibility metadata. All fields must match for
    cross-workload cache reuse. Constructed by the inference runtime adapter.
    """

    def __init__(
        self,
        model_arch: str,
        model_id: str,
        model_revision: str = "local",
        tokenizer_hash: int = 0,
        attention_kind: str = "",
        num_kv_heads: int = 0,
        head_dim: int = 0,
        num_layers: int = 0,
        rope_kind: str = "rope",
        rope_theta: int = 0,
        dtype: str = "",
        quant_format: str = "none",
        tensor_layout: str = "",
        block_size_tokens: int = 0,
        adapter_id: str = "",
        runtime: str = "",
        runtime_version: str = "",
    ):
        self.model_arch = model_arch
        self.model_id = model_id
        self.model_revision = model_revision
        self.tokenizer_hash = tokenizer_hash
        self.attention_kind = attention_kind
        self.num_kv_heads = num_kv_heads
        self.head_dim = head_dim
        self.num_layers = num_layers
        self.rope_kind = rope_kind
        self.rope_theta = rope_theta
        self.dtype = dtype
        self.quant_format = quant_format
        self.tensor_layout = tensor_layout
        self.block_size_tokens = block_size_tokens
        self.adapter_id = adapter_id
        self.runtime = runtime
        self.runtime_version = runtime_version

    def to_dict(self) -> dict:
        return {k: v for k, v in self.__dict__.items()}

    def check_compatible(self, other: "CacheCompat") -> list:
        diffs = []
        for field in self.__dict__:
            a = getattr(self, field)
            b = getattr(other, field)
            if a != b:
                diffs.append(f"{field}: {a!r} != {b!r}")
        return diffs

    def namespace_key(self) -> str:
        """Stable string key for registry lookup (mirrors Rust namespace_key)."""
        parts = [
            self.model_arch, self.model_id, self.model_revision,
            self.attention_kind, self.rope_kind, self.dtype,
            self.quant_format, self.tensor_layout, self.adapter_id,
            self.runtime, self.runtime_version,
            str(self.tokenizer_hash), str(self.num_kv_heads),
            str(self.head_dim), str(self.num_layers), str(self.rope_theta),
            str(self.block_size_tokens),
        ]
        return "|".join(parts)


# ---- Runtime adapter classes (thin wrappers; no inference logic here) ----

class VllmConnector:
    """Connector for vLLM's KVConnectorBase_V1 (v0.6+).

    FlatCake acts as a KVConnectorBase_V1 subclass: block-level offload and
    retrieval via the vLLM scheduler's block manager. Per-layer per-head
    tensors are serialized to FlatCake's byte blocks for offload and
    deserialized on retrieval.
    """

    RUNTIME = "vllm"
    SUPPORTED_OPS = {"offload", "retrieve", "release", "cancel", "inspect"}
    UNSUPPORTED_OPS = {"whole_seq_serialize"}

    def __init__(self, client: FlatCakeClient):
        self.client = client

    def compat(
        self, model_id: str, num_layers: int, num_kv_heads: int,
        head_dim: int, block_size: int, dtype: str = "auto",
    ) -> CacheCompat:
        return CacheCompat(
            model_arch="", model_id=model_id,
            num_kv_heads=num_kv_heads, head_dim=head_dim,
            num_layers=num_layers, dtype=dtype,
            tensor_layout="vllm_block", block_size_tokens=block_size,
            runtime=self.RUNTIME,
        )


class SGLangConnector:
    """Connector for SGLang's HiCacheBackend (hierarchical storage).

    FlatCake integrates as a hierarchical storage tier: token-level tree
    with per-layer KV tensors. No block_size (token-level addressing).
    """

    RUNTIME = "sglang"
    SUPPORTED_OPS = {"offload", "retrieve", "release", "inspect"}
    UNSUPPORTED_OPS = {"whole_seq_serialize", "cancel"}

    def __init__(self, client: FlatCakeClient):
        self.client = client

    def compat(self, model_id: str, num_layers: int, dtype: str = "bf16") -> CacheCompat:
        return CacheCompat(
            model_id=model_id, num_layers=num_layers, dtype=dtype,
            tensor_layout="sglang_tree", runtime=self.RUNTIME,
        )


class LlamaCppConnector:
    """Connector for llama.cpp's whole-sequence serialization API.

    llama.cpp does NOT expose a block-level KV API. Only whole-sequence
    serialization via llama_state_seq_get_data/save_data is available.
    FlatCake stores the serialized bytes as an opaque block; it cannot
    provide per-block retrieval or partial updates.
    """

    RUNTIME = "llama.cpp"
    SUPPORTED_OPS = {"whole_seq_serialize", "inspect"}
    UNSUPPORTED_OPS = {"offload", "retrieve", "release", "cancel", "block_access"}

    def __init__(self, client: FlatCakeClient):
        self.client = client

    def compat(self, model_id: str, num_layers: int, dtype: str = "f16") -> CacheCompat:
        return CacheCompat(
            model_id=model_id, num_layers=num_layers,
            dtype=dtype, quant_format="gguf",
            tensor_layout="llama_seq", runtime=self.RUNTIME,
        )


class TransformersConnector:
    """Connector for Hugging Face Transformers' Cache API.

    Integrates with DynamicCache/OffloadedCache: per-layer (key, value)
    tensors are serialized to FlatCake blocks for offload and deserialized
    on retrieval. Token-level addressing (not block-based).
    """

    RUNTIME = "transformers"
    SUPPORTED_OPS = {"offload", "retrieve", "inspect"}
    UNSUPPORTED_OPS = {"whole_seq_serialize", "cancel", "block_access"}

    def __init__(self, client: FlatCakeClient):
        self.client = client

    def compat(self, model_id: str, num_layers: int, dtype: str = "float16") -> CacheCompat:
        return CacheCompat(
            model_id=model_id, num_layers=num_layers, dtype=dtype,
            tensor_layout="hf_tuple", runtime=self.RUNTIME,
        )
