//! cio::stream — T13/T14: bounded direct reads (paper §3.4 layer pipeline).
//!
//! Per-worker [`Reader`] owns its NO_BUFFERING file handles (opened lazily per
//! `file_seq`) and its double-buffered window. [`read_ranges`] walks each
//! merged range in chunks of at most `window_bytes` (rounded down to the 4 KiB
//! I/O alignment, minimum one sector — chunk starts must stay sector-aligned):
//! while the current chunk is scattered to the sinks, the next chunk's
//! `seek_read` is kicked off into the alternate buffer on a scoped thread
//! (ping-pong, paper Fig. 5 layer pipeline).
//!
//! Memory per worker is O(window_bytes) regardless of request size (T14):
//! exactly two window buffers are cycled, no per-request allocation scales
//! with the read. Saturation/backpressure is handled ABOVE (scheduler queues,
//! ARCHITECTURE §3) — a read is never aborted on saturation.
//!
//! Padded read tails beyond the requested bytes are discarded and never
//! delivered; bridged-gap bytes are never delivered either (parts map real
//! descriptors only — T12 reconstruction is exact).
//!
//! `Reader::drop` closes all file handles (default drop of the handle map).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::thread;

use super::coalesce::MergedRange;
use super::layout::{align_up, scope_file_name, AlignedBuf, FILE_FLAG_NO_BUFFERING, IO_ALIGN};
use crate::types::Metrics;

/// Physical chunk step for a logical window: rounded down to a 4 KiB multiple
/// (all chunk starts must be sector-aligned), minimum one sector. For window
/// sizes that are 4 KiB multiples (the engine uses 2 layer-extents) this is
/// exactly `window_bytes`.
fn chunk_step(window_bytes: u64) -> u64 {
    (window_bytes / IO_ALIGN * IO_ALIGN).max(IO_ALIGN)
}

/// Per-worker direct reader (T13/T14). Owns its file handles and its
/// ping-pong window buffers; dropping the Reader closes all handles.
pub struct Reader {
    dir: PathBuf,
    handles: HashMap<u32, File>,
    window: [Option<AlignedBuf>; 2],
    window_step: usize,
}

impl Reader {
    /// New reader over the SSD scope-file directory (`cfg.ssd_dir`).
    pub fn new(dir: impl Into<PathBuf>) -> Reader {
        Reader {
            dir: dir.into(),
            handles: HashMap::new(),
            window: [None, None],
            window_step: 0,
        }
    }

    /// Lazily open a NO_BUFFERING read handle for `file_seq`.
    fn handle_for(&mut self, file_seq: u32) -> io::Result<&File> {
        if !self.handles.contains_key(&file_seq) {
            let path = self.dir.join(scope_file_name(file_seq));
            let f = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_NO_BUFFERING)
                .open(path)?;
            self.handles.insert(file_seq, f);
        }
        Ok(self.handles.get(&file_seq).expect("handle just inserted"))
    }

    /// Take the (possibly resized) window buffers out for one call. Allocates
    /// any missing slot (first use, size change, or a buffer lost on an error
    /// path) so callers always get two usable buffers.
    fn take_window(&mut self, step: usize) -> [Option<AlignedBuf>; 2] {
        if self.window_step != step {
            self.window = [None, None];
            self.window_step = step;
        }
        for slot in &mut self.window {
            if slot.is_none() {
                *slot = Some(AlignedBuf::new(step));
            }
        }
        std::mem::replace(&mut self.window, [None, None])
    }

    /// Return the window buffers so they are reused across calls.
    fn put_window(&mut self, bufs: [Option<AlignedBuf>; 2]) {
        self.window = bufs;
    }
}

/// Read all merged ranges through the bounded window, scattering bytes to
/// `sink(desc_idx, bytes)`. Each descriptor receives exactly its own bytes,
/// in file order (possibly split across several sink calls when a descriptor
/// crosses a chunk boundary — the sink must append). Padded tails and bridged
/// gaps are never delivered.
///
/// Metrics: `physical_read_ops` +1 and `physical_read_bytes` +padded length
/// actually read, per completed `seek_read`.
pub fn read_ranges(
    reader: &mut Reader,
    merged: &[MergedRange],
    window_bytes: u64,
    mut sink: impl FnMut(usize /*desc_idx*/, &[u8] /*desc bytes*/),
    metrics: &mut Metrics,
) -> io::Result<()> {
    if merged.is_empty() {
        return Ok(());
    }
    let step = chunk_step(window_bytes);
    let mut bufs = reader.take_window(step as usize);
    let mut result = Ok(());
    for range in merged {
        if range.len == 0 {
            continue;
        }
        let file = match reader.handle_for(range.file_seq) {
            Ok(f) => f,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        if let Err(e) = read_one_range(file, range, step, &mut bufs, &mut sink, metrics) {
            result = Err(e);
            break;
        }
    }
    reader.put_window(bufs);
    result
}

/// Stream one merged range through the double-buffered window.
fn read_one_range(
    file: &File,
    range: &MergedRange,
    step: u64,
    bufs: &mut [Option<AlignedBuf>; 2],
    sink: &mut impl FnMut(usize, &[u8]),
    metrics: &mut Metrics,
) -> io::Result<()> {
    // Layout contract (T20/ARCHITECTURE §6): every segment start is 4 KiB
    // aligned, hence every merged range start and every chunk start is too.
    assert!(
        range.start % IO_ALIGN == 0,
        "cio::stream: merged range start {} is not 4KiB-aligned (layout contract broken)",
        range.start
    );
    let nchunks = range.len.div_ceil(step);

    thread::scope(|s| -> io::Result<()> {
        // Prefetch handle: the thread owns the alternate buffer and returns it
        // together with its read result (safe ping-pong, no borrows held).
        let mut prefetch: Option<thread::ScopedJoinHandle<(AlignedBuf, io::Result<usize>)>> = None;
        for i in 0..nchunks {
            let cur = (i % 2) as usize;
            let ch_off = range.start + i * step;
            let logical = step.min(range.len - i * step);
            let padded = align_up(logical, IO_ALIGN) as usize;

            // 1. Obtain the current chunk: join the prefetch (which read this
            //    chunk during the previous scatter) or, for the very first
            //    chunk, read synchronously.
            let (buf, nread) = match prefetch.take() {
                Some(h) => {
                    let (b, r) = h
                        .join()
                        .map_err(|_| io::Error::other("cio prefetch reader thread panicked"))?;
                    let n = r?;
                    (b, n)
                }
                None => {
                    let mut b = bufs[cur]
                        .take()
                        .ok_or_else(|| io::Error::other("cio window buffer missing"))?;
                    let n = file.seek_read(&mut b[..padded], ch_off)?;
                    (b, n)
                }
            };
            if (nread as u64) < logical {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short read on scope file (range beyond file end?)",
                ));
            }
            metrics.physical_read_ops += 1;
            metrics.physical_read_bytes += nread as u64;

            // 2. Kick off the NEXT chunk's read into the alternate buffer now,
            //    so it overlaps the scatter below.
            if i + 1 < nchunks {
                let mut nbuf = bufs[1 - cur]
                    .take()
                    .ok_or_else(|| io::Error::other("cio window buffer missing"))?;
                let noff = range.start + (i + 1) * step;
                let nlogical = step.min(range.len - (i + 1) * step);
                let npadded = align_up(nlogical, IO_ALIGN) as usize;
                let f = file; // &File is Send+Sync (positional reads)
                prefetch = Some(s.spawn(move || {
                    let r = f.seek_read(&mut nbuf[..npadded], noff);
                    (nbuf, r)
                }));
            }

            // 3. Scatter this chunk's LOGICAL bytes to per-descriptor sinks.
            //    The padded tail beyond `logical` is discarded (never
            //    delivered); bridged gaps are covered by no part and thus
            //    never delivered either.
            for (k, &(desc_idx, rel_off)) in range.parts.iter().enumerate() {
                let plen = range.part_lens[k];
                let s0 = rel_off.max(ch_off);
                let e0 = (rel_off + plen).min(ch_off + logical);
                if s0 < e0 {
                    sink(desc_idx, &buf[(s0 - ch_off) as usize..(e0 - ch_off) as usize]);
                }
            }
            bufs[cur] = Some(buf);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_step_rounding() {
        assert_eq!(chunk_step(131072), 131072, "aligned window unchanged");
        assert_eq!(chunk_step(10000), 8192, "rounded down to alignment");
        assert_eq!(chunk_step(4095), 4096, "minimum one sector");
        assert_eq!(chunk_step(0), 4096, "minimum one sector");
    }
}
