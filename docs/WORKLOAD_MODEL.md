# WORKLOAD_MODEL.md — synthetic workload design (M3, T19)

Canonical generator: **`fckv gen-workload`** (Rust, `src/workload.rs`, D17). Python never generates traces.
Traces are JSONL at `data/workloads/`. Payloads are never stored — engines regenerate them from `payload_fill(session, layer, token_start, seed)` (frozen formula in `src/util.rs`), and replay verifies delivered bytes against regeneration.

## Trace schema (frozen, ARCHITECTURE §11)

`{"ts_us","op":"put|get","session","turn","layers","start_blk","n_blk","shared","reply_tokens","ctx_tokens","seed","shared_with"}`

- Session **1** = globally shared prompt (`shared:1` ⇒ refcount 2 ⇒ DRAM per Table 2 row 1). Real sessions start at **2**.
- `shared_with` = a session id whose blocks are **prepended** to a get request (hybrid requests mixing a DRAM-pinned system prompt with SSD-resident history, paper §3.3 / OQ-14). `0` = none.
- A put record covers `layers × n_blk` blocks (layer-major). A get covers the session's own prefix `[0, n_blk)` per layer plus the `shared_with` prefix.
- Pacing: **open-loop** — `ts_us = (record_index / qps) × 1e6` (ARCHITECTURE §9); the driver issues at `start + ts_us × time_scale`.

## Regimes → paper motivation mapping

| Regime | Paper pattern (source) | Generator behavior |
|---|---|---|
| `agentic` | Claude Code: each turn extends + replays the session prefix; sub-agent fan-out reuses prefixes immediately; mean reuse distance 0.85; multi-GiB footprints exceed DRAM (§2.2 Obs. 1 regime B) | Sessions of T turns, round-robin interleaved so several sessions are concurrently active; every turn t>1 issues a full-prefix get (`n_blk=(t-1)×bpt`, `shared_with=1`) plus `fanout` sub-agent replays of the same prefix at the same instant, then a put appending `bpt` blocks/layer. Session footprint grows each turn → crosses `l_agent_tokens` → SSD-bound scope (OQ-19 row 2); late-turn restores are multi-MiB → staging pressure / large coalesced reads. |
| `oneshot` | Conversational/API: 53.2–94.0% of blocks accessed once, heavy-tailed reuse distances (§2.2 Obs. 1 regime A, Table 1) | Many single-turn sessions; each writes `layers × bpt` blocks (one-shot, turn=1 ⇒ SSD per OQ-19 row 4); `reuse_rate` (default 0.1) of sessions later re-access **only the shared prompt** (long reuse distance: hundreds of intervening sessions). |
| `mixed` | Production mixes both regimes | Half agentic + half oneshot sessions, deterministically interleaved by record position. |

## Sizing guards (D20, EGM #8 parity)

- Agentic session bytes = `turns × bpt × layers × bytes_per_token` must be ≤ `gpu_file_bytes` (multi-file chaining deferred, OQ-11).
- Total put bytes must be ≤ `bucket_bytes × bucket_count` (SSD budget) — enforced with a clear error, never silently truncated.
- Defaults at 32 B/token (prototype payload density; the paper's 7.5 KiB/token/rank is GPU-model-specific and irrelevant at Level B — only *ratios* matter).

## Statistical targets (EGM #12e — asserted in trace self-checks)

- `agentic`: every turn t>1 re-accesses 100% of the session's own prefix (reuse distance ≈ fanout+1 requests); session ctx crosses `l_agent_tokens` within the configured turns.
- `oneshot`: fraction of blocks ever re-accessed ≈ `shared_blocks/(sessions×bpt×layers)` + `reuse_rate` — dominated by once-access, matching regime A's shape.
- `mixed`: both shapes present in one timeline.
- Verified by `fckv gen-workload` guard errors + replay-side assertions (M8).

## Known divergence from the paper's evaluation (honesty per D2)

The paper replays `claude-code-weka-0621` (98,827 requests, 21.6B tokens, 393 sessions) — **not** available here (OQ-18: license/size unverified, 16 GB machine). Our synthetic traces reproduce the paper's *stated reuse shapes and mechanisms*, not its data. No numeric comparability is claimed anywhere (D2, BENCHMARK_METHODOLOGY.md).
