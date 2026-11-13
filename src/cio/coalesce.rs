//! cio::coalesce — T12: read-time sort-merge (paper §3.4).
//! Sorts descriptors by `(file_seq, in_file_off)` and linearly merges:
//! - adjacent ranges (`next.start == cur.end`),
//! - overlaps (`next.start < cur.end`, extend by `max(end)`),
//! - bridged gaps (`gap = next.start - cur.end <= gap_bridge_bytes`; Δgap =
//!   64 KiB in the paper — inter-segment padding <= 4095 B is absorbed by
//!   construction, ARCHITECTURE §6).
//! Bridged bytes are read from disk (overread) but belong to NO descriptor:
//! `parts` maps real descriptors only, so scattering never delivers bridged
//! bytes and reconstruction is exact . Merging never crosses file_seq.

use crate::types::FileRange;

/// One coalesced physical read span.
/// `parts[k] = (desc_idx, rel_off)` maps descriptor `desc_idx` into the span,
/// where `rel_off` is the ABSOLUTE in-file offset of that descriptor's data.
/// `part_lens[k]` is the byte length of `parts[k]` (parallel vector).
/// Note: the planned 2-tuple `parts` alone cannot express per-part spans for
/// overlapping/duplicate descriptors (their rel_offs do not partition the span
/// monotonically), and T12 requires exact per-descriptor reconstruction — so a
/// parallel `part_lens` vector is added; the `parts` tuple type is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedRange {
    pub file_seq: u32,
    pub start: u64,
    pub len: u64,
    pub parts: Vec<(usize /*desc_idx*/, u64 /*rel_off*/)>,
    /// Parallel to `parts`: byte length of each part (see struct doc).
    pub part_lens: Vec<u64>,
    /// Bytes inside this span that belong to no descriptor (bridged gaps).
    pub bridged_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoalesceStats {
    pub merged_ranges: u64,
    pub bridged_gaps: u64,
    pub overread_bytes: u64,
}

/// Sort-merge descriptors into physical read spans .
/// Zero-length descriptors are skipped (they carry no bytes; their indices
/// simply never appear in `parts`). For equal sort keys the input order is
/// kept (stable sort); the merged *bytes* are identical for any permutation.
pub fn coalesce(mut descs: Vec<FileRange>, gap_bridge_bytes: u64) -> (Vec<MergedRange>, CoalesceStats) {
    let mut order: Vec<(usize, FileRange)> = descs
        .drain(..)
        .enumerate()
        .filter(|(_, d)| d.len > 0)
        .collect();
    order.sort_by_key(|(_, d)| (d.file_seq, d.in_file_off));

    let mut out: Vec<MergedRange> = Vec::new();
    let mut stats = CoalesceStats::default();

    for (idx, d) in order {
        let d_end = d.in_file_off + d.len;
        let mut merged = false;
        if let Some(cur) = out.last_mut() {
            if cur.file_seq == d.file_seq {
                let cur_end = cur.start + cur.len;
                if d.in_file_off < cur_end {
                    // Overlap (incl. exact duplicates): extend; both
                    // descriptors are kept and both receive their own bytes.
                    cur.len = cur.len.max(d_end - cur.start);
                    cur.parts.push((idx, d.in_file_off));
                    cur.part_lens.push(d.len);
                    merged = true;
                } else if d.in_file_off == cur_end {
                    // Adjacent: merged without bridging accounting.
                    cur.len += d.len;
                    cur.parts.push((idx, d.in_file_off));
                    cur.part_lens.push(d.len);
                    merged = true;
                } else {
                    let gap = d.in_file_off - cur_end;
                    if gap <= gap_bridge_bytes {
                        // Bridged gap: read it (overread) but map no part.
                        stats.bridged_gaps += 1;
                        stats.overread_bytes += gap;
                        cur.bridged_bytes += gap;
                        cur.len += gap + d.len;
                        cur.parts.push((idx, d.in_file_off));
                        cur.part_lens.push(d.len);
                        merged = true;
                    }
                }
            }
        }
        if !merged {
            out.push(MergedRange {
                file_seq: d.file_seq,
                start: d.in_file_off,
                len: d.len,
                parts: vec![(idx, d.in_file_off)],
                part_lens: vec![d.len],
                bridged_bytes: 0,
            });
        }
    }
    stats.merged_ranges = out.len() as u64;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(seq: u32, off: u64, len: u64) -> FileRange {
        FileRange { file_seq: seq, in_file_off: off, len }
    }

    #[test]
    fn adjacent_and_bridges() {
        let descs = vec![d(0, 0, 4096), d(0, 4096, 4096), d(0, 8192 + 2048, 4096)];
        let (m, s) = coalesce(descs, 4096);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].start, 0);
        assert_eq!(m[0].len, 4096 + 4096 + 2048 + 4096);
        assert_eq!(m[0].bridged_bytes, 2048);
        assert_eq!(m[0].parts.len(), 3);
        assert_eq!(s, CoalesceStats { merged_ranges: 1, bridged_gaps: 1, overread_bytes: 2048 });
    }

    #[test]
    fn never_crosses_files() {
        let descs = vec![d(0, 4096, 4096), d(1, 8192, 4096)];
        let (m, s) = coalesce(descs, 65536);
        assert_eq!(m.len(), 2);
        assert_eq!(s.merged_ranges, 2);
        assert_eq!(s.bridged_gaps, 0);
    }
}
