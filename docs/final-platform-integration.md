# final-platform-integration.md — FlatCake unified platform (P1–P4 + P1–P3 platform)

## Architecture

```
Terminal (/kv-cache, flatcake CLI)
    │
    ▼
flatcake CLI binary (src/bin/flatcake.rs)
    │  JSON-line over loopback TCP (token auth)
    ▼
Platform service (src/platform/service.rs)
    ├── RuntimeRegistry (src/platform/registry.rs) — runtime/project/session tracking
    ├── Telemetry (src/platform/telemetry.rs) — bounded event ring + counters
    ├── HTTP REST (src/platform/http_api.rs) — GET/POST /api/v1/*
    └── FlatCakeEngine (src/engine.rs) — FC-GVA + FC-MAS + FC-CIO
```

## CLI reference

| Command | Description |
|---|---|
| `flatcake serve` | Start the service with an embedded engine |
| `flatcake status [--json]` | Service status, protocol version, project, runtime count |
| `flatcake runtimes [--json]` | Connected runtimes (id, project, pid, connector, health) |
| `flatcake inspect [--json]` | Alias for kv-cache |
| `flatcake kv-cache [--json]` | Cache inspection: RAM/SSD, hits/misses, migrations, evictions |
| `flatcake metrics [--json]` | Engine metrics |
| `flatcake diagnostics [--json]` | Diagnostic counters |
| `flatcake config [--json]` | Active configuration |
| `flatcake shutdown` | Stop the service |

## /kv-cache host compatibility

| Host | Support | Adapter |
|---|---|---|
| Generic terminal | `flatcake kv-cache` | Universal fallback |
| Coding agents with custom slash commands | `/kv-cache` → invokes `flatcake kv-cache --json` | Thin shell script (host-specific) |
| Environments without slash commands | `flatcake kv-cache` | Universal fallback |

## Connector capability matrix

| Runtime | Offload | Retrieve | Release | Cancel | Inspect | Block | Whole-seq | Status |
|---|---|---|---|---|---|---|---|---|
| vLLM | Yes | Yes | Yes | Yes | Yes | Yes | No | Yes |
| SGLang | Yes | Yes | Yes | No | Yes | Yes (token) | No | Yes |
| llama.cpp | No | No | No | No | No | No | Yes | Yes |
| Transformers | Yes | Yes | No | No | Yes | No (layer) | No | Yes |

## Installation

```bash
git clone <repo>
cd KVCache
cargo build --release          # builds fckv.exe + flatcake.exe
cargo test --release           # 171 tests
./target/release/flatcake.exe serve  # start service
./target/release/flatcake.exe kv-cache  # inspect
```

## Security model

- Loopback TCP only (127.0.0.1), token-authenticated
- KV data never crosses the control plane
- Project isolation via registry namespace keys
- Service shutdown requires an explicit shutdown command

## Known limitations

- Linux/macOS: NOT TESTED (NO_BUFFERING is Windows-specific)
- Dedup disabled by default (F2 concurrency bug, documented)
- Recovery replay assumes same-workload restart
- No GPU/GDS (Level-C boundary)
