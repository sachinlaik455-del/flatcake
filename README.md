# FlatCake (fckv)

A CPU-based, real-SSD-I/O implementation of the architecture from *FlatCake: A Flat KVCache Storage Architecture for LLM Serving* (arXiv:2610.38355), built and tested on Windows 11 / 16 GB RAM / NVMe laptop. Rust (std-only, zero dependencies) + Python for plots.

**What this is:** fidelity Level B — the paper's FC-GVA / FC-MAS / FC-CIO mechanisms on real local SSD with unbuffered I/O, benchmarked against a tiered LRU baseline under identical budgets. **What this is not:** a GPU/GDS/SGLang reproduction (design-only: `docs/FUTURE_GDS_INTEGRATION.md`); the paper's numbers are never presented as ours (`DECISIONS.md` D2).

## Build & verify (Windows 11, Git Bash)

```bash
export PATH="$HOME/.cargo/bin:$PATH"    # rustup default stable-x86_64-pc-windows-gnu
cargo build --release                   # zero external crates
cargo test --release                    # 111 tests, all suites
./target/release/fckv.exe selftest      # GVA round-trip + config validation
./target/release/fckv.exe demo          # end-to-end demo, both systems (see below)
```

While a benchmark is running, use per-suite invocations (e.g. `cargo test --release --test engine_tests`) — a running bench holds the exe file lock on Windows.

## Demo (vertical slice, <2 min)

```bash
./target/release/fckv.exe demo
```
Generates a deterministic agentic trace (3 sessions × 10 turns + shared prompt), replays it open-loop through both systems with identical budgets, and verifies every delivered block byte-exactly. Observed: **flatcake 82.5% vs baseline 35.3% delivered hit rate, integrity_fail=0 both** (mechanism behavior at forced-SSD budgets; not a paper-scale claim).

## Benchmarks

```bash
bash benchmarks/run_sweep.sh            # 8 cold-start runs: {baseline,flatcake} x qps{1,4} x seeds{11,12}
bash benchmarks/run_p3.sh               # adaptive-placement experiment
bash benchmarks/run_p4.sh               # recovery-log overhead experiment
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
| `configs/` | benchmark configurations (m11 adaptive-placement, p3/p4 experiments) |
| `docs/` | analysis, architecture, results, security audit — see table below |

## Documentation

| Path | Contents |
|---|---|
| `docs/ARCHITECTURE.md` | module map, request lifecycle, lock discipline, config reference |
| `docs/PAPER_ANALYSIS.md` | full paper extraction (mechanisms, formulas, evaluation) |
| `docs/PAPER_TO_IMPLEMENTATION.md` | requirements → code → tests traceability (T1–T24) |
| `docs/RESULTS.md` | measured benchmark results and interpretation |
| `docs/BENCHMARK_METHODOLOGY.md` | metric definitions, parity table, protocol |
| `docs/WORKLOAD_MODEL.md` | trace format, regime→motivation mapping, sizing guards |
| `docs/OPEN_QUESTIONS.md` | recorded interpretations and unresolved decisions (OQ-1..21) |
| `DECISIONS.md` | numbered design decisions (D1–D26) with rationale |
| `docs/LIMITATIONS.md` | scope boundaries, resolved defect record, platform limits |
| `docs/FUTURE_GDS_INTEGRATION.md` | Level-C (GPU/GDS) migration path |
| `docs/final-security-audit.md` | final security audit (PASS WITH FINDINGS) |
| `CHANGELOG.md` | change history |

## Status

**Complete and audited.** 111 tests, 0 failures (warnings-as-errors clean); `docs/final-security-audit.md` verdict: PASS WITH FINDINGS. Security findings and their remediations are recorded there and in `docs/LIMITATIONS.md`.

Key results (Level-B, this laptop): FlatCake sustains a 100% delivered hit rate at every load vs 88.8–99.7% for the tiered baseline; 44–105× fewer physical read ops; write amplification 0.995× vs 3.27×; byte-exact integrity across all suites. See `docs/RESULTS.md`.

## License

Dual-licensed under the [MIT](LICENSE) and [GPL-3.0-or-later](LICENSE-GPL) licenses: you may use, modify, and redistribute this software under **either** license, at your option (`SPDX-License-Identifier: MIT OR GPL-3.0-or-later`).
