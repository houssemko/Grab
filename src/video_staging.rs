//! Staging/parts/manifest/resume: scratch dirs, yt-dlp output
//! templates, part-file census, sidecars, the JSON manifest and the
//! resume planner. Leaf module (file_names + video_tools leaves +
//! std/serde): the runner and engine expansion consume these
//! through the `video` facade.

use crate::video_prefs::subtitle_content_languages;
use crate::video_tools::VideoError;
use gettextrs::gettext;
use std::path::{Path, PathBuf};

/// Shared root for extraction scratch space.
pub fn staging_root() -> PathBuf {
    std::env::temp_dir().join("grab-video")
}

/// Per-item staging dir holding the split `.video`/`.audio` parts and the
/// muxed result while a merged video download is in flight.
pub fn staging_dir(item_id: u64) -> PathBuf {
    staging_root().join(item_id.to_string())
}

/// Create a staging dir and verify it really lives under Grab's staging
/// root: `create_dir_all` follows symlinks, so a pre-planted link at the
/// predicted path would otherwise redirect parts into attacker-chosen
/// dirs. Returns the canonical path on success.
pub fn ensure_staging_dir(dir: &Path) -> Result<PathBuf, VideoError> {
    std::fs::create_dir_all(dir).map_err(VideoError::staging)?;
    let canon = std::fs::canonicalize(dir).map_err(VideoError::staging)?;
    let root = std::fs::canonicalize(staging_root()).map_err(VideoError::staging)?;
    if canon.starts_with(&root) {
        Ok(canon)
    } else {
        Err(VideoError::staging(gettext(
            "staging directory escaped its root",
        )))
    }
}

/// Remove a staging dir. Guarded: never deletes anything outside Grab's own
/// staging root, so a buggy caller can't nuke user data. Canonicalized on
/// both sides so a symlinked root can't widen the guard either.
pub fn clean_staging(dir: &Path) {
    let (Ok(canon), Ok(root)) = (
        std::fs::canonicalize(dir),
        std::fs::canonicalize(staging_root()),
    ) else {
        return;
    };
    if canon.starts_with(&root) {
        let _ = std::fs::remove_dir_all(canon);
    }
}

/// Sidecar recording completed parts, so a retry (or a relaunch after a
/// crash) can skip straight to the merge instead of re-downloading.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct VideoManifest {
    pub(crate) page_url: String,
    pub(crate) quality: String,
    pub(crate) video_format_id: Option<String>,
    pub(crate) video_ext: String,
    pub(crate) audio_format_id: String,
    pub(crate) audio_ext: String,
    /// Final output size once the file has been renamed into place.
    /// Lets a retry after a crash adopt the finished file without any work.
    pub(crate) final_bytes: Option<u64>,
}

impl VideoManifest {
    /// Whether the sidecar describes this exact attempt (same page, prefs,
    /// and selected formats). Anything else means the formats shifted
    /// under us and the parts must be re-downloaded.
    fn matches(
        &self,
        page_url: &str,
        quality: &str,
        video: Option<(&str, &str)>,
        audio: (&str, &str),
    ) -> bool {
        self.page_url == page_url
            && self.quality == quality
            && self.audio_format_id == audio.0
            && self.audio_ext == audio.1
            && match (video, &self.video_format_id) {
                (Some((id, ext)), Some(mid)) => id == mid && ext == self.video_ext,
                (None, None) => self.video_ext.is_empty(),
                _ => false,
            }
    }
}

/// Fixed part names inside a row's staging dir: retry-after-crash finds the
/// same paths the failed attempt wrote. No caller today (`dest_part_path`
/// covers the destination side); kept as the staging-side constructor.
#[allow(dead_code)]
pub(crate) fn part_path(dir: &Path, kind: &str, ext: &str) -> PathBuf {
    dir.join(format!("{kind}.{ext}"))
}

/// Dest-dir part names (`<stem>.<kind>.<ext>` beside the finished file):
/// yt-dlp defaults — `.part` shells and fragments show up in the user's
/// folder while transferring, and yt-dlp's native `--continue` resumes
/// them in place. Deterministic across attempts (crash-resume finds the
/// same paths); unique per row via intake dedupe of the finished name.
///
/// This is a REAL filesystem path (single `%`, no template escapes):
/// callers feeding it to yt-dlp's `-o` must go through
/// [`ytdlp_output_template`], since yt-dlp parses `-o` as a template.
pub(crate) fn dest_part_path(dest: &Path, kind: &str, ext: &str) -> PathBuf {
    let dir = dest.parent().unwrap_or_else(|| Path::new(""));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    dir.join(format!("{stem}.{kind}.{ext}"))
}

/// Render a real filesystem path as a yt-dlp `-o` output template.
/// yt-dlp parses `-o` as a Python printf-style template, so a literal
/// `%` in the filename stem (percent-decoded titles, user-typed names
/// like `100%.mp4`) must be doubled (`%%`) or the template misparses
/// and the download fails. Genuine yt-dlp fields (`%(ext)s`, …​) are
/// left intact: only a `%` that does not start `%(name)s` is doubled.
/// yt-dlp renders `%%` back to a single `%`, so the on-disk name still
/// matches what `dest_part_path` and `is_grab_part` expect. (A user
/// title that itself contains `%(…)s` text passes through as a field:
/// inherent yt-dlp template ambiguity, pre-existing behavior.)
pub(crate) fn ytdlp_output_template(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '%' && !matches!(chars.clone().next(), Some('(')) {
            out.push('%');
        }
    }
    out
}

/// Grab-namespaced part infixes: the only names `clean_dest_parts` ever
/// touches. The finished file itself (`<stem>.<ext>`) never matches.
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live."];

/// Reserve a remux-temp slot in `staging` for this attempt:
/// `final.<n>.<ext>`, claimed with a `<...>.lease` sidecar.
///
/// The counter is what makes a completed remux durable across attempts.
/// Every attempt used to write the same `final.<ext>`, so a retry either
/// overwrote the previous attempt's finished recording or had its cleanup
/// sweep delete it — the only copy of a capture that could not be placed.
///
/// The lease is what makes the claim *exclusive*. Scanning for a free name
/// and then writing it is check-then-use: two attempts that overlap (a
/// cancelled worker whose child is still terminating, say) can both see
/// slot 1 free, and the loser's cleanup would then unlink the winner's
/// completed recording. `create_new` makes the claim atomic, and also
/// means an unreadable directory fails closed instead of looking empty.
///
/// The lease is a separate file because the temp itself cannot be
/// pre-created: ffmpeg is invoked without `-y` and refuses to overwrite an
/// existing output.
///
/// Fails rather than handing back an occupied slot — an exhausted range
/// must not produce a path the caller would later delete.
pub(crate) fn reserve_remux_temp(staging: &Path, ext: &str) -> Result<PathBuf, VideoError> {
    let taken = dir_file_names(staging);
    for n in 1..=9999u32 {
        let name = format!("final.{n}.{ext}");
        if taken
            .iter()
            .any(|t| t == &name || t == &format!("{name}.lease"))
        {
            continue;
        }
        let lease = staging.join(format!("{name}.lease"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lease)
        {
            Ok(_) => return Ok(staging.join(name)),
            // Someone else claimed it between the scan and the create.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // Unwritable staging: fail closed. Returning a guess here is
            // how two attempts end up sharing one slot.
            Err(e) => return Err(VideoError::staging(e.to_string())),
        }
    }
    Err(VideoError::staging("no free remux slot"))
}

/// Drop the lease that [`reserve_remux_temp`] created. Best-effort: a
/// stale lease only costs one slot, never a recording.
pub(crate) fn release_remux_lease(temp: &Path) {
    let mut lease = temp.as_os_str().to_os_string();
    lease.push(".lease");
    let _ = std::fs::remove_file(std::path::PathBuf::from(lease));
}

/// Suffix of `file_name` past a `<stem>.` prefix, if it has one.
/// `strip_prefix` (not slicing past `starts_with`): panic-free even if a
/// future edit reorders the guards. Pure.
fn strip_stem_suffix<'a>(file_name: &'a str, stem: &str) -> Option<&'a str> {
    file_name
        .strip_prefix(stem)
        .and_then(|r| r.strip_prefix('.'))
}

pub(crate) fn is_grab_part(file_name: &str, stem: &str) -> bool {
    strip_stem_suffix(file_name, stem).is_some_and(|r| PART_KINDS.iter().any(|k| r.starts_with(k)))
}

/// File names directly inside `dir`: a best-effort snapshot for intake
/// reservation. Unreadable or missing dirs read as empty; per-entry IO
/// errors drop that entry (fail-open, same posture as the sweeper
/// below, so neither side can conjure a phantom delete).
pub(crate) fn dir_file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `stem` already hosts Grab-namespaced part files or subtitle
/// sidecars in a snapshotted listing (`<stem>.video.*` etc. plus
/// `<stem>.<lang>.srt` for offered languages, plus yt-dlp `.part`
/// shells of the part files). Intake treats such a stem as taken:
/// claiming it would let a later `clean_dest_parts` sweep, subtitle
/// collection, or row delete touch files Grab never wrote. Empty stems
/// never match. Directories reserve too (the sweeper only deletes
/// files): deliberately fail-closed, at most an extra ` (1)` in the
/// claimed name.
pub(crate) fn stem_reserved_in(names: &[String], stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    names.iter().any(|n| is_grab_part(n, stem)) || stem_has_subtitle_sidecar(names, stem)
}

/// Whether `stem` already hosts a subtitle sidecar for any offered
/// language (`<stem>.<lang>.srt`) in a snapshotted listing. Checked
/// alongside `stem_reserved_in` at intake so a pre-existing sidecar
/// reserves the stem too. Deliberately NOT folded into `is_grab_part`:
/// that matcher backs `clean_dest_parts`'s sweep, and sidecars must
/// survive row removal, not be swept with it.
fn stem_has_subtitle_sidecar(names: &[String], stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    names.iter().any(|n| {
        strip_stem_suffix(n, stem)
            .and_then(|r| r.strip_suffix(".srt"))
            .is_some_and(|lang| subtitle_content_languages().any(|l| l == lang))
    })
}

/// Delete a row's dest-dir part files (finished parts plus yt-dlp `.part`
/// shells). The finished file is never matched. Intake never claims a
/// stem that already hosts part-namespace files or subtitle sidecars
/// (see `stem_reserved_in`), so a match here is Grab's own output —
/// except for files that arrived mid-download, which no claim-time
/// check can cover.
pub fn clean_dest_parts(dest: &Path) {
    let (Some(dir), Some(stem)) = (dest.parent(), dest.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_grab_part(name, stem)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `<output-stem>.<lang>.srt` beside `output`: where yt-dlp drops a
/// `--write-subs` sidecar for a `-o` path, and where Grab keeps it
/// beside the finished file. The language is always allowlisted (no
/// dots, no separators), so this can never escape its directory — and
/// a finished `<stem>.<lang>.srt` can never match [`PART_KINDS`], so
/// `clean_dest_parts` structurally leaves collected sidecars alone
/// while still sweeping stale part-namespaced ones.
pub(crate) fn sidecar_path_for(output: &Path, lang: &str) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    output
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{stem}.{lang}.srt"))
}

/// Best-effort sidecar collection: move an exact sidecar file beside
/// the finished download. A missing source (the page published no
/// subtitles) is the normal nothing-to-do; any other failure only
/// traces — subtitles must never fail a download that succeeded.
pub(crate) fn collect_sidecar(src: &Path, dest: &Path, lang: &str) {
    if !src.exists() {
        return;
    }
    let dst = sidecar_path_for(dest, lang);
    // Never clobber: a foreign sidecar arriving mid-download (after the
    // intake snapshot) must survive. Ours stays beside the part file,
    // where row removal sweeps it. Collection runs at most once per row
    // (post-claim), so an existing dst is always foreign.
    if let Err(e) = crate::file_names::rename_noreplace(src, &dst) {
        tracing::warn!(
            src = %src.display(),
            dst = %dst.display(),
            error = %e,
            "subtitle sidecar left beside the part file"
        );
    }
}

pub(crate) fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

pub(crate) fn read_manifest(dir: &Path) -> Option<VideoManifest> {
    std::fs::read_to_string(manifest_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub(crate) fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

/// Allocated bytes on disk, for sparse-shell detection. `None` where the
/// platform cannot say (non-Unix): callers treat that as dense, i.e. the
/// pre-existing behavior.
#[cfg(unix)]
fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).map(|m| m.blocks() * 512).ok()
}

#[cfg(not(unix))]
fn allocated_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether a part file is a sparse shell: full apparent size, (almost)
/// nothing on disk. The download engine pre-allocates part files and
/// tracks real progress in a sidecar it deletes on abort, so a killed
/// attempt leaves exactly this shape behind — and a naive size check
/// would then "adopt" gigabytes of zeros.
pub(crate) fn is_sparse_shell(path: &Path) -> bool {
    match (file_len(path), allocated_bytes(path)) {
        (Some(len), Some(allocated)) => len > 0 && allocated < len,
        _ => false,
    }
}

/// Heuristic upper bound for a sane unified temp: the planned combined
/// total plus 10% for `filesize_approx` underestimates (the primary
/// wobble — the total sums extractor estimates, not measured bytes)
/// plus 8 MiB flat for fixed post-merge additions (`--embed-metadata`,
/// container overhead). The temp on disk is yt-dlp's merged output
/// while the total sums the planned part sizes, so an exact comparison
/// wipes valid crash-recovery temps whenever the estimate understates
/// reality, forcing full re-downloads.
///
/// Deliberately generous, and honest about it: the flat floor disables
/// garbage detection below ~8 MiB almost entirely, and anything under
/// this bound is trusted as final — yt-dlp treats an existing file at
/// or past its expected size as "already downloaded" (exit 0, verified
/// against yt-dlp 2026.08.19), and the post-run path claims non-empty
/// output without re-verifying size. The counterweight is staging
/// isolation: yt-dlp is the sole writer to the row's staging dir and
/// the manifest selection must match, so foreign bytes here mean
/// same-selection byte variance across attempts, not arbitrary garbage.
/// Past this bound, treat as garbage: wipe and start over. Pure
/// function (no I/O), unit-testable directly.
pub(crate) fn unified_temp_limit(total: u64) -> u64 {
    total
        .saturating_add(total / 10)
        .saturating_add(8 * 1024 * 1024)
}

/// What the next attempt should do, decided from the sidecar and disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Finished file already in place (adopt it).
    Finished,
    /// Temp incomplete or absent: spawn the same command and let yt-dlp
    /// resume its own `.part` shell (or download fresh when nothing is
    /// there). Safe because the manifest matched: same selection.
    Resume,
    /// (Re)download everything, wiping staging first.
    Fresh,
}

/// Inputs for [`resume_plan`], bundled so the signature stays small.
pub(crate) struct ResumeQuery<'a> {
    pub manifest: Option<&'a VideoManifest>,
    pub dest: &'a Path,
    pub staging: &'a Path,
    pub page_url: &'a str,
    pub quality: &'a str,
    pub video: Option<(&'a str, &'a str)>,
    pub audio: (&'a str, &'a str),
    /// Freshly selected combined total, for the over-long check. `None`
    /// means unknown: without a total, oversize is undetectable and any
    /// existing bytes are reusable.
    pub total: Option<u64>,
}

pub(crate) fn resume_plan(q: &ResumeQuery) -> ResumePlan {
    let Some(m) = q.manifest else {
        return ResumePlan::Fresh;
    };
    if !m.matches(q.page_url, q.quality, q.video, q.audio) {
        return ResumePlan::Fresh;
    }
    if let Some(final_bytes) = m.final_bytes
        && file_len(q.dest) == Some(final_bytes)
    {
        return ResumePlan::Finished;
    }
    // The unified temp (if any) must be sane: far past the total, treat
    // as garbage (nothing valid that far past it), and a sparse
    // full-size shell would resume as zeros. Either way, wipe and start
    // over. Anything else — partial temp, or nothing at all — spawns
    // the same command and lets yt-dlp resume or download fresh on its
    // own; note an in-margin temp is then trusted as final (see
    // `unified_temp_limit`), since yt-dlp skips files already at or
    // past the expected size. The limit carries headroom because the
    // temp is yt-dlp's merged output while the total sums extractor
    // estimates: estimate error, not container overhead, is the wobble
    // source, and an exact comparison wipes valid crash recovery
    // whenever the estimate understates reality.
    if let Some(temp) = discover_unified_output(q.staging, None) {
        let len = file_len(&temp);
        if q.total
            .is_some_and(|t| len.is_some_and(|n| n > unified_temp_limit(t)))
            || is_sparse_shell(&temp)
        {
            return ResumePlan::Fresh;
        }
    }
    ResumePlan::Resume
}
/// Whether a staging filename may be the unified download's claimed
/// output: under our template prefix, not a merge-fragment leftover,
/// and not a `.part` shell, subtitle sidecar, or yt-dlp metadata
/// dropping (same exclusion set as the HLS discoverer). Used for both
/// the `after_move` fast path and the scan fallback so they agree.
pub(crate) fn unified_candidate(file_name: &str) -> bool {
    let ext = Path::new(file_name).extension().and_then(|e| e.to_str());
    file_name.starts_with("grab-media.")
        && !is_ytdlp_fragment(file_name)
        && !matches!(ext, Some("part" | "srt" | "ytdl" | "temp" | "tmp" | "frag"))
}

/// yt-dlp's own merge temp names (`<stem>.f<id>.<ext>`) inside a `-o`
/// template dir: never the claimed output, even when a merge fails and
/// leaves them behind. Pure for tests.
pub(crate) fn is_ytdlp_fragment(file_name: &str) -> bool {
    let Some(dot_f) = file_name.find(".f") else {
        return false;
    };
    let after_f = &file_name[dot_f + 2..];
    let digits = after_f.len()
        - after_f
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    digits > 0 && after_f[digits..].starts_with('.')
}

/// Locate the unified download's output in staging: prefer yt-dlp's
/// `after_move:filepath` print (canonicalized, must stay in staging),
/// else scan for the `grab-media.*` temp. Both paths use
/// [`unified_candidate`], so merge-fragment leftovers, `.part` shells,
/// subtitle sidecars and metadata droppings are never claimed.
pub(crate) fn discover_unified_output(staging: &Path, after_move: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = after_move
        && let Ok(canonical) = std::fs::canonicalize(path)
        && canonical.starts_with(staging)
        && canonical.is_file()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
        && unified_candidate(name)
    {
        return Some(canonical);
    }
    std::fs::read_dir(staging)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(unified_candidate)
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}
