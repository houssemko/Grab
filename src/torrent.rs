//! Magnet + .torrent intake and engine (librqbit), bridged onto the HTTP pump.
//!
//! Two intakes, one engine: `magnet:` links and `.torrent` files (archived
//! under the data dir and referenced by a `torrent:{abs-path}` pseudo-URL).
//! One lazy global [`Session`] serves every torrent row; per-row progress
//! arrives as the same [`EngineMsg`] the HTTP engine emits, so
//! pause/cancel/retry/persist keep working unchanged. Row identity is the
//! queue id; the torrent registry below maps it to the live handle for
//! pause/cancel/delete paths.

use std::{
    collections::HashMap,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

use gettextrs::gettext;
use gtk4::glib;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ConnectionOptions, ListenerOptions,
    ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig, TorrentStatsState,
    api::TorrentIdOrHash,
};
use tokio::sync::{Mutex, OnceCell, mpsc::UnboundedSender};

use crate::engine_msg::EngineMsg;
use crate::file_names::{dedupe_filename, sane_filename, shorten_filename};
use crate::runtime::tokio_rt;

// rqbit keeps the handle alias private (`torrent_state` is not public API),
// so name it locally: it is just a refcounted managed torrent.
type ManagedTorrentHandle = Arc<ManagedTorrent>;

/// A live torrent row: registered at spawn, removed on terminal/cancel.
struct Active {
    hash_hex: String,
    handle: Option<ManagedTorrentHandle>,
    /// Pause arrived while the torrent was still being added.
    paused: bool,
}

static SESSION: OnceCell<Arc<Session>> = OnceCell::const_new();
static ACTIVE: std::sync::LazyLock<Mutex<HashMap<u64, Active>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
/// Pre-chosen file indices for multi-file torrents, staged at intake
/// (file-list dialog) and consumed once by the engine at spawn. Keyed by
/// pseudo-URL: only archived files carry selections, magnets pass None.
static PENDING_SELECTIONS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Vec<usize>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Lock the staged-selection map, recovering from poisoning: a panicked
/// test must not deadlock the suite on the next lock.
fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, Vec<usize>>> {
    PENDING_SELECTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stage a file selection for the next spawn of `pseudo_url`.
pub fn stage_selection(pseudo_url: &str, only_files: Vec<usize>) {
    lock_pending().insert(pseudo_url.to_string(), only_files);
}

/// Read the staged selection for `pseudo_url`, if any. Peek, not take:
/// retries and re-spawns re-add from scratch and need the same filter.
/// Stale entries are pruned alongside archive sweeps (see
/// `prune_selections`); in-session the key outlives Undo because unremove
/// re-inserts the same pseudo-URL.
pub(crate) fn get_selection(pseudo_url: &str) -> Option<Vec<usize>> {
    lock_pending().get(pseudo_url).cloned()
}

/// Drop staged selections no live row references (companion to
/// `sweep_archives`, called with the same referenced set).
pub fn prune_selections(referenced: &std::collections::HashSet<String>) {
    lock_pending().retain(|k, _| referenced.contains(k));
}

/// Drop the staged selection for `pseudo_url` (companion to
/// `prune_selections`): a finished row never re-spawns, so keeping its
/// filter would pin the entry until the next sweep.
pub(crate) fn drop_selection(pseudo_url: &str) {
    lock_pending().remove(pseudo_url);
}

/// True for either torrent source: magnet links and archived .torrent
/// pseudo-URLs. Lifecycle paths (pause/park/cancel/delete) are id-based
/// and source-agnostic, so they key off this.
pub fn is_torrent(s: &str) -> bool {
    is_magnet(s) || is_torrent_url(s)
}

/// Case-insensitive ASCII prefix check on leading-whitespace-trimmed
/// input. `get` (not slicing): non-ASCII input must never panic the
/// classifier.
fn has_prefix_ci(s: &str, prefix: &str) -> bool {
    s.trim_start()
        .get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

pub fn is_magnet(s: &str) -> bool {
    // Match the bare `magnet:` prefix, not `magnet:?`: parse_magnet is the
    // real validator, and anything magnet:-shaped must never fall through
    // to the http scheme branch (tracker params contain `://`).
    has_prefix_ci(s, "magnet:")
}

/// Parse a magnet link, rejecting anything rqbit cannot resolve to BTv1.
pub fn parse_magnet(s: &str) -> Result<librqbit::Magnet, String> {
    let m = librqbit::Magnet::parse(s.trim()).map_err(|e| format!("Invalid magnet link: {e}"))?;
    if m.as_id20().is_none() {
        return Err(gettext("Only BitTorrent v1 magnets are supported"));
    }
    Ok(m)
}

/// Raw torrent display name, lossy-decoded. Callers apply their own gate
/// via [`safe_torrent_name`] and pick their own fallback (info-hash hex,
/// `"torrent"`, or stub), which deliberately differ per site — this helper
/// only decodes.
fn raw_torrent_name<B: AsRef<[u8]>>(name: Option<&B>) -> Option<String> {
    name.map(|n| String::from_utf8_lossy(n.as_ref()).into_owned())
}

/// A torrent's advertised name prepared for the filesystem: it must pass
/// the filename gate, then is shortened. `None` when the caller must fall
/// back (info-hash hex, `"torrent"`, … — the fallback deliberately differs
/// per site, so it stays with the caller).
fn safe_torrent_name(name: Option<String>) -> Option<String> {
    name.filter(|n| sane_filename(n))
        .map(|n| shorten_filename(&n))
}

/// Display stub for a magnet row: the advertised name, else the info-hash hex.
pub fn stub_name(magnet: &str) -> Option<String> {
    let m = parse_magnet(magnet).ok()?;
    // A hostile display name must never reach the filesystem: fall back to
    // the info-hash hex when dn is missing or fails the filename gate.
    m.name
        .clone()
        .filter(|n| crate::file_names::sane_filename(n))
        .or_else(|| m.as_id20().map(|id| id.as_string()))
}

fn bps(v: Option<u64>) -> Option<NonZeroU32> {
    v.and_then(|b| u32::try_from(b).ok())
        .and_then(NonZeroU32::new)
}

/// What the engine must fetch: a magnet link, or archived .torrent bytes.
#[derive(Debug, Clone)]
pub enum TorrentSource {
    Magnet(String),
    File(PathBuf),
}

/// Pseudo-URL prefix for archived .torrent files (never leaves the app).
pub fn is_torrent_url(s: &str) -> bool {
    has_prefix_ci(s, "torrent:")
}

/// Archive dir for .torrent files: alongside the session state.
fn torrents_dir() -> PathBuf {
    glib::user_data_dir().join("grab").join("torrents")
}

/// Absolute archive path for a `torrent:{abs-path}` pseudo-URL.
/// Constrained to the app's own archive dir (plus a `.torrent` suffix) so
/// queue rows can only ever resolve to — and delete — files Grab archived.
pub fn archive_path_for_url(url: &str) -> Option<PathBuf> {
    let path = url.trim_start().get("torrent:".len()..)?;
    let path = PathBuf::from(path);
    // Existence is part of validity: intake rejects doodled pseudo-URLs
    // fast, and restore only ever sees swept-kept archives.
    (path.is_absolute()
        && path.starts_with(torrents_dir())
        && path.extension().is_some_and(|e| e == "torrent")
        && path.exists())
    .then_some(path)
}

/// Sanitized file stem of a user-supplied file name, if it passes the
/// filename gate. Path traversal (`../`) never survives: `file_stem`
/// strips directories, and hostile stems fail `sane_filename`.
fn safe_stem(file_name: &str) -> Option<&str> {
    Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| sane_filename(s))
}

/// Copy `.torrent` bytes into the archive, named after the sanitized file
/// stem. Returns the pseudo-URL the queue row stores.
pub fn archive_torrent_file(file_name: &str, bytes: &[u8]) -> Result<String, String> {
    const MAX_TORRENT_BYTES: usize = 10_000_000;
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(gettext("Torrent file is too large (max 10 MB)"));
    }
    // Parse first: never archive bytes rqbit itself would reject.
    librqbit::torrent_from_bytes(bytes).map_err(|e| format!("Invalid torrent file: {e}"))?;
    let stem = safe_stem(file_name).unwrap_or("torrent");
    let dir = torrents_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot store torrent file: {e}"))?;
    // Dedupe against existing archives the same way downloads do.
    let candidate = dir.join(dedupe_filename(&format!("{stem}.torrent"), |n| {
        dir.join(n).exists()
    }));
    std::fs::write(&candidate, bytes).map_err(|e| format!("Cannot store torrent file: {e}"))?;
    Ok(format!("torrent:{}", candidate.to_string_lossy()))
}

/// Stub row name for an archived file: the sanitized file stem.
pub fn stub_name_for_file(file_name: &str) -> String {
    safe_stem(file_name)
        .map(shorten_filename)
        .unwrap_or_else(|| "torrent".to_string())
}

/// One listed file of a multi-file torrent: the lossy-decoded entry
/// path for filesystem operations, its sanitized display form for the
/// UI, and the byte length. The two paths are deliberately separate:
/// the display form is truncated and control-stripped, so it can differ
/// from the on-disk name and must never be joined against folders.
pub struct TorrentFileEntry {
    /// Entry path joined from the torrent's byte-string components with
    /// invalid UTF-8 replaced (`String::from_utf8_lossy`) — not the raw
    /// bytes. Distinct non-UTF-8 components can collapse to the same
    /// string (pinned by `raw_path_collapses_distinct_invalid_utf8`).
    /// Still the only path ever used for filesystem operations: for
    /// UTF-8 torrents it matches the names the engine writes, because
    /// the engine decodes components the same lossy way before
    /// touching disk.
    pub raw_path: String,
    /// Display form of `raw_path` (see `sanitize_display_path`). UI
    /// only — never a filesystem path.
    pub display_path: String,
    pub length: u64,
}

/// Upper bound for a `.torrent` file read from disk: torrents are
/// small metadata documents, and the read itself needs a ceiling.
pub(crate) const MAX_TORRENT_BYTES: u64 = 10_000_000;

/// Read a `.torrent` file with a single bounded open. The size gate and
/// the read must not be separate opens — a concurrent replacement
/// between `metadata` and `read` would invalidate the check — and
/// `std::fs::read` itself has no byte limit. Pure filesystem helper.
pub(crate) fn read_torrent_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_TORRENT_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    (buf.len() as u64 <= MAX_TORRENT_BYTES).then_some(buf)
}

/// Bounded read of the archived `.torrent` behind a queue pseudo-URL.
/// The single funnel for every archive read by pseudo-URL: the path is
/// constrained by `archive_path_for_url` and the bytes by
/// `read_torrent_bytes`, so a replaced or corrupted archive can never be
/// loaded unbounded. `None` when the archive is missing, unreadable, or
/// over the byte ceiling. (`run_torrent`'s `TorrentSource::File` arm
/// holds an already-resolved archive path, so it calls
/// `read_torrent_bytes` directly instead.)
pub(crate) fn read_archive_bytes(url: &str) -> Option<Vec<u8>> {
    read_torrent_bytes(&archive_path_for_url(url)?)
}

/// Parse `.torrent` bytes into a display list for the file picker.
/// Byte-string path components are lossy-converted; lengths are exact.
/// Display form of a torrent entry path: entry bytes are attacker
/// input, so bidi overrides and controls are stripped (row spoofing)
/// and the string is capped (dialog stretching). Selection is
/// index-based, so this never affects which files download. Pure.
fn sanitize_display_path(path: &str) -> String {
    const MAX_CHARS: usize = 120;
    fn bidi(c: char) -> bool {
        matches!(c, '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    }
    let mut out: String = path
        .chars()
        .filter(|c| !c.is_control() && !bidi(*c))
        .take(MAX_CHARS + 1)
        .collect();
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect();
        out.push('…');
    }
    out
}

pub fn torrent_file_list(bytes: &[u8]) -> Result<(String, Vec<TorrentFileEntry>), String> {
    let meta =
        librqbit::torrent_from_bytes(bytes).map_err(|e| format!("Invalid torrent file: {e}"))?;
    let name = raw_torrent_name(meta.info.data.name.as_ref())
        .filter(|n| sane_filename(n))
        .unwrap_or_else(|| "torrent".to_string());
    let entries: Vec<TorrentFileEntry> =
        meta.info.data.files.as_ref().map_or(Vec::new(), |files| {
            files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let parts: Vec<String> = f
                        .path
                        .iter()
                        .map(|c| String::from_utf8_lossy(c.as_ref()).into_owned())
                        .collect();
                    let raw = parts.join("/");
                    let shown = sanitize_display_path(&raw);
                    TorrentFileEntry {
                        raw_path: raw,
                        display_path: if shown.is_empty() {
                            format!("file {i}")
                        } else {
                            shown
                        },
                        length: f.length,
                    }
                })
                .collect()
        });
    Ok((name, entries))
}

/// Delete the archive behind a pseudo-URL. Missing files are fine.
pub fn delete_archive_for_url(url: &str) {
    if let Some(path) = archive_path_for_url(url) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove archives no live row references (stale Failed/Cancelled leftovers
/// are kept deliberately: retry needs them). Called from queue restore.
pub fn sweep_archives(referenced: &std::collections::HashSet<String>) {
    sweep_archives_in(&torrents_dir(), referenced);
}

/// Testable core: sweep a directory, keeping only referenced archives.
/// Split out so tests never touch the user's real archive dir.
pub fn sweep_archives_in(dir: &std::path::Path, referenced: &std::collections::HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
            continue;
        }
        let url = format!("torrent:{}", path.to_string_lossy());
        if !referenced.contains(&url) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Where a torrent's files go: multi-file torrents get a subfolder named
/// after the torrent (mirroring rqbit's own default, which our explicit
/// output_folder bypasses); single-file torrents sit flat in `dest`.
/// Magnet file counts only arrive mid-download, so magnets always land
/// flat. The fallback (info-hash hex) is always filesystem-safe.
fn output_folder_for(
    dest: &std::path::Path,
    name: Option<String>,
    multi: bool,
    fallback: &str,
) -> std::path::PathBuf {
    if !multi {
        return dest.to_path_buf();
    }
    let dir = safe_torrent_name(name).unwrap_or_else(|| fallback.to_string());
    dest.join(dir)
}

/// Two or more files means the torrent downloads into its own folder;
/// single-file torrents sit flat in `dest`. Boundary (`>= 2`) pinned by
/// `intake_plan_mirrors_engine_layout`.
fn is_multi_file<T>(files: Option<&[T]>) -> bool {
    files.is_some_and(|f| f.len() >= 2)
}

/// What the engine will download into for archived `.torrent` bytes:
/// the folder base plus whether it is multi-file. Mirrors `run_torrent`'s
/// File branch exactly, so intake can detect on-disk collisions upfront
/// instead of letting `overwrite: true` clobber existing files.
pub(crate) fn intake_plan(bytes: &[u8]) -> Option<(String, bool)> {
    let meta = librqbit::torrent_from_bytes(bytes).ok()?;
    let raw = raw_torrent_name(meta.info.data.name.as_ref());
    let multi = is_multi_file(meta.info.data.files.as_deref());
    let base = safe_torrent_name(raw).unwrap_or_else(|| meta.info_hash.as_string());
    Some((base, multi))
}

/// Real on-disk location of a torrent's files for deletion: recomputes the
/// engine's output folder deterministically from the archived metadata, so
/// Delete trashes what the engine actually wrote instead of the row's stub
/// path (never written for multi-file torrents). Returns None for magnets
/// and single-file torrents (caller falls back to the row's own file path).
pub fn torrent_output_dir(dest: &std::path::Path, url: &str) -> Option<PathBuf> {
    if !is_torrent_url(url) {
        return None;
    }
    let bytes = read_archive_bytes(url)?;
    let meta = librqbit::torrent_from_bytes(&bytes).ok()?;
    let multi = is_multi_file(meta.info.data.files.as_deref());
    if !multi {
        return None;
    }
    let name = raw_torrent_name(meta.info.data.name.as_ref());
    // Same fallback the engine uses (info-hash hex): deterministic match.
    let fallback = meta.info_hash.as_string();
    Some(output_folder_for(dest, name, true, &fallback))
}

/// Delete untoggled files after a filtered torrent finishes. librqbit
/// creates every file upfront and only gates piece requests, so untoggled
/// files linger as 0-byte placeholders (or shared-piece bytes) unless
/// removed here. Only listed files go; parents are pruned while empty,
/// stopping at `folder`. Skipped silently when nothing was staged, the
/// archive is gone, or an entry path looks hostile.
pub(crate) fn cleanup_unselected(folder: &std::path::Path, url: &str) {
    let Some(selected) = get_selection(url) else {
        return;
    };
    if selected.is_empty() {
        return;
    }
    let Some(bytes) = read_archive_bytes(url) else {
        return;
    };
    let Ok((_, entries)) = torrent_file_list(&bytes) else {
        return;
    };
    if entries.len() < 2 {
        return;
    }
    let keep: std::collections::HashSet<usize> = selected.into_iter().collect();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(target) = deletion_target(folder, i, &keep, entry) {
            remove_file_and_prune_parents(&target, folder);
        }
    }
}

/// Deletion target for one unselected torrent entry, if any: kept and
/// hostile entries yield `None`. The target joins the entry's RAW path
/// — the sanitized display path is truncated and control-stripped and
/// can name a file that does not exist on disk. Pure.
pub(crate) fn deletion_target(
    folder: &std::path::Path,
    index: usize,
    keep: &std::collections::HashSet<usize>,
    entry: &TorrentFileEntry,
) -> Option<std::path::PathBuf> {
    if keep.contains(&index) || is_hostile_entry(&entry.raw_path) {
        return None;
    }
    Some(folder.join(&entry.raw_path))
}

/// Whether a torrent entry path may escape its folder (`..`, absolute
/// paths, empty segments): hostile entries are never joined — the file
/// simply stays behind.
fn is_hostile_entry(path: &str) -> bool {
    path.split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
}

/// Remove a file, then prune parents left empty, stopping at (and never
/// removing) `folder`.
fn remove_file_and_prune_parents(path: &std::path::Path, folder: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == folder {
            break;
        }
        match std::fs::remove_dir(dir) {
            Ok(()) => parent = dir.parent(),
            Err(_) => break,
        }
    }
}
/// Network plan for one torrent add, resolved from settings at spawn.
/// A SOCKS5 proxy takes over TCP peers and HTTP trackers — but the engine
/// cannot proxy DHT (UDP), inbound connections, or UDP trackers, so those
/// (and the UPnP forwarding that serves them) go dark instead of leaking
/// around the tunnel. HTTP(S) proxies can't be used at all: passthrough.
/// Pure for tests.
pub(crate) struct TorrentNetPlan {
    pub dht: bool,
    /// Local Service Discovery: forced off under SOCKS5 (multicast can't
    /// traverse the tunnel and would leak local presence like DHT).
    pub lsd: bool,
    pub listen_port: i32,
    pub upnp: bool,
    pub trackers: Option<Vec<String>>,
    pub socks_proxy: Option<String>,
}

pub(crate) fn plan_torrent_net(
    dht: bool,
    lsd: bool,
    listen_port: i32,
    upnp: bool,
    trackers: Option<Vec<String>>,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) -> TorrentNetPlan {
    let Some(url) = proxy.and_then(|p| p.torrent_socks_url()) else {
        return TorrentNetPlan {
            dht,
            lsd,
            listen_port,
            upnp,
            trackers,
            socks_proxy: None,
        };
    };
    let trackers = trackers
        .map(|ts| {
            ts.into_iter()
                .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
                .collect::<Vec<_>>()
        })
        .filter(|ts| !ts.is_empty());
    TorrentNetPlan {
        dht: false,
        lsd: false,
        listen_port: 0,
        upnp: false,
        trackers,
        socks_proxy: Some(url),
    }
}

/// Creation-scoped torrent session inputs: everything `ensure_session`
/// needs that rqbit only reads when the session starts. Bundled into one
/// struct so the growing preference list doesn't trip clippy's
/// too-many-arguments lint at the call boundary.
#[derive(Clone, Debug)]
pub(crate) struct SessionConfig {
    pub dht: bool,
    pub peer_limit: Option<usize>,
    pub download_bps: Option<u64>,
    pub upload_bps: Option<u64>,
    pub listen_port: i32,
    pub upnp: bool,
    pub socks_proxy: Option<String>,
    pub blocklist_url: Option<String>,
    pub lsd: bool,
}

async fn ensure_session(cfg: SessionConfig) -> Result<Arc<Session>, String> {
    let SessionConfig {
        dht,
        peer_limit,
        download_bps,
        upload_bps,
        listen_port,
        upnp,
        socks_proxy,
        blocklist_url,
        lsd,
    } = cfg;
    SESSION
        .get_or_try_init(|| async {
            let dir = glib::user_data_dir().join("grab");
            std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create session dir: {e}"))?;
            // Fast resume: persist per-torrent progress (bitfields) so a
            // relaunch skips re-hashing completed pieces, and remember the
            // session's torrents across restarts. Creation re-adds every
            // remembered torrent (Grab re-adds its own rows right after and
            // adopts those handles); entries with no queue row left are
            // swept by `sweep_session_orphans` once the queue is restored.
            let mut opts = SessionOptions {
                fastresume: true,
                persistence: Some(SessionPersistenceConfig::Json {
                    folder: Some(dir.join("session")),
                }),
                peer_limit,
                ..Default::default()
            };
            if !dht {
                opts.dht = None;
            }
            // Creation-scoped like DHT above: no live setter exists, so
            // edits apply to sessions created after them.
            opts.blocklist_url = blocklist_url;
            opts.disable_local_service_discovery = !lsd;
            // SOCKS5 carries outgoing TCP peers (and HTTP trackers via the
            // session client); creation-scoped like DHT/listener below, so
            // proxy edits apply to sessions created after them — same rule
            // as every other network toggle here.
            if let Some(proxy_url) = socks_proxy {
                opts.connect = Some(ConnectionOptions {
                    proxy_url: Some(proxy_url),
                    ..Default::default()
                });
            }
            // The session struct carries no live setter for this: it applies
            // here (see the initializer above) and per add below, so new
            // downloads pick up edits.
            // 0 means disabled (status quo: no listener). Positive ports
            // bind dual-stack; the session only reads this at creation.
            if listen_port > 0 {
                opts.listen = Some(ListenerOptions {
                    listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, listen_port as u16).into(),
                    enable_upnp_port_forwarding: upnp,
                    ..Default::default()
                });
            }
            let session = Session::new_with_opts(dir, opts)
                .await
                .map_err(|e| format!("Cannot start torrent engine: {e}"))?;
            session.ratelimits.set_download_bps(bps(download_bps));
            // The upload cap rides the same session-wide limiter; like the
            // download cap it also live-applies through apply_live_limits.
            session.ratelimits.set_upload_bps(bps(upload_bps));
            Ok(session)
        })
        .await
        .map(Arc::clone)
}

/// Validate the peer-blocklist preference: empty disables it, anything
/// else must be an http(s) URL. Fails loudly instead of silently
/// torrenting without the blocklist the user asked for.
pub(crate) fn blocklist_url_of(raw: &str) -> Result<Option<String>, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Ok(None);
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(Some(url.to_string()));
    }
    Err(gettext("Peer blocklist must be an http(s) URL"))
}
/// Live-apply the speed caps (settings watchers, any thread): the rate
/// limiter is internally synchronized. Peer limit has no live setter in
/// rqbit, so it applies at session creation and per add instead.
pub(crate) fn apply_live_limits(download_bps: Option<u64>, upload_bps: Option<u64>) {
    if let Some(s) = SESSION.get() {
        s.ratelimits.set_download_bps(bps(download_bps));
        s.ratelimits.set_upload_bps(bps(upload_bps));
    }
}

/// Snapshot of the live session, if the engine has started.
pub(crate) fn session_handle() -> Option<Arc<Session>> {
    SESSION.get().cloned()
}

/// Info-hash hexes currently claimed by live spawns. Covers rows whose
/// engine hasn't finished adding yet (ACTIVE is registered before the
/// session is even created).
pub(crate) async fn active_hashes() -> std::collections::HashSet<String> {
    ACTIVE
        .lock()
        .await
        .values()
        .map(|a| a.hash_hex.clone())
        .collect()
}

/// Info-hash hex for a torrent queue URL: magnets parse the link, archived
/// .torrent files parse their bytes. `None` when unresolvable — the row
/// fails at spawn with the real error; the orphan sweep just skips it.
pub(crate) fn info_hash_for_url(url: &str) -> Option<String> {
    if is_magnet(url) {
        return parse_magnet(url).ok()?.as_id20().map(|h| h.as_string());
    }
    if is_torrent_url(url) {
        let bytes = read_archive_bytes(url)?;
        let meta = librqbit::torrent_from_bytes(&bytes).ok()?;
        return Some(meta.info_hash.as_string());
    }
    None
}

/// Drop session torrents no queue row claims. Session persistence re-adds
/// every remembered torrent at engine creation — including entries whose
/// rows vanished in a crash between row removal and the session delete.
/// Those orphans would otherwise download/seed with no UI row: `keep`
/// carries the queue's torrent hashes, live spawns are added here, and
/// everything else goes. Files stay on disk, like any cancel, so a later
/// re-add resumes instead of restarting.
pub(crate) async fn sweep_session_orphans(keep: &std::collections::HashSet<String>) {
    let Some(session) = session_handle() else {
        return;
    };
    // Auto-restore completes inside Session::new_with_opts, so every entry
    // listed here is either adopted by a row below or an orphan: a spawn
    // racing us is already in ACTIVE (registered before ensure_session) or
    // still queued (its hash came from the caller's queue set). ACTIVE is
    // snapshotted after the listing, so a spawn that claimed a listed
    // torrent in between is never swept out from under it.
    let api = Api::new(session.clone(), None);
    let torrents = api.api_torrent_list().torrents;
    let active = active_hashes().await;
    for t in torrents {
        if keep.contains(&t.info_hash) || active.contains(&t.info_hash) {
            continue;
        }
        let Some(id) = t.id else { continue };
        // Session::delete panics (expect) on torrents whose metadata hasn't
        // resolved yet — re-added magnets start metadata-less until the
        // swarm re-resolves them. Skip those: the orphan is retried on the
        // next launch, instead of poisoning the sweep forever.
        let resolved = session
            .get(TorrentIdOrHash::Id(id))
            .is_some_and(|h| h.metadata.load().is_some());
        if !resolved {
            tracing::debug!(info_hash = %t.info_hash, "keeping metadata-less orphan for the next launch");
            continue;
        }
        tracing::debug!(info_hash = %t.info_hash, "dropping orphaned session torrent");
        // Best effort: a racing session delete is harmless, and a missed
        // orphan is retried on the next launch.
        let _ = session.delete(TorrentIdOrHash::Id(id), false).await;
    }
}

/// Unpause an adopted persisted handle, then re-check the row's paused
/// flag: a pause racing this unpause wins instead of being silently lost.
/// Pause sets the flag under the ACTIVE lock before touching the session,
/// so a racing pause is either visible to the re-check or lands its own
/// session pause after our unpause — both end paused.
async fn unpause_adopted(session: &Arc<Session>, id: u64, handle: &ManagedTorrentHandle) {
    let _ = session.unpause(handle).await;
    if ACTIVE.lock().await.get(&id).is_some_and(|a| a.paused) {
        let _ = session.pause(handle).await;
    }
}

/// Pause a live torrent. Fire-and-forget: the engine task keeps polling, but
/// every state it reports from here on is Paused, which the pump ignores.
pub(crate) fn pause_download(id: u64) {
    tokio_rt().spawn(async move {
        let handle = {
            let mut active = ACTIVE.lock().await;
            match active.get_mut(&id) {
                Some(a) => {
                    a.paused = true;
                    a.handle.clone()
                }
                None => return,
            }
        };
        if let (Some(h), Some(s)) = (handle, SESSION.get()) {
            let _ = s.pause(&h).await;
        }
    });
}

/// Drop a torrent row from the session, keeping partial files on disk
/// (cancel/retry and remove/Undo resume instead of restarting; explicit
/// delete discards the files instead).
pub(crate) fn forget_download(id: u64) {
    tokio_rt().spawn(async move {
        let active = ACTIVE.lock().await.remove(&id);
        if let (Some(a), Some(s)) = (active, SESSION.get())
            && let Some(h) = a.handle
        {
            // Same skip-guard as the orphan sweep: Session::delete panics
            // on metadata-less magnets. The entry stays for the next sweep.
            if h.metadata.load().is_some() {
                let _ = s.delete(TorrentIdOrHash::Hash(h.info_hash()), false).await;
            } else {
                tracing::debug!(id, info_hash = %h.info_hash().as_string(), "keeping metadata-less torrent for the orphan sweep");
            }
        }
    });
}

/// Seed-limit check for the poll loop: ratio and/or seed-time rules.
/// Pure (and unit-testable — the loop itself is timing-dependent).
/// `t0` is the first finished tick; seed-time counts from there.
fn seed_limits_hit(
    seed_ratio: f64,
    seed_time_min: i32,
    t0: std::time::Instant,
    total_bytes: u64,
    uploaded_bytes: u64,
) -> bool {
    let ratio_hit = seed_ratio > 0.0
        && total_bytes > 0
        && uploaded_bytes as f64 >= seed_ratio * total_bytes as f64;
    let time_hit = seed_time_min > 0
        && t0.elapsed() >= std::time::Duration::from_secs(seed_time_min as u64 * 60);
    ratio_hit || time_hit
}

/// Evict a finished row from the session unless still seeding: every
/// managed torrent pins its chunk-tracker, storage and peer state, so
/// completed rows would leak RAM one torrent at a time. Files stay on
/// disk (`false` = keep files).
async fn maybe_evict(session: &Session, id: TorrentIdOrHash, finished: bool, seed_finished: bool) {
    if finished && !seed_finished {
        let _ = session.delete(id, false).await;
    }
}

/// Progress message for one poll tick. Torrent rows carry their live
/// upload counters here; HTTP rows send zeros, keeping the torrent-only
/// upload suffix in the pump empty for them.
fn progress_msg(downloaded: u64, total: Option<u64>, uploaded: u64, upload_bps: u64) -> EngineMsg {
    EngineMsg::Progress {
        downloaded,
        total,
        uploaded,
        upload_bps,
    }
}

/// Poll one managed torrent into the pump. Returns true when the download
/// finished (caller deletes the .torrent archive then; Failed/Cancelled
/// keep theirs for retry).
async fn poll_loop(
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    stub: &str,
    seed_finished: bool,
    seed_ratio: f64,
    seed_time_min: i32,
    tx: &UnboundedSender<EngineMsg>,
) -> bool {
    let mut suggested = false;
    let mut finished = false;
    // First tick the engine reported done: seed-time limits count from here.
    let mut finished_at: Option<std::time::Instant> = None;
    // Per-piece haves for the block map. Built once: it only borrows the
    // session, and polls on the same 500ms tick as progress (no extra
    // wakeups; a failed poll just skips a frame).
    let api = Api::new(session.clone(), None);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stats = handle.stats();
        if !suggested {
            suggested = true;
            if let Some(name) = handle.name() {
                let short = shorten_filename(&name);
                if name != stub
                    && sane_filename(&short)
                    && tx.send(EngineMsg::SuggestName(short)).is_err()
                {
                    break;
                }
            }
        }
        let total = (stats.total_bytes > 0).then_some(stats.total_bytes);
        if tx
            .send(progress_msg(
                stats.progress_bytes,
                total,
                stats.uploaded_bytes,
                stats
                    .live
                    .as_ref()
                    .map(|l| l.upload_speed.as_bytes())
                    .unwrap_or(0),
            ))
            .is_err()
        {
            break;
        }
        if let Ok((have, _)) = api.api_dump_haves(TorrentIdOrHash::Hash(handle.info_hash())) {
            let have: Vec<bool> = have.iter().by_vals().collect();
            if tx.send(EngineMsg::TorrentPieces(have)).is_err() {
                break;
            }
        }
        if matches!(stats.state, TorrentStatsState::Error) {
            let _ = tx.send(EngineMsg::Failed(
                stats.error.unwrap_or_else(|| gettext("Torrent failed")),
            ));
            break;
        }
        if stats.finished {
            // No seeding requested: finish immediately. The session is
            // paused so the already-downloaded files stay on disk.
            if !seed_finished {
                let _ = session.pause(&handle).await;
                let _ = tx.send(EngineMsg::Finished {
                    size: stats.total_bytes,
                });
                finished = true;
                break;
            }
            // Seeding requested: stay on the loop (progress ticks keep the
            // row's upload counters live) until a configured rule fires.
            // With neither rule set this is "seed forever": the loop never
            // breaks here, so the row stays active with a working Stop
            // action instead of flipping to Done while the engine keeps
            // seeding in the background.
            let t0 = *finished_at.get_or_insert_with(std::time::Instant::now);
            if seed_limits_hit(
                seed_ratio,
                seed_time_min,
                t0,
                stats.total_bytes,
                stats.uploaded_bytes,
            ) {
                let _ = session.pause(&handle).await;
                let _ = session
                    .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
                    .await;
                let _ = tx.send(EngineMsg::Finished {
                    size: stats.total_bytes,
                });
                finished = true;
                break;
            }
        }
    }
    finished
}

/// Torrent engine: add (or resume) one magnet, then poll stats into the pump.
/// Reuses an already-managed handle on retry-after-pause; a second queue row
/// with the same magnet fails fast instead of sharing one handle.
pub(crate) struct TorrentJob {
    pub id: u64,
    pub source: TorrentSource,
    pub dest: PathBuf,
    pub seed_finished: bool,
    pub seed_ratio: f64,
    pub seed_time_min: i32,
    pub dht: bool,
    pub peer_limit: Option<usize>,
    pub download_bps: Option<u64>,
    pub upload_bps: Option<u64>,
    pub listen_port: i32,
    /// Ask the router to forward the listen port via UPnP. Currently
    /// always false: with no listen port configured there is nothing to
    /// forward (forced off under SOCKS5 by the net plan regardless).
    pub upnp: bool,
    /// Extra tracker URLs from preferences (per-add, so edits apply to new
    /// downloads without restarting the engine).
    pub trackers: Option<Vec<String>>,
    /// SOCKS5 proxy URL for the engine session (planned at spawn: DHT,
    /// listener and UDP trackers already forced off alongside).
    pub socks_proxy: Option<String>,
    /// Peer blocklist URL from preferences (creation-scoped like DHT:
    /// the engine reads it once when the session starts).
    pub blocklist_url: Option<String>,
    /// Local Service Discovery: find peers on the local network.
    pub lsd: bool,
    /// Pre-chosen file indices for multi-file torrents (intake dialog).
    pub only_files: Option<Vec<usize>>,
    /// Intake recorded a collision-proof subfolder as dest: use it
    /// directly instead of recomputing from metadata.
    pub dest_is_final: bool,
    pub tx: UnboundedSender<EngineMsg>,
}

/// Claim the registry slot for a spawn. Resume when this row already
/// owns it (retry after pause); reject when a *different* row holds the
/// same torrent. Mutation stays under one lock hold; on `Err` the guard
/// drops with the return and the caller fails the row.
fn claim_slot(active: &mut HashMap<u64, Active>, id: u64, hash_hex: &str) -> Result<bool, ()> {
    if let Some(a) = active.get_mut(&id) {
        if a.hash_hex != hash_hex {
            a.hash_hex = hash_hex.to_string();
            a.handle = None;
            a.paused = false;
            Ok(false)
        } else {
            a.paused = false;
            Ok(a.handle.is_some())
        }
    } else if active.values().any(|a| a.hash_hex == hash_hex) {
        Err(())
    } else {
        active.insert(
            id,
            Active {
                hash_hex: hash_hex.to_string(),
                handle: None,
                paused: false,
            },
        );
        Ok(false)
    }
}

pub(crate) async fn run_torrent(job: TorrentJob) {
    let TorrentJob {
        id,
        source,
        dest,
        seed_finished,
        seed_ratio,
        seed_time_min,
        dht,
        peer_limit,
        download_bps,
        upload_bps,
        listen_port,
        upnp,
        trackers,
        socks_proxy,
        blocklist_url,
        lsd,
        only_files,
        dest_is_final,
        tx,
    } = job;
    let fail = |msg: String| {
        let _ = tx.send(EngineMsg::Failed(msg));
    };
    // Resolve identity before touching the registry: magnets parse the link,
    // files parse their archived bytes. Both yield (hash_hex, stub, adder).
    enum Adder {
        Magnet(String),
        File(Vec<u8>),
    }
    let (hash_hex, hash_id, stub, folder, adder) = match &source {
        TorrentSource::Magnet(magnet) => match parse_magnet(magnet) {
            Ok(m) => {
                let Some(hash_id) = m.as_id20() else {
                    fail("Only BitTorrent v1 magnets are supported".to_string());
                    return;
                };
                let hash_hex = hash_id.as_string();
                let stub = stub_name(magnet).unwrap_or_else(|| hash_hex.clone());
                // `dest` already points at the row's subfolder (recorded at
                // enqueue), so magnets land grouped instead of flat.
                (
                    hash_hex,
                    hash_id,
                    stub,
                    dest.clone(),
                    Adder::Magnet(magnet.clone()),
                )
            }
            Err(e) => {
                fail(e);
                return;
            }
        },
        TorrentSource::File(path) => {
            let Some(bytes) = read_torrent_bytes(path) else {
                fail(gettext("Cannot read torrent file"));
                return;
            };
            match librqbit::torrent_from_bytes(&bytes) {
                Ok(meta) => {
                    let hash_id = meta.info_hash;
                    let hash_hex = hash_id.as_string();
                    let raw_name = raw_torrent_name(meta.info.data.name.as_ref());
                    let multi = is_multi_file(meta.info.data.files.as_deref());
                    let stub =
                        safe_torrent_name(raw_name.clone()).unwrap_or_else(|| hash_hex.clone());
                    // Collision-recorded subfolder from intake: already final.
                    let folder = if dest_is_final {
                        dest
                    } else {
                        output_folder_for(&dest, raw_name, multi, &stub)
                    };
                    (hash_hex, hash_id, stub, folder, Adder::File(bytes))
                }
                Err(e) => {
                    fail(format!("Invalid torrent file: {e}"));
                    return;
                }
            }
        }
    };
    // Resume when this row already owns a live handle (retry after pause);
    // fail fast when a *different* row holds the same magnet.
    let resumed = {
        let mut active = ACTIVE.lock().await;
        match claim_slot(&mut active, id, &hash_hex) {
            Ok(resumed) => resumed,
            Err(()) => {
                drop(active);
                fail(gettext("Torrent is already in the queue"));
                return;
            }
        }
    };
    let session = match ensure_session(SessionConfig {
        dht,
        peer_limit,
        download_bps,
        upload_bps,
        listen_port,
        upnp,
        socks_proxy,
        blocklist_url,
        lsd,
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            ACTIVE.lock().await.remove(&id);
            fail(e);
            return;
        }
    };
    if resumed {
        if let Some(h) = ACTIVE.lock().await.get(&id).and_then(|a| a.handle.clone()) {
            unpause_adopted(&session, id, &h).await;
            let finished = poll_loop(
                session.clone(),
                h,
                &stub,
                seed_finished,
                seed_ratio,
                seed_time_min,
                &tx,
            )
            .await;
            // Finished rows leave the session unless still seeding (see
            // `maybe_evict`).
            maybe_evict(
                &session,
                TorrentIdOrHash::Hash(hash_id),
                finished,
                seed_finished,
            )
            .await;
        } else {
            fail(gettext("Torrent is no longer managed"));
        }
        ACTIVE.lock().await.remove(&id);
        return;
    }
    // A just-forgotten session entry vanishes asynchronously (forget is
    // fire-and-forget): wait for our hash to clear instead of tripping the
    // duplicate guard on our own deletion. Skips at once when absent;
    // genuine duplicates still fail below after ~3s.
    for _ in 0..12 {
        if session.get(TorrentIdOrHash::Hash(hash_id)).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    // Cloned before opts takes ownership (see the filter-match arm below).
    let want_files = only_files.clone();
    let opts = AddTorrentOptions {
        output_folder: Some(folder.to_string_lossy().into_owned()),
        // The intake collision-dedup path guarantees a fresh folder, so
        // nothing existing can be clobbered; computed folders and magnets
        // keep overwrite:true (partially-written resume data must open).
        overwrite: !dest_is_final,
        // Current preference at add time: running torrents keep theirs.
        peer_limit,
        // Extra trackers from preferences (same live-at-add rule).
        trackers,
        // Multi-file selection chosen at intake (no live setter exists).
        only_files,
        ..Default::default()
    };
    let add = match adder {
        Adder::Magnet(magnet) => AddTorrent::from_url(magnet),
        Adder::File(bytes) => AddTorrent::from_bytes(bytes),
    };
    let handle = match session.add_torrent(add, Some(opts)).await {
        // The session outlives our registry: only adopt a stale handle
        // on a file-filter match, else fail fast (delete the owner first).
        Ok(AddTorrentResponse::AlreadyManaged(_, handle)) if handle.only_files() != want_files => {
            ACTIVE.lock().await.remove(&id);
            fail(gettext("Torrent is already in the queue"));
            return;
        }
        Ok(resp) => resp.into_handle(),
        Err(e) => {
            ACTIVE.lock().await.remove(&id);
            fail(format!("Cannot add torrent: {e}"));
            return;
        }
    };
    let Some(handle) = handle else {
        ACTIVE.lock().await.remove(&id);
        fail(gettext("Torrent produced no files"));
        return;
    };
    // Cancelled while adding: the pump is gone, so clean up the orphan.
    let keep = {
        let mut active = ACTIVE.lock().await;
        match active.get_mut(&id) {
            Some(a) => {
                a.handle = Some(handle.clone());
                true
            }
            None => false,
        }
    };
    // Cancelled while adding: the pump is gone, so drop the session
    // entry; partial files stay for resume like any other cancel.
    if !keep {
        let _ = session
            .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
            .await;
        return;
    }
    if ACTIVE.lock().await.get(&id).is_some_and(|a| a.paused) {
        let _ = session.pause(&handle).await;
    } else if handle.is_paused() {
        // Adopted a persisted handle: session restore re-added it paused,
        // but a spawn always means "run". Fresh adds start live, so this
        // only fires on the persistence path (e.g. resuming a row that was
        // paused when the app last closed).
        unpause_adopted(&session, id, &handle).await;
    }
    let finished = poll_loop(
        session.clone(),
        handle,
        &stub,
        seed_finished,
        seed_ratio,
        seed_time_min,
        &tx,
    )
    .await;
    // Finished rows leave the session unless still seeding (see
    // `maybe_evict`). Files stay on disk.
    maybe_evict(
        &session,
        TorrentIdOrHash::Hash(hash_id),
        finished,
        seed_finished,
    )
    .await;
    ACTIVE.lock().await.remove(&id);
    // Archives live as long as their rows: delete_download() drops them,
    // remove() keeps them for Undo, sweep_archives() cleans orphans.
    // (Deleting on finish would break trash for Done rows, which need the
    // archive to locate a multi-file subfolder.)
}

#[cfg(test)]
#[path = "torrent_tests.rs"]
mod tests;
