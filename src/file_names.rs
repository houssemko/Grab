//! File-name primitives: sanitize, split, dedupe, derive from URL,
//! atomic rename, piece sizing, byte formatting. Leaf module (std +
//! url crate only) breaking the `download` import fan-out: the HTTP
//! engine, the torrent intake and the video pipeline share these.

/// Split a filename into stem and extension (extension keeps its dot).
/// `rfind`, not `split`: only the last dot counts, and a leading dot
/// (`".profile"`) is a stem, not an extension. Pure.
fn split_stem_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i..])),
        _ => (name, None),
    }
}

/// Cap a filename to filesystem limits (NAME_MAX is 255 bytes on
/// ext4/tmpfs), keeping the extension. Truncates the stem on a char
/// boundary; reserves room for the ` (n)` dedupe suffix.
pub(crate) fn shorten_filename(name: &str) -> String {
    const MAX_FILENAME_BYTES: usize = 240;
    if name.len() <= MAX_FILENAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = split_stem_ext(name);
    let ext_len = ext.map_or(0, str::len);
    let keep = stem.floor_char_boundary(MAX_FILENAME_BYTES.saturating_sub(ext_len));
    match ext {
        Some(e) => format!("{}{e}", &stem[..keep]),
        None => stem[..keep].to_string(),
    }
}

/// Fold a filename to plain ASCII, mirroring yt-dlp's `--restrict-filenames`
/// (`sanitize_filename(restricted=True)`) with a simpler documented fold:
///
/// - accented Latin letters map to their base letter (`é` → `e`, `ß` → `ss`,
///   `æ` → `ae`); every other non-ASCII character becomes `_`
/// - `"` and control characters are dropped outright (as in yt-dlp)
/// - runs of `_` collapse to one; leading/trailing `_` are stripped
///
/// The split keeps the extension: stem and extension are folded separately,
/// so `Café & Croissants.mp4` becomes `Cafe_Croissants.mp4`. A stem that
/// folds to nothing falls back to `"file"`, so the result is never empty.
pub(crate) fn restrict_filename_ascii(name: &str) -> String {
    let (stem, ext) = split_stem_ext(name);
    let stem = fold_ascii_part(stem);
    let stem = if stem.is_empty() {
        "file".to_string()
    } else {
        stem
    };
    match ext {
        Some(e) => format!("{stem}{}", fold_ascii_part(e)),
        None => stem,
    }
}

/// Fold one filename part (stem or extension) to ASCII.
fn fold_ascii_part(part: &str) -> String {
    /// Base-letter fold for the accented Latin ranges yt-dlp maps through
    /// its own accent table; anything unlisted here is not representable
    /// in ASCII and becomes `_` in the caller.
    fn fold_accent(c: char) -> Option<&'static str> {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' => Some("a"),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => Some("e"),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' => Some("i"),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' => Some("o"),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' => Some("u"),
            'ý' | 'ÿ' => Some("y"),
            'ñ' | 'ń' | 'ň' => Some("n"),
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => Some("c"),
            'ß' => Some("ss"),
            'æ' => Some("ae"),
            'œ' => Some("oe"),
            'ð' | 'ď' | 'đ' => Some("d"),
            'þ' => Some("th"),
            'ł' => Some("l"),
            'š' | 'ś' | 'ŝ' | 'ş' => Some("s"),
            'ž' | 'ź' | 'ż' => Some("z"),
            'ğ' => Some("g"),
            'ř' => Some("r"),
            'ť' | 'ţ' => Some("t"),
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' | 'Ǎ' => Some("A"),
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => Some("E"),
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' | 'Ĭ' | 'Į' => Some("I"),
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' | 'Ǒ' => Some("O"),
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' | 'Ǔ' => Some("U"),
            'Ý' | 'Ÿ' => Some("Y"),
            'Ñ' | 'Ń' | 'Ň' => Some("N"),
            'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => Some("C"),
            'Æ' => Some("AE"),
            'Œ' => Some("OE"),
            'Ð' | 'Ď' | 'Đ' => Some("D"),
            'Þ' => Some("TH"),
            'Ł' => Some("L"),
            'Š' | 'Ś' | 'Ŝ' | 'Ş' => Some("S"),
            'Ž' | 'Ź' | 'Ż' => Some("Z"),
            'Ğ' => Some("G"),
            'Ř' => Some("R"),
            'Ť' | 'Ţ' => Some("T"),
            _ => None,
        }
    }

    let mut out = String::with_capacity(part.len());
    for c in part.chars() {
        if let Some(base) = fold_accent(c) {
            out.push_str(base);
        } else if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else if c == '"' || c.is_control() {
            // Dropped outright, as in yt-dlp's restricted mode.
        } else {
            out.push('_');
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_underscore = false;
    for c in out.chars() {
        if c == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        collapsed.push(c);
    }
    collapsed.trim_matches('_').to_string()
}

/// Append ` (n)` before the extension until `taken` returns false.
///
/// Example: `dedupe_filename("f.iso", |n| n == "f.iso")` returns `"f (1).iso"`.
pub fn dedupe_filename(filename: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(filename) {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rfind('.') {
        Some(i) if i > 0 => (&filename[..i], Some(&filename[i + 1..])),
        _ => (filename, None),
    };
    let mut n = 1;
    for _ in 1..=9999 {
        let cand = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{filename} ({n})"),
        };
        if !taken(&cand) {
            return cand;
        }
        n += 1;
    }
    // Absurd collision count: return the next candidate anyway (a later
    // write visibly fails) rather than stat-ing the disk forever.
    match ext {
        Some(e) => format!("{stem} ({n}).{e}"),
        None => format!("{filename} ({n})"),
    }
}

/// File stem of a finished-name candidate (`Clip.mp4` → `Clip`,
/// extensionless `README` → `README`); `""` when there is none, which
/// never reserves (see `stem_reserved_in`).
pub(crate) fn name_stem(name: &str) -> &str {
    std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

pub(crate) fn sane_filename(s: &str) -> bool {
    /// Explicit bidi controls (marks, embeddings/overrides, isolates).
    /// No std helper exists, so match the assigned ranges with escapes
    /// (never literal glyphs: they are invisible in source).
    fn is_bidi_control(c: char) -> bool {
        matches!(c, '\u{200E}' | '\u{200F}' | '\u{61C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    }
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\0')
        && s != "."
        && s != ".."
        // Control/bidi-override characters deceive in listings and
        // notification text (FIND-03/04); servers love to send them.
        && !s.chars().any(|c| c.is_control() || is_bidi_control(c))
}

/// Best-effort filename from a URL path, falling back to `index.html`.
/// Decode `%XX` escapes (RFC 5987 `filename*=`); leaves everything else
/// (including `+`) untouched. No new dependency for ten lines.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut decoded = None;
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = ((h as char).to_digit(16), (l as char).to_digit(16))
        {
            decoded = Some((h << 4 | l) as u8);
        }
        if let Some(b) = decoded {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn filename_from_url(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()).map(|s| s.to_string()))
        })
        // Browsers decode %XX escapes: %20 is a space, not six chars.
        .map(|s| percent_decode(&s))
        .filter(|s| sane_filename(s))
        .unwrap_or_else(|| "index.html".to_string())
}

pub(crate) fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Smallest piece size: everything at or under ~4 GB splits into 1 MB
/// pieces, so one slow connection only ever delays the tail by ~1 MB.
pub(crate) const PIECE_MIN: u64 = 1024 * 1024;
/// Largest piece size: bounds per-request overhead on huge files without
/// starving the work-stealing queue (still thousands of pieces).
pub(crate) const PIECE_MAX: u64 = 16 * 1024 * 1024;
/// Pieces per download to aim for; beyond this the piece size grows.
const PIECE_TARGET_COUNT: u64 = 4096;

/// Byte range each segmented piece covers. Pure function of the total, so
/// persisted bitmaps stay valid across restarts: DO NOT change the formula
/// without a queue migration (restore drops mismatched bitmaps to a safe
/// single-stream resume instead of corrupting).
pub(crate) fn piece_len(total: u64) -> u64 {
    total
        .div_ceil(PIECE_TARGET_COUNT)
        .clamp(PIECE_MIN, PIECE_MAX)
}

/// Rename without clobbering: `std::fs::rename` silently replaces the
/// destination. Prefers `renameat2(RENAME_NOREPLACE)` (atomic on any
/// filesystem, FAT included); falls back to claiming `new` with a hard
/// link, to a plain rename only where hard links are unsupported, and to
/// a copy where source and destination live on different filesystems
/// (staging is on tmpfs, downloads usually are not).
pub(crate) fn rename_noreplace(
    old: &std::path::Path,
    new: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    match rename_noreplace_sys(old, new) {
        // Ancient kernels (< 3.15) lack renameat2: use the portable path.
        // ENOSYS is 38 in the Linux UAPI (asm-generic and x86 alike).
        Err(e) if e.raw_os_error() == Some(38) => {}
        // Cross-device: no rename variant can span filesystems (EXDEV
        // 18); copy through a `create_new` claim instead.
        Err(e) if e.raw_os_error() == Some(18) => return copy_noreplace(old, new),
        r => return r,
    }
    // Claim `new` atomically via the link: an `exists()` pre-check followed
    // by a plain rename is a TOCTOU — a rival rename can slip in between
    // and get clobbered. Retry the link on transient errors; fall back to
    // plain rename only where hard links cannot work at all (Linux UAPI
    // numbers: EPERM 1, EOPNOTSUPP 95, ENOSYS 38), and to a copy across
    // filesystems (EXDEV 18).
    loop {
        match std::fs::hard_link(old, new) {
            Ok(()) => return std::fs::remove_file(old),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(18) => {
                return copy_noreplace(old, new);
            }
            Err(e) if matches!(e.raw_os_error(), Some(1 | 95 | 38)) => {
                return std::fs::rename(old, new);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copy `old` to `new` without replacing an existing `new`, then remove
/// `old`. Cross-device fallback for [`rename_noreplace`]: neither rename
/// nor link can span filesystems, so bytes are copied through a
/// `create_new` handle — the atomic claim, no TOCTOU — and the source is
/// unlinked only after the copy lands. A failed copy removes the partial
/// destination, which only this call could have created.
fn copy_noreplace(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(old)?;
    let permissions = src.metadata().map(|m| m.permissions()).ok();
    let mut dst = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(new)?;
    let copy = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if copy.is_err() {
        let _ = std::fs::remove_file(new);
        return copy.map(|_| ());
    }
    drop(dst);
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(new, permissions);
    }
    std::fs::remove_file(old)
}

/// `renameat2(olddirfd, old, newdirfd, new, RENAME_NOREPLACE)` without a
/// libc dependency: one syscall, three stable constants.
#[cfg(target_os = "linux")]
fn rename_noreplace_sys(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    unsafe extern "C" {
        fn renameat2(
            olddirfd: std::os::raw::c_int,
            oldpath: *const std::os::raw::c_char,
            newdirfd: std::os::raw::c_int,
            newpath: *const std::os::raw::c_char,
            flags: std::os::raw::c_uint,
        ) -> std::os::raw::c_int;
    }
    const AT_FDCWD: std::os::raw::c_int = -100;
    const RENAME_NOREPLACE: std::os::raw::c_uint = 1; // renameat2(2)
    // Queue/dedupe names never contain NUL (sane_filename), but fail
    // visibly instead of truncating if one ever slips through.
    let cvt = |p: &std::path::Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let (old, new) = (cvt(old)?, cvt(new)?);
    // SAFETY: NUL-terminated buffers outlive the call; the rest are integers.
    let r = unsafe {
        renameat2(
            AT_FDCWD,
            old.as_ptr(),
            AT_FDCWD,
            new.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether a row name is just the page URL derived at intake:
/// dialog-less rows skip the picker, so their names are URL stems
/// ("watch"). Matches the derived stem modulo intake-dedupe ` (N)`
/// suffixes. Dialog-seeded and typed names never match (unless
/// perversely identical to the URL stem). Pure for tests.
pub(crate) fn is_url_derived_name(current: &str, page_url: &str) -> bool {
    let derived = filename_from_url(page_url);
    current == derived || strip_dedupe_suffix(current) == derived
}

/// Intake-dedupe suffix stripped: `watch (12)` → `watch`,
/// `Clip (3).mp4` → `Clip.mp4`. ASCII-boundary operations only, so
/// non-ASCII titles are never split mid-codepoint. Pure for tests.
pub(crate) fn strip_dedupe_suffix(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i..])),
        _ => (name, None),
    };
    if let Some(open) = stem.rfind(" (") {
        let inner = &stem[open + 2..];
        if !inner.is_empty()
            && inner
                .strip_suffix(')')
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            let base = &stem[..open];
            if base.is_empty() {
                return name.to_string();
            }
            return match ext {
                Some(e) => format!("{base}{e}"),
                None => base.to_string(),
            };
        }
    }
    name.to_string()
}
