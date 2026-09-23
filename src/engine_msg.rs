//! Engine-to-UI messages: the single channel every download engine
//! (HTTP, video, torrent) reports progress, completion and failure on.
//! Leaf module (std + tokio + media types only) breaking the
//! `download ↔ video ↔ torrent` import cycles: engines send these,
//! the pump in `download.rs` receives them.

use std::time::SystemTime;

pub(crate) enum EngineMsg {
    Progress {
        downloaded: u64,
        total: Option<u64>,
        /// Torrent upload counters (HTTP sends zeros): shown on the row
        /// while downloading and while seeding toward a seed limit.
        uploaded: u64,
        upload_bps: u64,
    },
    Finished {
        size: u64,
    },
    /// A video row resolved playlist-shaped with no picked entry: the
    /// pump queues one row per item (worker tasks never touch the
    /// main-thread manager) and retires the carrier.
    ExpandPlaylist(crate::media_types::PlaylistInfo),
    Failed(String),
    /// A multi worker finished one piece; the UI thread records it for resume.
    PieceDone(u64),
    /// Multi attempt failed terminally: shrink the file to the completed
    /// prefix (kept bitmap stays valid for a later segmented retry).
    TruncatePrefix,
    /// Fresh multi probe succeeded; the UI thread creates the resume bitmap.
    SegmentsInit {
        total: u64,
    },
    /// The server is throttling parallel connections: the UI thread shrinks
    /// the file to the completed prefix and drops the bitmap, then acks so
    /// the engine may continue single-stream. Handshake (not fire-and-forget)
    /// so a concurrent pause/resume can never observe bitmap without file.
    FallbackSingle {
        ack: tokio::sync::mpsc::Sender<()>,
    },
    /// Server-advertised filename (Content-Disposition). Stored for
    /// adoption at Finished, when the current name qualifies.
    SuggestName(String),
    /// Server-advertised Last-Modified (HTTP date). Stored for application
    /// at Finished when the keep-server-date setting is on. Best-effort:
    /// a missing or unparsable header simply sends nothing.
    LastModified(SystemTime),
    /// A dialog-less row resolved live (its source never marked
    /// it): track it so Stop finalizes the capture instead of killing
    /// it like a stalled VOD attempt.
    LiveDetected,
    /// The server object changed mid-download (version check failed). The
    /// UI thread drops the resume bitmap so a later retry starts fresh
    /// instead of failing on the dead file version forever.
    FailedVersion(String),
    /// Torrent per-piece haves polled from the session (500ms tick).
    /// Replaces the stored bitfield; the block map redraws off progress
    /// ticks arriving on the same tick, so this needs no extra signal.
    TorrentPieces(Vec<bool>),
    /// Free-form phase label from engines whose progress has stages the
    /// byte counters don't capture (video resolve/merge). Applied as the
    /// row detail; the next Progress tick renders over it as usual.
    Phase(String),
}

/// A fresh run found someone else's file at our path (it appeared after
/// dedupe): the pump requeues under a fresh name instead of failing.
pub(crate) const DEST_EXISTS: &str = "Destination already exists";
