//! Browser cookies for the direct HTTP engine, extracted via yt-dlp into an in-memory jar.
//! Temp file is owner-only and deleted after parse; values never logged, failures mean no cookies.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::cookie::CookieStore as _;

/// Jar reuse TTL: outlives hourly site expiries, yet browser logout takes effect without restart.
const JAR_TTL: Duration = Duration::from_secs(5 * 60);

struct CachedJar {
    jar: Arc<reqwest::cookie::Jar>,
    at: Instant,
}

fn jar_cache() -> &'static Mutex<std::collections::HashMap<String, CachedJar>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, CachedJar>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// One parsed Netscape line; `None` for comments, blanks and malformed lines.
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
    // Set-Cookie shape with Domain scope; skip `;` values rather than truncating, keep Secure https-only.
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

/// Adopt exported cookies; one bad line can't poison the profile.
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

/// Dump one profile's cookies through yt-dlp to a temp file; caller deletes it.
async fn export_cookies(
    youtube_bin: &Path,
    spec: &str,
    page_url: &str,
    timeout: Duration,
) -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = crate::video::staging_root();
    let _ = std::fs::create_dir_all(&dir);
    // Unique per attempt, created atomically owner-only: a planted symlink fails instead of diverting the dump.
    let path: PathBuf = loop {
        let candidate: PathBuf = dir.join(format!(
            "grab-cookies-{}-{}.txt",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(_) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                tracing::debug!(error = %e, "cookie export temp file failed");
                return None;
            }
        }
    };
    let mut cmd = tokio::process::Command::new(youtube_bin);
    cmd.arg("--ignore-config")
        .arg("--no-progress")
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
    let mut group = crate::video_spawn::ProcessGroupGuard::new(&child);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        // Clean exit: leader reaped, so disarm before post-wait work.
        Ok(Ok(status)) => {
            group.disarm();
            status
        }
        _ => {
            // Timeout/wait failure: SIGKILL the group off the cookie DB lock, reap, remove temp file, fall back.
            crate::video_spawn::reap_child(&mut child, &mut group).await;
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    if !status.success() {
        tracing::debug!("cookie export failed, continuing without cookies");
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    text
}

/// Cookie jar for one browser spec, cached briefly; `None` means plain requests.
pub(crate) async fn jar_for_browser(
    cookies_browser: &str,
    youtube_bin: &Path,
    page_url: &str,
) -> Option<Arc<reqwest::cookie::Jar>> {
    if cookies_browser.is_empty() || cookies_browser == "none" {
        return None;
    }
    if let Some(cached) = crate::runtime::lock_recover(jar_cache())
        .get(cookies_browser)
        .filter(|c| c.at.elapsed() < JAR_TTL)
    {
        return Some(cached.jar.clone());
    }
    // Same spec the download spawns use, so exports authenticate as the same profile.
    let spec = crate::video_tools::cookies_browser_spec(cookies_browser)?;
    let text = export_cookies(youtube_bin, &spec, page_url, Duration::from_secs(120)).await?;
    let (jar, count) = jar_from_export(&text);
    tracing::debug!(cookies = count, "exported browser cookies");
    crate::runtime::lock_recover(jar_cache()).insert(
        cookies_browser.to_string(),
        CachedJar {
            jar: jar.clone(),
            at: Instant::now(),
        },
    );
    // Empty jar is valid (logged-out profile): send no Cookie header.
    Some(jar)
}

/// `Cookie` value for one URL, if the jar holds anything in scope.
pub(crate) fn cookie_header_for(
    jar: &reqwest::cookie::Jar,
    url: &str,
) -> Option<reqwest::header::HeaderValue> {
    let parsed: url::Url = url.parse().ok()?;
    jar.cookies(&parsed)
}
