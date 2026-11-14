# platform-foundation.md — P1 platform foundation

## Architecture

FlatCake P1 establishes a modular platform layer on top of the existing P1–P4 engine without modifying it. The engine remains the sole owner of cache storage, block allocation, placement, migration, eviction, reclamation, and I/O scheduling. The platform layer adds coordination and observation only.

```
┌─────────────────────────────────────────────┐
│               flatcake CLI                  │
│  (status / runtimes / inspect / metrics)    │
└────────────────────┬────────────────────────┘
                     │ 127.0.0.1 TCP + token auth
                     │ JSON-line protocol
┌────────────────────▼────────────────────────┐
│           Platform Service                  │
│  ┌──────────────┐  ┌─────────────────────┐  │
│  │ Runtime      │  │ Telemetry           │  │
│  │ Registry     │  │ (bounded ring)      │  │
│  └──────────────┘  └─────────────────────┘  │
│  ┌──────────────────────────────────────┐   │
│  │ FlatCake Engine (embedded)           │   │
│  │ FC-GVA + FC-MAS + FC-CIO             │   │
│  └──────────────────────────────────────┘   │
└─────────────────────────────────────────────┘
```

## Connector contract

A connector declares its identity and capabilities via `ConnectorInfo`:

```rust
ConnectorInfo {
    name: "sglang",           // stable identity
    version: "1.0",           // connector version
    protocol_version: 1,      // must match PROTOCOL_VERSION
    capabilities: caps::STATUS | caps::CACHE_INSPECT,
}
```

Capabilities are declared, not assumed. `caps::STATUS` is the minimum (required for registration). A connector must never claim direct KV access unless the underlying runtime actually exposes it. The platform validates protocol version and required capabilities at registration; mismatches are rejected with a clear error.

## Runtime registry

`RuntimeRegistry` tracks connected runtimes keyed by a unique runtime_id. Each entry carries: project_id (per-directory identity), process id, connector info, health, registration/heartbeat timestamps. Entries are isolated per project — one project's metadata is never exposed as another's. Stale entries (no heartbeat for 120 s) are dropped on list operations.

## Project identity

Project id is derived from the canonicalized working directory (FNV-1a hash) with an explicit override via `FLATCAKE_PROJECT` env var. No file contents are read; no sensitive data is collected.

## Local control service

The service binds to **127.0.0.1** on an ephemeral port and writes a discovery file (`<ssd_dir>/flatcake-service.json`) containing the port and a session token. All requests must include the token. The service embeds one FlatCakeEngine instance and exposes read-only JSON-line methods:

| Method | Returns |
|---|---|
| status | service status, protocol version, project id, runtime count |
| metrics | engine hits (hbm/dram/ssd), misses, blocks placed/retrieved, migrations, evictions, SSD I/O counters, memory used |
| kv-cache | project id, runtimes, RAM/SSD usage, cache stats, queue pressure (unavailable in P1) |
| runtimes | list of connected runtimes with connector identity and health |
| register | register a runtime (id, project, pid, connector info) |
| heartbeat | refresh a runtime's liveness |
| disconnect | remove a runtime |
| diagnostics | engine diagnostic counters (promotions, demotions, spills, reclaim events) |
| config | active configuration values |
| shutdown | stop the service |

KV data never crosses the control plane. The service validates request size (64 KiB max) and rejects unknown methods with a clear error.

## CLI commands

The `flatcake` binary (separate from the `fckv` engine driver) provides:

```
flatcake serve        start the platform service (embedded engine)
flatcake status       service status
flatcake runtimes     connected runtimes
flatcake inspect      project cache inspection (alias: kv-cache)
flatcake metrics      engine metrics
flatcake diagnostics  diagnostic counters
flatcake config       active configuration
flatcake kv-cache     KV-cache inspection (for /kv-cache adapters)
flatcake shutdown     stop the service
```

All commands accept `--json` for machine-readable output. When the service is not running, the CLI prints `flatcake: service not running` and exits 1 — no partial or fabricated output.

## /kv-cache adapter design

For terminal hosts that support custom slash commands, `/kv-cache` is a thin adapter that invokes `flatcake kv-cache --json` and renders the output. The adapter is optional and independent of the engine; the CLI command is the universal fallback. Host-specific adapter implementations are P2 scope.

## Security boundaries

- The service binds to **127.0.0.1 only** — never exposed to the network.
- All requests require a session token from the discovery file.
- KV tensor contents never cross the control plane.
- Project identity is per-directory; cross-project metadata exposure is prevented by the registry's project filter.
- No secrets, prompts, model outputs, or raw KV contents are persisted or transmitted.
- Request size bounded at 64 KiB; unknown methods rejected with a clear error.

## Known limitations (P1)

- The service runs embedded (in-process); a standalone daemon is deferred to P2.
- `/kv-cache` slash-command adapters for specific hosts are P2 scope; the CLI command is the universal fallback.
- Queue pressure is reported as "unavailable" (no real-time queue depth instrumentation yet).
- CPU utilization and IOPS are not instrumented.
- Recovery-log rotation is not implemented (unbounded growth over very long uptimes).
- Multi-file chaining for sessions larger than `gpu_file_bytes` is deferred (OQ-11).
