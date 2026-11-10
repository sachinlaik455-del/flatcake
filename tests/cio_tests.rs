//! FC-CIO integration tests (T11–T14, T20).
//!
//! Layout: round-trip through real NO_BUFFERING scope-file I/O, layer-major
//! formula, alignment invariant, session-size guard (OQ-11/D20), free/recycle.
//! Coalesce: adjacent/overlap/bridge/gap/cross-file/shuffled/empty/duplicate
//! plus exact reconstruction (bridged bytes never delivered). Stream: bounded
//! read ops, tiny-window chunked correctness, padded-tail discard.
//!
//! Every test uses a unique temp dir (temp_dir + pid + test tag); the repo
//! `data/` dir is never touched.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fckv::cio::{
    coalesce, read_ranges, CoalesceStats, MergedRange, Reader, SsdLayoutImpl,
};
use fckv::config::Config;
use fckv::types::{FileRange, Metrics, SsdLayout};
use fckv::util::payload_fill;

// ---------------------------------------------------------------- helpers --

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fckv_cio_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn cfg_for(dir: &Path, file_bytes: u64, pool: u64) -> Config {
    let mut c = Config::default();
    c.ssd_dir = dir.to_string_lossy().into_owned();
    c.gpu_file_bytes = file_bytes;
    c.gpu_file_pool = pool;
    c
}

/// Deterministic test payload (frozen SplitMix64 formula, seed 7).
fn payload(session: u64, layer: u32, token_start: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    payload_fill(session, layer, token_start, 7, &mut v);
    v
}

/// Build a synthetic scope-file image (buffered setup handle + sync_all so the
/// NO_BUFFERING reader sees every byte), then drop the handle.
fn write_image(dir: &Path, file_bytes: u64, writes: &[(u64, &[u8])]) {
    let p = dir.join(fckv::cio::scope_file_name(0));
    let mut f = File::create(&p).unwrap();
    f.set_len(file_bytes).unwrap();
    for (off, data) in writes {
        f.seek(SeekFrom::Start(*off)).unwrap();
        f.write_all(data).unwrap();
    }
    f.sync_all().unwrap();
}

fn d(seq: u32, off: u64, len: u64) -> FileRange {
    FileRange { file_seq: seq, in_file_off: off, len }
}

fn sentinel(len: usize) -> Vec<u8> {
    vec![0xA5u8; len]
}

fn read_all(
    dir: &Path,
    merged: &[MergedRange],
    window: u64,
    n_descs: usize,
    metrics: &mut Metrics,
) -> Vec<Vec<u8>> {
    let mut bufs: Vec<Vec<u8>> = vec![Vec::new(); n_descs];
    let mut reader = Reader::new(dir);
    read_ranges(&mut reader, merged, window, |idx, bytes| {
        bufs[idx].extend_from_slice(bytes);
    }, metrics)
    .unwrap();
    drop(reader); // close handles before dir cleanup
    bufs
}

// ------------------------------------------------------------------ layout --

/// T11/T20: 3-turn batch (2 layers x 4 blocks), layer-major formula, every
/// in_file_off 4 KiB-aligned, round-trip through the Reader byte-exact.
#[test]
fn t11_roundtrip_layer_major_alignment() {
    let dir = temp_dir("t11_roundtrip");
    let cfg = cfg_for(&dir, 4 << 20, 2);
    let layout = SsdLayoutImpl::new(&cfg).unwrap();

    let session = 5u64;
    let (layers, bpl) = (2u32, 4u32);
    let bb = cfg.block_bytes(); // 8192
    assert_eq!(bb, 8192);
    let seg_len = bpl as u64 * bb; // 32768
    let block_tokens = cfg.block_tokens; // 256

    let mut all_gvas = Vec::new();
    for turn in 0..3u64 {
        let gvas = layout
            .append_scope_batch(session, layers, bpl, |l, i| {
                let tok = (turn * bpl as u64 + i as u64) * block_tokens;
                payload(session, l, tok, bb as usize)
            })
            .unwrap();
        assert_eq!(gvas.len(), (layers * bpl) as usize);
        for l in 0..layers {
            for i in 0..bpl {
                let g = gvas[(l * bpl + i) as usize];
                assert!(g.is_ssd(), "SSD GVA classification");
                assert_eq!(g.node(), 0);
                let fr = layout.resolve(g, bb);
                assert_eq!(fr.file_seq, 0);
                let want = turn * layers as u64 * seg_len + l as u64 * seg_len + i as u64 * bb;
                assert_eq!(fr.in_file_off, want, "layer-major formula, turn {turn} layer {l} block {i}");
                assert_eq!(fr.in_file_off % 4096, 0, "T20 alignment invariant");
            }
        }
        all_gvas.extend(gvas);
    }
    assert_eq!(layout.logical_bytes(), 3 * layers as u64 * seg_len);
    assert_eq!(layout.file_capacity(), cfg.gpu_file_bytes);

    // Read back through the bounded reader (window = 2 layer segments).
    let descs: Vec<FileRange> = all_gvas.iter().map(|g| layout.resolve(*g, bb)).collect();
    let (merged, stats) = coalesce(descs.clone(), 65536);
    assert_eq!(merged.len(), 1, "contiguous scope coalesces into one range");
    assert_eq!(stats, CoalesceStats { merged_ranges: 1, bridged_gaps: 0, overread_bytes: 0 });
    assert_eq!(merged[0].start, 0);
    assert_eq!(merged[0].len, 3 * layers as u64 * seg_len);

    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, 2 * seg_len, descs.len(), &mut metrics);
    assert_eq!(metrics.physical_read_ops, 3, "recorded exact physical read ops (window = 2 segments)");
    assert_eq!(metrics.physical_read_bytes, 3 * layers as u64 * seg_len);
    let per_turn = (layers * bpl) as usize;
    for (idx, buf) in bufs.iter().enumerate() {
        let t = (idx / per_turn) as u64;
        let r = idx % per_turn;
        let l = (r as u32) / bpl;
        let i = (r as u32) % bpl;
        let tok = (t * bpl as u64 + i as u64) * block_tokens;
        assert_eq!(buf, &payload(session, l, tok, bb as usize), "desc {idx} bytes exact");
    }

    drop(layout);
    cleanup(&dir);
}

/// T11/T20: block_bytes NOT a 4 KiB multiple — segment starts stay aligned,
/// inter-segment padding <= 4095 B is bridged at read time and never delivered.
#[test]
fn t11_padding_inter_segment_alignment() {
    let dir = temp_dir("t11_padding");
    let mut cfg = cfg_for(&dir, 1 << 20, 1);
    cfg.block_tokens = 128;
    cfg.bytes_per_token = 33; // block_bytes = 4224 (not 4 KiB-multiple)
    let layout = SsdLayoutImpl::new(&cfg).unwrap();

    let bb = cfg.block_bytes();
    assert_eq!(bb, 4224);
    let bpl = 4u32;
    let seg_len = bpl as u64 * bb; // 16896
    let seg_pad = (seg_len + 4095) / 4096 * 4096; // 20480
    assert_eq!(seg_pad % 4096, 0);
    assert!(seg_pad - seg_len <= 4095, "padding < gap_bridge (ARCHITECTURE §6)");

    // Batch 1: 2 layers -> seg0 @ 0, seg1 @ 20480.
    let gvas1 = layout
        .append_scope_batch(9, 2, bpl, |l, i| payload(9, l, i as u64 * 128, bb as usize))
        .unwrap();
    assert_eq!(layout.resolve(gvas1[0], bb).in_file_off, 0);
    assert_eq!(layout.resolve(gvas1[bpl as usize], bb).in_file_off, seg_pad);
    assert_eq!(layout.resolve(gvas1[0], bb).in_file_off % 4096, 0);

    // Batch 2 (same session): aligns up from 20480 + 16896 = 37376 -> 40960.
    let gvas2 = layout
        .append_scope_batch(9, 1, bpl, |l, i| payload(9, l, 1000 + i as u64 * 128, bb as usize))
        .unwrap();
    assert_eq!(layout.resolve(gvas2[0], bb).in_file_off, 40960);
    assert_eq!(layout.logical_bytes(), 3 * seg_len);

    // Read back via SEGMENT descriptors (segment starts are the aligned units).
    let seg_descs = vec![d(0, 0, seg_len), d(0, seg_pad, seg_len), d(0, 40960, seg_len)];
    let (merged, stats) = coalesce(seg_descs, 65536);
    assert_eq!(merged.len(), 1, "padding gaps (<=4095B) are bridged");
    assert_eq!(stats, CoalesceStats { merged_ranges: 1, bridged_gaps: 2, overread_bytes: 7168 });
    assert_eq!(merged[0].bridged_bytes, 7168);

    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, seg_pad, 3, &mut metrics);
    assert_eq!(metrics.physical_read_ops, 3);
    assert_eq!(metrics.physical_read_bytes, 61440); // 3 x padded 20480
    let mut expect = vec![Vec::new(); 3];
    for i in 0..bpl {
        expect[0].extend_from_slice(&payload(9, 0, i as u64 * 128, bb as usize));
        expect[1].extend_from_slice(&payload(9, 1, i as u64 * 128, bb as usize));
        expect[2].extend_from_slice(&payload(9, 0, 1000 + i as u64 * 128, bb as usize));
    }
    for k in 0..3 {
        assert_eq!(bufs[k].len() as u64, seg_len, "pad bytes never delivered (seg {k})");
        assert_eq!(bufs[k], expect[k], "segment {k} bytes exact (pad excluded)");
    }

    drop(layout);
    cleanup(&dir);
}

/// OQ-11/D20: a scope must fit one gpu_file_bytes file — exact fit is OK,
/// anything beyond errors (InvalidData), for appends and fresh sessions.
#[test]
fn t11_session_size_guard() {
    let dir = temp_dir("t11_guard");
    let cfg = cfg_for(&dir, 1 << 20, 1);
    let layout = SsdLayoutImpl::new(&cfg).unwrap();
    let bb = cfg.block_bytes(); // 8192

    let gen = |salt: u64| move |l: u32, i: u32| payload(salt, l, i as u64 * 256, bb as usize);

    // Exactly 2 x 64 blocks x 8192 = 1 MiB == gpu_file_bytes: allowed.
    layout.append_scope_batch(1, 2, 64, gen(1)).unwrap();

    // Second batch would exceed: error.
    let err = layout.append_scope_batch(1, 1, 1, gen(1)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    // Fresh session exceeding in one batch: error (no scope left behind).
    let err2 = layout.append_scope_batch(2, 2, 65, gen(2)).unwrap_err();
    assert_eq!(err2.kind(), std::io::ErrorKind::InvalidData);
    // Retry with a fitting batch works (allocation was rolled back).
    layout.append_scope_batch(2, 1, 2, gen(2)).unwrap();

    drop(layout);
    cleanup(&dir);
}

/// T6/OQ-11: free_extent is metadata-only accounting (logical_bytes drops);
/// a fully-freed file is recycled for the next scope (same file_seq) and the
/// recycled file carries no stale bytes.
#[test]
fn t6_free_extent_recycle_and_accounting() {
    let dir = temp_dir("t6_recycle");
    let cfg = cfg_for(&dir, 1 << 20, 1);
    let layout = SsdLayoutImpl::new(&cfg).unwrap();
    let bb = cfg.block_bytes();

    // Session 1: 2 layers x 2 blocks -> two 16384B segments, 32768 logical.
    let gvas_s1 = layout
        .append_scope_batch(1, 2, 2, |l, i| payload(1, l, i as u64 * 256, bb as usize))
        .unwrap();
    assert_eq!(gvas_s1.len(), 4);
    assert_eq!(layout.logical_bytes(), 32768);

    // O(1) metadata free of both layer segments (zero payload ops).
    layout.free_extent(0, 0, 16384);
    assert_eq!(layout.logical_bytes(), 16384);
    layout.free_extent(0, 16384, 16384);
    assert_eq!(layout.logical_bytes(), 0);
    assert!(layout.release_session(1), "session detached");

    // Session 2 recycles file 0 (pool has only file 0; a non-recycled
    // allocation would have created gf_000001.bin).
    let gvas2 = layout
        .append_scope_batch(2, 2, 2, |l, i| payload(2, l, i as u64 * 256, bb as usize))
        .unwrap();
    assert_eq!(layout.resolve(gvas2[0], bb).file_seq, 0, "file recycled");
    assert_eq!(layout.logical_bytes(), 32768);

    // Round-trip: recycled file holds session 2's bytes (stale data gone).
    let descs: Vec<FileRange> = gvas2.iter().map(|g| layout.resolve(*g, bb)).collect();
    let (merged, _) = coalesce(descs.clone(), 65536);
    assert_eq!(merged.len(), 1);
    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, 4096, descs.len(), &mut metrics);
    assert_eq!(metrics.physical_read_ops, 8);
    assert_eq!(metrics.physical_read_bytes, 32768);
    for (idx, buf) in bufs.iter().enumerate() {
        let l = (idx / 2) as u32;
        let i = (idx % 2) as u64;
        assert_eq!(buf, &payload(2, l, i * 256, bb as usize));
    }

    drop(layout);
    cleanup(&dir);
}

// --------------------------------------------------------------- coalesce --

/// T12: adjacent merge, no bridging accounting.
#[test]
fn t12_adjacent_merge() {
    let descs = vec![d(0, 0, 4096), d(0, 4096, 4096)];
    let (m, s) = coalesce(descs, 65536);
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].start, m[0].len, m[0].bridged_bytes), (0, 8192, 0));
    assert_eq!(m[0].parts, vec![(0, 0), (1, 4096)]);
    assert_eq!(m[0].part_lens, vec![4096, 4096]);
    assert_eq!(s, CoalesceStats { merged_ranges: 1, bridged_gaps: 0, overread_bytes: 0 });
}

/// T12: overlapping merge extends; both descriptors kept.
#[test]
fn t12_overlap_merge() {
    let descs = vec![d(0, 0, 4096), d(0, 2048, 4096)];
    let (m, s) = coalesce(descs, 65536);
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].start, m[0].len), (0, 6144));
    assert_eq!(m[0].parts.len(), 2);
    assert_eq!(s.bridged_gaps, 0);
    assert_eq!(s.overread_bytes, 0);
}

/// T12: separated ranges — gap larger than the bridge threshold: two ranges.
#[test]
fn t12_gap_not_bridged() {
    let descs = vec![d(0, 0, 4096), d(0, 12288, 4096)]; // gap 4096 > thr 4095?
    let (m, s) = coalesce(descs, 4095);
    assert_eq!(m.len(), 2);
    assert_eq!(s, CoalesceStats { merged_ranges: 2, bridged_gaps: 0, overread_bytes: 0 });
}

/// T12: small gap bridged — exact bridged_gaps/overread_bytes accounting.
#[test]
fn t12_gap_bridged_stats() {
    let descs = vec![d(0, 0, 4096), d(0, 8192, 4096)]; // gap 4096
    let (m, s) = coalesce(descs, 4096);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].len, 12288);
    assert_eq!(m[0].bridged_bytes, 4096);
    assert_eq!(m[0].parts, vec![(0, 0), (1, 8192)]);
    assert_eq!(s, CoalesceStats { merged_ranges: 1, bridged_gaps: 1, overread_bytes: 4096 });
}

/// T12: never merge across file_seq even when numerically adjacent.
#[test]
fn t12_no_merge_across_files() {
    let descs = vec![d(0, 4096, 4096), d(1, 8192, 4096)];
    let (m, s) = coalesce(descs, 65536);
    assert_eq!(m.len(), 2);
    assert_eq!(m[0].file_seq, 0);
    assert_eq!(m[1].file_seq, 1);
    assert_eq!(s.merged_ranges, 2);
    assert_eq!(s.bridged_gaps, 0);
}

/// T12: shuffled input equals sorted-input result (canonical comparison).
#[test]
fn t12_shuffled_equals_sorted() {
    let mut st = 42u64;
    let mut descs = Vec::new();
    for _ in 0..24 {
        let seq = (fckv::util::splitmix64(&mut st) % 2) as u32;
        let start = (fckv::util::splitmix64(&mut st) % 40) * 1024;
        let len = 1024 + (fckv::util::splitmix64(&mut st) % 8) * 1024;
        descs.push(d(seq, start, len));
    }
    // Deterministic shuffle: interleave three strides.
    let n = descs.len();
    let order: Vec<usize> = (0..n)
        .step_by(3)
        .chain((0..n).skip(1).step_by(3))
        .chain((0..n).skip(2).step_by(3))
        .collect();
    let shuffled: Vec<FileRange> = order.iter().map(|&i| descs[i]).collect();

    let canonical = |ms: Vec<MergedRange>| -> Vec<(u32, u64, u64, Vec<(u64, u64)>, u64)> {
        // desc_idx labels differ across permutations (they are positions in
        // the input vec), so compare parts as their byte-span multiset.
        let mut v: Vec<_> = ms
            .into_iter()
            .map(|m| {
                let mut parts: Vec<(u64, u64)> = m
                    .parts
                    .iter()
                    .zip(&m.part_lens)
                    .map(|(&(_, o), &l)| (o, l))
                    .collect();
                parts.sort();
                (m.file_seq, m.start, m.len, parts, m.bridged_bytes)
            })
            .collect();
        v.sort();
        v
    };

    let (a, sa) = coalesce(descs, 65536);
    let (b, sb) = coalesce(shuffled, 65536);
    assert_eq!(canonical(a), canonical(b), "permutation-invariant result");
    assert_eq!(sa, sb);
    assert!(sa.merged_ranges > 0);
}

/// T12: empty input -> empty output, zero stats.
#[test]
fn t12_empty_input() {
    let (m, s) = coalesce(Vec::new(), 65536);
    assert!(m.is_empty());
    assert_eq!(s, CoalesceStats::default());
}

/// T12: duplicate descriptors merge into one span with BOTH parts mapped.
#[test]
fn t12_duplicate_descriptors() {
    let descs = vec![d(0, 0, 100), d(0, 0, 100)];
    let (m, s) = coalesce(descs, 65536);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].len, 100);
    assert_eq!(m[0].parts.len(), 2, "both duplicates mapped");
    assert_eq!(m[0].part_lens, vec![100, 100]);
    assert_eq!(s.bridged_gaps, 0);
}

/// Δgap=0 edge: exact adjacency still merges; a 1-byte gap does not bridge.
#[test]
fn t12_threshold_zero_edge() {
    let adj = vec![d(0, 0, 100), d(0, 100, 100)];
    let (m, s) = coalesce(adj, 0);
    assert_eq!(m.len(), 1);
    assert_eq!(s.bridged_gaps, 0, "adjacency is not a bridge");

    let sep = vec![d(0, 0, 100), d(0, 101, 100)];
    let (m2, s2) = coalesce(sep, 0);
    assert_eq!(m2.len(), 2);
    assert_eq!(s2.bridged_gaps, 0);
}

/// T12 reconstructed bytes: coalesce over a synthetic file image, read through
/// the Reader, scatter to per-desc buffers — exact equality, including a
/// bridged gap whose sentinel bytes are NOT delivered, and duplicate
/// descriptors both receiving their (identical) bytes.
#[test]
fn t12_reconstruction_with_bridged_gap_and_sentinels() {
    let dir = temp_dir("t12_recon");
    // Image: 34 sectors. A @4096..12288, GAP @12288..16384 (sentinel),
    // B @16384..24576, C/D @65536..69636 (duplicates), E @131072..136192,
    // TAIL @136192..139264 (sentinel).
    let cap = 139264u64;
    let pa = payload(1000, 0, 0, 8192);
    let pb = payload(1001, 0, 64, 8192);
    let pc = payload(1002, 0, 128, 4096);
    let pe = payload(1004, 0, 192, 5120);
    write_image(
        &dir,
        cap,
        &[
            (4096, &pa),
            (12288, &sentinel(4096)),
            (16384, &pb),
            (65536, &pc),
            (131072, &pe),
            (136192, &sentinel(3072)),
        ],
    );

    let descs = vec![d(0, 4096, 8192), d(0, 16384, 8192), d(0, 65536, 4096), d(0, 65536, 4096), d(0, 131072, 5120)];
    let (merged, stats) = coalesce(descs.clone(), 4096);
    assert_eq!(merged.len(), 3, "A+B bridged; C+D overlapped; E separate");
    assert_eq!(stats, CoalesceStats { merged_ranges: 3, bridged_gaps: 1, overread_bytes: 4096 });
    assert_eq!(merged[0], MergedRange {
        file_seq: 0,
        start: 4096,
        len: 20480,
        parts: vec![(0, 4096), (1, 16384)],
        part_lens: vec![8192, 8192],
        bridged_bytes: 4096,
    });
    assert_eq!(merged[1].parts, vec![(2, 65536), (3, 65536)], "duplicate parts kept");
    assert_eq!(merged[1].part_lens, vec![4096, 4096]);

    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, 8192, descs.len(), &mut metrics);
    // 3 chunks (R1) + 1 (R2) + 1 (R3, padded 5120 -> 8192) = 5 ops, 32768 B.
    assert_eq!(metrics.physical_read_ops, 5);
    assert_eq!(metrics.physical_read_bytes, 32768);

    assert_eq!(bufs[0], pa, "desc A exact (bridge sentinel excluded)");
    assert_eq!(bufs[1], pb, "desc B exact");
    assert_eq!(bufs[2], pc, "desc C exact");
    assert_eq!(bufs[3], pc, "duplicate desc D receives the same bytes");
    assert_eq!(bufs[4], pe, "desc E exact (padded tail sentinel excluded)");
    // Exact equality above IS the sentinel check: delivered buffers equal the
    // payloads byte-for-byte, so the 0xA5 sentinel regions (bridge gap and
    // padded tails, written into the image) can never be part of them.
    assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), 29696, "delivered = sum(desc lens)");

    cleanup(&dir);
}

// ------------------------------------------------------------------ stream --

/// T13: full-prefix restore of a 2-layer x 8-block scope with
/// window_bytes = 2 * segment_len: physical_read_ops <= merged_ranges
/// (exact count recorded).
#[test]
fn t13_read_ops_bounded_full_restore() {
    let dir = temp_dir("t13_ops");
    let cfg = cfg_for(&dir, 4 << 20, 1);
    let layout = SsdLayoutImpl::new(&cfg).unwrap();
    let bb = cfg.block_bytes(); // 8192
    let (layers, bpl) = (2u32, 8u32);
    let seg_len = bpl as u64 * bb; // 65536

    let gvas = layout
        .append_scope_batch(77, layers, bpl, |l, i| payload(77, l, i as u64 * 256, bb as usize))
        .unwrap();
    assert_eq!(gvas.len(), 16);

    let descs: Vec<FileRange> = gvas.iter().map(|g| layout.resolve(*g, bb)).collect();
    let (merged, stats) = coalesce(descs.clone(), 65536);
    assert_eq!(merged.len(), 1, "whole scope is one physical span");
    assert_eq!(stats.merged_ranges, 1);

    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, 2 * seg_len, descs.len(), &mut metrics);
    assert!(
        (metrics.physical_read_ops as usize) <= merged.len(),
        "T13: O(layers) physical reads (ops={} ranges={})",
        metrics.physical_read_ops,
        merged.len()
    );
    assert_eq!(metrics.physical_read_ops, 1, "recorded exact count");
    assert_eq!(metrics.physical_read_bytes, 131072);
    for (idx, buf) in bufs.iter().enumerate() {
        let l = (idx / 8) as u32;
        let i = (idx % 8) as u64;
        assert_eq!(buf, &payload(77, l, i * 256, bb as usize));
    }

    drop(layout);
    cleanup(&dir);
}

/// T14: tiny window (much smaller than the range) still produces correct
/// bytes — chunked correctness with the window memory bound.
#[test]
fn t14_tiny_window_chunked_correctness() {
    let dir = temp_dir("t14_tiny");
    let cfg = cfg_for(&dir, 4 << 20, 1);
    let layout = SsdLayoutImpl::new(&cfg).unwrap();
    let bb = cfg.block_bytes();
    let (layers, bpl) = (2u32, 8u32);

    let gvas = layout
        .append_scope_batch(78, layers, bpl, |l, i| payload(78, l, i as u64 * 256, bb as usize))
        .unwrap();
    let descs: Vec<FileRange> = gvas.iter().map(|g| layout.resolve(*g, bb)).collect();
    let (merged, _) = coalesce(descs.clone(), 65536);

    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, 4096, descs.len(), &mut metrics);
    assert_eq!(metrics.physical_read_ops, 32, "131072 / 4096 chunks");
    assert_eq!(metrics.physical_read_bytes, 131072, "aligned reads: padded == logical");
    for (idx, buf) in bufs.iter().enumerate() {
        let l = (idx / 8) as u32;
        let i = (idx % 8) as u64;
        assert_eq!(buf, &payload(78, l, i * 256, bb as usize), "chunk {idx} bytes exact");
    }

    drop(layout);
    cleanup(&dir);
}

/// T14/T12: padded read tail beyond the requested bytes is read but NEVER
/// delivered (sentinel bytes past the logical end stay out of the sink).
#[test]
fn t14_padded_tail_discard() {
    let dir = temp_dir("t14_tail");
    // Image: 3 sectors. F @4096..9216 (5120 B, ends mid-sector),
    // sentinel @9216..12288 (the padded tail region).
    let cap = 12288u64;
    let pf = payload(2000, 0, 0, 5120);
    write_image(&dir, cap, &[(4096, &pf), (9216, &sentinel(3072))]);

    let descs = vec![d(0, 4096, 5120)];
    let (merged, _) = coalesce(descs.clone(), 65536);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].len, 5120);

    let mut metrics = Metrics::default();
    let bufs = read_all(&dir, &merged, 4096, descs.len(), &mut metrics);
    // Window 4096: chunk0 [4096,8192) logical 4096, chunk1 [8192,9216)
    // logical 1024 padded to 4096 — tail read, never delivered.
    assert_eq!(metrics.physical_read_ops, 2);
    assert_eq!(metrics.physical_read_bytes, 8192);
    assert_eq!(bufs[0], pf, "exactly the requested 5120 bytes (padded tail excluded)");

    cleanup(&dir);
}
