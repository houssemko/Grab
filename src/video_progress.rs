//! Progress-line parsing: the `[Grab];` template model, merge/leg
//! detectors and pump helpers. Leaf module (no crate deps): the
//! runner consumes these, tests pin the line shapes directly.

/// Progress reports are throttled to this many bytes between row updates:
/// per-chunk reports would churn the UI for no visible gain, but the bar
/// must still feel live on slow links (HIG: indeterminate-or-smooth,
/// never a frozen bar).
pub(crate) const PROGRESS_GRANULARITY: u64 = 16384;

/// One parsed template line: absolute byte counts (never percents), so
/// callers accumulate instead of re-deriving. `total` already folds the
/// estimate fallback; `None` means unknown (live/unsized), not zero.
/// `speed`/`eta` are parsed and pinned by tests but not consumed — the
/// pump recomputes both from ticks — so they stay (they document the
/// line shape and cost nothing). `finished` marks a leg boundary:
/// yt-dlp prints one `status=finished` line per completed format, and
/// `downloaded_bytes` resets for the next leg.
#[derive(Debug, PartialEq)]
pub(crate) struct YtProgress {
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    pub speed: Option<f64>,
    pub eta: Option<u64>,
    pub finished: bool,
}

pub(crate) fn parse_ytdlp_template(line: &str) -> Option<YtProgress> {
    let rest = line.strip_prefix("[Grab];")?;
    let mut f = rest.split(';');
    let status = f.next()?;
    // "error" lines carry no usable counts; finished lines do.
    if status == "error" {
        return None;
    }
    let num = |s: Option<&str>| {
        s.filter(|v| *v != "NA")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let downloaded = num(f.next()).map(|v| v as u64);
    let total = num(f.next()).map(|v| v as u64);
    let estimate = num(f.next()).map(|v| v as u64);
    let speed = num(f.next());
    let eta = f
        .next()
        .filter(|v| *v != "NA" && *v != "Unknown")
        .and_then(|v| v.parse::<u64>().ok());
    Some(YtProgress {
        downloaded,
        total: total.or(estimate),
        speed,
        eta,
        finished: status == "finished",
    })
}

/// Whether a `--newline` line announces a merge/extract phase.
pub(crate) fn is_ytdlp_merge_line(line: &str) -> bool {
    line.starts_with("[Merger]") || line.starts_with("[ExtractAudio]")
}

/// Final path from `--print after_move:filepath`: a bare absolute
/// path line (every other stdout line carries a `[tag]` prefix).
pub(crate) fn parse_ytdlp_after_move(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    (!trimmed.is_empty() && !trimmed.starts_with('[') && trimmed.starts_with('/'))
        .then_some(trimmed)
}

/// Whether a fresh total starts a new format leg (video→audio)
/// rather than HLS/DASH estimate wobble. Totals are re-estimated per
/// fragment while bytes climb monotonically, so wobble moves the total
/// alone; a new leg moves the total substantially (either direction —
/// the audio leg is usually much smaller, but need not be) AND resets
/// downloaded back near zero (legs download sequentially, so bytes are
/// high when the second leg starts). Unknown bytes count as reset: a
/// leg's first lines may carry no count yet. The first known total
/// always (re)inits. Pure for tests.
pub(crate) fn leg_changed(
    max_total: Option<u64>,
    max_dl: u64,
    total: u64,
    downloaded: Option<u64>,
) -> bool {
    if total == 0 {
        return false;
    }
    match max_total {
        None | Some(0) => true,
        Some(m) => {
            let total_moved = total < m / 2 || total > m.saturating_mul(2);
            total_moved && downloaded.is_none_or(|d| d <= max_dl / 2)
        }
    }
}

/// Newly completed piece indices as byte progress grows against a
/// known total. Shared by the HLS progress tasks so the byte→cell
/// math stays unit-tested in one place.
pub(crate) fn piece_marks(piece_len: u64, marked: &mut u64, downloaded: u64) -> Vec<u64> {
    let mut out = Vec::new();
    if piece_len == 0 {
        return out;
    }
    while *marked < downloaded / piece_len {
        out.push(*marked);
        *marked += 1;
    }
    out
}

/// Trace yt-dlp's selected-format line (`[info] … Downloading N
/// format(s): …`) as it streams past: on success it names what
/// actually downloaded (audit our pick against yt-dlp's sort and
/// id aliasing); on failure the tail below still carries the error.
/// `pending` carries a line split across 4 KiB reads.
pub(crate) fn trace_format_lines(pending: &mut String, chunk: &[u8]) {
    pending.push_str(&String::from_utf8_lossy(chunk));
    while let Some(pos) = pending.find('\n') {
        let line: String = pending.drain(..=pos).collect();
        let line = line.trim_end();
        if is_format_selection_line(line) {
            tracing::info!("{line}");
        }
    }
}

/// Whether a yt-dlp stderr line announces the selected formats.
pub(crate) fn is_format_selection_line(line: &str) -> bool {
    line.contains("Downloading ") && line.contains("format(s)")
}

/// Last non-blank line of captured child output, for error detail.
/// `fallback` names the tool when the output carries nothing usable.
pub(crate) fn last_log_line(output: &str, fallback: &str) -> String {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(fallback)
        .trim()
        .to_string()
}
