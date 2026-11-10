# PAPER_ANALYSIS.md — FlatCake: A Flat KVCache Storage Architecture for LLM Serving

Source of truth: `research/paper/flatcake-paper.pdf` (arXiv:2610.38355v1 [cs.DC], 28 Oct 2026, 14 pages).
All quotes/figures below are from the paper. Anything we *add* for the prototype is explicitly marked **[PROTOTYPE]** and listed in `OPEN_QUESTIONS.md`.

---

## 1. Bibliographic record

| Field | Value |
|---|---|
| Title | FlatCake: A Flat KVCache Storage Architecture for LLM Serving |
| Authors | Xin Huang, Teng Ma, Zhuoran Li, Yibo Zhao, Zhaoshi Li, Qianchao Zhu, Wei Yang, Chenxi Wang, Tao Xie, Youwei Zhuo |
| Affiliations | Fudan University; Independent Researcher; Peking University; MetaX Technology; ICT Chinese Academy of Sciences; (Xiaohongshu email domain for one author) |
| Venue | arXiv preprint, arXiv:2610.38355v1 [cs.DC], 28 Oct 2026 |
| Keywords | LLM serving, KV cache management, memory tiering, flat KV storage, GPUDirect Storage |

## 2. Problem statement

Prefix caching lets requests sharing a prefix reuse precomputed KV tensors. Workloads have evolved from short shared system prompts → multi-turn conversations → agentic coding sessions appending tool outputs per turn (production Claude Code trace: **219K input tokens/request average**). Active KV working sets reach **tens of GiB per session**, span hours, far exceed GPU HBM and host DRAM. Recomputing evicted history consumes up to **93% of prefill compute** [12]. Therefore NVMe SSD must serve as a **primary KV cache medium on the critical read path**.

Existing systems (Mooncake, LMCache, SGLang HiCache) organize HBM→DRAM→SSD as a **tiered hierarchy** (DRAM = L2 cache + staging layer) built on two premises:

1. **Temporal filtering** — recency-based DRAM caching captures the active working set; SSD serves only cold misses off the critical path.
2. **Staging and coalescing** — staging SSD accesses through host DRAM buffers reassembles fragmented reads and hides device latency.

The paper shows production LLM workloads violate **both premises simultaneously**.

## 3. Observation 1 — Bimodal reuse defeats DRAM temporal filtering (§2.2, Table 1)

KV reuse falls into two regimes; neither fits a DRAM cache filter.

**Regime A — conversational/API (7 traces):** 53.2–94.0% of blocks accessed only once; reusable prefixes have heavy-tailed reuse distances (mean 2,550 and P99 54,728 intervening requests in Qwen to-B API; 489/4,085 in Mooncake ToolAgent). Admitting every block into DRAM pollutes it with one-shot blocks that flush reusable prefixes to SSD before their next turn. Policy change doesn't help: even when the fast tier holds 40.1% of total blocks, LRU/FIFO/queue-enhanced policies all achieve DRAM hit rates near 20% [12].

**Regime B — agentic coding (Claude Code):** opposite reuse shape, same outcome. Mean reuse distance 0.85 requests (P99 14), only 16.6% one-shot, 98.31% infinite-capacity hit rate — but each request spans **3,421 blocks (218,922 tokens) on average** vs 23 blocks in Mooncake Conversation. Under DeepSeek-V4-Flash FP8, a single request needs **median 0.52 GiB / mean 0.80 GiB history KV per GPU rank** (4.1/6.4 GiB across 8 ranks, 16.0 GiB at P90). Concurrent sessions exceed host DRAM.

**Consequence:** in *both* regimes most cache hits must be served from SSD; migrating blocks between DRAM and SSD adds data movement without keeping hot data in DRAM.

### Table 1 — KV cache reuse statistics (infinite-capacity cache), verbatim

| Workload | Requests | Unique blocks | Once-access | Reuse dist Mean | P99 | Hit rate |
|---|---|---|---|---|---|---|
| Mooncake Conversation | 12,031 | 182,790 | 75.8% | 728 | 5,527 | 36.64% |
| Mooncake Synthetic | 3,993 | 43,924 | 58.4% | 430 | 2,235 | 63.96% |
| Mooncake ToolAgent | 23,608 | 183,300 | 78.5% | 489 | 4,085 | 55.26% |
| Qwen to-C | 43,058 | 2,530,337 | 53.2% | 1,450 | 13,973 | 59.4% |
| Qwen to-B API | 172,800 | 3,830,390 | 94.0% | 2,550 | 54,728 | 61.3% |
| Qwen Thinking | 10,812 | 1,735,015 | 85.7% | 155 | 2,357 | 46.2% |
| Qwen Coder | 43,011 | 5,202,517 | 61.0% | 1,095 | 16,224 | 66.4% |
| Claude Code (agentic) | 98,827 | 5,717,185 | 16.6% | 0.85 | 14 | 98.31% |

Native block sizes: 512 tokens (Mooncake), 16 (Qwen Bailian), 64 (Claude Code); reuse distance in intervening requests. Top 10% of blocks account for 34–65% of accesses in the non-agentic traces (Fig. 2a).

## 4. Observation 2 — DRAM staging and eviction-ordered layout bottleneck SSD restoration

Since SSD serves most hits, end-to-end prefill is governed by the **SSD restoration path**. Raw NVMe bandwidth is not the limit: even with SSD emulated in DRAM at 16.9 GiB/s, KV offloading achieves only 2.0 GiB/s [31].

**(a) Staging concurrency bottleneck.** Tiered restore = SSD → host staging buffers → layer-by-layer H2D → GPU, while background promotion separately fills the DRAM cache. Chunked pipelining bounds single-request time near max(T_SSD→DRAM, T_DRAM→GPU), but the engine must hold staging buffers for every in-flight prefix: staging demand scales **O(context × concurrency)**. Under load, competing writes/restores exhaust the bounded staging pool → prefetches delayed or **aborted** → blocks already on SSD get recomputed.

**(b) Eviction-ordered layout defeats GDS.** Even bypassing DRAM with batched GPUDirect Storage, tiered systems populate/reclaim SSD space in **eviction or FIFO-bucket order**, not session order. A session's blocks end up scattered across non-contiguous SSD offsets; coarse bucket reclamation invalidates active prefixes across sessions. Each NVMe read command needs a contiguous LBA range → restoring a scattered prefix issues **O(layers × blocks) small reads**; a 3.3× SSD bandwidth increase yields only 1.33× throughput gain [12]. As prefix length grows, CPU submission overhead + fragmented reads + GPU stalls push tiered restoration **above the cost of full recomputation** (Fig. 2c).

**Bucket-eviction math (§3.2):** if affected session *i* has re-access probability *p_i*, one bucket eviction invalidates ≥1 active session prefix with probability 1 − Π(1−p_i) → approaches certainty as the number of sessions *k* grows.

**Takeaway (verbatim intent):** tiering is designed for *mutable* pages whose future access patterns and spatial grouping are unknown when written. LLM KV blocks are **write-once, immutable, and reused together at session scope** — so their ideal on-disk grouping is already known at creation time.

## 5. Architecture (§3.1, Fig. 1b, Fig. 3)

FlatCake exposes host DRAM and NVMe SSD as **peer media behind a unified 64-bit Global Virtual Address (GVA) space**. Each block is placed once at write time on its target medium and **never migrates**. The eliminated DRAM middle layer used to do three jobs — spill selection (eviction), latency hiding (pre-staging), reassembly (staging buffers) — which FC-GVA/FC-MAS/FC-CIO replace:

| Question | Component | Answer |
|---|---|---|
| Q1: bind blocks to peer media at creation, O(1) routing? | **FC-GVA** (§3.2) | write-once scope-direct placement; immutable GVA; session-granularity reclaim |
| Q2: schedule across heterogeneous media latencies without DRAM pre-staging? | **FC-MAS** (§3.3) | dual queues, size-aware prefetch priority, arrival-ordered re-admission |
| Q3: coalesce + stream SSD reads into a bounded GPU window? | **FC-CIO** (§3.4) | write-time layer-major layout + read-time sort-merge + layer-pipelined GDS streaming |

## 6. FC-GVA — Global Virtual Address Manager (§3.2)

Enables O(1) write-time binding by exploiting: (1) **reuse scope is observable at request arrival** (shared prompt vs short chat vs agentic session is known when blocks are generated); (2) **asymmetric misprediction cost** — placing a reusable block on SSD is not a cache miss; the hit is served via coalesced GDS at SSD bandwidth instead of DRAM bandwidth (SSD scopes are contiguous by design, §3.4).

### 6.1 Write-once scope-direct placement (Table 2, verified verbatim)

| Reuse scope | Write-time signal | Target medium |
|---|---|---|
| Shared prompt | Prefix refcount > 1 | DRAM (m=0) |
| Short-turn chat | Turn > 1, prior reply < τ | DRAM (m=0) |
| Agentic / Long | Context ≥ L_agent or reply ≥ τ | SSD (m=1) |
| One-shot / Spill | Turn = 1 or DRAM full | SSD (m=1) |

- τ, L_agent are **not given numerical values** in the paper (→ OQ-1/OQ-2).
- A multi-turn/agentic session appending tokens later binds new blocks to the **same session scope** so history stays grouped on its assigned medium.
- Cross-node: a lightweight coordinator tracks aggregate DRAM/SSD capacity and assigns GVAs on writes; capacity coordination stays off the critical read path (reads dominate KV traffic).

### 6.2 Unified 64-bit addressing and routing (verified verbatim)

- **Level 1:** DRAM-resident hash table maps block key **(session ID, layer, token range)** → immutable 64-bit GVA in a single O(1) lookup.
- **Publish-after-write:** to prevent read-after-write hazards *without locking readers*, the GVA is allocated before asynchronous write submission, but the key→GVA entry is published into the Level-1 index **only after the DMA or GDS write callback completes**.
- **Level 2 (the GVA itself):**
  - `MediaType [63:60]` — 4 bits: 0=DRAM, 1=SSD, reserved encodings for future media
  - `NodeID [59:48]` — 12 bits, up to 4,096 nodes
  - `Offset [47:0]` — 48-bit byte offset, up to 256 TiB per node
- MediaType in the MSBs partitions the address space into disjoint numeric ranges: **GVA < 2^60 ⇒ DRAM, GVA ≥ 2^60 ⇒ SSD** → a request's medium is classified by a single integer comparison (max over its blocks' GVAs).
- Register-level bit-mask extraction routes each access to its transport: local DRAM via `cudaMemcpyAsync`, remote DRAM via RDMA, local SSD via GDS cuFile, remote SSD via NVMe-oF.

### 6.3 Session-granularity SSD reclamation

- Zero dirty-block writeback (blocks are never modified in place).
- Atomic reference count tracks **active readers per session scope**.
- When SSD utilization crosses a **high watermark**, reclaim the **oldest unreferenced session extents** by returning their pre-allocated GPU Files to the free pool in **O(1) metadata operations**.
- Contrast: Mooncake allocates SSD pages incrementally across fixed-size buckets recycled FIFO → bucket eviction slices across k distinct sessions and invalidates active prefixes (probability above) → forced recomputation. Session-extent reclaim preserves the contiguous on-disk layout FC-CIO requires.

## 7. FC-MAS — Medium-Aware Request Scheduler (§3.3)

Removing DRAM pre-staging eliminates the O(context × concurrency) staging bottleneck but exposes a **5× bandwidth gap** between local DRAM and NVMe SSD directly to the scheduler (H2D 25–32 GB/s vs SSD 5.5–6.5 GB/s per worker, Table 3). Two hazards: shared FCFS queue → multi-GiB SSD restore blocks sub-ms DRAM hits (head-of-line blocking); strict DRAM-first → large SSD-backed agentic sessions starve. FC-MAS resolves both:

1. **Dual-queue separation.** On arrival: stamp `arrival_ts`, resolve prefix block GVAs (§3.2), classify by **slowest required medium**. All blocks in DRAM (`max_j GVA_j < 2^60`) → **DRAM-Ready Queue**, dispatched immediately. Any SSD block (`max_j GVA_j ≥ 2^60`, including hybrid requests mixing a DRAM-pinned system prompt with SSD-resident history) → **SSD-Preparing Queue**: DRAM-resident prefix blocks are pinned in place while FC-CIO asynchronously prefetches SSD-resident blocks into GPU HBM, overlapping SSD I/O with GPU execution on DRAM-ready requests.
2. **Size-aware HRRN prefetch priority (Eq. 1, verified verbatim):**

   ```
   Priority_i = 1 + wait_time_i / KV_size_i
   ```

   Substitutes SSD KV transfer volume for service time in highest-response-ratio-next [5]. Smaller restores get higher initial priority (lower mean queuing delay); `wait_time_i` grows linearly with waiting, so multi-GiB restores are guaranteed eventual dispatch. *(Recomputation timing of priority is not specified → OQ-4.)*
3. **Arrival-ordered re-admission.** When a prefetch completes, the request's KV is resident in GPU HBM. Instead of tail-appending to the DRAM-Ready Queue, FC-MAS **inserts it ordered by its original `arrival_ts`** — time spent in SSD prefetching already counts toward queue seniority → prevents starvation across media.

## 8. FC-CIO — Coalescing Direct I/O Engine (§3.4)

Hardware constraint: NVMe scatter-gather can place data into non-contiguous GPU pages **only if the source bytes occupy a single contiguous LBA range**. Co-designs three stages as one producer-consumer pipeline (Fig. 5):

1. **Write-time layer-major scope layout.** Attention at layer *l* consumes all cached prefix tokens for layer *l* before layer *l+1* — layout matches this consumption order. Each SSD-assigned session scope maps 1:1 to a **logical GPU File = one pre-allocated single-extent file** in the NVMe File Pool. Within each turn's write batch, blocks are laid out **layer-major** (all token blocks of layer l occupy one physically contiguous byte range); subsequent turns **append** new layer-major segments to the same GPU File. Ideal NVMe read count: **O(layers)** vs O(layers × blocks) for LRU-eviction-ordered placement.
2. **Read-time sort-merge coalescing.** The radix tree queries prefixes at page granularity → hundreds of thousands of descriptors, possibly spanning turns/scopes, arriving out of offset order. FC-CIO sorts descriptors by **(gpu_file_id, offset)**, scans linearly, coalesces adjacent ranges into single large read descriptors. Gaps smaller than a configurable threshold **Δgap = 64 KiB** are **bridged** (bounded extra sequential bytes traded for eliminating NVMe command split overhead). Merged descriptors striped round-robin across worker threads, each with a private `cuFileHandle`, CUDA stream, and `cuFileBatchIOSubmit`.
3. **Layer-pipelined direct GPU streaming.** SGLang's paged KV allocates 256-token HBM pages non-contiguously → direct GDS into page slots would split each layer extent back into per-page DMA. Instead, coalesced layer extents stream via GDS into a **small, double-buffered GPU HBM staging window** (two transformer layers per active stream), then a GPU **scatter kernel** copies 256-token pages into their paged KV slots. While compute executes layer *l*, the I/O stream runs `cuFileBatchIOSubmit` + scatter for layer *l+1*. Staging memory bounded to a **constant O(1) HBM window per GPU**. Under saturation, incoming prefetches **wait on CUDA stream events** rather than revoking in-flight prefetches and recomputing cached tokens.

Fig. 5(c) also depicts two non-GDS restore paths — (i) SSD restore + promotion (SSD copy retained, re-read + staging into host, DRAM KV copy made) and (ii) DRAM restore — used by tiered baselines; (iii) is FlatCake's path. The exact role of (i) inside FlatCake itself is unspecified → OQ-5.

## 9. Implementation facts (§4)

- **~8.8K lines C++17/CUDA + 7.1K lines Python**, integrated into **SGLang [47]** behind the standard **HiCacheStorage interface (`exists`, `get`, `put`)**; no model runner or attention kernel changes.
- **Pinned DRAM pool:** pre-allocated host KV pool via `mmap(MAP_POPULATE | MAP_HUGETLB)` with 2 MB hugepages, registered once with `cudaHostRegister` → no page-fault traps, TLB thrashing, per-transfer pinning.
- **NVMe file pool:** fixed-size single-extent files pre-allocated on **XFS/RAID-0** via `fallocate`; opened `O_DIRECT` + `cuFileHandleRegister`; all logical block offsets and GPU staging slices aligned to **4 KiB** (libcufile DMA requirement, avoids driver bounce buffering).
- **GPU staging:** fixed per-GPU HBM staging window registered **once** (`cuFileBufRegister` — dynamic per-request registration costs ms-scale BAR1/IOMMU mapping); two ping-pong layer buffers; `cudaEventRecord`/`cudaStreamWaitEvent` sync with compute stream.
- **Lock-free Level-1 index:** concurrent open-addressed hash table with 64-bit CAS; GVAs immutable once published; session reclamation frees scopes asynchronously → scheduler queries are **wait-free reads**.

## 10. Evaluation (§5) — author-reported numbers, clearly separated from anything we measure

### Setup (Table 3 verified)
- 8× NVIDIA B200 (180 GB HBM3e each), NVLink 18 links/pair, PCIe Gen5; 2× Xeon Platinum 8570 (112 cores, 4 NUMA); 3 TB host DRAM (1 TiB KV); 8× 3.84 TB Samsung PM9A3 RAID 0 (4 TiB KV); GDS cuFile 5.5–6.5 GB/s per worker.
- Stack: SGLang v0.5.19, Mooncake v0.3.13, CUDA 13.1, GDS 1.16.1 (libcufile).
- Model: DeepSeek-V4-Flash-0731 — 284B hybrid MoE, 43 hybrid MLA layers, 1M context, fp8_e4m3 KV, 256-token pages, 7.5 KiB/token per rank.
- Workload: production Claude Code trace `claude-code-weka-0621` [33] — 98,827 requests, 21.6B input tokens, 393 multi-turn sessions; replayed in recorded arrival order, 0.1–1.0 QPS, **no cache warm-up**.
- Generality: Llama-3.1-70B (dense GQA), TP4 on 4× A100, 8K–128K context sweep.
- Baselines: **+L2 Host DRAM** (SGLang HiCache 2-tier HBM–DRAM LRU, no SSD), **+L3 Mooncake** (3-tier, host-staged SSD reads), **Mooncake-GDS** (GDS transport, tiered eviction-ordered placement retained), **Recompute-only**.
- Metrics: mean/P50 TTFT, total token throughput, **delivered cache hit rate** (only blocks transferred to GPU HBM before prefill), SSD read BW, SSD prefetch latency, normalized SSD read/write amplification.

### Author-reported results (NOT our measurements)
- **E1:** FlatCake hit rate **94.16–94.57%** across load (ceiling 94.72%, within 0.6 pp). Mooncake collapses 87.9% → **64.26%** at ≥0.5 QPS (staging contention → SGLang revokes blocked prefetches → recompute). +L2 flat at 53.30%. At 1.0 QPS: mean TTFT **7.57 s** (1.6× vs Mooncake 12.24 s; 2.3× vs L2 17.51 s); P50 **6.26 s** (1.8× / 2.9×); throughput **98.3K tok/s** (1.6× / 2.3×).
- **E2 (Fig. 7):** FlatCake per-medium hits: SSD 69.3% → 52.6% of hits as load rises; DRAM steady 8.0–11.7%; GPU HBM share grows 13.6% → 33.6% (fast restores keep prefixes HBM-resident). Mooncake L3 contribution collapses 2.8× (51.7% → 18.4%). Write amplification: FlatCake **1.000×** (write-exactly-once); Mooncake 1.007×–1.018×.
- **E3:** FC-MAS ablation (single FCFS queue, FC-GVA/FC-CIO retained): mean TTFT −10–11%, **P99 TTFT −27%**, throughput +12%. FC-CIO: 4.96–5.12 GB/s SSD read BW/worker (5.03 @1 QPS, 1.9× Mooncake 2.69); mean prefetch latency flat 1.47–1.75 s (**1.64 s** @1 QPS) vs Mooncake 3.16 → 9.50 s (**5.8× higher** at peak); Mooncake read amplification **6.51×–6.72×** vs FlatCake 1.000×.
- **Cross-model (Fig. 10):** Llama-3.1-70B: TTFT 2.3×–4.9× vs Recompute-only, 1.5×–2.4× vs Mooncake, up to 1.8× vs Mooncake-GDS. Mooncake-GDS matches FlatCake at 16K (2.31 vs 2.32 s) but degrades to 6.60 s @64K (vs 3.74 s) and 8.6K tok/s (vs 14.0K) → **GDS transport alone is insufficient; write-time layout + coalescing are the essential parts**.

## 11. Assumptions and stated/derived limitations

1. KV blocks are write-once and immutable; no in-place modification, no dirty writeback. (Design foundation.)
2. Reuse scope (session, turn count, prior reply length, prefix refcount, context size) is observable at block creation.
3. Misprediction is asymmetric and cheap: a reusable block on SSD still gets served efficiently (contiguous GDS).
4. Reads dominate KV traffic → capacity coordination off critical read path.
5. Hardware-dependent parts: CUDA, GPUDirect Storage/libcufile (4 KiB DMA alignment), XFS + RAID-0 NVMe, pinned-memory registration, NVLink/PCIe — none available on the target laptop (→ Level-B prototype, Part 16 boundary).
6. SSD scope files are pre-allocated single extents; **file size and pool sizing not stated** (→ OQ-11).
7. Reclamation only for unreferenced scopes; watermark values not stated (→ OQ-12).
8. Single-node focus; multi-node via coordinator + RDMA/NVMe-oF described but not evaluated in detail (→ out of scope for prototype).

## 12. Traceability

Every component above maps to a module + test in `PAPER_TO_IMPLEMENTATION.md`; unresolved items are enumerated in `OPEN_QUESTIONS.md`.
