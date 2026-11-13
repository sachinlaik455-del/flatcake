//! FC-CIO — cache I/O layer (paper §3.4): SSD scope-file layout (T11/T20),
//! read-time sort-merge coalescing , bounded direct streaming reads
//! (T13/T14). Depends only on the frozen contract (types.rs, config.rs,
//! util.rs); the engine glues cio and gva together later.
//! Conformance notes (ARCHITECTURE §2/§6):
//! - All scope-file reads AND writes use `FILE_FLAG_NO_BUFFERING` handles with
//!   4 KiB-aligned offsets, 4 KiB-aligned buffers and padded lengths .
//! - SSD GVA offset packing: `off[47:0] = file_seq[47:28] | in_file_off[27:0]`.
//! - Appends are layer-major per turn batch, each segment start 4 KiB-aligned.
#![allow(dead_code)]

pub mod coalesce;
pub mod layout;
pub mod stream;

pub use coalesce::{coalesce, CoalesceStats, MergedRange};
pub use layout::{align_up, scope_file_name, AlignedBuf, SsdLayoutImpl, FILE_FLAG_NO_BUFFERING, IO_ALIGN};
pub use stream::{read_ranges, Reader};
