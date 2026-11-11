# PHASE_2_ARCHITECTURE.md — as-built module map, contract verification, and integration risks (A2)

Written by AGENT 2 (system architect / integration engineer), P2-M2. Scope: verify that the
cross-component contracts are consistent and complete enough to integrate `src/engine.rs`, and
document the architecture **as built** (actual code, not doc claims). Read-only audit; the only
file written is this one. No source was modified, no git commands run.

Verification method: every public signature below was read from the current source (mtimes
2026-11-11 05:20–05:44); contract calls in `src/engine.rs` were checked against callee signatures
line-by-line **and** empirically: a scratch copy of `src/` (outside the repo, temp dir) with
`pub mod engine;` added was compiled with `cargo check` (rustc 1.99.1 GNU). Result: **17 errors,
all in `src/engine.rs`; the rest of the lib (including `src/mas/**` as of 05:44) compiles clean.**
Line numbers below refer to the repo sources as of this writing; the FC-MAS agent is still
in-flight on `src/mas/scheduler.rs` + `tests/mas_tests.rs`, so mas line numbers may shift.

---

## 1. As-built module map (actual files and public APIs)

### 1.1 Frozen contract (Phase-0, unchanged)

| File | Lines | Public API (as built) |
|---|---|---|
| `src/types.rs` | 202 | `MEDIA_DRAM/MEDIA_SSD: u64`; `Gva(u64)` + `encode(media,node,offset)->Option<Gva>`, `media()`, `node()`, `offset()`, `is_ssd()`; `SSD_IN_FILE_BITS=28`, `SSD_FILE_MAX=2^28`; `BlockKey{session,layer,token_start}`; `Medium{Dram,Ssd}`; `Lookup{Miss,Pending,Hit(Gva)}`; `PlacementSignals{session,refcount,turn,reply_tokens,ctx_tokens}`; `Request{id,session,arrival_ts_us,blocks,size_bytes}` + `is_ssd_bound()`; trait `DramArena` (`reserve/write/read/free_extent/used_bytes/capacity`); `ArenaFull`; trait `SsdLayout` (`resolve(gva,len)->FileRange`, `free_extent(file_seq,off,len)`, `logical_bytes()`, `file_capacity()`); `FileRange{file_seq:u32,in_file_off:u64,len:u64}`; `Metrics` (24 counters + 3 sample vectors; `delivered_hit_rate/read_amp/write_amp/throughput_blocks_per_s/mem_high_water_bytes/track_high_water`) |
| `src/config.rs` | 175 | `Config` (25 fields + `block_bytes()`), `Default` (values = ARCHITECTURE §10), `load(path) -> Result<Config,String>` (bounds-checked, rejects unknown keys) |
| `src/util.rs` | 136 | `fnv1a64`, `key_hash(session,layer,token_start)`, `splitmix64`, `payload_fill(session,layer,token_start,seed,buf)`, `pct`, `mean`, `Json` (`kv_raw/kv_u64/kv_f64/kv_str/kv_nested/build`), `stats_json` |

### 1.2 FC-GVA (`src/gva/`, complete per ledger M4; 855 lines)

| File | Lines | Public API (as built) |
|---|---|---|
| `gva/mod.rs` | 18 | `pub mod arena/index/placement/reclaim`; **lock-order doc: registry → {index, arena, layout}; never the reverse** (mod.rs:9–11) |
| `gva/arena.rs` | 176 | `struct Arena`; `Arena::new(cfg: &Config)`, `Arena::with_capacity(bytes)`; `impl DramArena` — `reserve(len)->Result<u64,ArenaFull>` (first-fit free list, then bump), `write(off,buf)`, `read(off,len,out)`, `free_extent(off,len)` (coalescing), `used_bytes()`, `capacity()`. All payload copies under the arena `Mutex`. No inherent duplicates of the trait methods |
| `gva/index.rs` | 245 | `const EMPTY/PENDING/PUBLISHED/TOMBSTONE: u64`; `struct IndexFull`; `struct Index`; `Index::new(capacity: u64)`, `Index::from_config(cfg)`, `capacity()`; `insert_pending(&BlockKey)->Result<usize,IndexFull>` (idempotent for duplicates), `publish(slot,Gva)->bool`, `rollback(slot)->bool`, `invalidate(&BlockKey)->bool`, `lookup(&BlockKey)->Lookup`. Lock-free atomics; hash stored by the CAS winner only (D21) |
| `gva/placement.rs` | 55 | `pub fn decide(sig:&PlacementSignals, sticky:Option<Medium>, cfg:&Config)->Medium` — frozen §4 precedence verbatim; `Some(Ssd)` short-circuits |
| `gva/reclaim.rs` | 361 | `struct ScopeState{medium,creation_ts_us,reader_refs,writer_refs,dram_extents:Vec<(u64,u64)>,ssd_extents:Vec<(u32,u64,u64)>,index_keys:Vec<BlockKey>}`; `struct ScopeRegistry` (`new`, `create_scope(session,medium,ts)->bool` idempotent, `add_dram_extent`, `add_ssd_extent`, `add_key`, `pin_reader(session)->bool`, `unpin_reader`, `pin_writer(session)->bool`, `unpin_writer`, `sticky_medium(session)->Option<Medium>`, `flip_to_ssd(session)->bool`, `drop_scope`, `scope_exists`); `enum ReclaimStore<'a>{Dram(&'a dyn DramArena), Ssd(&'a dyn SsdLayout)}` (Clone+Copy); `pub fn reclaim_pass(medium,usage_bytes,cfg:&Config,registry:&ScopeRegistry,index:&Index,store:ReclaimStore<'_>,metrics:&mut Metrics)->u64`. Registry `Mutex` held per-victim across select→tombstone→free→remove (reclaim.rs:283–354) |

### 1.3 FC-CIO (`src/cio/`, complete per ledger M6; 881 lines)

| File | Lines | Public API (as built) |
|---|---|---|
| `cio/mod.rs` | 19 | `pub mod coalesce/layout/stream`; re-exports `coalesce, CoalesceStats, MergedRange, align_up, scope_file_name, AlignedBuf, SsdLayoutImpl, FILE_FLAG_NO_BUFFERING, IO_ALIGN, read_ranges, Reader` |
| `cio/layout.rs` | 485 | `IO_ALIGN=4096`, `FILE_FLAG_NO_BUFFERING=0x0200_0000`, `align_up(v,a)`, `scope_file_name(seq)->"gf_{seq:06}.bin"`; `struct AlignedBuf` (`new(len)`, `len()`, Deref/DerefMut `[u8]`, zero-filled, 4 KiB-aligned); `struct SsdLayoutImpl`; inherent: `new(cfg)->io::Result<Self>` (pre-allocates `gpu_file_pool` files), **`append_scope_batch(session, layers, blocks_per_layer, provider: impl FnMut(u32,u32)->Vec<u8>) -> io::Result<Vec<Gva>>`** (allocator decision under `inner` lock; provider + NO_BUFFERING `seek_write` under the per-file `Mutex`), **`release_session(session)->bool`** (inherent; D24 notes "engine calls it"); `impl SsdLayout`: `resolve(gva,len)->FileRange` (`file_seq = off>>28`), `free_extent(file_seq,off,len)` (O(1) metadata; whole-file recycle when `freed==appended`), `logical_bytes()`, `file_capacity()` |
| `cio/coalesce.rs` | 138 | `struct MergedRange{file_seq,start,len,parts:Vec<(usize,u64)>,part_lens:Vec<u64>,bridged_bytes:u64}` (D23 parallel `part_lens`); `struct CoalesceStats{merged_ranges,bridged_gaps,overread_bytes}`; `pub fn coalesce(descs: Vec<FileRange>, gap_bridge_bytes: u64) -> (Vec<MergedRange>, CoalesceStats)` (sort-merge by `(file_seq,in_file_off)`, never crosses files) |
| `cio/stream.rs` | 238 | `struct Reader` (`new(dir: impl Into<PathBuf>) -> Reader` — not a Result; lazy per-file NO_BUFFERING handles; 2 ping-pong window buffers); **`pub fn read_ranges(reader:&mut Reader, merged:&[MergedRange], window_bytes:u64, sink: impl FnMut(usize /*desc_idx*/, &[u8] /*desc bytes*/), metrics:&mut Metrics) -> io::Result<()>`** — 5 parameters. Contract at stream.rs:96–100: *"each descriptor receives exactly its own bytes, in file order (possibly split across several sink calls when a descriptor crosses a chunk boundary — the sink must append)"*. Metrics: `physical_read_ops`/`physical_read_bytes` incremented by callee |

### 1.4 FC-MAS (`src/mas/`, in flight as of 05:44; 400 lines; **compiles clean**)

| File | Lines | Public API (as built) |
|---|---|---|
| `mas/mod.rs` | 27 | `pub mod scheduler`; re-exports `hrrn_priority, Clock, PoppedReq, Scheduler, SchedulerStats`; `pub fn classify(req:&Request)->bool` (`req.is_ssd_bound()`); doc: hybrid requests are SSD-bound, *"their DRAM blocks stay pinned in place"* (mod.rs:20–24) |
| `mas/scheduler.rs` | 373 | `pub fn hrrn_priority(wait_s, size_bytes)->f64` (Eq. 1; `size==0 → 1 MiB` guard); `type Clock = Box<dyn Fn()->u64 + Send+Sync>`; `struct PoppedReq{req:Request, wait_us:u64}`; `struct SchedulerStats{dram_queue_len, ssd_queue_len, dram_popped, ssd_popped, readmitted_count, max_dram_wait_us, max_ssd_wait_us, wait_samples:Vec<u64>, hrrn_scans}`; `struct Scheduler`: **`new(queue_cap:usize)->Arc<Self>`** (returns an already-wrapped `Arc`), `with_clock(queue_cap,clock)->Arc<Self>`, `queue_cap()`, **`enqueue(req,ssd:bool)`** (blocks on condvar at cap; no-op after shutdown), **`pop_dram()->Option<PoppedReq>`** (blocks; `None` only after shutdown), **`pick_ssd()->Option<PoppedReq>`** (HRRN argmax, lazy; picked req moves to `awaiting` table), **`readmit(req:Request)`** (restores seq_no + enqueued_us from `awaiting`; keys by ORIGINAL `arrival_ts_us`), `cancel(req_id)->bool`, `shutdown()` (idempotent, wakes all), `stats()->SchedulerStats` |

### 1.5 Orchestrator-owned integration layer

| File | Lines | Public API (as built) |
|---|---|---|
| `src/engine_api.rs` | 60 | `PutSpec{session,turn,layers,start_blk,n_blk,shared,reply_tokens,ctx_tokens,seed}` + `block_key(layer,i,block_tokens)`, `n_blocks()`; `GetSpec{id,arrival_ts_us,blocks:Vec<BlockKey>,seed_by_block:Vec<u64>,signals:PlacementSignals}`; trait `KvEngine: Send+Sync` — `put_batch(&PutSpec)->io::Result<()>`, `get_request(&GetSpec, &mut dyn FnMut(usize,&[u8]))->io::Result<()>`, `snapshot()->Metrics`, `mem_used()->u64`, `shutdown()` |
| `src/hbm.rs` | 97 | `struct HbmWindow` (`new(cap_bytes)`, `used_bytes()`, `get(session,layer,token_start)->Option<&[u8]>` (no recency refresh), `record_delivery(session,layer,token_start,bytes:&[u8])` (LRU-by-delivery, oversize never resident), `invalidate(session,layer,token_start)`) |
| `src/engine.rs` | 647 | `FlatCakeEngine::new(cfg:&Config)->io::Result<FlatCakeEngine>`; `impl KvEngine` (5 methods); internals: `EngineCore{cfg,hbm,index,arena,registry,layout,metrics,scheduler,slots,next_id}`, `Slot{out,map,done}` + per-request `(Mutex<Slot>,Condvar)` in `slots:HashMap<u64,_>`; workers `dram_worker` / `ssd_worker` (2+2 threads per config), `finish_slot`; `SSD_WINDOW_BYTES = 8 MiB` const (engine.rs:39, **not** config-driven). **NOT declared in `lib.rs`** — build is red only for that reason (see §2) |
| `src/baseline.rs` | 686 | `BaselineEngine::new(cfg)->io::Result<Self>` + `impl KvEngine`; internal `DramCache` (LRU, first-fit+bump), `SsdBuckets` (FIFO `bucket_count × bucket_bytes`, NO_BUFFERING, cross-session invalidate), `StagingPool` (`reserve(len,timeout,retries)->Option<u64 waited>`; exhaustion → abort → recompute). Lock discipline: locks acquired one at a time, never nested (comment at baseline.rs:499–500) |
| `src/replay.rs` | 302 | `run(engine:Arc<dyn KvEngine>, records:&[TraceRecord], cfg:&Config, time_scale:f64, csv_window_us:u64, csv_path:Option<&str>)->ReplayOutput{metrics,elapsed_s,integrity_failures,requests_issued}`; 32 worker threads; open-loop pacing at `t0 + ts_us×time_scale`; in-flight cap = `queue_cap` (blocks issue thread); integrity = regenerated payload vs delivered bytes; request-level `lat_samples` replace engine samples at the end |
| `src/results.rs` | 113 | `write_run_json(cfg,system,seed,trace,m,elapsed_s,integrity_failures,requests_issued,path)` — §11 schema + additive fields `trace, requests_issued, integrity_failures, elapsed_s` |
| `src/workload.rs` | 456 | `Op{Put,Get}`; `TraceRecord` (12 fields incl. `shared_with`) + `to_jsonl/parse_line`; `parse_trace(path)`; `GenParams`; `generate(&GenParams)->Result<(u64,u64),String>` (regimes `agentic|oneshot|shift|mixed`; session-size + SSD-budget guards; uniform `ts_us` pacing) |
| `src/lib.rs` | 20 | declares `baseline, config, engine_api, hbm, types, util, workload, gva, cio, replay, results, mas` — **`pub mod engine;` absent (pending MAS-landing announcement; see PHASE_2_STATUS.md)** |
| `src/main.rs` | 92 | `fckv selftest`, `fckv gen-workload --...`; uses `Config::default()` — no subcommand loads `fckv.conf` yet (bench/demo pending M7/M9) |

---

## 2. Contract verification (caller assumptions vs callee reality)

### 2.1 engine → gva

| Call site (engine.rs) | Callee reality | Verdict |
|---|---|---|
| `Index::from_config(cfg)` :74; `insert_pending(&key)` :253,287,325; `publish(slot,gva)` :171,268,289,344; `rollback(slot)` :276,372; `lookup(b)` :406 | index.rs:91,102,144,157,207 — exact match | OK |
| `Arena::new(cfg)` :75; `reserve/write/read` :166–168,264–266,558; `used_bytes` :130,518; `capacity` :132 | arena.rs:41,64,99,111,167,173 (trait impl only — **no inherent methods**) | **MISMATCH M1**: engine never imports `DramArena` (import list engine.rs:26–28 has no trait) → 8 × E0599 at :130,132,166,168,264,266,518,558. Fix: add `use crate::types::DramArena;` |
| `ScopeRegistry::new/sticky_medium/create_scope/flip_to_ssd/pin_writer/unpin_writer/pin_reader/unpin_reader/add_dram_extent/add_ssd_extent/add_key` :116–124,163,177,221,224,225,242,378,392,490,271,294,352 | reclaim.rs:87–230 — signatures match; `pin_reader()->bool` correctly consumed at :489 | OK (but see §5 R8: return values of `pin_writer/add_*` ignored) |
| `reclaim_pass(Medium,usage,&cfg,&registry,&index,ReclaimStore::Dram(&arena)/Ssd(&layout),&mut metrics)` :134–142,147–155 | reclaim.rs:247–255 — exact match; `ReclaimStore` enum coerces `&Arena`→`&dyn DramArena`, `&SsdLayoutImpl`→`&dyn SsdLayout` | OK |
| `placement::decide(sig,sticky,&cfg)` :117 | placement.rs:28 — exact match; precedence table verbatim §4 | OK |

### 2.2 engine → cio

| Call site (engine.rs) | Callee reality | Verdict |
|---|---|---|
| `SsdLayoutImpl::new(cfg)?` :77 | layout.rs:221 → `io::Result<Self>` | OK |
| `append_scope_batch(session,layers,n_blk,\|l,i\|…Vec<u8>)` :183,206,279–284,330–340 | layout.rs:257–263 — exact match; provider length validated == `block_bytes` (layout.rs:325); GVA order layer-major | OK |
| `layout.logical_bytes()` :144; `layout.resolve(gva,len)` :189,212,293,351,598 | layout.rs:445,402 — **trait methods only** | **MISMATCH M1 (cont.)**: engine never imports `SsdLayout` → 6 × E0599 at :144,189,212,293,351,598. Fix: add `use crate::types::SsdLayout;` |
| `layout.release_stale_sessions()` :156 | layout.rs:394 has **`release_session(session: u64) -> bool`** (inherent, added per ledger D24 "engine calls it") | **MISMATCH M2**: name + arity differ (E0599 at :156). Deeper gap: even `release_session` needs the reclaimed session ids, and `reclaim_pass` returns only freed byte counts (reclaim.rs:360) — the engine has no way to learn which sessions to detach. See §5 R3 |
| `coalesce(descs, gap_bridge_bytes)` :601 | coalesce.rs:50 — `(Vec<FileRange>, u64) -> (Vec<MergedRange>, CoalesceStats)`; stats fields used :604–606 exist | OK |
| `read_ranges(reader,&merged,SSD_WINDOW_BYTES,\|desc_idx,bytes\|…)` :608–618 | stream.rs:104–110 — **5 parameters, the 5th being `metrics:&mut Metrics`** | **MISMATCH M3**: engine supplies 4 args (E0061 at :608). Consequences beyond the compile error: as long as `metrics` is not passed, `physical_read_ops`/`physical_read_bytes` are **never recorded by FlatCake** → `read_amp` is structurally 0.0 for the FlatCake system (only baseline.rs:612–615 fills read metrics). Fix note: the naive fix deadlocks — the engine sink (:614–616) locks `core.metrics` inside the callback; the `&mut Metrics` argument must come from a guard taken **once, outside** `read_ranges`, and the sink must reuse it |
| `Reader::new(&ssd_dir)` :95 | stream.rs:52 — `new(dir: impl Into<PathBuf>) -> Reader`; `&String` coerces via `AsRef<OsStr>` | OK |

### 2.3 engine → mas

| Call site (engine.rs) | Callee reality | Verdict |
|---|---|---|
| `Scheduler::new(cfg.queue_cap)` wrapped again: `scheduler: Arc::new(Scheduler::new(...))` :79 | scheduler.rs:160 — **`new` already returns `Arc<Self>`** | **MISMATCH M4**: field type becomes `Arc<Arc<Scheduler>>` — E0308 at :79. Fix: drop the outer `Arc::new` (deref makes the rest of the code work unchanged) |
| `enqueue(req, is_ssd)` :473; `pop_dram()` :543; `pick_ssd()` :575; `readmit(Request{…})` :625; `stats().wait_samples` :513; `shutdown()` :523; `PoppedReq{req,..}` :545,577 | scheduler.rs:198,227,255,295,359,347 — all match; `None` from `pop_dram`/`pick_ssd` only on shutdown → engine's `None => break` is correct | OK |
| Classification: engine hand-rolls `is_ssd = ssd_count > 0` (:442) instead of calling `mas::classify` (:25) | Semantically identical (any-SSD ⇔ ssd_count>0) | OK, style only |

### 2.4 replay → engine (`KvEngine`)

| Call site (replay.rs) | Callee reality | Verdict |
|---|---|---|
| `Arc<dyn KvEngine>`; `put_batch(&spec)` :100; `get_request(&spec, &mut \|idx,bytes\|…)` :107–115; `snapshot()` :212,236 | engine_api.rs:51–60 — signatures match; both engines implement the trait | OK (compiles in the scratch check) |
| Integrity check compares delivered bytes against regenerated payload; blocks never delivered (see R5) skip the sink silently → **integrity failures will NOT catch undelivered blocks** | — | Gap noted in §5 R5 |

**Empirical error census (scratch `cargo check`, engine declared):** 17 errors = 14 × E0599 missing-trait-imports (M1) + 1 × E0599 `release_stale_sessions` (M2) + 1 × E0061 `read_ranges` arity (M3) + 1 × E0308 `Arc<Arc<Scheduler>>` (M4). Warnings: unused `MEDIA_DRAM` import (engine.rs:27 — the code writes `Gva::encode(0,0,off)` with a literal); unneeded `mut` in baseline.rs:411 and results.rs:55,58.

---

## 3. State-ownership audit (ARCHITECTURE §3) and lock-order audit

### 3.1 Single-source-of-truth per state

| State (§3) | As-built owner | Duplicated? |
|---|---|---|
| key → GVA | `gva::index` atomics | No. `ScopeState.index_keys` (reclaim.rs:77) is a reclaim-bookkeeping copy of keys, not a second lookup truth — consistent by construction (reclaim re-resolves each key through the index, reclaim.rs:309–323) |
| DRAM block bytes | `gva::arena` `Mutex<Pool>` | No (baseline's `DramCache` is the separate baseline system, as designed) |
| GPU Files, append offsets, scope map, per-file free lists | `cio::layout` `Inner{sessions,files,free_files}` + per-file `Mutex<FileMeta>` | **Partial**: the layout owns append cursors + free lists; the registry separately owns which extents belong to which scope (`ssd_extents`). These are complementary, not duplicates — but they are only reconciled by the engine calling `release_session` at reclaim time, which is exactly the broken call (M2). Until fixed, `layout.sessions` can outlive a reclaimed registry scope (see §5 R3: stale append cursor into a recycled file) |
| Read staging windows | `cio::stream::Reader` per ssd-worker | No. Note: window size is a hard-coded `SSD_WINDOW_BYTES = 8 MiB` (engine.rs:39), not "2 layer-extents" as §3/§7 describe and not config-driven (deviation, harmless: bounded O(1) per worker regardless) |
| Queues | `mas::scheduler` `Mutex<Inner>` + `Condvar` | No. `awaiting` table (in-prefetch) is scheduler-internal, single source |
| Reader/writer refcounts, watermarks | `gva::reclaim` (registry `Mutex` + per-scope atomics) | No |
| hbm window | `engine.rs` (`Mutex<HbmWindow>`), same type+policy used by baseline | No — identical budget/policy both systems as §7 requires |
| Baseline LRU/staging/buckets | `baseline.rs` | No |

### 3.2 Lock-order doc vs actual acquisition graph

Documented order (gva/mod.rs:9–11, reclaim.rs:23–26): `registry → {index, arena, layout}`; reclaim_pass holds the registry across select→tombstone→free→remove (reclaim.rs:280–354) and takes arena/layout locks inside — engine complies because it enters reclaim only via `reclaim_pass`, and registry methods never nest (pin/unpin take and drop the mutex).

Additional edges that exist in as-built code (all leaf-ward; **no cycle found**):

| Edge | Where | Reverse edge? |
|---|---|---|
| metrics → registry / arena / layout | `maybe_reclaim` holds the `metrics` guard across the whole `reclaim_pass` (engine.rs:129–157) | None: reclaim takes `&mut Metrics` (no lock); no code locks metrics while holding registry/arena/layout |
| hbm → metrics | get_request hbm-hit branch keeps the hbm guard while locking metrics (engine.rs:397–404) | None (delivery loop locks hbm without metrics, engine.rs:496–500) |
| slot → metrics; slot → layout | dram_worker locks metrics under the slot guard (engine.rs:551–563); ssd_worker calls `layout.resolve` under the slot guard (engine.rs:588–600) | None |
| metrics → scheduler | `snapshot` (engine.rs:511–514) | None (scheduler never locks metrics) |
| layout-internal: `inner` → per-file meta | layout.rs:274–316, 339–369 — consistent one-way nesting; per-file write I/O happens under the per-file mutex only (by design, §3 "file writes sequential") | — |

Verdict: the documented gva order is honored; the engine adds metrics/hbm/slot edges that extend the order without violating it. Suggestion for the orchestrator (doc-only): record the extended order (metrics and hbm/slot as leaves below {registry, arena, layout}) in gva/mod.rs or the engine header so future modules don't invert it. One long-hold caveat: `maybe_reclaim` holds `metrics` across index probing + per-extent frees — cheap (all O(1)/metadata), so no I/O-under-lock violation (the §3 invariant holds; both `free_extent` impls are metadata-only, arena.rs:126, layout.rs:415).

---

## 4. Lifecycle and error semantics (as built)

### 4.1 Index slot states (index.rs:55–58)

```
EMPTY --insert_pending CAS--> PENDING --publish--> PUBLISHED
  ^                              |                     
  |                     rollback CAS                     invalidate CAS (reclaim)
  +--------- TOMBSTONE <--------+------------------------+
```
- TOMBSTONE is reusable only by new inserts (`insert_pending` accepts EMPTY|TOMBSTONE); lookups skip it; EMPTY terminates the probe.
- Idempotent duplicate insert returns the existing PENDING/PUBLISHED slot (index.rs:123–129); `publish` on a non-PENDING slot is a no-op returning false (index.rs:144–153) — re-puts and racing recompute-persists cannot clobber a published GVA.
- Undefined/absent transition: **the documented "startup asserts unique blocks ≤ 0.7·capacity" (ARCHITECTURE §3, also claimed in index.rs:4's doc comment) does not exist anywhere in code.** The only full behavior is `insert_pending → Err(IndexFull)` at 100% capacity. In the engine this error is handled in the DRAM path (error + break, engine.rs:256–262) but **silently dropped** in the SSD path (`.ok()` at engine.rs:325 → the block is written to SSD, its GVA returned, but never published → permanent recompute-miss for that block, plus space leak until reclaim). Also `recompute_persist` drops it (`.ok()`, engine.rs:164).

### 4.2 Request lifecycle (get)

1. Caller thread: `next_id` → classify blocks (hbm window → index lookup) → reader pin (engine.rs:392, before resolution, per D15; `false` = scope already reclaimed → subsequent lookups return Miss → recompute, which is the documented clean-miss semantics) → recompute path runs **inline on the caller thread** (modeled cost + re-persist via `recompute_persist`, engine.rs:430–437) → remaining blocks routed to exactly one queue (SSD iff any SSD block) with a per-request `Slot` registered in `slots` map → condvar wait.
2. DRAM queue: `dram_worker` pops in `(arrival_ts_us, seq_no)` order, arena-reads each map entry, counts `hit_dram`, `finish_slot` (done=true, notify, remove from map).
3. SSD queue: `ssd_worker` pops by HRRN → resolve → coalesce → `read_ranges` → per-block `hit_ssd` → **re-admit marker with `blocks=[]` by original `arrival_ts_us`** → a dram_worker pops the marker, skips SSD map entries, `finish_slot`.
4. Caller wakes, merges worker bytes into `out`, unpins reader, delivers to sink in original idx order + hbm `record_delivery`, updates `logical_block_gets`/`delivered_requests`.

**Defined-transition gaps (each verified in code):**
- **G1 (hybrid drop)**: a request with both DRAM- and SSD-resident index hits enqueues only the SSD subset (engine.rs:441–452); the DRAM subset is neither delivered nor counted (no hit, no miss) while `logical_block_gets` still counts it. The agentic regime generates exactly these requests (workload.rs:244–257: shared prompt + own history). mas/mod.rs:20–24 says hybrid DRAM blocks "stay pinned in place" but nothing serves them. See §5 R5.
- **G2 (error swallow on SSD reads)**: `let _ = read_ranges(...).ok()` (engine.rs:608–618) — an I/O error yields partial delivery, no error to the caller, request still completes Ok; undelivered blocks are uncounted (metrics + integrity blind).
- **G3 (shutdown with in-flight gets)**: workers exit on `None` (post-shutdown), queued-but-unpopped requests never complete; the waiting caller's `while !guard.done` loop (engine.rs:476–478) has no timeout/abort → hang. Safe today because replay drains to depth 0 before shutdown (replay.rs:219–224), but `fckv demo` (blocking mode, M7) must not shut down with in-flight requests.
- **G4 (post-enqueue drop)**: `enqueue` after `shutdown` silently drops the request (scheduler.rs:201–203) — same hang as G3 by another door.
- **G5 (put vs reclaim race window)**: `scope_medium` (creates the scope) runs before `pin_writer` (engine.rs:240–242); if a reclaim pass removes the scope in between (possible: it may hold extents with zero refs from earlier puts), `pin_writer`/`add_dram_extent`/`add_key` return false and the engine ignores the returns → arena/layout extents and index keys registered nowhere → leaked extents (never reclaimed) and keys that reclaim can never tombstone. Narrow but real; see §5 R8.

### 4.3 Scope lifecycle (registry)

created (`create_scope`, first wins, ts pinned) → medium flip (`flip_to_ssd`, one-way) → pinned (reader/writer refs; reclaim skips) → reclaimed: oldest-first by `(creation_ts_us, session)`, refs must be 0, must own extents of the pass medium; tombstone-this-medium's keys first, free extents, remove when no extents of either medium and no unresolved keys (reclaim.rs:280–358). Mixed-medium scopes retire across two passes (D23 ledger row).

Undefined residue states:
- **S1**: a scope whose keys were kept as `Lookup::Pending` (reclaim.rs:318–321) but whose extents were both emptied → never a victim again (victim filter requires non-empty extents, reclaim.rs:286–295) → permanent registry entry (bounded by session count; benign leak).
- **S2**: the layout-side session entry outliving the registry scope (M2) — see §5 R3; this one is *corrupting*, not just leaking.

### 4.4 Put lifecycle

DRAM scope: `insert_pending` (PENDING pins) → `arena.reserve` → `arena.write` → `publish` → `add_dram_extent` + `add_key`; reserve-fail → `rollback` (PENDING→TOMBSTONE) → `flip_to_ssd` → per-block SSD append via `append_scope_batch(1,1)` → publish under the new GVA (engine.rs:249–312). SSD scope: all slots reserved PENDING before write; publish after `append_scope_batch` returns GVAs; on batch error all slots rolled back (engine.rs:314–376). Writer pin held for the whole batch (D15). Matches the gva contract note "pin_writer before arena.reserve; add extents+keys before unpin_writer".

One semantic wrinkle: the SSD path registers a **single** scope extent `[gvas[0], n×bb]` (engine.rs:350–358) although the physical layout pads between layer segments when `block_bytes % 4096 != 0` (layout.rs:269,287). Exact under the default config (8192 B blocks); geometrically imprecise otherwise. Harmless today because free extents are never individually re-allocated (only whole-file recycle, layout.rs:437–439, and recycle clears the free list) — but it silently assumes `block_bytes` is a 4 KiB multiple, as do `read_ranges`' alignment assert (stream.rs:148–152) and block offsets `i×block_bytes`. Config permits violating configs (e.g. `block_tokens=384`) → runtime panic in the ssd worker. See §5 R7.

---

## 5. Integration risks, ranked (most likely to break compilation / runtime first)

| # | Risk | Evidence | One-line fix |
|---|---|---|---|
| R1 | **`read_ranges` arity + metrics plumbing (E0061)** — engine supplies 4 args; callee takes 5 incl. `&mut Metrics`. Naive fix deadlocks (sink re-locks `core.metrics` inside the callback while the guard passed as the 5th arg is alive) | engine.rs:608–618 vs stream.rs:104–110 | Lock `core.metrics` once before `read_ranges`, pass `&mut m` as the 5th arg, and move the sink's counter updates onto that same guard (no nested lock) |
| R2 | **Missing trait imports `DramArena` + `SsdLayout` (14 × E0599)** | engine.rs:26–28 vs arena.rs:63/layout.rs:399 (trait-only impls) | Add both to the `crate::types::{…}` import in engine.rs (also drop the unused `MEDIA_DRAM` or use it in the two `Gva::encode(0,0,off)` sites) |
| R3 | **`release_stale_sessions()` does not exist; `release_session(session)` does — and nothing tells the engine which sessions were reclaimed.** Unfixed, after an SSD reclaim the layout's `sessions` entry survives; a re-put by that session appends at the old cursor into a file that may have been recycled to another scope → **two sessions appending into one file = silent data corruption** (wrong bytes delivered under valid GVAs) | engine.rs:156 vs layout.rs:394; reclaim_pass returns only a byte count (reclaim.rs:360) | Change `reclaim_pass` to also return the removed session ids (or take an `&mut Vec<u64>` out-param) and have the engine call `layout.release_session(id)` per removed session in the SSD pass |
| R4 | **`Arc::new(Scheduler::new(...))` double-wrap (E0308)** | engine.rs:79 vs scheduler.rs:160 | Delete the outer `Arc::new` — `Scheduler::new` already returns `Arc<Self>` |
| R5 | **Hybrid DRAM+SSD requests silently drop their DRAM-resident blocks** (delivered to nobody; uncounted in hits/misses while `logical_block_gets` counts them; integrity check can't see it). Generated by the primary agentic workload (shared prompt + own history) | engine.rs:441–452; mas/mod.rs:20–24; workload.rs:244–257 | Put the dram_gvas into the same `Slot.map` and have `dram_worker` serve them when the re-admitted marker arrives (paper Fig. 4(b) semantics), or serve them inline before enqueueing the SSD subset |
| R6 | **`fckv.conf` is documented (§1/§10) but does not exist and nothing loads it** (`main.rs` uses `Config::default()`; `config::load` is exercised only by a negative selftest) | repo root; main.rs:43 | Write the §10 defaults into `fckv.conf` and load it in the bench/demo CLIs when they land (M7/M9) |
| R7 | **Alignment assumptions are config-dependent**: `read_ranges` asserts 4 KiB-aligned range starts (stream.rs:148–152), block offsets are `i×block_bytes`, and the single-extent registration (engine.rs:350–358) assumes no inter-layer padding — all true only when `block_bytes % 4096 == 0` (default 8192). A non-multiple config passes config.rs bounds but panics/corrupts at runtime | layout.rs:269,287; config.rs:132–133 | Assert `cfg.block_bytes() % IO_ALIGN == 0` once in `FlatCakeEngine::new` (and in `gen-workload` sizing) so bad configs fail at startup, not mid-run |
| R8 | **Ignored boolean returns on the pin/registration API** (`pin_writer`, `add_dram_extent`, `add_ssd_extent`, `add_key`) — a scope reclaimed in the `scope_medium`→`pin_writer` window leaves extents/keys unregistered → leaked arena bytes + untombstoneable index keys (stale-GVA exposure) | engine.rs:242,269–272,294–301,348,352; reclaim.rs:117–150 | Check the returns (or make them `debug_assert!(…)`); on failure, treat the put as a clean retry (re-create scope + re-pin) rather than writing orphaned extents |
| R9 | **Metric coverage holes that skew headline numbers**: `physical_read_ops/bytes` and `logical_read_bytes` never written by FlatCake (M3 side effect) → `read_amp ≡ 0.0` for FlatCake vs real values for baseline; `mem_high_water` never tracked (`track_high_water` has zero callers in src) → `mem_high_water_bytes ≡ 0` in results; `ssd_over_high_watermark_us` (§5) has no writer anywhere | grep: track_high_water/ssd_over_high_watermark_us/logical_read_bytes | Wire read metrics via R1; call `metrics.track_high_water(self.mem_used())` in `snapshot()` (both engines); record the advisory-watermark dwell time in `maybe_reclaim` |
| R10 | **Shutdown hang for in-flight gets** (G3/G4) — safe for replay (drains first) but a trap for `fckv demo` | engine.rs:476–478, 521–529; scheduler.rs:347–356 | In `shutdown`, after `scheduler.shutdown()`, mark all remaining slots done (error or partial) before joining workers |
| R11 | **Silent `insert_pending(...).ok()` in the SSD put path and recompute_persist** — index-full writes a block that can never be published (permanent recompute + leaked SSD bytes) | engine.rs:325,164 vs engine.rs:253–262 (DRAM path handles it) | Propagate `IndexFull` as `io::Error` in all three paths (the DRAM path already shows the pattern) |
| R12 | **SSD-window size is a hard-coded 8 MiB const** not derived from layers/block_bytes as §7 describes ("2 layer-extents") and not configurable | engine.rs:39 | Either compute `2 × n_blk × block_bytes` at engine construction or add a `ssd_window_bytes` config key (requires §10 amendment) |
| R13 | **Doc drift (low runtime risk, high audit risk)**: (a) the 0.7 load-factor startup assert claimed by §3/index.rs:4 doesn't exist (R11's IndexFull is the only guard); (b) DECISIONS.md D23/D24 numbering collides with the ledger's D23/D24 rows (ledger M6 says "D24: MergedRange.part_lens … release_session", DECISIONS.md has D23=part_lens, D24=chunk-step); (c) §10's `clients = 0` key has no config.rs counterpart (R6-adjacent) | grep evidence in §6 | One ledger errata row for (a)+(b); strike or implement `clients` for (c) |

R1–R4 are exact compile blockers (17 errors total, verified). R5–R9 will surface as wrong results rather than compile failures. R10–R12 are conditional runtime traps. None of these require redesign; all fixes are local.

---

## 6. Config coverage: ARCHITECTURE §10 vs src/config.rs

All 25 keys implemented in `Config` with defaults byte-identical to §10 and bounds-checked (`config.rs:122–146`, power-of-two + watermark ordering at :148–155, unknown-key rejection at :156–173). Every field is consumed:

| §10 key | config.rs field | Consumers | Verdict |
|---|---|---|---|
| dram_pool_bytes, dram_high/low_watermark | ✓ | Arena::new (arena.rs:41); maybe_reclaim + reclaim_pass (engine.rs:131–142; reclaim.rs:261) | OK |
| hbm_window_bytes | ✓ | engine.rs:73; baseline.rs:342 | OK |
| gpu_file_bytes, gpu_file_pool | ✓ | SsdLayoutImpl::new (layout.rs:221–229); ssd cap in maybe_reclaim (engine.rs:145); workload guard (workload.rs:193) | OK |
| ssd_dir / baseline_ssd_dir | ✓ | engine.rs:93 (Reader) + layout; baseline.rs:344 | OK |
| bucket_bytes, bucket_count | ✓ | baseline.rs:344–348; SSD budget in gen-workload (main.rs:59) | OK |
| block_tokens, bytes_per_token | ✓ | block_bytes() everywhere; payload/replay | OK |
| tau_tokens, l_agent_tokens | ✓ | placement::decide (placement.rs:38,42,50) | OK |
| gap_bridge_bytes | ✓ | engine.rs:601 coalesce | OK |
| ssd_high/low_watermark | ✓ | reclaim_pass SSD branch (reclaim.rs:263–266) | OK |
| ssd_workers, dram_workers | ✓ | worker spawn (engine.rs:85–97) | OK |
| queue_cap | ✓ | Scheduler::new (engine.rs:79); replay in-flight cap (replay.rs:131) | OK |
| staging_pool_bytes | ✓ | baseline staging (baseline.rs:349) | OK |
| recompute_us_per_token | ✓ | engine.rs:433–434; baseline.rs:398 | OK |
| index_capacity | ✓ | Index::from_config (index.rs:91) | OK |
| time_scale, csv_window_us | ✓ | passed as explicit params to replay::run (replay.rs:44–46) — no caller yet passes the config values (bench CLI pending) | OK, pending wiring |
| **clients = 0** | **✗ absent** | — | **MISSING**: §10 documents a `clients` key ("0 = open-loop driver") that config.rs neither parses nor allows — a `fckv.conf` containing it is rejected as an unknown key (config.rs:156–173). Implement it or amend §10 (the driver is open-loop-only as built) |

Also: `fckv.conf` itself does not exist at the repo root (ARCHITECTURE §1 shows it in the tree), and no code path loads it today (R6). No unused config fields (all 25 are read somewhere in src).

**Results-schema cross-check (bonus)**: results.rs emits the §11 metrics object faithfully and adds four additive keys (`trace`, `requests_issued`, `integrity_failures`, `elapsed_s`) — additive extension, Python-side safe. One semantic note: the CSV column `delivered` is `hit_hbm+hit_dram+hit_ssd` (hits), not `delivered_requests` (replay.rs:283–295) — consistent with §11's `delivered` header only if read as "delivered blocks".

---

## 7. Bottom line for the orchestrator

The frozen contract (types.rs) is honored by gva, cio, and mas as built; the engine's *logic* matches the documented protocols (pin-before-reserve, publish-after-write, D15 pinning, watermark drain, T9 re-admission, hbm-before-classification). Integration is blocked by exactly four mechanical mismatches (R1–R4, 17 compile errors, all in engine.rs) plus one contract-reconciliation gap that is a silent-corruption risk, not a compile error (R3's `release_session` wiring). R5 (hybrid block drop) is the most important *semantic* defect to fix before any benchmark run because the primary workload generates hybrid requests. Lock-order and state-ownership hold as documented; the extended lock edges (metrics/hbm/slot) are safe but should be written down. The `clients` config key and the 0.7 load-factor assert are the two doc-vs-code claims that should be resolved by amendment rather than implementation.
