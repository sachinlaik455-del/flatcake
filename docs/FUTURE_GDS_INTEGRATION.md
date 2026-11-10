# FUTURE_GDS_INTEGRATION.md — Level-C boundary (Directive Part 16)

Nothing in this repository fakes GPU capabilities. No CUDA stubs exist. This document is the honest map from the Level-B CPU prototype to the paper's full implementation (§4).

## What the paper's implementation requires (verified §4, Table 3)

| Requirement | Purpose in paper | Status on this machine |
|---|---|---|
| Linux + XFS + RAID-0 NVMe (8× PM9A3) | cuFile/GDS requires Linux; XFS single-extent pre-allocation (`fallocate`) | ❌ Windows 11; WSL2 broken; GDS does not support Windows |
| NVIDIA GPU + CUDA 13.1 + GDS 1.16.1 (libcufile) | direct SSD→HBM DMA, registered staging windows | ❌ no CUDA toolkit; RTX 3050 6 GB laptop GPU is not a supported/meaningful target |
| SGLang v0.5.19 + HiCacheStorage interface | serving integration (`exists/get/put`) | ❌ no LLM serving stack on a 16 GB machine |
| Pinned 2 MB hugepages + `cudaHostRegister` | DMA-safe host arena | ❌ GPU-dependent |
| `cuFileHandleRegister`/`cuFileBatchIOSubmit` + registered per-GPU HBM window | batched direct reads, O(1) staging | ❌ GPU-dependent |

## Where the prototype already matches the paper's structure

- Exact 64-bit GVA encoding (`MediaType[63:60]|NodeID[59:48]|Offset[47:0]`, 2^60 partition) — `src/types.rs`.
- Publish-after-write index protocol — `gva/index.rs` (PENDING invisible until PUBLISHED).
- Write-time layer-major scope layout in pre-allocated single-extent files, 4 KiB-aligned — `cio/layout.rs` (alignment discipline carried over verbatim; `FILE_FLAG_NO_BUFFERING` is the Windows analog of `O_DIRECT`).
- Sort-merge coalescing with Δgap=64 KiB — `cio/coalesce.rs`.
- Bounded per-worker read windows (O(1) per worker, wait-not-abort) — `cio/stream.rs`.
- Session-granularity reclaim with tombstone-invalidation — `gva/reclaim.rs`.
- Per-worker private file handles — mirrors per-worker `cuFileHandle` (§4).

## Migration path (Level B → Level C)

1. Port `cio/stream.rs` + `cio/layout.rs` to Linux: replace `seek_read/seek_write` with `cuFileRead`/`cuFileBatchIOSubmit` on `cuFileHandleRegister`ed single-extent files (the `SsdLayout`/reader traits in `types.rs` are the seam).
2. Replace the consumer copy (`sink`) with GDS reads directly into a registered GPU HBM staging window; add the CUDA scatter kernel into SGLang's paged KV pool + `cudaEventRecord/cudaStreamWaitEvent` sync (paper §3.4 layer pipeline; our double-buffered window already models the shape).
3. Replace the DRAM arena with pinned hugepages + `cudaHostRegister` for the DRAM-Ready path (`cudaMemcpyAsync`).
4. Wire `KvEngine` (engine_api.rs) behind SGLang's `HiCacheStorage` (`exists/get/put`) — the API shape already matches (T15).
5. Re-introduce the multi-node coordinator (NodeID field is implemented but always 0) with RDMA/NVMe-oF transports.

Hardware needed for step 1 alone: any Linux box with NVMe + a supported NVIDIA GPU + CUDA/GDS userspace. Steps scale with the paper's setup (8×B200 + 3 TB DRAM + 8× PM9A3).
