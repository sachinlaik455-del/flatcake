# FINAL_REPORT.md — FlatCake Level-B prototype (Directive Part 24)

## What was implemented

A complete, buildable, tested Level-B (CPU + real local SSD I/O) implementation of the FlatCake architecture (arXiv:2610.38355): FC-GVA (write-once scope-direct placement, exact 64-bit GVA `MediaType[63:60]|NodeID[59:48]|Offset[47:0]`, publish-after-write index, session-granularity reclamation), FC-MAS (dual queues, HRRN `Priority = 1 + wait/KV_size`, original-arrival-ts re-admission), FC-CIO (layer-major scope files, sort-merge coalescing with Δgap=64 KiB, bounded unbuffered windows), integrated engine, a fair tiered LRU baseline with staging contention and FIFO-bucket reclaim, deterministic workload generator, open-loop replay driver with byte-exact integrity verification, benchmark harness + plots, and the adaptive-placement research extension. Rust, std-only, zero dependencies; Python for plots.

## What follows the paper exactly

GVA bit layout and the 2^60 DRAM/SSD partition (verified verbatim, p.5); publish-after-write key visibility (§3.2); Table-2 placement signals with a documented precedence resolution (OQ-19); session-extent reclamation with reader/writer pins and tombstone-first invalidation (strengthened per D15); HRRN Eq. 1 with documented units/timing choices (OQ-4/15); layer-major scope layout with append-per-turn and 4 KiB alignment (§3.4); sort-merge coalescing with 64 KiB gap bridging; O(1)-per-worker bounded windows with wait-not-abort saturation; cold-start no-warm-up replay protocol (§5.1). Full mapping: docs/PAPER_TO_IMPLEMENTATION.md (T1–T21 ✅).

## What was adapted (and why)

No GPU path at Level B (consumer buffers instead of HBM; `FILE_FLAG_NO_BUFFERING` ≈ `O_DIRECT` — NOT GDS); hybrid-request DRAM portions delivered inline (the §3.3 "pinned in place" semantics); hbm window = a byte-resident delivered-blocks cache identical for both systems; recompute modeled as a symmetric bookkeeping cost (labeled); scope→file is 1:1 with workload-enforced size bounds (chaining deferred, OQ-11); index fixed-capacity with a loud startup guard (OQ-16). All traced to decisions D1–D26 with the independent reviews that forced them.

## What remains incomplete / out of scope

Level C (Linux + CUDA + GDS + SGLang + real models + the production trace) — design only, `docs/FUTURE_GDS_INTEGRATION.md`. Recovery log cut (OQ-9). Multi-file chaining deferred (OQ-20/OQ-11). Arena-copy contention optimization deferred with measured evidence (audit U2). M10 tuning beyond the measure-first audit was not needed for correctness and was deliberately skipped (Part 15).

## Hardware available vs required

Available: Windows 11 laptop — i5-14450HX (10C/16T), 15.7 GB RAM, KIOXIA 512 GB NVMe (single drive). Paper requires: Linux, 8× B200 + 3 TB DRAM + 8× PM9A3 RAID-0, CUDA 13.1, GDS 1.16.1, SGLang v0.5.19. The RTX 3050 laptop GPU is irrelevant (GDS is Linux-only; no CUDA stubs were created).

## Tests

**91/91 passing** across 11 targets: lib 10 (util/hbm unit), baseline 4, cio 17, engine 2 (vertical slice + concurrency), gva 33, mas 15, p2_matrix 7, replay 2 (integrity + open-loop pacing), workload_shape 1 (T19 statistical shapes). Every failure ever observed was root-caused and fixed (PHASE_2_TEST_REPORT defect table); zero known flaky tests; zero unresolved defects at audit.

## What was measured (ours — never the paper's numbers)

- Demo: flatcake 82.5% vs baseline 35.3% delivered hit rate, integrity 0/0 (forced-SSD budgets).
- Sweep (8 cold-start runs): FlatCake **100.00%** delivered hits at every load/seed vs baseline 86.15–99.57%; **109× fewer physical read ops** (352 vs 38,272) at qps4 for identical logical reads; write-amp **0.995× vs 3.269×**; read-amp ≈1.0× both (Δgap bridging self-consistent); integrity 0 everywhere.
- Raw-disk calibration: 1 MiB streaming 1,075 MB/s vs 4 KiB random 56 MB/s unbuffered (~19× coalescing ceiling); buffered "4 KiB random" would have shown 2,900 MB/s — a 52× artifact proving why D12 exists.
- These reproduce the paper's *mechanism claims directionally* (hit-rate stability under load; sequential-restore efficiency; write-exactly-once). They do not — and cannot — reproduce its absolute numbers (different hardware, no GPU, synthetic traces).

## Adaptive extension (honest negative)

Adaptive placement achieved **parity** with the paper's static policy (100% everywhere) and never beat it; fixed-DRAM collapsed (83–96%) — confirming the paper's write-once/asymmetric-cost design rather than improving on it. Tuning grid limited to declared defaults (documented; D25). docs/ADAPTIVE_EXTENSION.md.

## Known defects

None unresolved (audit report table lists 7 resolved findings with evidence; 2 documented non-defect limitations: modeled-recompute latency caveat, arena-lock contention as a measured M10 candidate).

## Hardware blockers

GPU/GDS/SGLang reproduction blocked by hardware (Linux + NVIDIA server class) — Level-B prototype deliberately remains useful and independently testable without it.

## Audit

docs/AUDIT_REPORT.md: **PASS** on all 16 Part-22 acceptance criteria, with the delegation-compression constraint documented (sub-agent quota exhaustion; A1/A2 independent agent reviews + evidence-based self-audit).

## Reproduction (exact Windows 11 commands)

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
cargo test --release                    # 91/91
./target/release/fckv.exe selftest
./target/release/fckv.exe demo
bash benchmarks/run_sweep.sh            # ~5-7 h (M9); results -> results/*.json
python python/plot_results.py results/ results/plots
bash benchmarks/run_m11.sh              # ~1 min (M11); results -> results/m11/
python -I scripts/audit_io_probe.py     # raw-disk calibration
```
Dependencies: rustup stable-x86_64-pc-windows-gnu (1.99.1 used), Python 3.14 + matplotlib (plots only). No other dependencies exist.
