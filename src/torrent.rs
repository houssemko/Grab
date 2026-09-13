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

use gtk4::gio::prelude::SettingsExt;
use gtk4::{gio, glib};
use librqbit::{
    api::TorrentIdOrHash, AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent,
    Session, SessionOptions, TorrentStatsState,
};
use tokio::sync::{mpsc::UnboundedSender, Mutex, OnceCell};

use crate::download::{dedupe_filename, sane_filename, shorten_filename, tokio_rt, EngineMsg};

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
/// The total file count rides along so rows can show "N of M files".
/// Staged file selection: chosen indices + total file count.
type StagedSelection = (Vec<usize>, usize);

static PENDING_SELECTIONS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, StagedSelection>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Stage a file selection for the next spawn of `pseudo_url`.
pub fn stage_selection(pseudo_url: &str, only_files: Vec<usize>, total_files: usize) {
    PENDING_SELECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pseudo_url.to_string(), (only_files, total_files));
}

/// Read the staged selection for `pseudo_url`, if any. Peek, not take:
/// retries and re-spawns re-add from scratch and need the same filter.
/// Stale entries are pruned alongside archive sweeps (see
/// `prune_selections`); in-session the key outlives Undo because unremove
/// re-inserts the same pseudo-URL.
pub(crate) fn get_selection(pseudo_url: &str) -> Option<Vec<usize>> {
    PENDING_SELECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pseudo_url)
        .map(|(sel, _)| sel.clone())
}

/// Total file count staged alongside the selection, if any (for row text).
pub(crate) fn selection_total(pseudo_url: &str) -> Option<usize> {
    PENDING_SELECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(pseudo_url)
        .map(|(_, total)| *total)
}

/// Drop staged selections no live row references (companion to
/// `sweep_archives`, called with the same referenced set).
pub fn prune_selections(referenced: &std::collections::HashSet<String>) {
    PENDING_SELECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|k, _| referenced.contains(k));
}

/// True for either torrent source: magnet links and archived .torrent
/// pseudo-URLs. Lifecycle paths (pause/park/cancel/delete) are id-based
/// and source-agnostic, so they key off this.
pub fn is_torrent(s: &str) -> bool {
    is_magnet(s) || is_torrent_url(s)
}

pub fn is_magnet(s: &str) -> bool {
    // `get` (not slicing): non-ASCII input must never panic the classifier.
    // Match the bare `magnet:` prefix, not `magnet:?`: parse_magnet is the
    // real validator, and anything magnet:-shaped must never fall through
    // to the http scheme branch (tracker params contain `://`).
    s.trim_start()
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
}

/// Parse a magnet link, rejecting anything rqbit cannot resolve to BTv1.
pub fn parse_magnet(s: &str) -> Result<librqbit::Magnet, String> {
    let m = librqbit::Magnet::parse(s.trim()).map_err(|e| format!("Invalid magnet link: {e}"))?;
    if m.as_id20().is_none() {
        return Err("Only BitTorrent v1 magnets are supported".to_string());
    }
    Ok(m)
}

/// Display stub for a magnet row: the advertised name, else the info-hash hex.
pub fn stub_name(magnet: &str) -> Option<String> {
    let m = parse_magnet(magnet).ok()?;
    // A hostile display name must never reach the filesystem: fall back to
    // the info-hash hex when dn is missing or fails the filename gate.
    m.name
        .clone()
        .filter(|n| crate::download::sane_filename(n))
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
    s.trim_start()
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("torrent:"))
}

/// Archive dir for .torrent files: alongside the session state.
fn torrents_dir() -> PathBuf {
    glib::user_data_dir().join("grab").join("torrents")
}

/// Absolute archive path for a `torrent:{abs-path}` pseudo-URL.
pub fn archive_path_for_url(url: &str) -> Option<PathBuf> {
    let path = url.trim_start().get(8..)?;
    let path = PathBuf::from(path);
    // Existence is part of validity: intake rejects doodled pseudo-URLs
    // fast, and restore only ever sees swept-kept archives.
    (path.is_absolute() && path.exists()).then_some(path)
}

/// Copy `.torrent` bytes into the archive, named after the sanitized file
/// stem. Returns the pseudo-URL the queue row stores.
pub fn archive_torrent_file(file_name: &str, bytes: &[u8]) -> Result<String, String> {
    const MAX_TORRENT_BYTES: usize = 10_000_000;
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err("Torrent file is too large (max 10 MB)".to_string());
    }
    // Parse first: never archive bytes rqbit itself would reject.
    librqbit::torrent_from_bytes(bytes).map_err(|e| format!("Invalid torrent file: {e}"))?;
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| sane_filename(s))
        .unwrap_or("torrent");
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
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| sane_filename(s));
    match stem {
        Some(s) => crate::download::shorten_filename(s),
        None => "torrent".to_string(),
    }
}

/// One listed file of a multi-file torrent: display path + byte length.
pub struct TorrentFileEntry {
    pub path: String,
    pub length: u64,
}

/// Parse `.torrent` bytes into a display list for the file picker.
/// Byte-string path components are lossy-converted; lengths are exact.
pub fn torrent_file_list(bytes: &[u8]) -> Result<(String, Vec<TorrentFileEntry>), String> {
    let meta =
        librqbit::torrent_from_bytes(bytes).map_err(|e| format!("Invalid torrent file: {e}"))?;
    let name = meta
        .info
        .data
        .name
        .as_ref()
        .map(|n| String::from_utf8_lossy(n.as_ref()).into_owned())
        .filter(|n| sane_filename(n))
        .unwrap_or_else(|| "torrent".to_string());
    let mut entries = Vec::new();
    if let Some(files) = meta.info.data.files.as_ref() {
        for (i, f) in files.iter().enumerate() {
            let parts: Vec<String> = f
                .path
                .iter()
                .map(|c| String::from_utf8_lossy(c.as_ref()).into_owned())
                .collect();
            let path = parts.join("/");
            entries.push(TorrentFileEntry {
                path: if path.is_empty() {
                    format!("file {i}")
                } else {
                    path
                },
                length: f.length,
            });
        }
    }
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
    let dir = name
        .filter(|n| sane_filename(n))
        .map(|n| shorten_filename(&n))
        .unwrap_or_else(|| fallback.to_string());
    dest.join(dir)
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
    let path = archive_path_for_url(url)?;
    let bytes = std::fs::read(path).ok()?;
    let meta = librqbit::torrent_from_bytes(&bytes).ok()?;
    let multi = meta.info.data.files.as_ref().is_some_and(|f| f.len() >= 2);
    if !multi {
        return None;
    }
    let name = meta
        .info
        .data
        .name
        .as_ref()
        .map(|n| String::from_utf8_lossy(n.as_ref()).into_owned());
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
    let Some(bytes) = archive_path_for_url(url).and_then(|p| std::fs::read(p).ok()) else {
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
        if keep.contains(&i) {
            continue;
        }
        // Never let a hostile entry escape the folder (join would follow
        // `..` or an absolute path); the file simply stays behind.
        if entry
            .path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            continue;
        }
        let path = folder.join(&entry.path);
        let _ = std::fs::remove_file(&path);
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
}

async fn ensure_session(
    dht: bool,
    peer_limit: Option<usize>,
    download_bps: Option<u64>,
) -> Result<Arc<Session>, String> {
    SESSION
        .get_or_try_init(|| async {
            let dir = glib::user_data_dir().join("grab");
            std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create session dir: {e}"))?;
            let mut opts = SessionOptions::default();
            if !dht {
                opts.dht = None;
            }
            // The session struct carries no live setter for this: it applies
            // here and per add below, so new downloads pick up edits.
            opts.peer_limit = peer_limit;
            let session = Session::new_with_opts(dir, opts)
                .await
                .map_err(|e| format!("Cannot start torrent engine: {e}"))?;
            session.ratelimits.set_download_bps(bps(download_bps));
            Ok(session)
        })
        .await
        .map(Arc::clone)
}

/// Map the peer-limit preference to rqbit's shape: 0 means unlimited, so
/// the default limit applies when no limit is set.
pub(crate) fn peer_limit_of(settings: &gio::Settings) -> Option<usize> {
    match settings.int("torrent-peer-limit") {
        0 => None,
        n => Some(n.max(0) as usize),
    }
}

/// Live-apply the download cap (speed-limit watcher, any thread): the rate
/// limiter is internally synchronized. Peer limit has no live setter in
/// rqbit, so it applies at session creation and per add instead.
pub(crate) fn apply_live_limits(download_bps: Option<u64>) {
    if let Some(s) = SESSION.get() {
        s.ratelimits.set_download_bps(bps(download_bps));
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

/// Drop a torrent row from the session. `delete_files` removes partial
/// files (cancel); finished rows keep their files (delete path passes false).
pub(crate) fn forget_download(id: u64, delete_files: bool) {
    tokio_rt().spawn(async move {
        let active = ACTIVE.lock().await.remove(&id);
        if let (Some(a), Some(s)) = (active, SESSION.get()) {
            if let Some(h) = a.handle {
                let _ = s
                    .delete(TorrentIdOrHash::Hash(h.info_hash()), delete_files)
                    .await;
            }
        }
    });
}

/// Poll one managed torrent into the pump. Returns true when the download
/// finished (caller deletes the .torrent archive then; Failed/Cancelled
/// keep theirs for retry).
async fn poll_loop(
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    stub: &str,
    seed_finished: bool,
    tx: &UnboundedSender<EngineMsg>,
) -> bool {
    let mut suggested = false;
    let mut finished = false;
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
            .send(EngineMsg::Progress {
                downloaded: stats.progress_bytes,
                total,
            })
            .is_err()
        {
            break;
        }
        if matches!(stats.state, TorrentStatsState::Error) {
            let _ = tx.send(EngineMsg::Failed(
                stats.error.unwrap_or_else(|| "Torrent failed".to_string()),
            ));
            break;
        }
        if stats.finished {
            if !seed_finished {
                let _ = session.pause(&handle).await;
            }
            let _ = tx.send(EngineMsg::Finished {
                size: stats.total_bytes,
            });
            finished = true;
            break;
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
    pub dht: bool,
    pub peer_limit: Option<usize>,
    pub download_bps: Option<u64>,
    /// Pre-chosen file indices for multi-file torrents (intake dialog).
    pub only_files: Option<Vec<usize>>,
    pub tx: UnboundedSender<EngineMsg>,
}

pub(crate) async fn run_torrent(job: TorrentJob) {
    let TorrentJob {
        id,
        source,
        dest,
        seed_finished,
        dht,
        peer_limit,
        download_bps,
        only_files,
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
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    fail(format!("Cannot read torrent file: {e}"));
                    return;
                }
            };
            match librqbit::torrent_from_bytes(&bytes) {
                Ok(meta) => {
                    let hash_id = meta.info_hash;
                    let hash_hex = hash_id.as_string();
                    let raw_name = meta
                        .info
                        .data
                        .name
                        .as_ref()
                        .map(|n| String::from_utf8_lossy(n.as_ref()).into_owned());
                    let multi = meta.info.data.files.as_ref().is_some_and(|f| f.len() >= 2);
                    let stub = raw_name
                        .clone()
                        .filter(|n| sane_filename(n))
                        .map(|n| shorten_filename(&n))
                        .unwrap_or_else(|| hash_hex.clone());
                    let folder = output_folder_for(&dest, raw_name, multi, &stub);
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
        if let Some(a) = active.get_mut(&id) {
            if a.hash_hex != hash_hex {
                a.hash_hex = hash_hex.clone();
                a.handle = None;
                a.paused = false;
                false
            } else {
                a.paused = false;
                a.handle.is_some()
            }
        } else {
            if active.values().any(|a| a.hash_hex == hash_hex) {
                drop(active);
                fail("Torrent is already in the queue".to_string());
                return;
            }
            active.insert(
                id,
                Active {
                    hash_hex: hash_hex.clone(),
                    handle: None,
                    paused: false,
                },
            );
            false
        }
    };
    let session = match ensure_session(dht, peer_limit, download_bps).await {
        Ok(s) => s,
        Err(e) => {
            ACTIVE.lock().await.remove(&id);
            fail(e);
            return;
        }
    };
    if resumed {
        if let Some(h) = ACTIVE.lock().await.get(&id).and_then(|a| a.handle.clone()) {
            let _ = session.clone().unpause(&h).await;
            poll_loop(session, h, &stub, seed_finished, &tx).await;
        } else {
            fail("Torrent is no longer managed".to_string());
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
        overwrite: true,
        // Current preference at add time: running torrents keep theirs.
        peer_limit,
        // Multi-file selection chosen at intake (no live setter exists).
        only_files,
        ..Default::default()
    };
    let add = match adder {
        Adder::Magnet(magnet) => AddTorrent::from_url(magnet),
        Adder::File(bytes) => AddTorrent::from_bytes(bytes),
    };
    let handle = match session.add_torrent(add, Some(opts)).await {
        Ok(resp) => match resp {
            // The session outlives our registry: only adopt a stale handle
            // on a file-filter match, else fail fast (delete the owner first).
            AddTorrentResponse::AlreadyManaged(_, handle) if handle.only_files() != want_files => {
                ACTIVE.lock().await.remove(&id);
                fail("Torrent is already in the queue".to_string());
                return;
            }
            _ => resp.into_handle(),
        },
        Err(e) => {
            ACTIVE.lock().await.remove(&id);
            fail(format!("Cannot add torrent: {e}"));
            return;
        }
    };
    let Some(handle) = handle else {
        ACTIVE.lock().await.remove(&id);
        fail("Torrent produced no files".to_string());
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
    if !keep {
        let _ = session
            .delete(TorrentIdOrHash::Hash(handle.info_hash()), true)
            .await;
        return;
    }
    if ACTIVE.lock().await.get(&id).is_some_and(|a| a.paused) {
        let _ = session.pause(&handle).await;
    }
    poll_loop(session, handle, &stub, seed_finished, &tx).await;
    ACTIVE.lock().await.remove(&id);
    // Archives live as long as their rows: delete_download() drops them,
    // remove() keeps them for Undo, sweep_archives() cleans orphans.
    // (Deleting on finish would break trash for Done rows, which need the
    // archive to locate a multi-file subfolder.)
}

#[cfg(test)]
#[path = "torrent_tests.rs"]
mod tests;
