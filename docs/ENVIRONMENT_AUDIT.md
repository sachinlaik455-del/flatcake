# ENVIRONMENT_AUDIT.md — M1 (audited 2026-11-11, epoch 1794349728)

## Hardware

| Component | Value | Implication |
|---|---|---|
| CPU | Intel Core i5-14450HX, 10 cores / 16 threads | Enough for bounded concurrency stress tests |
| RAM | 15.7 GB total, **3.9 GB free at audit time** | **Hard budget**: DRAM KV pool default **512 MiB**; benchmarks keep working sets ≤ ~2 GiB total; no unbounded buffers |
| GPU | NVIDIA GeForce RTX 3050 6 GB Laptop GPU | Present but **irrelevant at Level B**: GPUDirect Storage/cuFile is Linux-only; 6 GB VRAM ≠ paper's 8×B200. No GPU code will be written or faked |
| SSD | KIOXIA KBG60ZNV512G 512 GB NVMe (single physical drive, partitioned) | Real NVMe under the benchmark volume; **not** the paper's 8-drive PM9A3 RAID0 — absolute throughput numbers are not comparable |
| Volumes | E: 300 GB (275 GB free) — project volume; C: 177 GB (34 GB free) | All generated data under `E:\project ml\KVCache\data\` (bounded, monitored) |

## Software

| Component | Status | Notes |
|---|---|---|
| OS | Windows 11, build 10.0.26300 (24H2+) | Target platform per directive |
| Python | **3.14.7** (`python`, `py`) | Workload generation, orchestration, plots |
| Git | 2.55.0.windows.5 | Version control enabled |
| Rust | **not installed** → installing rustup + `stable-x86_64-pc-windows-gnu` minimal profile (~400 MB disk) | Core engine language |
| C/C++ | gcc/g++ 6.3.0 MinGW.org only (32-bit heritage, 2016-era) | **Rejected as core language**: unsafe thread/atomic support for a concurrency-heavy storage engine; installing MSVC Build Tools (~4 GB) disproportionate for Level B |
| CMake / make / ninja | none | Rust/cargo needs none |
| WSL2 | broken (`Wsl/CallMsi/Install/REGDB_E_CLASSNOTREG`) | Not used; Windows-native path chosen |
| Docker | 29.8.2 | Available but **not used** (directive: no container platform needed) |
| CUDA toolkit / nvcc | not installed | Level C boundary documented in FUTURE_GDS_INTEGRATION.md |

## Stack selection (Decision D10)

- **Core storage engine: Rust** (edition 2021, stable GNU toolchain).
  - Memory safety + fearless concurrency for GVA index / dual queues / coalescing engine; cargo gives build+test+bench in one tool with **zero additional dependencies required** (std only; no external crates for the core — ponytail rule).
  - C++ rejected (above). Python rejected as core: GIL defeats the concurrent scheduler simulation that is the paper's core mechanism.
- **Python 3.14**: workload generator, benchmark orchestration, analysis/plots (matplotlib installed later only if missing, with documented disk impact).
- **Future Level-C path unchanged**: the paper's implementation is C++17/CUDA + cuFile + SGLang; the Rust core exposes plain C-ABI interfaces so the storage layer could be re-targeted/wrapped later. Documented in FUTURE_GDS_INTEGRATION.md — no CUDA stubs are created now.

## Resource-safety commitments (directive Part 21)

1. DRAM pool, queues, read buffers, and file pools are bounded and configured in one config file.
2. Generated benchmark data capped (default ≤ 4 GiB on E:); disk free checked before each benchmark.
3. No model weights, no trace datasets, no containers, no paid services.
4. Nothing outside the project directory is modified except the standard user-level rustup install documented here.
