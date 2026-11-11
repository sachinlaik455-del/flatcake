# ARCHITECTURE.md — FlatCake Level-B prototype (M2, rev 2 after EGM design review)

Requirements source: `PAPER_TO_IMPLEMENTATION.md` (T1–T24). Decisions: `OPEN_QUESTIONS.md`, `DECISIONS.md`.
Rev 2 incorporates the EGM design-critic and implementation-readiness findings (ledger M2 entry): reclamation protocol, `FILE_FLAG_NO_BUFFERING`, open-loop replay, index lifecycle, placement precedence, frozen types, pinned schemas.

## 1. Top-level shape

Single std-only Rust crate (`fckv`); Python 3.14 **for plots only**. The replay loop lives in Rust (in-process, real latencies). **One canonical workload generator: `fckv gen-workload` (Rust)** — payload byte patterns exist only in Rust, so generator and verifier cannot drift (EGM #8/#10).

```
KVCache/
├── Cargo.toml              # crate fckv, edition 2021, zero dependencies
├── fckv.conf               # single small config file (all tunables, validated)
├── src/
│   ├── main.rs             # CLI: `fckv gen-workload` | `fckv demo` | `fckv selftest` | `fckv bench`
│   ├── types.rs            # ★ FROZEN shared contract (EGM blocker #2): Gva, BlockKey, Medium, Lookup,
│   │                       #   PlacementSignals, Request, Metrics, ArenaReservation, layout::resolve
│   ├── config.rs           # `key = value` parser + Config (bounds-checked)
│   ├── util.rs             # SplitMix64 payload pattern (frozen formula), FNV-1a 64, JSON writer, timers
│   ├── gva/
│   │   ├── addr.rs         # T1: GVA encode/decode (MediaType[63:60]|NodeID[59:48]|Offset[47:0])
│   │   ├── index.rs        # T2/T3/T21: fixed-capacity lock-free table, states EMPTY/PENDING/PUBLISHED/TOMBSTONE
│   │   ├── placement.rs    # T4/T5: frozen precedence table (§4 below)
│   │   └── reclaim.rs      # T6: full protocol (§5 below): tombstone-invalidate, reader+writer refs, watermarks
│   ├── mas/
│   │   └── scheduler.rs    # T7/T8/T9: dual queues, HRRN Eq.1 (lazy, at dispatch decisions), arrival_ts re-admission
│   ├── cio/
│   │   ├── layout.rs       # T11/T20: GPU Files, layer-major formula (§6), per-file free lists, resolve()
│   │   ├── coalesce.rs     # T12: sort-merge by (file,offset), Δgap=64 KiB bridging
│   │   └── stream.rs       # T13/T14: bounded double-buffered windows, NO_BUFFERING positional reads
│   ├── engine.rs           # T10/T15: FlatCakeEngine + hbm window (§7) + symmetric miss model (§8)
│   ├── baseline.rs         # T17: tiered DRAM-LRU→SSD, staging-pool contention, FIFO-bucket reclaim
│   └── replay.rs           # open-loop driver (§9) + metric collection (pinned JSON/CSV schema)
├── tests/                  # integration + stress
├── python/plot_results.py  # reads results/*.json — plots only
├── data/                   # generated KV files (gitignored, capped)
└── results/                # benchmark records (gitignored)
```

Build order (EGM readiness): **Phase 0 freeze** (Cargo.toml, types.rs, config.rs, util.rs) → **Phase 1** FC-GVA ∥ FC-CIO (only frozen types) → **Phase 2** FC-MAS + engine/replay/baseline.

## 2. Storage I/O — honest `measured:` labels (EGM critical #2)

All scope-file reads AND writes open with **`FILE_FLAG_NO_BUFFERING`** (`OpenOptionsExt::custom_flags`, std-only) + 4 KiB-aligned offsets/buffers/padded lengths. Without it, NTFS page cache absorbs the working set and every SSD number is a cache artifact; with it, both systems see raw NVMe and run-order contamination is eliminated. Metadata/allocator ops stay cached (irrelevant to measured I/O). One caveat recorded: `File::set_len` pre-allocation + NO_BUFFERING appends require aligned segment lengths (formula in §6 guarantees it).

## 3. Concurrency & state ownership (single source of truth each)

| State | Owner | Lock discipline |
|---|---|---|
| key → GVA | `gva::index` atomic slot table (fixed capacity `index_capacity`, **no resize**; startup asserts unique blocks ≤ 0.7·capacity — OQ-16 load factor is a code constant) | wait-free reads; CAS EMPTY→PENDING (loser re-probes next slot); store PUBLISHED after write completes; TOMBSTONE on reclaim/failed write; TOMBSTONE slots reusable for new inserts only (probing skips them for lookups but may reclaim for insert) |
| DRAM block bytes | `gva` arena (pre-allocated, bump + session-extent free) | `Mutex`; payload copies inside lock — **real cost ms-scale for MB payloads** (EGM #12), accepted for correctness, profiled at M10 |
| GPU Files, append offsets, scope map, per-file free lists | `cio::layout` | `Mutex` per-scope state; file writes sequential; never held across reads |
| Read staging windows | `cio::stream` per-worker | worker-owned window (2 layer-extents) + own file handles |
| Queues | `mas::scheduler` | `Mutex`+`Condvar`; bounded `queue_cap`; blocking send = backpressure |
| Session reader/writer refcounts, watermarks | `gva::reclaim` | atomics; see §5 protocol |
| hbm window (delivered-blocks cache) | `engine.rs` (both systems, identical policy/budget) | `Mutex` LRU, byte-resident |
| Baseline LRU / staging pool / buckets | `baseline.rs` | `Mutex`+`Condvar` staging budget |

**Invariant: no lock is held across physical file I/O.** Published payloads are immutable (write-once, T5).

## 4. Frozen placement precedence (EGM blocker #1; resolves paper tension → OQ-19)

Evaluated per **scope creation** (first put of a session scope); appends inherit scope medium **if scope is SSD** (SSD contiguity is a paper invariant, §3.4); appends to DRAM scopes re-evaluate rows 2–5 fresh (DRAM grouping is irrelevant; no migration either way):

| Order | Signal | Medium |
|---|---|---|
| 1 | prefix refcount > 1 (`shared:1` ⇒ refcount 2, `shared:0` ⇒ 1) | DRAM |
| 2 | ctx_tokens ≥ l_agent_tokens | SSD |
| 3 | reply_tokens ≥ tau_tokens | SSD |
| 4 | turn == 1 (unshared) | SSD (one-shot) |
| 5 | turn > 1 ∧ reply_tokens < tau_tokens | DRAM |
| — | DRAM-full spill (arena reserve fails): this put → SSD, **scope flips to SSD**, `metrics.dram_spill_puts++` (EGM major #5) | SSD |

Rationale documented in OQ-19: Table 2's "short-turn chat → DRAM" row conflicts with one-shot turn-1 classification; we resolve by evaluating at scope creation and relying on the paper's own asymmetric-cost argument (a reusable block on SSD is served by coalesced GDS, not a miss).

## 5. Reclamation protocol (EGM critical #1 — closes all three race windows)

- **Refcounts**: atomic per-session-scope, covering **readers (taken at GVA-resolution time, held until that request's I/O completes — closes the resolve→read TOCTOU)** and **writers (held from GVA allocation until write completion — a PENDING write pins its scope)**.
- **Reclaim sequence** (watermark trigger): (1) pick oldest unreferenced scope (order key = scope creation `ts_us`); (2) **tombstone-invalidate all its index entries first** (prevents stale GVA→freed-offset wrong-data hits); (3) return extent to per-file free list via `cio::layout::free_extent(file_seq, off, len)` — **O(1) = zero payload-byte operations, ≤ constant free-list pops (asserted via instrumentation counter, EGM #12c)**; (4) arena extents freed likewise for DRAM scopes.
- **DRAM lifecycle (EGM #5)**: `dram_high/low_watermark` (0.9/0.7) reclaim unreferenced DRAM session extents identically; DRAM-full is therefore transient, not permanent.
- **Watermark semantics**: denominator = logical bytes written ÷ (`gpu_file_pool × gpu_file_bytes`); if nothing is reclaimable, watermark is **advisory** (writes continue, `metrics.ssd_over_high_watermark_us` recorded) — bounded instead by workload sizing (BENCHMARK_METHODOLOGY.md parity table).
- **Miss-after-reclaim**: a get on a reclaimed block = clean miss → symmetric recompute model (§8).

## 6. Frozen CIO layout formula (EGM blocker #4)

- Scope → GPU File: one pre-allocated `gpu_file_bytes` file per SSD scope; **workload enforces session size ≤ gpu_file_bytes; multi-file chaining deferred** (dead code at our densities, EGM #8 — asserted, not implemented).
- Append order: `for turn t { for layer l { segment(t,l) at align_up(cur,4096); cur += n_blk(t,l) × block_bytes } }` → per-layer contiguous extents; inter-segment padding ≤ 4095 B < Δgap ⇒ absorbed by bridging (T13 bound holds by construction).
- SSD GVA offset packs the file: `off[47:0] = file_seq[47:28] | in_file_off[27:0]` (2^28 = 256 MiB file); DRAM GVA offset = arena byte offset. `cio::layout::resolve(gva) -> (file_seq, in_file_off)` frozen in types.rs.
- Reads: per-worker positional `seek_read` on NO_BUFFERING handles into the worker's bounded window.

## 7. hbm window (EGM #6 — byte-resident, identical both systems)

`engine.rs` owns a **global 64 MiB LRU keyed by `(session, BlockKey)`**, LRU-by-delivery, **holding actual bytes**: on delivery, payload copied into window (population path defined; request larger than window ⇒ only smaller blocks resident — partial residency by block granularity); checked **per block before scheduler classification**: hbm-resident blocks delivered immediately; remaining blocks classified by max-GVA rule (hybrid hbm+DRAM requests = mixed delivery, all counted per-medium). Identical budget & policy for baseline (fairness; EGM #6 asymmetry fixed).

## 8. Symmetric miss & recompute model (EGM #7 — normative)

- Per-block accounting: hit = block delivered to consumer (hbm ∨ DRAM ∨ SSD); miss otherwise.
- **Both systems**: a missed block's payload is regenerated deterministically, charged `recompute_us_per_token` **(same constant both systems, labeled `modeled:`)**, and **re-persisted via the normal put path** (reproduce the paper's recompute→re-persist → write-amplification mechanism).
- PENDING (get racing in-flight put) = miss **by design** (publish-after-write, T3); recorded separately as `pending_racing_gets`.
- Partial delivery: present blocks deliver, absent ones recompute (per-block costs sum).

## 9. Open-loop replay driver (EGM critical #3)

- Issue thread fires each request at wall-clock `start + ts_us × time_scale` (`time_scale` config, default 1.0; QPS sweeps encoded in generated `ts_us`; `ts_us` non-decreasing per session — generator contract).
- In-flight cap = `queue_cap`; when saturated, the issue thread blocks (overload policy) — `arrival_ts_us` = **intended** `ts_us`, so measured latency includes queue wait (honest TTFT analog).
- Worker pools + dual queues give FC-MAS its real effect (Fig. 4): DRAM hits of other in-flight requests are never stuck behind SSD restores.
- Blocking-client mode retained **only for `fckv demo`** (smoke test).
- Cold start, no warm-up (paper protocol §5.1); measurement window = full run (EGM #11).

## 10. Config (`fckv.conf`, validated; EGM #10 additions)

```
dram_pool_bytes = 536870912      dram_high_watermark = 0.90     dram_low_watermark = 0.70
hbm_window_bytes = 67108864      # identical both systems
gpu_file_bytes = 268435456       gpu_file_pool = 16             ssd_dir = data/fckv
baseline_ssd_dir = data/baseline                                                # separate dirs (EGM #10c)
bucket_bytes = 33554432          bucket_count = 128             # baseline FIFO buckets (Mooncake-like)
block_tokens = 256               bytes_per_token = 32
tau_tokens = 4096                l_agent_tokens = 65536
gap_bridge_bytes = 65536         # Δgap (paper)
ssd_high_watermark = 0.90        ssd_low_watermark = 0.70       # advisory (§5)
ssd_workers = 2                  dram_workers = 2               # open-loop driver is fixed at 32 replay workers
queue_cap = 256                  staging_pool_bytes = 134217728 # baseline-only
recompute_us_per_token = 8       # modeled:, identical both systems
index_capacity = 1048576         # no resize; load factor 0.7 is a code constant
time_scale = 1.0                 csv_window_us = 1000000
```

## 11. Data formats (pinned; EGM blocker #3/#9)

- **Trace JSONL** (Rust generator, normative): `{"ts_us":u64,"op":"put|get","session":u64,"turn":u64,"layers":u32,"start_blk":u64,"n_blk":u32,"shared":0|1,"reply_tokens":u64,"ctx_tokens":u64,"seed":u64}` — `token_start = start_blk × block_tokens`; payload = SplitMix64(seed ^ FNV1a64(session, layer, token_start)) filling 8-byte LE words (frozen formula, util.rs doc-comment).
- **Recovery log: cut from scope** (EGM #9 — no T-row exercises it); OQ-9 resolved as "not implemented, documented".
- **results/<run>.json** (field names = `Metrics` struct fields = JSON keys):
```json
{"config":{...},"system":"flatcake|baseline","seed":0,
 "metrics":{"delivered_hit_rate":0.0,"hit_hbm":0,"hit_dram":0,"hit_ssd":0,"misses":0,"pending_racing_gets":0,
   "read_amp":0.0,"write_amp":0.0,"physical_read_ops":0,"logical_block_gets":0,"physical_read_bytes":0,"logical_read_bytes":0,
   "physical_write_bytes":0,"logical_write_bytes":0,
   "coalesce":{"merged_ranges":0,"bridged_gaps":0,"overread_bytes":0},
   "queue_wait_us":{"mean":0.0,"p50":0.0,"p95":0.0,"p99":0.0},
   "prep_us":{"mean":0.0,"p50":0.0,"p95":0.0,"p99":0.0},
   "latency_us":{"mean":0.0,"p50":0.0,"p95":0.0,"p99":0.0},
   "throughput_blocks_per_s":0.0,"mem_high_water_bytes":0,"dram_spill_puts":0,
   "ssd_over_high_watermark_us":0,"modeled_recompute_us":0,"reclaim_events":0,"reclaim_payload_ops":0}}
```
- **results/<run>.csv** (per window): `window_idx,t_us,delivered,requested,hit_rate,phys_reads,read_amp`.

## 12. Sizing & fairness parity (EGM #8 — full table in BENCHMARK_METHODOLOGY.md at M9)

Total-memory parity: FlatCake ≈ hbm 64 MiB + DRAM pool 512 MiB + per-worker windows; baseline ≈ same + staging pool 128 MiB (staging is baseline-mechanism-specific, documented). SSD budgets equal (16 × 256 MiB logical). Write-amp = 1.000× only guaranteed when no reclaim-rewrite occurs inside the window — primary runs sized to stay below watermark; a dedicated reclaim-behavior run exercises it separately.

## 13. Agent work-split (unchanged; max 2 concurrent)

Orchestrator: types.rs, config.rs, util.rs, engine.rs, baseline.rs, replay.rs, main.rs. FC-GVA agent: `src/gva/**` + `tests/gva_*.rs`. FC-MAS agent: `src/mas/**` + `tests/mas_*.rs`. FC-CIO agent: `src/cio/**` + `tests/cio_*.rs`. Testing agent: integration/stress. Benchmark agent: analysis + plots (Python side consumes pinned schemas).
