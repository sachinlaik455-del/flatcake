# agent-and-developer-integrations.md — P3 agent/developer integration layer

## Python SDK (`python/flatcake_sdk.py`)

Typed client for the FlatCake local service. Context manager, connection lifecycle, runtime registration, metrics, diagnostics, cache inspection.

```python
from flatcake_sdk import FlatCake

with FlatCake() as fc:
    print(fc.status())
    print(fc.kv_cache())
    fc.register_runtime("my-runtime", "dir:...", os.getpid(), "vllm", "0.6.3", 63)
```

All methods map to service JSON-line requests over loopback TCP with token auth.

## Agent framework adapters (`python/flatcake_agent_adapters.py`)

Optional, modular, degrading gracefully. Each adapter registers the agent with FlatCake's registry and sends heartbeats. They do NOT intercept KV cache contents.

| Framework | Adapter class | Mechanism | Tested |
|---|---|---|---|
| LangChain | `LangChainCacheObserver` | Callback handler (on_chain_start/end, on_llm_start/end, on_agent_action/finish) | NOT TESTED (LangChain not installed) |
| CrewAI | `CrewAICacheObserver` | Crew/agent registration + task hooks | NOT TESTED (CrewAI not installed) |
| AutoGen | `AutoGenCacheObserver` | Agent registration + message hooks | NOT TESTED (AutoGen not installed) |
| OpenAI Agents SDK | `OpenAIAgentsCacheObserver` | Agent start/end hooks | NOT TESTED (SDK not installed) |

All adapters: stdlib-only core; framework import is lazy (graceful degradation). They report workload identity — NOT KV cache contents.

## REST API (loopback HTTP on service port)

| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/status` | Service status, protocol version, project, runtime count |
| GET | `/api/v1/metrics` | Engine metrics (hits/misses by medium, migrations, evictions, mem) |
| GET | `/api/v1/kv-cache` | Cache inspection (project, runtimes, RAM/SSD, migrations) |
| GET | `/api/v1/runtimes` | Connected runtime list |
| GET | `/api/v1/diagnostics` | Diagnostic counters |
| GET | `/api/v1/config` | Active configuration |
| POST | `/api/v1/shutdown` | Stop the service |

Auth: `Authorization: Bearer <token>` (token from discovery file).

## CLI (`flatcake`)

`flatcake status|runtimes|inspect|kv-cache|metrics|diagnostics|config|shutdown [--json]`

Reads the discovery file, connects to the service, prints JSON (pretty by default). `/kv-cache` adapters invoke `flatcake kv-cache --json` — thin wrappers, no business logic.

## Security boundaries

- Project isolation: registry entries keyed by project_id
- Token auth: all requests require the service token
- KV data: never crosses the control plane
- Bounded requests: max 64 KB per request; 64 KiB per HTTP body

## Compatibility

Same-workload cache reuse requires exact CacheCompat match (17 fields). Cross-runtime reuse requires verified compatibility. Default: isolated namespaces per model+dtype+layout.
