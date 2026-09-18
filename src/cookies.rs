//! Browser cookies for the direct HTTP engine, so plain downloads
//! authenticate like the browser does.
//!
//! yt-dlp spawns already receive `--cookies-from-browser`; this module
//! covers the only remaining gap: Grab's own reqwest requests. The
//! mechanism reuses yt-dlp itself for extraction (it owns the
//! keyring/SQLite knowledge for every supported browser), then serves
//! the result from memory:
//!
//! ```text
//! yt-dlp --cookies-from-browser SPEC --cookies TMP --skip-download URL
//!   → Netscape jar on disk (0600, deleted after parse)
//!   → parsed into reqwest::cookie::Jar (process memory only)
//!   → per-request `Cookie` headers via Jar::cookies(url)
//! ```
//!
//! Privacy posture: cookie names and values are never logged (counts
//! only); the temp file is owner-only and removed right after parsing;
//! failures resolve to *no cookies* (downstream servers answer loudly
//! instead of rows bricking); the update checker never sees this jar.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::cookie::CookieStore as _;

/// How long an exported jar is reused across attempts of one browser
/// profile. Fresh enough to outlive site expiries measured in hours,
/// short enough that a browser logout takes effect without restart.
const JAR_TTL: Duration = Duration::from_secs(5 * 60);

struct CachedJar {
    jar: Arc<reqwest::cookie::Jar>,
    at: Instant,
}

fn jar_cache() -> &'static Mutex<std::collections::HashMap<String, CachedJar>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, CachedJar>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// One parsed Netscape cookie line: domain + `name=value` pair.
/// Returns `None` for comments, blanks and malformed lines.
fn parse_netscape_line(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split('\t');
    let domain = fields.next()?.trim();
    fields.next()?; // subdomain flag
    fields.next()?; // path
    let secure = fields.next()?.trim();
    fields.next()?; // expiry
    let name = fields.next()?.trim();
    let value = fields.next()?.trim();
    if domain.is_empty() || name.is_empty() {
        return None;
    }
    // add_cookie_str takes Set-Cookie shape; the Domain attribute scopes
    // the cookie exactly like the jar file means it. Values with `;`
    // would truncate and are skipped rather than sent mangled. The
    // Secure flag is preserved so secure cookies stay https-only, like
    // the browser and yt-dlp treat them.
    if value.contains(';') {
        return None;
    }
    let secure = if secure.eq_ignore_ascii_case("TRUE") {
        "; Secure"
    } else {
        ""
    };
    Some((
        domain.to_string(),
        format!("{name}={value}; Domain={domain}{secure}"),
    ))
}

/// URL owning a cookie domain: leading dots are registry noise.
fn url_for_domain(domain: &str) -> Option<url::Url> {
    url::Url::parse(&format!("https://{}/", domain.trim_start_matches('.'))).ok()
}

/// Adopt exported cookies into a jar. Malformed lines are skipped, so a
/// single weird entry can never poison the whole profile.
pub(crate) fn jar_from_export(text: &str) -> (Arc<reqwest::cookie::Jar>, usize) {
    let jar = Arc::new(reqwest::cookie::Jar::default());
    let mut count = 0;
    for line in text.lines() {
        let Some((domain, cookie)) = parse_netscape_line(line) else {
            continue;
        };
        let Some(url) = url_for_domain(&domain) else {
            continue;
        };
        jar.add_cookie_str(&cookie, &url);
        count += 1;
    }
    (jar, count)
}

/// Dump one browser profile's cookies through yt-dlp (which owns the
/// keyring/SQLite knowledge) into a temp Netscape file. Returns the
/// file text; the caller deletes the file.
async fn export_cookies(
    youtube_bin: &Path,
    spec: &str,
    page_url: &str,
    timeout: Duration,
) -> Option<String> {
    let dir = crate::video::staging_root();
    let _ = std::fs::create_dir_all(&dir);
    let path: PathBuf = dir.join(format!("grab-cookies-{}.txt", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Owner-only from birth: cookie values must never sit
        // world-readable, even briefly.
        let _ = std::fs::write(&path, "");
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let mut cmd = tokio::process::Command::new(youtube_bin);
    cmd.arg("--no-progress")
        .arg("--cookies-from-browser")
        .arg(spec)
        .arg("--cookies")
        .arg(&path)
        .arg("--skip-download")
        .arg("--")
        .arg(page_url);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().ok()?;
    let status = tokio::time::timeout(timeout, child.wait())
        .await
        .ok()?
        .ok()?;
    if !status.success() {
        tracing::debug!("cookie export failed, continuing without cookies");
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    text
}

/// Cookie jar for one browser spec, cached briefly across attempts.
/// `None` (setting off, tools missing, export/parse failure) means
/// plain requests — servers answer loudly instead of rows bricking.
pub(crate) async fn jar_for_browser(
    cookies_browser: &str,
    youtube_bin: &Path,
    page_url: &str,
) -> Option<Arc<reqwest::cookie::Jar>> {
    if cookies_browser.is_empty() || cookies_browser == "none" {
        return None;
    }
    if let Some(cached) = jar_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(cookies_browser)
        .filter(|c| c.at.elapsed() < JAR_TTL)
    {
        return Some(cached.jar.clone());
    }
    // Ask yt-dlp for the browser *name* the way our spawns do: the
    // setting may carry a resolved `browser:/profile` spec.
    let spec = cookies_browser.split(':').next().unwrap_or(cookies_browser);
    let text = export_cookies(youtube_bin, spec, page_url, Duration::from_secs(120)).await?;
    let (jar, count) = jar_from_export(&text);
    tracing::debug!(browser = spec, cookies = count, "exported browser cookies");
    jar_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            cookies_browser.to_string(),
            CachedJar {
                jar: jar.clone(),
                at: Instant::now(),
            },
        );
    // An empty jar is still a valid answer (logged-out profile):
    // callers send no Cookie header and move on.
    Some(jar)
}

/// Ready-to-send `Cookie` value for one request URL, if the jar holds
/// anything in scope. Values stay in memory; only presence is logged
/// by callers that care.
pub(crate) fn cookie_header_for(
    jar: &reqwest::cookie::Jar,
    url: &str,
) -> Option<reqwest::header::HeaderValue> {
    let parsed: url::Url = url.parse().ok()?;
    jar.cookies(&parsed)
}
