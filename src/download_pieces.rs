//! Segmented-piece math: connection counts, piece splits and display-cell downsample.

use crate::file_names::piece_len;

/// Split only with at least this much per connection; small downloads stay single-stream.
const MIN_SEGMENT: u64 = 4 * 1024 * 1024;

/// Largest server-claimed size eligible for splitting; above stays single-stream (preallocates nothing).
pub(crate) const MAX_SEGMENTED_TOTAL: u64 = 1 << 40;

/// Display cells the bitmap downsamples to, so every row renders the same compact strip.
pub(crate) const BLOCK_CELLS: usize = 256;

/// Connection count: 2..=16, never more than one per MIN_SEGMENT.
pub(crate) fn split_count(total: u64, connections: usize) -> usize {
    (connections.max(1) as u64).min(total / MIN_SEGMENT).min(16) as usize
}

/// Downsample a bitmap to `n` cells (half-done reads done); empty in, empty out.
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

/// Split `total` into `(start, end)` pieces; empty when too small or too big to trust (caller goes single-stream).
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
/// Resume bitmap persisted in the queue file; `done[i]` covers `[i*piece_len(total), min((i+1)*piece_len(total), total))`.
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

    /// Completed prefix in bytes, capped at `total`.
    pub(crate) fn prefix_len(&self) -> u64 {
        (self.done.iter().take_while(|b| **b).count() as u64 * piece_len(self.total))
            .min(self.total)
    }

    /// Forget pieces from the first gap on, matching a file truncated to the completed prefix.
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
