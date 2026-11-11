# FlatCake Level-B Prototype (fckv)

A CPU-based, real-SSD-I/O implementation of the architecture from *FlatCake: A Flat KVCache Storage Architecture for LLM Serving* (arXiv:2610.38355), built and tested on Windows 11 / 16 GB RAM / NVMe laptop. Rust (std-only, zero dependencies) + Python for plots.

**What this is:** fidelity Level B — the paper's FC-GVA / FC-MAS / FC-CIO mechanisms on real local SSD with unbuffered I/O, benchmarked against a tiered LRU baseline under identical budgets. **What this is not:** a GPU/GDS/SGLang reproduction (design-only: `docs/FUTURE_GDS_INTEGRATION.md`); the paper's numbers are never presented as ours (`DECISIONS.md` D2).

## Build & verify (Windows 11, Git Bash)

```bash
export PATH="$HOME/.cargo/bin:$PATH"    # rustup default stable-x86_64-pc-windows-gnu
cargo build --release                   # zero external crates
cargo test --release                    # 90 tests, all suites
./target/release/fckv.exe selftest      # GVA round-trip + config validation
./target/release/fckv.exe demo          # end-to-end demo, both systems (see below)
```

While a benchmark is running, use per-suite invocations (`cargo test --release --test p2_matrix_tests`) — the bench holds the exe file lock (Windows).

## Demo (vertical slice, <2 min)

```bash
./target/release/fckv.exe demo
```
Generates a deterministic agentic trace (3 sessions × 10 turns + shared prompt), replays it open-loop through BOTH systems with identical budgets, verifies every delivered block byte-exactly. Observed 2026-11-11: **flatcake 82.5% vs baseline 35.3% delivered hit rate, integrity_fail=0 both** (mechanism behavior at forced-SSD budgets; not a paper-scale claim).

## Benchmarks

```bash
bash benchmarks/run_sweep.sh            # 8 cold-start runs: {baseline,flatcake} x qps{1,4} x seeds{11,12}
python python/plot_results.py results/ results/plots   # summary.csv + PNG charts
python -I scripts/audit_io_probe.py     # raw-disk calibration (buffered vs NO_BUFFERING)
```
Protocol: `docs/BENCHMARK_METHODOLOGY.md` (cold start, no warm-up, symmetric miss model, parity table). Raw records: `results/*.json` (pinned schema).

## Layout

| Path | Contents |
|---|---|
| `src/types.rs` | frozen contracts: 64-bit GVA (`MediaType[63:60]\|NodeID[59:48]\|Offset[47:0]`), metrics |
| `src/gva/` | FC-GVA: Level-1 index (publish-after-write), placement (Table-2 precedence), DRAM arena, session reclamation |
| `src/mas/` | FC-MAS: dual queues, HRRN `Priority = 1 + wait/KV_size`, arrival-ts re-admission |
| `src/cio/` | FC-CIO: layer-major scope files, sort-merge coalescing (Δgap 64 KiB), bounded unbuffered reads |
| `src/engine.rs` | integrated FlatCakeEngine (KvEngine) |
| `src/baseline.rs` | tiered LRU + staging + FIFO buckets (the system the paper critiques, fairly built) |
| `src/workload.rs`, `src/replay.rs` | deterministic traces; open-loop driver with integrity verification |
| `docs/` | paper analysis, traceability (T1–T24), open questions (OQ-1..21), architecture, Phase-2 reports |
| `EXECUTION_LEDGER.md`, `DECISIONS.md` | evidence-based milestone log; 24+ numbered decisions |

## Status

Phase 1 (research → architecture) and Phase 2 (implementation) complete: 90/90 tests, demo + sweep verified. See `PHASE_2_STATUS.md`, `docs/PHASE_2_TEST_REPORT.md`, `docs/PHASE_2_PAPER_FIDELITY.md`. Remaining: benchmark analysis (M9), adaptive-placement extension (M11), final audit (M12).
