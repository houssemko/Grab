//! Tool provisioning: finding, vetting, installing and feeding
//! yt-dlp/ffmpeg to every spawn. Leaf module (yt_dlp client, tokio,
//! libc, gettext + file_names/runtime leaves): the dialog, prefs and
//! engines consume these through the `video` facade.

use gettextrs::gettext;
use std::path::{Path, PathBuf};
use thiserror::Error;
use yt_dlp::client::deps::{Libraries, LibraryInstaller};
use yt_dlp::model::DrmStatus;
use yt_dlp::model::format::{Format, FormatType, Protocol};

/// Errors surfaced by the video pipeline. User-facing strings are translated
/// at construction; match on the variant to branch the UI
/// (install banner vs retry toast).
#[derive(Debug, Error)]
pub enum VideoError {
    /// Neither the Flatpak bundle nor the user library dir nor PATH has the
    /// yt-dlp/ffmpeg tools. Offer the install flow.
    #[error("{0}")]
    MissingLibraries(String),
    /// The page could not be extracted (wrong URL, offline, …). Retryable.
    #[error("{0}")]
    Fetch(String),
    /// Background task machinery failed (panic/join) — internal.
    #[error("{0}")]
    Runtime(String),
    /// Other translated failure.
    #[error("{0}")]
    Message(String),
}

impl VideoError {
    pub(crate) fn missing_tools() -> Self {
        Self::MissingLibraries(gettext("Media downloads need the yt-dlp support tools"))
    }
    pub(crate) fn fetch(e: impl std::fmt::Display) -> Self {
        Self::Fetch(
            gettext("Couldn't read the video page: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn install(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't install the media support tools: {detail}")
                .replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn staging(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't prepare video staging: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn runtime(e: impl std::fmt::Display) -> Self {
        Self::Runtime(e.to_string())
    }
    pub(crate) fn unavailable() -> Self {
        Self::Message(gettext("No suitable formats found for this media"))
    }
    /// Same failure with a rejection census, so a page of manifest-only
    /// variants (live/HLS pages) reads differently from a DRM or
    /// link-less one instead of guessing.
    pub(crate) fn unavailable_detail(formats: &[Format]) -> Self {
        let total = formats.len();
        if total == 0 {
            return Self::Message(gettext(
                "No suitable formats found for this media (the page listed none)",
            ));
        }
        let (mut manifest, mut drm, mut no_link, mut video_only, mut unclassified) =
            (0, 0, 0, 0, 0);
        for f in formats {
            if f.protocol != Protocol::Https {
                manifest += 1;
            } else if matches!(f.has_drm, Some(DrmStatus::Yes)) {
                drm += 1;
            } else if f
                .download_info
                .url
                .as_deref()
                .filter(|u| !u.is_empty())
                .is_none()
            {
                no_link += 1;
            } else if f.format_type() == FormatType::Unknown {
                unclassified += 1;
            } else if f
                .codec_info
                .audio_codec
                .as_deref()
                .is_none_or(|c| c == "none")
            {
                video_only += 1;
            }
        }
        let mut reasons = Vec::new();
        for (n, label) in [
            (manifest, gettext("manifest")),
            (drm, gettext("DRM")),
            (no_link, gettext("no link")),
            (video_only, gettext("video-only")),
            (unclassified, gettext("unclassified")),
        ] {
            if n > 0 {
                reasons.push(format!("{label}: {n}"));
            }
        }
        Self::Message(
            gettext("No suitable formats found for this media ({total} listed: {reasons})")
                .replace("{total}", &total.to_string())
                .replace("{reasons}", &reasons.join(", ")),
        )
    }
    pub(crate) fn part_failed(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Media download failed: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn combine(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't merge video and audio: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    pub(crate) fn interrupted() -> Self {
        Self::Message(gettext("Download interrupted"))
    }
    pub(crate) fn outdated() -> Self {
        Self::Message(gettext("Video tools are too old — update them to continue"))
    }
    /// The exact [`crate::engine_msg::DEST_EXISTS`] sentence, so the pump's
    /// foreign-file requeue path picks a fresh name and retries the merge.
    pub(crate) fn exists() -> Self {
        Self::Message(crate::engine_msg::DEST_EXISTS.to_string())
    }
}

/// Whether we run inside the Flatpak sandbox. Only there is the Install
/// button the viable path (users cannot install host packages into the
/// sandbox); tarball/dev builds get guided self-install instead.
pub(crate) fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Package manager commands for the detected distro. `None` means unknown
/// distro: show manual install links instead of a wrong command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DistroPackages {
    /// Pretty distro name for the dialog title, e.g. "Fedora".
    pub distro: String,
    /// Full install command for yt-dlp, e.g. "sudo dnf install yt-dlp".
    pub yt_dlp: String,
    /// Full install command for ffmpeg.
    pub ffmpeg: String,
}

fn package_manager(id: &str) -> Option<&'static str> {
    match id {
        "fedora" | "rhel" | "centos" | "almalinux" | "rocky" => Some("sudo dnf install"),
        "ubuntu" | "debian" | "pop" | "linuxmint" | "elementary" | "zorin" => {
            Some("sudo apt install")
        }
        "arch" | "manjaro" | "endeavouros" | "cachyos" => Some("sudo pacman -S"),
        "opensuse-tumbleweed" | "opensuse-leap" | "sles" | "opensuse" => {
            Some("sudo zypper install")
        }
        "alpine" => Some("sudo apk add"),
        "gentoo" => Some("sudo emerge --ask"),
        "void" => Some("sudo xbps-install -S"),
        "solus" => Some("sudo eopkg install"),
        _ => None,
    }
}

/// Parse `/etc/os-release` content into install commands. Takes the file
/// content (not the path) so unit tests feed fixtures directly. Falls
/// back to `ID_LIKE` tokens when `ID` itself is unknown.
pub(crate) fn distro_packages(os_release: &str) -> Option<DistroPackages> {
    let mut id: Option<&str> = None;
    let mut id_like = "";
    let mut name: Option<&str> = None;
    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        match key {
            "ID" => id = Some(value),
            "ID_LIKE" => id_like = value,
            "NAME" => name = Some(value),
            _ => {}
        }
    }
    let id = id?;
    let pm =
        package_manager(id).or_else(|| id_like.split_whitespace().find_map(package_manager))?;
    Some(DistroPackages {
        distro: name.unwrap_or(id).to_string(),
        yt_dlp: format!("{pm} yt-dlp"),
        ffmpeg: format!("{pm} ffmpeg"),
    })
}

/// Directory where on-demand tool installs keep the yt-dlp and ffmpeg
/// binaries: `$XDG_DATA_HOME/grab/libs`. Both Flatpak and tarball/dev
/// builds fetch the tools here ([`install_ytdlp`], [`install_ffmpeg`]);
/// `/app/bin` and PATH remain as fallbacks for system-provided copies.
pub fn user_lib_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::env::temp_dir().join("grab-fallback-data"));
    base.join("grab").join("libs")
}

/// Bundled-tool directory inside the Flatpak sandbox. This is Flatpak
/// convention (the app tree is mounted at `/app`), not an XDG standard —
/// XDG only defines user directories, never bundle layouts. Absent
/// outside Flatpak, where the lookup simply skips it.
const FLATPAK_APP_BIN: &str = "/app/bin";

/// Candidate directories for the tools, in priority order: the user's own
/// installs first (so Update actually takes effect over any bundled copy),
/// then `/app/bin` (Flatpak), then PATH (which inside Flatpak includes the
/// runtime's /usr/bin where ffmpeg ships). A stale user copy cannot pin
/// old tools: the version floor refuses it with an update prompt.
fn tool_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![user_lib_dir(), PathBuf::from(FLATPAK_APP_BIN)];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()));
    }
    dirs
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

pub(crate) fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

/// Locate the yt-dlp and ffmpeg binaries, or say why they are missing.
pub fn resolve_libraries() -> Result<Libraries, VideoError> {
    let dirs = tool_search_dirs();
    let youtube = find_in_dirs("yt-dlp", &dirs).ok_or_else(VideoError::missing_tools)?;
    let ffmpeg = find_in_dirs("ffmpeg", &dirs).ok_or_else(VideoError::missing_tools)?;
    Ok(Libraries::new(youtube, ffmpeg))
}

/// Install just yt-dlp into the user library dir. Split from ffmpeg so the
/// UI can report honest per-tool stages; the crate installer exposes no
/// progress of its own. Await from a spawned task — never block the GTK
/// thread on it.
pub async fn install_ytdlp() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::runtime::tokio_rt()
        .spawn(async move { LibraryInstaller::new(dir).install_youtube(None).await });
    match handle.await {
        Ok(Ok(path)) => Ok(path),
        Ok(Err(e)) => Err(VideoError::install(&e)),
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// Install the ffmpeg toolchain (ffmpeg *and* ffprobe) into the user
/// library dir. The yt-dlp crate's installer only extracts the `ffmpeg`
/// binary, which leaves `--ffmpeg-location` pointing at a dir without
/// ffprobe — so Grab downloads the static-build archive itself and
/// extracts both tools in one pass. See [`install_ytdlp`].
pub async fn install_ffmpeg() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle =
        crate::runtime::tokio_rt().spawn(async move { install_ffmpeg_toolchain(dir).await });
    match handle.await {
        Ok(res) => res,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// Download one boul2gom/ffmpeg-builds archive and extract the `ffmpeg`
/// and `ffprobe` binaries into `dir`. Returns the ffmpeg path. Await
/// from a spawned task — never block the GTK thread on it.
async fn install_ffmpeg_toolchain(dir: PathBuf) -> Result<PathBuf, VideoError> {
    use yt_dlp::client::deps::ffmpeg::BuildFetcher;

    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(VideoError::install)?;
    let release = BuildFetcher::new()
        .fetch_binary()
        .await
        .map_err(VideoError::install)?;
    let archive = dir.join(&release.name);
    release
        .download(&archive)
        .await
        .map_err(VideoError::install)?;
    tokio::task::spawn_blocking(move || extract_ffmpeg_toolchain(&archive, &dir))
        .await
        .map_err(VideoError::runtime)?
        .map_err(VideoError::install)
}

/// Extract the `ffmpeg` and `ffprobe` binaries from a static-build
/// archive into `dir`, mark them executable, and delete the archive.
/// Entries are matched by file name, so both flat zips (`ffmpeg` at the
/// root, as the crate's own extractor assumes) and `bin/`-style layouts
/// work. Returns the ffmpeg path; a missing ffmpeg entry is an error,
/// a missing ffprobe entry is not — callers keep working the way they
/// did before this toolchain existed.
pub(crate) fn extract_ffmpeg_toolchain(archive: &Path, dir: &Path) -> Result<PathBuf, String> {
    // Always clean up the (large) archive, even when extraction fails.
    let result = extract_ffmpeg_toolchain_inner(archive, dir);
    std::fs::remove_file(archive).ok();
    result
}

/// Inner extraction; see [`extract_ffmpeg_toolchain`].
fn extract_ffmpeg_toolchain_inner(archive: &Path, dir: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;

    let file = std::fs::File::open(archive)
        .map_err(|e| format!("couldn't open ffmpeg archive {}: {e}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("couldn't read ffmpeg archive: {e}"))?;
    let mut ffmpeg_path = None;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("couldn't read ffmpeg archive entry: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let tool = match Path::new(entry.name()).file_name().and_then(|n| n.to_str()) {
            Some("ffmpeg") => "ffmpeg",
            Some("ffprobe") => "ffprobe",
            _ => continue,
        };
        let dest = dir.join(tool);
        let mut out = std::fs::File::create(&dest)
            .map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("couldn't extract {}: {e}", dest.display()))?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("couldn't mark {} executable: {e}", dest.display()))?;
        if tool == "ffmpeg" {
            ffmpeg_path = Some(dest);
        }
    }
    ffmpeg_path.ok_or_else(|| "ffmpeg binary not found in the downloaded archive".to_string())
}

/// Minimum accepted yt-dlp version by release date. Older binaries predate
/// the JS-challenge era and fail extraction in ways that look like broken
/// pages; refusing them with an actionable message beats a mystery
/// failure. Newer versions always pass.
pub const MIN_YTDLP_VERSION: [u32; 3] = [2026, 1, 1];

/// Parse a `yt-dlp --version` first line (`2026.08.19`) into comparable
/// parts. Anything else (nightlies, forks, garbage) is unverifiable.
pub(crate) fn parse_yt_dlp_version(first_line: &str) -> Option<[u32; 3]> {
    let mut parts = first_line.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some([major, minor, patch])
}

/// Whether a release `tag` is newer than the installed version line.
/// Unparseable tags never trigger an update prompt: an unknown upstream
/// shape must not nag. Tags carry no `v` prefix (`2026.08.19`), but one
/// is tolerated.
pub(crate) fn ytdlp_update_available(installed: &str, tag: &str) -> bool {
    match (
        parse_yt_dlp_version(installed),
        parse_yt_dlp_version(tag.trim_start_matches('v')),
    ) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

/// Real home directory from the passwd database, bypassing any sandbox
/// `$HOME` remapping (inside Flatpak `$HOME` is the app sandbox dir, not
/// the user's home). `None` on non-Unix or lookup failure.
#[cfg(unix)]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer to static storage (or null); we
    // only read pw_dir up to its NUL terminator on the calling thread.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        let dir = (*pw).pw_dir;
        if dir.is_null() {
            return None;
        }
        let len = libc::strlen(dir);
        if len == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(dir as *const u8, len);
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

#[cfg(not(unix))]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    None
}

/// Real host config directory, even inside a Flatpak sandbox where
/// `$HOME`/`$XDG_CONFIG_HOME` point at the app's own sandbox dirs.
/// Priority: `HOST_XDG_CONFIG_HOME` (Flatpak exposes the host value),
/// then passwd-database home + `.config`, then the normal XDG fallback.
pub(crate) fn real_config_home() -> PathBuf {
    if let Some(host) = std::env::var_os("HOST_XDG_CONFIG_HOME") {
        let p = PathBuf::from(&host);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(home) = real_home_dir() {
        return home.join(".config");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/etc/xdg"))
}

/// Profile directories holding a Chromium `Cookies` database, best first:
/// `Default`, then a top-level `Cookies` file, then `Profile *`.
fn chromium_profile_dirs(config: &Path, subdir: &str) -> Vec<PathBuf> {
    let base = config.join(subdir);
    let mut out = vec![base.join("Default"), base.clone()];
    if let Ok(entries) = std::fs::read_dir(&base) {
        let mut rest: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("Profile "))
            })
            .collect();
        rest.sort();
        out.extend(rest);
    }
    out.into_iter()
        .filter(|p| p.join("Cookies").is_file())
        .collect()
}

/// Firefox profile directories under one base dir, default first. Parses
/// every `[Profile*]` section of `profiles.ini` (`Path` + `IsRelative`
/// + `Default`); falls back to a directory scan when there is no ini.
fn firefox_profile_dirs(base: &Path) -> Vec<PathBuf> {
    if let Ok(text) = std::fs::read_to_string(base.join("profiles.ini")) {
        let mut ranked: Vec<(PathBuf, bool)> = Vec::new();
        let mut path: Option<String> = None;
        let mut relative = true;
        let mut is_default = false;
        let mut flush = |path: &mut Option<String>, relative: &mut bool, is_default: &mut bool| {
            if let Some(p) = path.take() {
                let dir = if *relative && !std::path::Path::new(&p).is_absolute() {
                    base.join(&p)
                } else {
                    PathBuf::from(&p)
                };
                ranked.push((dir, *is_default));
            }
            *relative = true;
            *is_default = false;
        };
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                flush(&mut path, &mut relative, &mut is_default);
            } else if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "Path" => path = Some(value.trim().to_string()),
                    "IsRelative" => relative = value.trim() != "0",
                    "Default" => is_default = value.trim() == "1",
                    _ => {}
                }
            }
        }
        flush(&mut path, &mut relative, &mut is_default);
        ranked.sort_by_key(|(_, d)| !d);
        return ranked
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| p.join("cookies.sqlite").is_file())
            .collect();
    }
    if let Ok(entries) = std::fs::read_dir(base) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".default-release") || n.ends_with(".default"))
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| base.join(n))
            .filter(|p| p.join("cookies.sqlite").is_file())
            .collect()
    } else {
        Vec::new()
    }
}

/// Chromium config subdirs per browser, most common first. One entry covers
/// the whole family: stable, beta, nightly/canary and dev builds all
/// resolve under it. Within one channel the first profile wins
/// ([`chromium_profile_dirs`] order); across channels the freshest `Cookies`
/// database wins — see [`freshest_chromium_profile`].
pub(crate) fn chromium_subdirs(browser: &str) -> &'static [&'static str] {
    match browser {
        "brave" => &[
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser-Beta",
            "BraveSoftware/Brave-Browser-Nightly",
            // Rebranded builds seen in the wild; skipped when absent.
            // Add non-standard forks only with a reported real path.
            "BraveSoftware/Brave-Origin-Beta",
            "BraveSoftware/Brave-Origin-Nightly",
            "BraveSoftware/Brave-Browser-Origin-Nightly",
        ],
        "chrome" => &[
            "google-chrome",
            "google-chrome-beta",
            "google-chrome-unstable",
        ],
        "chromium" => &["chromium", "chromium-beta"],
        "edge" => &[
            "microsoft-edge",
            "microsoft-edge-beta",
            "microsoft-edge-dev",
            // Not shipped on Linux today; harmless if absent.
            "microsoft-edge-canary",
        ],
        "opera" => &["opera", "opera-beta", "opera-developer"],
        "vivaldi" => &["vivaldi", "vivaldi-snapshot"],
        "whale" => &["naver-whale"],
        _ => &[],
    }
}

/// Best profile directory across every channel subdir of one Chromium
/// browser: each channel contributes its preferred profile (same order as
/// [`chromium_profile_dirs` — `Default` first), then the channel whose
/// `Cookies` database was modified most recently wins. That is the browser
/// the user actually runs; a stale install whose directory merely exists
/// (e.g. Brave stable shadowing a daily-driven Origin Beta) must not win.
/// Ties keep channel order via the stable sort; unreadable mtimes sort
/// after every readable one, so the outcome stays deterministic.
fn freshest_chromium_profile(config_home: &Path, browser: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = chromium_subdirs(browser)
        .iter()
        .filter_map(|sub| chromium_profile_dirs(config_home, sub).into_iter().next())
        .collect();
    candidates.sort_by(|a, b| {
        let mtime = |profile: &PathBuf| {
            std::fs::metadata(profile.join("Cookies"))
                .and_then(|meta| meta.modified())
                .ok()
        };
        // Descending; `Option` orders `None` last.
        mtime(b).cmp(&mtime(a))
    });
    candidates.into_iter().next()
}

/// Absolute browser profile directory for `--cookies-from-browser`, resolved
/// against the real host config/home dirs (see [`real_config_home`]) so it
/// works inside the Flatpak sandbox where `$HOME` is remapped. Testable core:
/// `config_home` stands in for [`real_config_home`], `home` for the passwd
/// home (snap Firefox lives under it, not under the config dir).
pub(crate) fn browser_profile_dir_in(
    config_home: &Path,
    home: &Path,
    browser: &str,
) -> Option<PathBuf> {
    if browser == "firefox" {
        return [
            home.join(".mozilla/firefox"),
            config_home.join("mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
        ]
        .into_iter()
        .find_map(|base| firefox_profile_dirs(&base).into_iter().next());
    }
    // Zen is Firefox-based (same profiles.ini + cookies.sqlite layout),
    // but keeps its profiles under ~/.zen instead of ~/.mozilla/firefox.
    if browser == "zen" {
        return [home.join(".zen"), config_home.join("zen")]
            .into_iter()
            .find_map(|base| firefox_profile_dirs(&base).into_iter().next());
    }
    freshest_chromium_profile(config_home, browser)
}

/// [`browser_profile_dir_in`] against the real host directories. When
/// `HOST_XDG_CONFIG_HOME` is set (Flatpak, and the unit tests), the home
/// dir is its parent — `<home>/.config` — so both roots stay consistent
/// without touching the sandbox `$HOME`.
pub(crate) fn browser_profile_dir(browser: &str) -> Option<PathBuf> {
    let config = real_config_home();
    let home = std::env::var_os("HOST_XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .and_then(|p| p.parent().map(PathBuf::from))
        .or_else(real_home_dir)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    browser_profile_dir_in(&config, &home, browser)
}

/// Browsers offered for `--cookies-from-browser`, in combo order. Values
/// are the yt-dlp browser names; labels come from [`cookies_browser_labels`].
pub const COOKIES_BROWSERS: &[&str] = &[
    "none", "brave", "chrome", "chromium", "edge", "firefox", "opera", "vivaldi", "whale", "zen",
];

/// Spec for `--cookies-from-browser`: `browser:/absolute/profile/dir` when
/// the profile resolves on disk, else the bare browser name so yt-dlp falls
/// back to its own `$HOME`-relative lookup (correct outside Flatpak).
/// `None`/unknown means off. The profile path must be the profile
/// *directory* — yt-dlp opens and decrypts the cookie database itself
/// (keyring included); a raw `Cookies` file is not a `--cookies` export.
pub(crate) fn cookies_browser_spec(value: &str) -> Option<String> {
    if value.is_empty() || value == "none" || !COOKIES_BROWSERS.contains(&value) {
        return None;
    }
    // yt-dlp has no "zen" browser; Zen is Firefox-based (same
    // cookies.sqlite layout), so its resolved profile is handed to the
    // Firefox extractor.
    let ytdlp_browser = if value == "zen" { "firefox" } else { value };
    if let Some(dir) = browser_profile_dir(value) {
        return Some(format!("{ytdlp_browser}:{}", dir.display()));
    }
    Some(ytdlp_browser.to_string())
}

/// Shared trailing argv for every yt-dlp spawn: YouTube player-client
/// workaround, browser cookies, user agent, then the page URL behind
/// `--`. One helper so these flags can never drift between extraction,
/// parts, HLS and live-resolve spawns (or let a hostile URL parse as a
/// flag). `None` user agent keeps today's extraction behavior (yt-dlp
/// default UA there).
pub(crate) fn ytdlp_identity_args(
    cookies_browser: &str,
    user_agent: Option<&str>,
    page_url: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    // YouTube force-enables SABR-only streaming for the `web` player
    // client (yt-dlp#12482): its formats come back URL-less and its
    // playability status fails the whole extraction ("The page needs to
    // be reloaded"). The `web` client only enters yt-dlp's default
    // rotation when a JS runtime is available — e.g. the Flatpak's
    // bundled deno — so exclude it everywhere and stay on the working
    // clients. Scoped to the youtube extractor: a no-op for other sites.
    args.push("--extractor-args".to_string());
    args.push("youtube:player_client=-web".to_string());
    if let Some(spec) = cookies_browser_spec(cookies_browser) {
        args.push(format!("--cookies-from-browser={spec}"));
    }
    if let Some(ua) = user_agent.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--user-agent".to_string());
        args.push(ua.to_string());
    }
    // `--` before the page URL: option parsing ends here, so a hostile
    // or malformed URL can never be read as a flag.
    args.push("--".to_string());
    args.push(page_url.to_string());
    args
}

/// First search dir holding a complete ffmpeg toolchain (`ffmpeg` plus
/// `ffprobe`). yt-dlp resolves both tools from `--ffmpeg-location` and
/// never falls back to PATH for a missing sibling, so pointing it at a
/// dir with only `ffmpeg` (e.g. Grab's own user-lib install, which
/// shadows a perfectly good system install) breaks post-processing with
/// "ffprobe not found" even when the system ships both binaries.
pub(crate) fn toolchain_dir_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .find(|d| is_executable(&d.join("ffmpeg")) && is_executable(&d.join("ffprobe")))
        .cloned()
}

/// Directory form of a resolved tool binary for `--ffmpeg-location`
/// (yt-dlp wants the directory; Grab resolves the binary). Prefers a
/// dir with both ffmpeg and ffprobe; falls back to the binary's own dir
/// (the previous behavior) when no complete toolchain is on hand.
pub(crate) fn ffmpeg_location_dir(ffmpeg_bin: &Path) -> String {
    toolchain_dir_in(&tool_search_dirs())
        .or_else(|| ffmpeg_bin.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/usr/bin"))
        .to_string_lossy()
        .into_owned()
}
