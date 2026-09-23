//! Intake normalization: URL length cap + user-input routing
//! (magnets and .torrent pseudo-URLs go to the torrent engine,
//! everything else to http(s)). One-way edge into `torrent` (its
//! classifiers/validators): the intake runs before any engine, and
//! `torrent` never imports this module.

use gettextrs::gettext;

/// Maximum accepted URL length (bytes); browsers and servers rarely
/// tolerate more, and it bounds queue-file and UI memory.
pub const MAX_URL_LEN: usize = 2048;

/// Normalize user input into a URL string, adding `https://` to bare hosts.
///
/// # Errors
/// Returns a display-ready message when the input is not a usable URL.
pub fn normalize_url(input: &str) -> Result<String, String> {
    // Strip a pasted BOM: trim() leaves U+FEFF, which would defeat the
    // magnet classifier below and route magnets to the scheme branch.
    let trimmed = input.trim().trim_start_matches('\u{feff}');
    if crate::torrent::is_magnet(trimmed) {
        // Magnet links skip URL parsing and the HTTP length cap entirely:
        // parsed locally by the torrent engine, never sent as a request
        // line. Still capped against abuse (Ubuntu magnets run ~2-4 KB).
        const MAX_MAGNET_LEN: usize = 16384;
        if trimmed.len() > MAX_MAGNET_LEN {
            return Err(gettext("Magnet link is too long (max {n} characters)")
                .replace("{n}", &MAX_MAGNET_LEN.to_string()));
        }
        // Validated here so the row stores the trimmed link, re-parsed by
        // the torrent engine.
        return crate::torrent::parse_magnet(trimmed).map(|_| trimmed.to_string());
    }
    if crate::torrent::is_torrent_url(trimmed) {
        // Archived .torrent pseudo-URLs skip URL parsing and the HTTP
        // length cap like magnets: validated here so the row stores the
        // trimmed pseudo-URL, resolved by the torrent engine. The archive
        // must exist; a swept archive means the queue entry is stale.
        return crate::torrent::archive_path_for_url(trimmed)
            .map(|_| trimmed.to_string())
            .ok_or_else(|| gettext("Torrent file is missing from the archive"));
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(gettext("URL is too long (max {n} characters)")
            .replace("{n}", &MAX_URL_LEN.to_string()));
    }
    // An explicit scheme is authoritative: only http(s) passes, so the
    // separate validate pass is unnecessary. (Bare `host:port` inputs must
    // not take this branch: `localhost:8080/f` parses with scheme
    // "localhost" and still needs `https://` prepended below.)
    if trimmed.contains("://") {
        let u = url::Url::parse(trimmed)
            .map_err(|_| gettext("Invalid URL: {url}").replace("{url}", trimmed))?;
        if !u.username().is_empty() || u.password().is_some() {
            return Err(gettext("URLs with a username/password are not supported"));
        }
        return match u.scheme() {
            "http" | "https" => Ok(u.to_string()),
            "ftp" => Err(gettext("FTP is not supported (use http/https)")),
            s => Err(gettext("Unsupported scheme: {s} (use http/https)").replace("{s}", s)),
        };
    }
    let bare =
        !trimmed.contains(' ') && (trimmed.contains('.') || trimmed.starts_with("localhost"));
    if bare {
        let with_scheme = format!("https://{trimmed}");
        if let Ok(u) = url::Url::parse(&with_scheme)
            && u.username().is_empty()
            && u.password().is_none()
        {
            return Ok(u.to_string());
        }
    }
    Err(gettext("Invalid URL: {url}").replace("{url}", trimmed))
}
