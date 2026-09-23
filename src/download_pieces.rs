//! Segmented-piece math: connection counts, piece splits and the
//! display-cell downsample. Leaf module (file_names only): the
//! engine and row widgets consume these directly.

use crate::file_names::piece_len;

/// A file is split only when it holds at least this much per connection
/// (aria2-style: connections x MIN_SEGMENT), keeping small downloads on the
/// cheaper single-stream path.
const MIN_SEGMENT: u64 = 4 * 1024 * 1024;

/// Largest server-claimed size eligible for splitting. A lying Content-Range
/// would otherwise size a bitmap and sparse file to absurdity; above this,
/// downloads stay single-stream (which preallocates nothing).
pub(crate) const MAX_SEGMENTED_TOTAL: u64 = 1 << 40;

/// Block-map cells the piece bitmap downsamples to for display. Native
/// piece counts vary (up to 4096); the widget aggregates to this width so
/// every row renders the same compact strip.
pub(crate) const BLOCK_CELLS: usize = 256;

/// How many connections a download may use: at least 2 to bother splitting,
/// at most 16, and never more than one per MIN_SEGMENT of file.
pub(crate) fn split_count(total: u64, connections: usize) -> usize {
    (connections.max(1) as u64).min(total / MIN_SEGMENT).min(16) as usize
}

/// Downsample a piece bitmap to `n` display cells: cell `i` covers
/// `bits[i*len/n..(i+1)*len/n)` and reads done when at least half its
/// pieces are. Empty in, empty out.
pub(crate) fn aggregate(bits: &[bool], n: usize) -> Vec<bool> {
    if bits.is_empty() || n == 0 {
        return Vec::new();
    }
    (0..n)
        .map(|i| {
            let (lo, hi) = (i * bits.len() / n, (i + 1) * bits.len() / n);
            let span = hi.saturating_sub(lo).max(1);
            let done = bits[lo..hi.max(lo + 1)].iter().filter(|b| **b).count();
            2 * done >= span
        })
        .collect()
}

/// Split `total` bytes into `piece_len` `(start, end)` pieces (inclusive
/// ends). Empty when the file is too small — or too big to trust — to
/// split: caller uses single-stream.
pub(crate) fn plan_pieces(total: u64, connections: usize) -> Vec<(u64, u64)> {
    if total > MAX_SEGMENTED_TOTAL || split_count(total, connections) < 2 || total == 0 {
        return Vec::new();
    }
    let piece = piece_len(total);
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + piece).min(total) - 1;
        pieces.push((start, end));
        start = end + 1;
    }
    pieces
}
/// Resume bitmap for one segmented download, persisted in the queue file
/// (same JSON shape) so restarts resume segmented instead of starting over.
/// Piece bitmap: `done[i]` covers
/// `[i * piece_len(total), min((i+1) * piece_len(total), total))`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SegmentState {
    pub(crate) total: u64,
    pub(crate) done: Vec<bool>,
}

impl SegmentState {
    pub(crate) fn new(total: u64) -> Self {
        Self {
            total,
            done: vec![false; total.div_ceil(piece_len(total)) as usize],
        }
    }

    pub(crate) fn mark(&mut self, idx: u64) {
        if let Some(slot) = self.done.get_mut(idx as usize) {
            *slot = true;
        }
    }

    /// Missing `(piece index, start, end)` ranges, in order.
    pub(crate) fn missing(&self) -> Vec<(u64, u64, u64)> {
        let piece = piece_len(self.total);
        let mut out = Vec::new();
        for (i, done) in self.done.iter().enumerate() {
            if !done {
                let start = i as u64 * piece;
                out.push((i as u64, start, (start + piece).min(self.total) - 1));
            }
        }
        out
    }

    /// Contiguous completed prefix, in bytes (never past `total`: the tail
    /// piece is usually short, so an uncapped count would overshoot).
    pub(crate) fn prefix_len(&self) -> u64 {
        (self.done.iter().take_while(|b| **b).count() as u64 * piece_len(self.total))
            .min(self.total)
    }

    /// Forget every piece from the first gap on, keeping the bitmap
    /// consistent with a file truncated to the completed prefix. Used
    /// wherever the file is shrunk while the bitmap is kept.
    pub(crate) fn forget_beyond_prefix(&mut self) {
        let mut gap = false;
        for slot in self.done.iter_mut() {
            if !*slot {
                gap = true;
            } else if gap {
                *slot = false;
            }
        }
    }

    /// Bytes already on disk according to the bitmap.
    pub(crate) fn completed_bytes(&self) -> u64 {
        let piece = piece_len(self.total);
        self.done
            .iter()
            .enumerate()
            .map(|(i, d)| {
                if *d {
                    piece.min(self.total.saturating_sub(i as u64 * piece))
                } else {
                    0
                }
            })
            .sum()
    }
}
