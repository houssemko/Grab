use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{
    dedupe_filename, filename_from_url, fmt_bytes, name_stem, piece_len, rename_noreplace,
    restrict_filename_ascii, sane_filename, shorten_filename,
};
use crate::runtime::tokio_rt;
use gettextrs::{gettext, ngettext};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

/// Facade: HTTP fetch engine lives in [`download_fetch`](crate::download_fetch)
/// now (no re-exports: the manager consumes it here, tests import it directly).
use crate::download_fetch::{
    FetchCtx, StartMode, apply_torrent_limits, run_download, truncate_to_prefix,
};
/// Facade: intake normalization lives in [`download_intake`](crate::download_intake)
/// now; the re-exports keep the in-tree `crate::download::X` paths working.
pub use crate::download_intake::normalize_url;
/// Facade: network options + proxy/client plumbing lives in
/// [`download_net`](crate::download_net) now; the re-exports keep the
/// in-tree `crate::download::X` paths working. (Names used only inside
/// this module or its tests stay imported below without re-export.)
use crate::download_net::http_client_for;
pub use crate::download_net::{
    DownloadOptions, PROXY_MODE_MANUAL, proxy_mode_index, proxy_mode_labels, proxy_mode_value,
    proxy_type_index, proxy_type_labels, proxy_type_value,
};
use crate::download_pieces::SegmentState;
/// Facade: segmented-piece math lives in [`download_pieces`](crate::download_pieces)
/// now (no re-exports: the engine consumes it here, tests import it directly).
use crate::download_pieces::{BLOCK_CELLS, MAX_SEGMENTED_TOTAL};
/// Facade: rate parsing/pacing + progress text lives in
/// [`download_rate`](crate::download_rate) now (no re-exports: the
/// engine consumes it here, tests import it directly).
use crate::download_rate::{fmt_eta, format_amounts, parse_rate, publish_rate_limit};
/// Facade: the row object lives in [`download_row`](crate::download_row) now;
/// this re-export keeps every `crate::download::X` path working.
pub use crate::download_row::DownloadItem;
pub use crate::download_store::DownloadStatus;
/// Facade: queue persistence model lives in [`download_store`](crate::download_store)
/// now; the status re-export keeps every `crate::download::X` path working.
/// (The stored structs stay imported below without re-export.)
use crate::download_store::{QUEUE_VERSION, StoredItem, StoredQueue};

pub struct DownloadManager {
    // Borrow discipline: RefCells are never held across `set_*` property
    // notifies or `changed()` — GTK notifies re-enter through updater
    // closures that take shared borrows, so any borrow_mut added inside
    // a notify-reachable path panics at runtime with no compile-time
    // guard. Keep borrows scoped to the smallest statement.
    store: gio::ListStore,
    settings: crate::settings::AppSettings,
    running: RefCell<HashMap<u64, tokio::task::JoinHandle<()>>>,
    next_id: Cell<u64>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
    batch: Cell<u32>,
    /// Server-advertised names waiting for their download to finish. The
    /// move happens at Finished so the engine never writes through a
    /// renamed path mid-transfer (stale size reads, split files).
    pending_names: RefCell<HashMap<u64, String>>,
    /// Cached count of queued rows; backs the per-row Queue button without
    /// scanning the store on every progress tick. Refreshed in changed(),
    /// which follows every status transition (progress-only updates change
    /// no statuses, so they need no recount).
    queued: Cell<usize>,
    /// Spawn generation per row, bumped on every engine start. A pump
    /// future whose generation is stale (defer or a quick pause-resume
    /// started a newer engine first) must not touch progress, status,
    /// notifications, or the new engine's handle.
    epoch: RefCell<HashMap<u64, u64>>,
    /// Resume bitmaps for segmented downloads (session-only, main thread).
    segment_state: RefCell<HashMap<u64, SegmentState>>,
    /// Torrent per-piece haves by row (session-only, main thread, never
    /// persisted: re-polled from the session on every spawn).
    torrent_pieces: RefCell<HashMap<u64, Vec<bool>>>,
    /// Server-advertised Last-Modified by row, applied at Finished when
    /// the keep-server-date setting is on. Best-effort only.
    server_mtime: RefCell<HashMap<u64, SystemTime>>,
    /// Video-page source by row (in-memory only, like the maps above):
    /// persisted on [`StoredItem`] and re-staged on restore.
    video_sources: RefCell<HashMap<u64, crate::media_types::VideoSource>>,
    /// Abort senders for running resolver workers, by row. Signalled (then
    /// dropped) from pause/park/cancel paths so the worker stops its
    /// extractor streams promptly; the pump tail also drops them.
    video_abort: RefCell<HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
    /// Rows currently capturing a live stream. Pause/cancel/park only
    /// signal these (no task abort, no status preset): the worker
    /// finalizes the partial and its message drives the row to Done.
    /// Set per attempt in `spawn_video`; dropped on terminal messages.
    live_rows: RefCell<std::collections::HashSet<u64>>,
    /// Set by shutdown(): stale engine futures must not re-persist or
    /// re-mark rows once the authoritative shutdown persist has run.
    draining: Cell<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemovedSnapshot {
    pub url: String,
    pub dest_dir: String,
    pub filename: String,
    pub status: DownloadStatus,
    pub progress: f64,
    pub detail: String,
    pub output_dir: String,
    pub segments: Option<SegmentState>,
    /// Staged video source, so Undo on a video row restores the Page
    /// marker instead of demoting it to a plain download.
    pub video_source: Option<crate::media_types::VideoSource>,
}

/// One validated queue entry awaiting the restore apply phase.
/// Defined at module level: `restore_queue` collects these first so a
/// mid-loop validation failure can never leave half-spawned engines.
struct PendingRestore {
    item: StoredItem,
    output_dir: Option<String>,
    segments: Option<SegmentState>,
}

/// Queue + engine owner: persists the queue, spawns downloads, notifies the UI.
impl DownloadManager {
    /// Create a manager over `store`; call [`DownloadManager::restore_queue`] once.
    pub fn new(store: gio::ListStore, settings: crate::settings::AppSettings) -> Rc<Self> {
        let this = Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
            batch: Cell::new(0),
            pending_names: RefCell::new(HashMap::new()),
            queued: Cell::new(0),
            epoch: RefCell::new(HashMap::new()),
            segment_state: RefCell::new(HashMap::new()),
            torrent_pieces: RefCell::new(HashMap::new()),
            server_mtime: RefCell::new(HashMap::new()),
            video_sources: RefCell::new(HashMap::new()),
            video_abort: RefCell::new(HashMap::new()),
            live_rows: RefCell::new(std::collections::HashSet::new()),
            draining: Cell::new(false),
        });
        // Live preferences: raising the download limit must wake queued
        // rows now (nothing else re-runs start_next until the next
        // insert/finish event); lowering it parks the newest running rows
        // back to queued. Speed edits republish the shared engine cap.
        // Weak ref: the settings object would otherwise keep the
        // manager alive forever.
        //
        // Owner-thread guard: engine futures are bound to the thread that
        // spawned them, so queue actions must only run where the manager
        // was created. Invariant: in production every settings write
        // originates on the main thread (preferences UI, dconf dispatch),
        // so this never skips there; foreign-thread writes only happen
        // through the test suite's shared memory backend, where reacting
        // would spawn engines on the wrong thread.
        let owner = std::thread::current().id();
        let weak = Rc::downgrade(&this);
        this.settings
            .connect_changed(Some(crate::settings::key::MAX_CONCURRENT), move |_, _| {
                if std::thread::current().id() != owner {
                    return;
                }
                if let Some(m) = weak.upgrade() {
                    m.start_next();
                    m.preempt_excess();
                    m.changed();
                }
            });
        let settings_weak = this.settings.downgrade();
        this.settings
            .connect_changed(Some(crate::settings::key::SPEED_LIMIT), move |_, _| {
                if let Some(s) = settings_weak.upgrade() {
                    let s = crate::settings::AppSettings::from(s);
                    publish_rate_limit(&s);
                    apply_torrent_limits(&s);
                }
            });
        let settings_weak = this.settings.downgrade();
        this.settings.connect_changed(
            Some(crate::settings::key::TORRENT_UPLOAD_LIMIT),
            move |_, _| {
                if let Some(s) = settings_weak.upgrade() {
                    let s = crate::settings::AppSettings::from(s);
                    apply_torrent_limits(&s);
                }
            },
        );
        this
    }

    /// UI refresh callback, invoked after every state change.
    pub fn set_on_change(&self, cb: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(cb));
    }

    /// Delay queue persists across bulk inserts (playlist picker,
    /// worker expansion): each `enqueue` otherwise rewrites + fsyncs the
    /// whole queue file, turning a 500-item expansion into 500 full
    /// rewrites. Nesting-safe counter:
    /// pairs of `begin_batch` / `end_batch` may overlap.
    pub fn begin_batch(&self) {
        self.batch.set(self.batch.get() + 1);
    }

    /// Persist once after a `begin_batch` block and refresh the UI.
    pub fn end_batch(self: &Rc<Self>) {
        self.batch.set(self.batch.get().saturating_sub(1));
        self.persist_queue();
        self.changed();
    }

    fn changed(&self) {
        self.queued.set(
            (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .filter(|it| it.status() == DownloadStatus::Queued)
                .count(),
        );
        if let Some(cb) = self.on_change.borrow().as_ref() {
            cb();
        }
    }

    fn alloc_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// The underlying download list.
    pub fn store(&self) -> &gio::ListStore {
        &self.store
    }

    /// Find an item by id.
    pub fn find(&self, id: u64) -> Option<DownloadItem> {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .find(|it| it.id() == id)
    }

    /// Preferences backing this queue (for dialog defaults).
    pub fn settings(&self) -> &crate::settings::AppSettings {
        &self.settings
    }

    /// Video-page source staged for a row, if any.
    pub fn video_source(&self, id: u64) -> Option<crate::media_types::VideoSource> {
        self.video_sources.borrow().get(&id).cloned()
    }

    /// Whether the row is currently capturing a live stream (stop-and-keep
    /// applies: pausing/cancelling finalizes instead of discarding).
    pub fn is_live_video(&self, id: u64) -> bool {
        self.live_rows.borrow().contains(&id)
    }

    /// Validate, dedupe and queue a download, starting it when a slot is free.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    /// Explicit absolute dest, else the effective download dir. Shared by
    /// enqueue and the torrent collision check so both agree on the folder.
    fn resolve_dir(&self, dest_dir: Option<&str>) -> String {
        // An explicit destination must be absolute: a relative dir would
        // resolve against the launcher CWD (and fail the sandbox). Restore
        // already rejects these; live input gets the same gate.
        dest_dir
            .filter(|s| !s.is_empty() && std::path::Path::new(s).is_absolute())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.effective_download_dir())
    }

    pub fn enqueue(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(url)?;
        let dir = self.resolve_dir(dest_dir);
        let name = filename
            .filter(|s| sane_filename(s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if crate::torrent::is_magnet(&url) {
                    // The real name arrives with metadata; the info-hash stub
                    // labels the row until SuggestName renames it.
                    crate::torrent::stub_name(&url).unwrap_or_else(|| filename_from_url(&url))
                } else {
                    filename_from_url(&url)
                }
            });
        let name = shorten_filename(&name);
        // Torrents record engine subfolders as output_dir, so the taken
        // check covers those too: a magnet stub must never equal a recorded
        // file-torrent folder (or vice versa).
        let name = dedupe_filename(&name, |n| {
            let p = std::path::Path::new(&dir).join(n);
            p.exists()
                // No part-namespace gate here: plain rows never reach
                // `clean_dest_parts` (remove() only sweeps video rows),
                // so their stems need no reservation.
                || (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .any(|it| {
                        (it.dest_dir() == dir && it.filename() == n)
                            || it.output_dir() == p.to_string_lossy()
                    })
        });
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        if crate::torrent::is_magnet(&url) {
            // Magnets carry no archive to recompute the output folder
            // from at delete time, so record their subfolder now: the
            // engine is spawned with this folder as its destination.
            // The deduped stub above is filesystem-safe by construction.
            item.set_output_dir(
                std::path::Path::new(&dir)
                    .join(&name)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        Ok(self.insert(item))
    }

    /// Intake for .torrent files: archive the bytes, then enqueue the
    /// pseudo-URL like any other download (stub from the file stem, real
    /// name arrives with metadata via SuggestName).
    ///
    /// # Errors
    /// Returns a display-ready message when the bytes are not a valid torrent.
    pub fn enqueue_torrent_file(
        self: &Rc<Self>,
        bytes: Vec<u8>,
        file_name: &str,
        dest_dir: Option<&str>,
        only_files: Option<Vec<usize>>,
    ) -> Result<DownloadItem, String> {
        let pseudo = crate::torrent::archive_torrent_file(file_name, &bytes)?;
        if let Some(sel) = only_files {
            crate::torrent::stage_selection(&pseudo, sel);
        }
        let stub = crate::torrent::stub_name_for_file(file_name);
        let item = self.enqueue(&pseudo, dest_dir, Some(&stub))?;
        // The engine writes with overwrite:true, so a pre-existing
        // dest/<torrent-name> would be clobbered: record a deduped
        // subfolder the engine then uses as-is (delete/reveal follow the
        // recorded dir, and restore re-attaches it).
        if let Some((base, _)) = crate::torrent::intake_plan(&bytes) {
            let dir = self.resolve_dir(dest_dir);
            if std::path::Path::new(&dir).join(&base).exists() {
                let name = dedupe_filename(&base, |n| {
                    let p = std::path::Path::new(&dir).join(n);
                    p.exists()
                        // Subfolder claims need no part-namespace gate (see
                        // plain `enqueue` above): only video-row finished
                        // names reserve stems.
                        || (0..self.store.n_items())
                            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                            .any(|it| it.output_dir() == p.to_string_lossy())
                });
                item.set_output_dir(
                    std::path::Path::new(&dir)
                        .join(&name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        Ok(item)
    }

    /// Enqueue a video page: dialog-routed (listed domains) or probe-
    /// proven (unlisted pages with extractable media). No domain gate
    /// here — the dialog owns routing, and misuse fails loudly at
    /// resolve instead of silently saving HTML.
    ///
    /// [`crate::media_types::VideoSource::Page`] staged before insert, so the
    /// persist inside [`DownloadManager::insert`] already carries it and
    /// [`DownloadManager::start_next`] parks the row for the resolver
    /// worker instead of feeding the page to the HTTP engine.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    pub fn enqueue_video(
        self: &Rc<Self>,
        page_url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        choices: crate::media_types::VideoChoices,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(page_url)?;
        let dir = self.resolve_dir(dest_dir);
        let name = filename
            .filter(|s| sane_filename(s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| filename_from_url(&url));
        let name = shorten_filename(&name);
        let name = if self.settings.restrict_filenames() {
            // Opt-in ASCII-only filenames. yt-dlp's --restrict-filenames only
            // sanitizes its own output-template fields, but Grab passes yt-dlp
            // literal output paths, so the flag would be a no-op here: fold
            // where Grab actually names the file, before dedupe reserves it.
            restrict_filename_ascii(&name)
        } else {
            name
        };
        // One readdir per intake: the reservation probe below must not
        // stat the download dir once per dedupe candidate.
        let existing = crate::video_staging::dir_file_names(std::path::Path::new(&dir));
        let name = dedupe_filename(&name, |n| {
            let p = std::path::Path::new(&dir).join(n);
            p.exists()
                || crate::video_staging::stem_reserved_in(&existing, name_stem(n))
                || (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .any(|it| {
                        (it.dest_dir() == dir && it.filename() == n)
                            || it.output_dir() == p.to_string_lossy()
                    })
        });
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        item.set_detail(if choices.audio_only {
            gettext("Waiting to resolve audio…")
        } else {
            gettext("Waiting to resolve media…")
        });
        self.video_sources.borrow_mut().insert(
            item.id(),
            crate::media_types::VideoSource::Page {
                page_url: url,
                media_url: None,
                expires_at: None,
                quality: choices.quality,
                audio_only: choices.audio_only,
                is_live: choices.is_live,
                video_format_id: choices.video_format_id,
                playlist_item_id: choices.playlist_item_id,
            },
        );
        Ok(self.insert(item))
    }

    /// Queue one row per playlist entry when a row resolves
    /// collection-shaped with no picked entry (dialog-less rows
    /// never see the picker). Stories address their segments directly;
    /// other entries keep their listed pages; unusable and
    /// self-referential entries skip, so highlights (whose items point
    /// back at the probed collection) expand to nothing. Returns
    /// (added, reported total). One persist for the whole import.
    /// Retrying a Done carrier re-expands (dedupe keeps it safe but
    /// duplicated); the video source stays so the row remains
    /// retryable. Choices
    /// inherit the carrier row (quality preset, audio mode).
    fn expand_playlist_rows(
        self: &Rc<Self>,
        id: u64,
        item: &DownloadItem,
        pl: &crate::media_types::PlaylistInfo,
    ) -> (usize, usize) {
        let (quality, audio_only) = match self.video_source(id) {
            Some(crate::media_types::VideoSource::Page {
                quality,
                audio_only,
                ..
            }) => (quality, audio_only),
            _ => return (0, pl.total),
        };
        let dest_dir = item.dest_dir().to_string();
        self.begin_batch();
        let mut added = 0;
        for entry in &pl.items {
            let Some(url) =
                crate::video_probe::expand_child_target(&item.url(), &pl.page_url, entry)
            else {
                continue;
            };
            // Live streams queued from a playlist take the VOD path;
            // each child re-resolves its own page anyway (same posture
            // as the picker). Names stay URL-derived: the post-fetch
            // rename titles them once metadata resolves.
            if self
                .enqueue_video(
                    &url,
                    Some(&dest_dir),
                    None,
                    crate::media_types::VideoChoices {
                        quality: quality.clone(),
                        audio_only,
                        video_format_id: None,
                        is_live: false,
                        playlist_item_id: None,
                    },
                )
                .is_ok()
            {
                added += 1;
            }
        }
        self.end_batch();
        // Reported total, not attempted: a truncated or leniently-parsed
        // list is honest about its tail ("500 of 600").
        (added, pl.total)
    }

    /// Re-queue one persisted entry, preserving its intent (paused/failed stay).
    /// A validated piece bitmap resumes segmented instead of restarting.
    /// Names restore verbatim (renaming would break resume identity and
    /// recorded folders): a foreign part-namespaced file landing while
    /// the app is closed is the accepted residual — live claims reserve
    /// stems, restored ones predate the reservation.
    /// The video-page source (if any) is staged *before* insert so
    /// [`DownloadManager::start_next`] parks the row instead of spawning
    /// the HTTP engine on the watch page. A source whose page URL doesn't
    /// match the row is dropped (hand-edited queue file), same trust
    /// posture as the torrent folder check.
    ///
    /// # Errors
    /// Returns a display-ready message when the stored entry is invalid.
    pub fn restore_existing(
        self: &Rc<Self>,
        url: &str,
        dest_dir: &str,
        filename: &str,
        status: DownloadStatus,
        segments: Option<SegmentState>,
        video_source: Option<crate::media_types::VideoSource>,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(url)?;
        if !sane_filename(filename) {
            return Err(format!("Invalid filename in queue: {filename}"));
        }
        if !std::path::Path::new(dest_dir).is_absolute() {
            return Err(format!("Invalid destination in queue: {dest_dir}"));
        }
        let filename = shorten_filename(filename);
        let item = DownloadItem::new(self.alloc_id(), &url, &filename, dest_dir);
        // Resumed rows requeue; only settled rows keep their status.
        item.set_status(match status {
            DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Done => status,
            _ => DownloadStatus::Queued,
        });
        if let Some(st) = segments {
            self.segment_state.borrow_mut().insert(item.id(), st);
        }
        if let Some(src) = video_source {
            let matches = matches!(&src, crate::media_types::VideoSource::Page { page_url, .. } if *page_url == url);
            if matches {
                let audio_only = matches!(
                    &src,
                    crate::media_types::VideoSource::Page {
                        audio_only: true,
                        ..
                    }
                );
                item.set_detail(if audio_only {
                    gettext("Waiting to resolve audio…")
                } else {
                    gettext("Waiting to resolve media…")
                });
                self.video_sources.borrow_mut().insert(item.id(), src);
            } else {
                tracing::warn!("dropping video source with mismatched page URL");
            }
        }
        Ok(self.insert(item))
    }

    fn insert(self: &Rc<Self>, item: DownloadItem) -> DownloadItem {
        self.store.append(&item);
        self.persist_queue();
        self.changed();
        self.start_next();
        item
    }

    fn insert_history(
        self: &Rc<Self>,
        url: String,
        dir: String,
        name: String,
        progress: f64,
        output_dir: Option<String>,
    ) {
        let Ok(url) = normalize_url(&url) else {
            tracing::warn!("skipping history entry with bad URL");
            return;
        };
        if !sane_filename(&name) {
            tracing::warn!("skipping invalid history entry for {name}");
            return;
        }
        if !std::path::Path::new(&dir).is_absolute() {
            tracing::warn!("skipping history entry with relative destination");
            return;
        }
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        item.set_progress(progress.clamp(0.0, 1.0));
        item.set_status(DownloadStatus::Done);
        // Already trust-checked by the caller (must sit inside `dir`).
        if let Some(folder) = output_dir {
            item.set_output_dir(folder);
        }
        let size = std::fs::metadata(item.file_path())
            .map(|m| m.len())
            .unwrap_or(0);
        item.set_detail(if size > 0 {
            gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
        } else {
            gettext("Finished")
        });
        // Restored duplicates collapse too: queue files written before
        // dedup may hold several Done rows per URL; the last one wins.
        self.drop_finished_duplicates(&url, item.id());
        self.insert(item);
    }

    /// Whether finish/fail desktop notifications are enabled.
    pub fn notifications_enabled(&self) -> bool {
        self.settings.show_notifications()
    }

    /// Whether closing over active downloads notifies.
    pub fn background_notifications_enabled(&self) -> bool {
        self.settings.notify_background()
    }

    /// Configured folder, or the system Downloads folder when empty or
    /// relative (a relative dir would resolve against the launcher CWD).
    pub fn effective_download_dir(&self) -> String {
        let configured = self.settings.download_dir();
        if !configured.is_empty() && std::path::Path::new(&configured).is_absolute() {
            return configured;
        }
        glib::user_special_dir(glib::UserDirectory::Downloads)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_string())
    }

    /// Simultaneous-download limit (at least 1).
    pub fn max_concurrent(&self) -> usize {
        self.settings.max_concurrent().max(1) as usize
    }

    fn start_next(self: &Rc<Self>) {
        while self.running.borrow().len() < self.max_concurrent() {
            let next = (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .find(|it| it.status() == DownloadStatus::Queued);
            match next {
                Some(item) => self.spawn(item),
                None => break,
            }
        }
    }

    fn spawn(self: &Rc<Self>, item: DownloadItem) {
        let opts = DownloadOptions::from_settings(&self.settings);
        // Re-stash per attempt: a stale Last-Modified from a previous run
        // must never apply when the new run's server sends none.
        self.server_mtime.borrow_mut().remove(&item.id());
        let dest = item.file_path();
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Fail fast on junk; the running engine follows the live value,
        // so later edits apply without re-queueing.
        let limit = opts.limit_rate.trim();
        if !limit.is_empty() && limit != "0" && parse_rate(limit).is_none() {
            item.set_status(DownloadStatus::Failed);
            item.set_detail(gettext("Invalid speed limit: {limit}").replace("{limit}", limit));
            self.changed();
            return;
        }
        publish_rate_limit(&self.settings);
        let url = item.url().to_string();
        if crate::torrent::is_torrent(&url) {
            return self.spawn_torrent(item, url);
        }
        if let Some(crate::media_types::VideoSource::Page {
            page_url,
            quality,
            audio_only,
            is_live,
            video_format_id,
            playlist_item_id,
            ..
        }) = self.video_source(item.id())
        {
            return self.spawn_video(SpawnVideoParams {
                item,
                page_url,
                quality,
                audio_only,
                video_format_id,
                is_live,
                playlist_item_id,
            });
        }
        let connections = (opts.connections.max(1) as usize).min(16);
        let timeout = Duration::from_secs(30);
        // A saved bitmap means this item wrote non-contiguous pieces: only a
        // segmented resume is correct (single-stream appends at EOF).
        let mode = match self.segment_state.borrow().get(&item.id()).cloned() {
            Some(st) => {
                // The bitmap is only valid if the file still holds at least
                // the completed prefix (and nothing beyond the total): the
                // user may have deleted, truncated, or replaced the partial
                // file while paused. A stale bitmap would skip pieces that
                // are no longer on disk, so drop it and start over instead.
                let len = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                if len < st.prefix_len() || len > st.total {
                    self.segment_state.borrow_mut().remove(&item.id());
                    let _ = std::fs::remove_file(&dest);
                    StartMode::Fresh
                } else {
                    let frac =
                        (st.completed_bytes() as f64 / st.total.max(1) as f64).clamp(0.0, 1.0);
                    item.set_progress(frac);
                    StartMode::Resume(st)
                }
            }
            None => match std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) {
                0 => StartMode::Fresh,
                _ => StartMode::Single,
            },
        };
        let generation = self.epoch.borrow().get(&item.id()).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(item.id(), generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Invalid manual proxy fails the row loudly here: silently
        // routing direct would leak around an explicit user demand.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(e);
                self.changed();
                return;
            }
        };
        let ctx = FetchCtx {
            client: http_client_for(proxy.as_ref()),
            url,
            dest,
            opts,
            cookies: None,
            timeout,
            tx,
        };
        let handle = tokio_rt().spawn(run_download(ctx, connections, mode));
        self.running.borrow_mut().insert(item.id(), handle);
        item.set_status(DownloadStatus::Downloading);
        // Every attempt starts indeterminate: retries and resumes may
        // carry a stale fraction, which would otherwise freeze as a
        // determinate bar through the whole connecting phase (the row
        // pulses while progress is zero; the engine publishes real
        // fractions on its first tick).
        item.set_progress(0.0);
        let host = url::Url::parse(&item.url())
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        item.set_detail(if host.is_empty() {
            gettext("Starting…")
        } else {
            gettext("Connecting to {host}…").replace("{host}", &host)
        });
        self.changed();

        let item_id = item.id();
        self.pump(item, item_id, generation, rx);
    }

    /// Whether a finished-name candidate is taken: a file on disk, a
    /// reserved part/sidecar stem, or a live row already holding it.
    /// Shared by the Finished claim loop and the DEST_EXISTS requeue so
    /// both agree on what "taken" means. `existing` is the intake-time
    /// snapshot (one readdir per call, not per candidate). Video engines
    /// never suggest today, but this is a generic finished-name claim:
    /// the reservation stays uniform across all of them.
    fn is_name_taken(&self, dir: &str, existing: &[String], n: &str) -> bool {
        std::path::Path::new(dir).join(n).exists()
            || crate::video_staging::stem_reserved_in(existing, name_stem(n))
            || (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .any(|it| it.dest_dir() == dir && it.filename() == n)
    }

    /// Drain one engine's message channel into its row. Shared by the HTTP
    /// and torrent engines: every arm below is engine-generic, so arms the
    /// other engine never sends simply never fire.
    fn pump(
        self: &Rc<Self>,
        item: DownloadItem,
        id: u64,
        generation: u64,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
    ) {
        let this = Rc::clone(self);
        // Speed baseline: deltas from here, not totals from zero. Resumes
        // seed `downloaded` with pre-existing bytes, which lifetime-average
        // math would otherwise report as fantasy GB/s on the first updates.
        let mut base: Option<(u64, Instant)> = None;
        // Set on Finished/Failed. If the channel closes first, the engine
        // task died without reporting (panic): fail the row instead of
        // stranding it as "Downloading" forever.
        let mut done = false;
        // The row URL never changes and its file filter is fixed at
        // intake: hoist both out of the per-tick path (no String alloc,
        // Vec clone or map lock on every progress message).
        let url = item.url().to_string();
        let sel_suffix = match crate::torrent::get_selection(&url) {
            Some(sel) => format!(" • {} files", sel.len()),
            None => String::new(),
        };
        glib::spawn_future_local(async move {
            while let Some(msg) = rx.recv().await {
                // Superseded by a newer spawn for this row: its progress
                // reports would drag the bar backwards, and its tail would
                // fail the row or steal the new engine's handle.
                if !this.is_current(id, generation) {
                    break;
                }
                match msg {
                    EngineMsg::Progress {
                        downloaded,
                        total,
                        uploaded,
                        upload_bps,
                    } => {
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let (d0, tb) = match base {
                            Some((d0, tb)) if downloaded >= d0 => (d0, tb),
                            _ => {
                                let b = (downloaded, Instant::now());
                                base = Some(b);
                                b
                            }
                        };
                        let bps = downloaded.saturating_sub(d0) as f64
                            / Instant::now().duration_since(tb).as_secs_f64().max(0.001);
                        let speed = format!("{}/s", fmt_bytes(bps as u64));
                        // Torrent upload counters (HTTP rows send zeros, so
                        // their labels stay exactly as before).
                        let up_suffix = if uploaded > 0 || upload_bps > 0 {
                            let ratio = total.filter(|&t| t > 0).map_or_else(String::new, |t| {
                                format!(" · ratio {:.1}", uploaded as f64 / t as f64)
                            });
                            gettext(" • ↑ {upspeed}/s · {up} up{ratio}")
                                .replace("{upspeed}", &fmt_bytes(upload_bps))
                                .replace("{up}", &fmt_bytes(uploaded))
                                .replace("{ratio}", &ratio)
                        } else {
                            String::new()
                        };
                        // Torrent rows with a file filter show the count
                        // (hoisted above: fixed for the row's lifetime).
                        match total {
                            Some(t) if t > 0 => {
                                let frac = (downloaded as f64 / t as f64).clamp(0.0, 1.0);
                                item.set_progress(frac);
                                let eta = if bps > 0.0 {
                                    fmt_eta((t.saturating_sub(downloaded) as f64 / bps) as u64)
                                } else {
                                    "—".to_string()
                                };
                                item.set_detail(
                                    gettext("{pct}% ({amounts}) • {speed} • About {eta} left{filter}{up}")
                                        .replace("{pct}", &((frac * 100.0) as u64).to_string())
                                        .replace("{amounts}", &format_amounts(downloaded, t))
                                        .replace("{speed}", &speed)
                                        .replace("{eta}", &eta)
                                        .replace("{filter}", &sel_suffix)
                                        .replace("{up}", &up_suffix),
                                );
                            }
                            _ => {
                                item.set_detail(
                                    gettext("{done} • {speed}{filter}{up}")
                                        .replace("{done}", &fmt_bytes(downloaded))
                                        .replace("{speed}", &speed)
                                        .replace("{filter}", &sel_suffix)
                                        .replace("{up}", &up_suffix),
                                );
                            }
                        }
                    }
                    EngineMsg::Finished { size } => {
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // A pause keeps its pending name: resumed
                            // single-stream attempts never re-suggest.
                            let pending = this.pending_names.borrow_mut().remove(&id);
                            if let Some(name) = pending {
                                let current = item.filename().to_string();
                                let dir = item.dest_dir().to_string();
                                let existing = crate::video_staging::dir_file_names(
                                    std::path::Path::new(&dir),
                                );
                                let taken = |n: &str| this.is_name_taken(&dir, &existing, n);
                                // Claim-then-move so a file appearing between the
                                // dedupe check and the rename is never clobbered:
                                // retry with a fresh deduped name instead.
                                let old_path = item.file_path();
                                let mut final_name = dedupe_filename(&name, taken);
                                let mut moved = !old_path.exists();
                                for _ in 0..8 {
                                    if moved
                                        || old_path == std::path::Path::new(&dir).join(&final_name)
                                    {
                                        break;
                                    }
                                    match rename_noreplace(
                                        &old_path,
                                        &std::path::Path::new(&dir).join(&final_name),
                                    ) {
                                        Ok(()) => {
                                            moved = true;
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                            final_name = dedupe_filename(&name, taken);
                                        }
                                        Err(_) => break,
                                    }
                                }
                                if moved && final_name != current {
                                    item.set_filename(final_name);
                                }
                            }
                            // Size off the final path: the engine measured the
                            // pre-rename one.
                            let size = std::fs::metadata(item.file_path())
                                .map(|m| m.len())
                                .unwrap_or(size);
                            // Server file date, when asked: best-effort (a
                            // read-only handle suffices; failure keeps the
                            // download-time mtime, never fails the row).
                            let mtime = this.server_mtime.borrow_mut().remove(&id);
                            if this.settings.keep_server_date()
                                && let Some(t) = mtime
                                && let Ok(f) = std::fs::File::open(item.file_path())
                            {
                                let _ = f.set_modified(t);
                            }
                            item.set_progress(1.0);
                            item.set_status(DownloadStatus::Done);
                            item.set_detail(if size > 0 {
                                gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
                            } else {
                                gettext("Finished")
                            });
                            // One finished record per URL (Parabolic parity):
                            // an older Done row for this URL leaves now, so
                            // re-downloads replace instead of stacking.
                            this.drop_finished_duplicates(&url, id);
                            this.segment_state.borrow_mut().remove(&id);
                            this.torrent_pieces.borrow_mut().remove(&id);
                            // Filtered torrents: drop the untoggled files
                            // (0-byte placeholders and shared-piece bytes)
                            // now that every selected byte is on disk.
                            // Skipped while seeding: serving still reads
                            // those spans, and deleting them would corrupt
                            // shared pieces for peers.
                            if crate::torrent::is_torrent(&url)
                                && !this.settings.torrent_seed_finished()
                            {
                                let folder = Self::torrent_folder(&item);
                                if folder.is_dir() {
                                    crate::torrent::cleanup_unselected(&folder, &url);
                                }
                                // The intake filter is spent: keeping it
                                // would pin the index list (and confuse a
                                // re-add) for a row that is done.
                                crate::torrent::drop_selection(&url);
                            }
                            this.notify_finished(&item, Ok(()));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::ExpandPlaylist(pl) => {
                        // Pause/cancel during resolve must not enqueue:
                        // the carrier stays put and nothing new appears.
                        // (Pause doesn't bump epoch, so no staleness
                        // guard saves us — check first.)
                        if item.status() == DownloadStatus::Cancelled
                            || item.status() == DownloadStatus::Paused
                        {
                            done = true;
                            break;
                        }
                        // Worker-side playlist expansion, on the main
                        // thread where queueing is legal: one row per
                        // usable entry, then retire the carrier Done. No
                        // history record and no notification: the new
                        // rows are the feedback. Nothing usable expands
                        // to today's collection error instead.
                        let (added, total) = this.expand_playlist_rows(id, &item, &pl);
                        if added == 0 {
                            let e = crate::video_probe::playlist_resolve_error(None);
                            if item.status() != DownloadStatus::Cancelled
                                && item.status() != DownloadStatus::Paused
                            {
                                item.set_status(DownloadStatus::Failed);
                                item.set_detail(e.to_string());
                                this.torrent_pieces.borrow_mut().remove(&id);
                                this.notify_finished(&item, Err(e.to_string()));
                            }
                        } else if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Done);
                            item.set_detail(
                                ngettext(
                                    "Expanded into {added} of {total} item",
                                    "Expanded into {added} of {total} items",
                                    added as u32,
                                )
                                .replace("{added}", &added.to_string())
                                .replace("{total}", &total.to_string()),
                            );
                            item.set_progress(1.0);
                            this.pending_names.borrow_mut().remove(&id);
                            this.server_mtime.borrow_mut().remove(&id);
                            // Like Finished: an older Done carrier for
                            // the URL leaves so re-expansions replace
                            // instead of stacking.
                            this.drop_finished_duplicates(&url, id);
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        // A failed download keeps its URL-derived name.
                        this.pending_names.borrow_mut().remove(&id);
                        this.server_mtime.borrow_mut().remove(&id);
                        if e == DEST_EXISTS
                            && item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // A foreign file appeared at our path after
                            // dedupe: pick a fresh free name and requeue
                            // instead of failing. Fresh short of absurd
                            // collision counts (dedupe caps at 9999), so
                            // this terminates.
                            let dir = item.dest_dir().to_string();
                            let current = item.filename().to_string();
                            let existing =
                                crate::video_staging::dir_file_names(std::path::Path::new(&dir));
                            let new_name = dedupe_filename(&current, |n| {
                                this.is_name_taken(&dir, &existing, n)
                            });
                            item.set_filename(new_name);
                            item.set_status(DownloadStatus::Queued);
                            this.persist_queue();
                            this.changed();
                            this.start_next();
                            done = true;
                            break;
                        }
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Failed);
                            item.set_detail(e.clone());
                            this.torrent_pieces.borrow_mut().remove(&id);
                            this.notify_finished(&item, Err(e));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::FailedVersion(e) => {
                        // The bytes on the server changed mid-download: any
                        // resume bitmap describes a dead file version, so
                        // drop it. The next attempt (or Retry) starts fresh
                        // and re-probes instead of failing forever.
                        this.segment_state.borrow_mut().remove(&id);
                        this.pending_names.borrow_mut().remove(&id);
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Failed);
                            item.set_detail(e.clone());
                            this.notify_finished(&item, Err(e));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::SegmentsInit { total } => {
                        this.segment_state
                            .borrow_mut()
                            .insert(id, SegmentState::new(total));
                    }
                    EngineMsg::PieceDone(idx) => {
                        if let Some(st) = this.segment_state.borrow_mut().get_mut(&id) {
                            st.mark(idx);
                        }
                    }
                    EngineMsg::TorrentPieces(have) => {
                        this.torrent_pieces.borrow_mut().insert(id, have);
                    }
                    EngineMsg::Phase(detail) => {
                        if item.status() == DownloadStatus::Downloading {
                            item.set_detail(detail);
                        }
                    }
                    EngineMsg::TruncatePrefix => {
                        if let Some(st) = this.segment_state.borrow_mut().get_mut(&id) {
                            truncate_to_prefix(&item.file_path(), st);
                            // The file just lost everything past the prefix;
                            // the bitmap must forget it too, or a later resume
                            // would skip pieces that are no longer on disk.
                            st.forget_beyond_prefix();
                        }
                    }
                    EngineMsg::FallbackSingle { ack } => {
                        // Server throttled parallel connections: shrink to the
                        // completed prefix and forget the bitmap, all here on
                        // the main thread so no spawn can observe a half-done
                        // transition. The engine waits for this ack before it
                        // appends single-stream at EOF.
                        if let Some(st) = this.segment_state.borrow().get(&id) {
                            truncate_to_prefix(&item.file_path(), st);
                        }
                        this.segment_state.borrow_mut().remove(&id);
                        ack.send(()).await.ok();
                    }
                    EngineMsg::SuggestName(name) => {
                        // Stash the server-advertised name for adoption at
                        // Finished. Moving mid-transfer would desync the
                        // engine (stale size reads, split files), so only
                        // fresh single-stream attempts suggest at all.
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let current = item.filename().to_string();
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        // Container truth from video engines: same stem,
                        // truer extension (a native webm merge under an
                        // mp4 intake name). The finished-name adoption
                        // below dedupes it; the Chromium rules underneath
                        // stay for server-advertised names. Gated on
                        // video rows so plain-engine behavior is
                        // bit-identical; the extension guard keeps a
                        // future sender from stripping names through
                        // this path.
                        let is_video_row = matches!(
                            this.video_source(id),
                            Some(crate::media_types::VideoSource::Page { .. })
                        );
                        if is_video_row
                            && name.contains('.')
                            && name_stem(&name) == name_stem(&current)
                            && name != current
                        {
                            // Same shortening the Chromium path applies:
                            // the stem is unchanged from an already-short
                            // intake, so this is symmetry, not truncation.
                            let name = shorten_filename(&name);
                            this.pending_names.borrow_mut().insert(id, name);
                            continue;
                        }
                        // Chromium parity: the server-advertised name wins
                        // over the URL-derived one. Adopt when the current
                        // name is a placeholder/extensionless, or when the
                        // server name has an extension and is shorter (long
                        // tracked/tokenized URL names lose to the real file).
                        // A generic extensionless server name never clobbers
                        // a good URL-derived name.
                        let placeholder = current == "index.html" || !current.contains('.');
                        if !placeholder && !(name.contains('.') && name.len() < current.len()) {
                            continue;
                        }
                        let name = shorten_filename(&name);
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        this.pending_names.borrow_mut().insert(id, name);
                    }
                    EngineMsg::LastModified(t) => {
                        // Latest attempt wins; applied at Finished when the
                        // keep-server-date setting is on.
                        this.server_mtime.borrow_mut().insert(id, t);
                    }
                    EngineMsg::LiveDetected => {
                        // Downloading only: a stop/pause during resolve
                        // must not leave a stale live_rows member behind
                        // (it would skip staging cleanup on remove and
                        // take live signal paths). Once per attempt,
                        // idempotent insert; paused rows re-detect on
                        // resume.
                        if item.status() == DownloadStatus::Downloading {
                            this.live_rows.borrow_mut().insert(id);
                        }
                    }
                }
            }
            // Superseded pump future: touch nothing, especially not the
            // new engine's handle in `running`.
            if !this.is_current(id, generation) {
                return;
            }
            this.running.borrow_mut().remove(&id);
            this.video_abort.borrow_mut().remove(&id);
            this.live_rows.borrow_mut().remove(&id);
            if this.draining.get() {
                return;
            }
            if !done && item.status() == DownloadStatus::Downloading {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(gettext("Download interrupted"));
                this.notify_finished(&item, Err(gettext("Download interrupted")));
            }
            this.persist_queue();
            this.changed();
            this.start_next();
        });
    }

    /// Spawn the torrent engine for a magnet row. Mirrors `spawn`'s contract
    /// (epoch bump, running slot, Downloading status, shared pump) so pause,
    /// cancel, retry, persist and the stale-pump guard keep working unchanged.
    fn spawn_torrent(self: &Rc<Self>, item: DownloadItem, magnet: String) {
        let dir = std::path::PathBuf::from(item.dest_dir().to_string());
        // Magnets recorded their subfolder at enqueue (no archive exists
        // to recompute it from later); archived torrents recompute theirs
        // from metadata, so plain dest stays correct for them.
        let recorded = item.output_dir().to_string();
        let dest = if recorded.is_empty() {
            dir.clone()
        } else {
            std::path::PathBuf::from(&recorded)
        };
        let _ = std::fs::create_dir_all(&dest);
        let settings = &self.settings;
        let seed_finished = settings.torrent_seed_finished();
        let peer_limit: Option<usize> = None; // rqbit default
        let download_bps = parse_rate(settings.speed_limit().trim());
        let upload_bps = parse_rate(settings.torrent_upload_limit().trim());
        let trackers: Option<Vec<String>> = None;
        // A malformed blocklist URL fails loudly like the manual proxy:
        // silently torrenting without the blocklist would betray the
        // user's intent.
        let blocklist_url =
            match crate::torrent::blocklist_url_of(&settings.torrent_blocklist_url()) {
                Ok(url) => url,
                Err(e) => {
                    item.set_status(DownloadStatus::Failed);
                    item.set_detail(e);
                    self.changed();
                    return;
                }
            };
        // Invalid manual proxy fails loudly like every other engine: no
        // silent direct torrent while the user asked for a tunnel.
        let proxy = match crate::download::DownloadOptions::from_settings(settings).proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(e);
                self.changed();
                return;
            }
        };
        // SOCKS5 takes over TCP peers + HTTP trackers (DHT, LSD, listener
        // and UDP trackers go dark alongside); anything else stays direct.
        // No listen port (rqbit default: no listener), so UPnP port
        // forwarding has nothing to forward: hardcoded off.
        let net = crate::torrent::plan_torrent_net(
            settings.torrent_dht(),
            settings.torrent_lsd(),
            0,
            false,
            trackers,
            proxy.as_ref(),
        );
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let is_file = crate::torrent::is_torrent_url(&magnet);
        let source = if is_file {
            match crate::torrent::archive_path_for_url(&magnet) {
                Some(path) => crate::torrent::TorrentSource::File(path),
                None => {
                    item.set_status(DownloadStatus::Failed);
                    item.set_detail(gettext("Torrent file is missing from the archive"));
                    self.changed();
                    return;
                }
            }
        } else {
            crate::torrent::TorrentSource::Magnet(magnet)
        };
        let only_files = crate::torrent::get_selection(&item.url().to_string());
        // Intake-recorded collision subfolders (file torrents only) are
        // already final; magnets use their recorded dir by construction.
        let dest_is_final = !recorded.is_empty() && is_file;
        let handle = tokio_rt().spawn(crate::torrent::run_torrent(crate::torrent::TorrentJob {
            id,
            source,
            dest,
            seed_finished,
            seed_ratio: settings.torrent_seed_ratio(),
            seed_time_min: settings.torrent_seed_time(),
            dht: net.dht,
            peer_limit,
            download_bps,
            upload_bps,
            listen_port: net.listen_port,
            upnp: net.upnp,
            trackers: net.trackers,
            socks_proxy: net.socks_proxy,
            blocklist_url,
            lsd: net.lsd,
            only_files,
            dest_is_final,
            tx,
        }));
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        // Attempts start indeterminate (see spawn): stale fractions must
        // not freeze as determinate bars through "Starting torrent…".
        item.set_progress(0.0);
        item.set_detail(gettext("Starting torrent…"));
        self.changed();
        self.pump(item, id, generation, rx);
    }
}

/// Video-page spawn inputs: everything `spawn_video` needs from the
/// row's [`crate::media_types::VideoSource::Page`]. Bundled into one struct so the growing
/// field list doesn't trip clippy's too-many-arguments lint at the
/// call boundary.
struct SpawnVideoParams {
    item: DownloadItem,
    page_url: String,
    quality: String,
    audio_only: bool,
    video_format_id: Option<String>,
    is_live: bool,
    playlist_item_id: Option<String>,
}

impl DownloadManager {
    /// Spawn the resolver worker for a video-page row. Mirrors `spawn`'s
    /// contract (epoch bump, running slot, Downloading status, shared pump)
    /// so pause, cancel, retry, persist and the stale-pump guard keep
    /// working unchanged. The worker speaks [`EngineMsg`] like every other
    /// engine; its abort sender lets pause/cancel stop the extractor
    /// streams promptly instead of only dropping the Grab-side task.
    fn spawn_video(self: &Rc<Self>, params: SpawnVideoParams) {
        let SpawnVideoParams {
            item,
            page_url,
            quality,
            audio_only,
            video_format_id,
            is_live,
            playlist_item_id,
        } = params;
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
        // Overwrite any stale sender: its task is dead or guarded stale.
        self.video_abort.borrow_mut().insert(id, abort_tx);
        // Live rows finalize in the worker on stop: track them so
        // pause/cancel/park signal without aborting or presetting.
        if is_live {
            self.live_rows.borrow_mut().insert(id);
        } else {
            self.live_rows.borrow_mut().remove(&id);
        }
        let opts = DownloadOptions::from_settings(&self.settings);
        // Invalid manual proxy fails the row loudly here, before any
        // network happens: silently routing direct would leak around an
        // explicit user demand.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(e);
                self.changed();
                return;
            }
        };
        let job = crate::video::VideoJob {
            item_id: id,
            page_url,
            playlist_item_id,
            quality,
            audio_only,
            dest: item.file_path(),
            // Shared throttle: parsed once here; empty/0/invalid means
            // unlimited (the preferences row flags junk live).
            speed_limit: parse_rate(opts.limit_rate.as_str()),
            keep_server_date: self.settings.keep_server_date(),
            // Extraction quality for audio-only rows; the schema range is
            // 0..=10 and 5 is yt-dlp's own default, clamped at spawn.
            audio_quality: self.settings.audio_quality().clamp(0, 10),
            video_format_id,
            is_live,
            live_from_start: self.settings.live_from_start(),
            newest_codecs: self.settings.video_codec_newest(),
            cookies_browser: self.settings.cookies_browser(),
            // Audio-only rows never subtitle (no video leg exists), so
            // resolve to `None` here rather than gating at every use.
            subtitles: if audio_only {
                None
            } else {
                crate::video_prefs::subtitle_lang_active(&self.settings.subtitle_language())
            },
            embed_subs: self.settings.embed_subs(),
            sponsorblock_remove: self.settings.sponsorblock_remove(),
            sponsorblock_mark: self.settings.sponsorblock_mark(),
            // Audio-only rows never remux (no video leg exists), so
            // resolve to `None` here rather than gating at every use.
            // Note the enqueue→spawn gap: the row name took the remux
            // pref at dialog time, this flag at spawn time — a pref
            // change on a queued row can disagree on the extension.
            remux_video: if audio_only {
                None
            } else {
                crate::video_prefs::remux_video_active(&self.settings.remux_video())
            },
            embed_chapters: self.settings.embed_chapters(),
            proxy,
        };
        let handle = tokio_rt().spawn(async move {
            let worker_tx = tx.clone();
            match crate::video::run_video_download(job, abort_rx, worker_tx).await {
                Ok(crate::video::VideoOutcome::Finished(size)) => {
                    tx.send(EngineMsg::Finished { size }).ok();
                }
                // Aborted: the pauser/canceller already set the row status,
                // so send nothing and let the pump tail no-op.
                Ok(crate::video::VideoOutcome::Aborted) => {}
                Ok(crate::video::VideoOutcome::Expand(pl)) => {
                    // Handed to the pump: queueing rows needs the
                    // manager, which the worker task must never touch
                    // (Rc is main-thread only).
                    tx.send(EngineMsg::ExpandPlaylist(pl)).ok();
                }
                Err(e) => {
                    tracing::warn!(item_id = id, error = %e.to_string(), "video attempt failed");
                    tx.send(EngineMsg::Failed(e.to_string())).ok();
                }
            }
        });
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        // Attempts start indeterminate (see spawn): a retried row may
        // carry its old fraction, which would otherwise sit frozen
        // through the whole "Resolving media…" phase.
        item.set_progress(0.0);
        item.set_detail(if audio_only {
            gettext("Resolving audio…")
        } else {
            gettext("Resolving media…")
        });
        self.changed();
        self.pump(item, id, generation, rx);
    }

    fn notify_finished(&self, item: &DownloadItem, result: Result<(), String>) {
        if !self.notifications_enabled() {
            return;
        }
        if let Some(app) = gio::Application::default() {
            let ok = result.is_ok();
            let n = gio::Notification::new(&if ok {
                gettext("Download finished")
            } else {
                gettext("Download failed")
            });
            let mut body = if ok {
                item.filename().to_string()
            } else {
                format!(
                    "{} → {}",
                    item.filename(),
                    item.file_path().to_string_lossy()
                )
            };
            if let Err(h) = result {
                body.push_str(&format!("\n{h}"));
            }
            n.set_body(Some(&body));
            n.set_default_action_and_target_value("app.present", None);
            app.send_notification(Some(&format!("dl-{}", item.id())), &n);
        }
    }

    /// Pause a running download, freeing its slot for the next queued item.
    pub fn pause(self: &Rc<Self>, id: u64) {
        if self.live_rows.borrow().contains(&id) {
            // Live captures finalize in the worker: signal it and let
            // its Finished flip the row to Done. Aborting the task or
            // presetting Paused would drop the recording or strand it.
            // The pump tail frees the slot on completion.
            self.stop_video_worker(id);
            return;
        }
        self.stop_video_worker(id);
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
            if item.status() == DownloadStatus::Downloading {
                item.set_status(DownloadStatus::Paused);
                item.set_detail(
                    gettext("Paused • {pct}%")
                        .replace("{pct}", &((item.progress() * 100.0) as u64).to_string()),
                );
            }
        }
        // Persist the bitmap too: a kill while paused must resume segmented.
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Re-queue a paused download.
    pub fn resume(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id)
            && item.status() == DownloadStatus::Paused
        {
            item.set_status(DownloadStatus::Queued);
            self.persist_queue();
            self.changed();
            self.start_next();
        }
    }

    /// Rename a download: the list label plus the file on disk when it is
    /// already fetched. Only completed rows (a real file to move) and
    /// queued rows (nothing on disk yet) qualify: mid-transfer the engine
    /// owns the path, and torrent rows map to session outputs, not files.
    ///
    /// # Errors
    /// Returns a display-ready message when the row cannot be renamed.
    pub fn rename_download(self: &Rc<Self>, id: u64, new_name: &str) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        if crate::torrent::is_torrent(&item.url()) {
            return Err(gettext("Renaming torrent downloads isn't supported"));
        }
        match item.status() {
            DownloadStatus::Done | DownloadStatus::Queued => {}
            DownloadStatus::Downloading => {
                return Err(gettext("Pause or wait for the download to finish first"));
            }
            _ => {
                return Err(gettext(
                    "Only completed or not-yet-started downloads can be renamed",
                ));
            }
        }
        let name = new_name.trim();
        if name == item.filename() {
            return Ok(());
        }
        if name.is_empty() || !sane_filename(name) {
            return Err(gettext("That isn't a valid file name"));
        }
        if item.status() == DownloadStatus::Done {
            let new_path = std::path::PathBuf::from(item.dest_dir().to_string()).join(name);
            match rename_noreplace(&item.file_path(), &new_path) {
                Ok(()) => {}
                // Deleted behind our back: the label update below still applies.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!("Could not rename {}: {e}", item.filename()));
                }
            }
        }
        item.set_filename(name);
        self.persist_queue();
        self.changed();
        Ok(())
    }

    /// Send a downloading/paused row to the back of the queue, keeping its
    /// progress and partial file. The freed slot goes to the longest-waiting
    /// queued row.
    pub fn defer(self: &Rc<Self>, id: u64) {
        let Some(item) = self.find(id) else {
            return;
        };
        if !matches!(
            item.status(),
            DownloadStatus::Downloading | DownloadStatus::Paused
        ) {
            return;
        }
        self.park(id);
        self.move_to_back(id);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Signal a running resolver worker to stop its extractor streams. The
    /// Grab task abort follows (or already ran): whichever wins, the
    /// attempt is over and a later resume replays from the sidecar.
    fn stop_video_worker(&self, id: u64) {
        if let Some(stop) = self.video_abort.borrow_mut().remove(&id) {
            let _ = stop.send(());
        }
    }

    /// Stop the engine for `id`, keeping file, bitmap and progress, and
    /// mark it queued. Unlike pause the row yields its slot; unlike cancel
    /// nothing is deleted and progress is kept. Live rows only signal:
    /// the worker finalizes and its message completes the row.
    fn park(&self, id: u64) {
        if self.live_rows.borrow().contains(&id) {
            self.stop_video_worker(id);
            return;
        }
        self.stop_video_worker(id);
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
            if matches!(
                item.status(),
                DownloadStatus::Downloading | DownloadStatus::Paused
            ) {
                item.set_status(DownloadStatus::Queued);
                item.set_detail(item.status().label());
            }
        }
    }

    fn move_to_back(&self, id: u64) {
        let pos = (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<DownloadItem>()
                .map(|it| it.id() == id)
                .unwrap_or(false)
        });
        if let Some(pos) = pos
            && let Some(obj) = self.store.item(pos)
        {
            self.store.remove(pos);
            self.store.append(&obj);
        }
    }

    /// Park running rows past the shrunk limit, lowest ids (earliest
    /// enqueued) keep their slots. Id order approximates start order;
    /// parked rows keep progress and resume later either way.
    fn preempt_excess(&self) {
        let max = self.max_concurrent();
        let mut ids: Vec<u64> = self.running.borrow().keys().cloned().collect();
        if ids.len() <= max {
            return;
        }
        ids.sort_unstable();
        for id in ids.into_iter().skip(max) {
            self.park(id);
        }
        self.persist_queue();
        self.changed();
    }

    /// Cancel a download; retry with [`DownloadManager::retry`].
    pub fn cancel(self: &Rc<Self>, id: u64) {
        self.cancel_inner(id, false);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    fn cancel_inner(&self, id: u64, keep_partial: bool) {
        // Live rows keep what's recorded (Stop, not Cancel): signal the
        // worker and skip the task abort plus the Cancelled preset, so
        // its Finished still lands. Discard via explicit row removal.
        let live = self.live_rows.borrow().contains(&id);
        self.stop_video_worker(id);
        if !live {
            if let Some(handle) = self.running.borrow().get(&id) {
                handle.abort();
            }
            self.running.borrow_mut().remove(&id);
        }
        self.pending_names.borrow_mut().remove(&id);
        let had_segments = self.segment_state.borrow_mut().remove(&id).is_some();
        self.torrent_pieces.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            // Segmented partials have holes: without the bitmap they can
            // never be appended to, so cancel drops the file — unless the
            // row may come back via Undo, which restores the bitmap.
            if had_segments && !keep_partial {
                let _ = std::fs::remove_file(item.file_path());
            }
            // Unfinished torrent rows drop their session entry but keep
            // partial files, so cancel/retry and remove/Undo resume instead
            // of restarting (explicit delete discards the files instead).
            if crate::torrent::is_torrent(&item.url()) && item.status() != DownloadStatus::Done {
                crate::torrent::forget_download(id);
            }
            if !live {
                item.set_status(DownloadStatus::Cancelled);
                item.set_detail(item.status().label());
            }
        }
    }

    /// Re-queue a failed or cancelled download.
    pub fn retry(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id) {
            match item.status() {
                DownloadStatus::Failed | DownloadStatus::Cancelled => {
                    // A kept segment bitmap resumes where it left off, so
                    // leave the progress bar there instead of flashing 0%.
                    if !self.segment_state.borrow().contains_key(&id) {
                        item.set_progress(0.0);
                    }
                    item.set_detail(String::new());
                    item.set_status(DownloadStatus::Queued);
                    self.persist_queue();
                    self.changed();
                    self.start_next();
                }
                _ => {}
            }
        }
    }

    /// Cancel and drop a row; restore with [`DownloadManager::unremove`].
    /// The partial file is kept so Undo can resume segmented rows instead
    /// of restarting them (plain cancel still discards it).
    pub fn remove(self: &Rc<Self>, id: u64) {
        self.cancel_inner(id, true);
        self.epoch.borrow_mut().remove(&id);
        // Video rows keep split parts beside the finished file
        // (`<stem>.<kind>.<ext>`, yt-dlp defaults) plus the staging
        // sidecar: drop both with the row. Pause/cancel keep them for
        // resume; remove and delete never resume (an Undo'd row
        // re-extracts fresh URLs and restarts), so orphaned parts would
        // otherwise sit in Downloads until deleted by hand. Live rows are
        // exempt: their worker was only signaled, not aborted, and its
        // finalize path cleans up after itself.
        if !self.live_rows.borrow().contains(&id) && self.video_sources.borrow().contains_key(&id) {
            crate::video::clean_staging(&crate::video::staging_dir(id));
            if let Some(item) = self.find(id) {
                crate::video::clean_dest_parts(&item.file_path());
            }
        }
        // The snapshot carries the source for Undo; the live map drops it
        // with the row (cancel keeps it, remove doesn't).
        self.video_sources.borrow_mut().remove(&id);
        if let Some(pos) = (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<DownloadItem>()
                .map(|it| it.id() == id)
                .unwrap_or(false)
        }) {
            self.store.remove(pos);
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Segment bitmap snapshot for Undo: clone before [`DownloadManager::remove`]
    /// drops the row's live bitmap so [`DownloadManager::unremove`] can resume it.
    pub fn segments_of(&self, id: u64) -> Option<SegmentState> {
        self.segment_state.borrow().get(&id).cloned()
    }

    /// Re-insert a previously removed download (Undo). Restores the prior
    /// status except `Downloading`, which restarts as `Queued`. A restored
    /// segment bitmap resumes instead of restarting; a stale one (partial
    /// file gone) is dropped by the spawn-time file checks.
    pub fn unremove(self: &Rc<Self>, snap: RemovedSnapshot) -> DownloadItem {
        let item = DownloadItem::new(self.alloc_id(), &snap.url, &snap.filename, &snap.dest_dir);
        item.set_progress(snap.progress.clamp(0.0, 1.0));
        item.set_detail(snap.detail);
        item.set_status(match snap.status {
            DownloadStatus::Downloading => DownloadStatus::Queued,
            s => s,
        });
        item.set_output_dir(snap.output_dir);
        if let Some(st) = snap.segments {
            self.segment_state.borrow_mut().insert(item.id(), st);
        }
        // Re-stage before insert: the persist inside `insert` already
        // carries it and `start_next` dispatches on it.
        if let Some(src) = snap.video_source {
            self.video_sources.borrow_mut().insert(item.id(), src);
        }
        self.insert(item.clone());
        item
    }

    /// Engine's real output folder for a torrent row: recorded at enqueue
    /// (magnets), else recomputed from the archived .torrent, else the
    /// row's own path as a last resort.
    fn torrent_folder(item: &DownloadItem) -> std::path::PathBuf {
        let recorded = item.output_dir().to_string();
        if !recorded.is_empty() {
            return std::path::PathBuf::from(recorded);
        }
        let dest = std::path::PathBuf::from(item.dest_dir().to_string());
        crate::torrent::torrent_output_dir(&dest, &item.url()).unwrap_or_else(|| item.file_path())
    }

    /// Per-piece completion for the block map, native resolution:
    /// segmented HTTP bitmap, torrent session haves, or a prefix fill
    /// from byte progress for plain single-stream rows. Empty when the
    /// row is unknown or nothing is known yet.
    pub fn piece_bitmap(&self, id: u64) -> Vec<bool> {
        if let Some(st) = self.segment_state.borrow().get(&id) {
            return st.done.clone();
        }
        if let Some(have) = self.torrent_pieces.borrow().get(&id) {
            return have.clone();
        }
        let Some(item) = self.find(id) else {
            return Vec::new();
        };
        if crate::torrent::is_torrent(&item.url()) || item.progress() <= 0.0 {
            return Vec::new();
        }
        let filled = (item.progress().clamp(0.0, 1.0) * BLOCK_CELLS as f64) as usize;
        (0..BLOCK_CELLS).map(|i| i < filled).collect()
    }

    /// Move the downloaded file to Trash, then remove the row.
    ///
    /// # Errors
    /// Returns a display-ready message when trashing fails.
    pub fn delete_download(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        // Torrent rows (any status): drop the session entry and the
        // archive, drop the row, then Trash the real files. Multi-file
        // torrents live in dest/<torrent-name>/ rather than the stub path
        // (never written), so trash the folder recorded at enqueue
        // (magnets) or recomputed from the archived .torrent; a missing
        // path is fine when metadata never resolved. Matches the HTTP
        // delete contract (Trash, recoverable).
        if crate::torrent::is_torrent(&item.url()) {
            let path = Self::torrent_folder(&item);
            crate::torrent::forget_download(id);
            crate::torrent::delete_archive_for_url(&item.url());
            self.remove(id);
            return match gio::File::for_path(path).trash(gio::Cancellable::NONE) {
                Ok(()) => Ok(()),
                Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {
                    Ok(())
                }
                Err(e) => Err(format!("Could not move {} to Trash: {e}", item.filename())),
            };
        }
        match gio::File::for_path(item.file_path()).trash(gio::Cancellable::NONE) {
            Ok(()) => {}
            Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
            Err(e) => return Err(format!("Could not move {} to Trash: {e}", item.filename())),
        }
        // Collected subtitle sidecars travel with the video: trash every
        // `<stem>.<lang>.srt` this row could own. The language pref is
        // global rather than per-row, so every offered code is a
        // candidate; failures only warn (the row delete must not fail
        // over a sidecar). Plain rows never wrote sidecars — gate on the
        // staged video source like remove()'s part cleanup does.
        if self.video_sources.borrow().contains_key(&id) {
            for lang in crate::video_prefs::subtitle_content_languages() {
                let sidecar = crate::video_staging::sidecar_path_for(&item.file_path(), lang);
                match gio::File::for_path(&sidecar).trash(gio::Cancellable::NONE) {
                    Ok(()) => {}
                    Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
                    Err(e) => tracing::warn!(
                        sidecar = %sidecar.display(),
                        error = %e,
                        "subtitle sidecar left behind"
                    ),
                }
            }
        }
        self.remove(id);
        Ok(())
    }

    /// Cancel every queued, downloading or paused item.
    pub fn cancel_all(self: &Rc<Self>) {
        // One persist/sync at the end, and no per-row start_next: cancel()
        // would briefly spawn the next queued row only to cancel it right
        // after, leaving stray engine tasks and UI futures behind.
        self.for_matching(
            |s| {
                matches!(
                    s,
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            },
            |m, id| m.cancel_inner(id, false),
        );
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Retry every failed/cancelled row. Returns the retried count for
    /// the caller's toast.
    pub fn retry_failed(self: &Rc<Self>) -> usize {
        let mut n = 0;
        self.for_matching(
            |s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled),
            |m, id| {
                m.retry(id);
                n += 1;
            },
        );
        n
    }

    /// Drop every finished row (files stay on disk). Still-seeding
    /// torrents leave the session first: clearing the record must not
    /// leave invisible uploading running. Returns the cleared count.
    pub fn clear_finished(self: &Rc<Self>) -> usize {
        let before = self.store.n_items();
        self.for_matching(
            |s| matches!(s, DownloadStatus::Done),
            |m, id| m.drop_finished_row(id),
        );
        let n = (before - self.store.n_items()) as usize;
        if n > 0 {
            // Same hygiene as a restart: archives and staged selections
            // no remaining row references go now (cleared rows have no
            // Undo path holding them, unlike `remove`).
            let referenced: std::collections::HashSet<String> = (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent_url(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            crate::torrent::prune_selections(&referenced);
            self.persist_queue();
            self.changed();
        }
        n
    }

    /// Finished rows (for the clear-finished confirm and toast counts).
    pub fn finished_count(&self) -> usize {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| it.status() == DownloadStatus::Done)
            .count()
    }

    /// Snapshots of every finished row, in list order, for Clear
    /// Finished's Undo. Done rows hold no engines, bitmaps or partials,
    /// so the snapshot is just the record [`DownloadManager::unremove`]
    /// needs to re-insert it.
    pub fn finished_snapshots(&self) -> Vec<RemovedSnapshot> {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| it.status() == DownloadStatus::Done)
            .map(|it| RemovedSnapshot {
                url: it.url().to_string(),
                dest_dir: it.dest_dir().to_string(),
                filename: it.filename().to_string(),
                status: it.status(),
                progress: it.progress(),
                detail: it.detail().to_string(),
                output_dir: it.output_dir().to_string(),
                segments: self.segments_of(it.id()),
                video_source: self.video_source(it.id()),
            })
            .collect()
    }

    /// Drop finished rows for `url` other than `keep_id`: one finished
    /// record per URL (Parabolic parity). Only Done rows — active,
    /// paused, failed and cancelled rows are user intent and never
    /// touched. Unnormalizable URLs skip quietly (rows are validated
    /// at intake, so this is unreachable in practice). Store-only;
    /// callers persist.
    pub(crate) fn drop_finished_duplicates(&self, url: &str, keep_id: u64) {
        let Ok(key) = normalize_url(url) else {
            return;
        };
        let ids: Vec<u64> = (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| {
                it.status() == DownloadStatus::Done
                    && it.id() != keep_id
                    && normalize_url(&it.url()).is_ok_and(|u| u == key)
            })
            .map(|it| it.id())
            .collect();
        for id in ids {
            self.drop_finished_row(id);
        }
    }

    /// Drop one finished row: a lingering torrent session leaves first
    /// (see `clear_finished`), staged sources and epochs release, and
    /// the row leaves the store. Files stay on disk. Done rows hold no
    /// engines, bitmaps or partials, so unlike `remove` there is nothing
    /// to cancel, snapshot or clean.
    fn drop_finished_row(&self, id: u64) {
        if let Some(item) = self.find(id)
            && crate::torrent::is_torrent(&item.url())
        {
            crate::torrent::forget_download(id);
        }
        self.video_sources.borrow_mut().remove(&id);
        self.epoch.borrow_mut().remove(&id);
        if let Some(pos) = (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<DownloadItem>()
                .map(|it| it.id() == id)
                .unwrap_or(false)
        }) {
            self.store.remove(pos);
        }
    }

    fn for_matching(
        self: &Rc<Self>,
        matches: impl Fn(DownloadStatus) -> bool,
        mut op: impl FnMut(&Rc<Self>, u64),
    ) {
        let ids: Vec<u64> = (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| matches(it.status()))
            .map(|it| it.id())
            .collect();
        for id in ids {
            op(self, id);
        }
    }

    /// Whether `gen` is still the row's latest engine spawn.
    fn is_current(&self, id: u64, generation: u64) -> bool {
        self.epoch.borrow().get(&id).cloned().unwrap_or(0) == generation
    }

    /// Rows waiting queued (cached; see `queued`).
    pub fn queued_count(&self) -> usize {
        self.queued.get()
    }

    /// Whether any item is queued, downloading or paused.
    pub fn has_active(&self) -> bool {
        self.active_count() > 0
    }

    /// Rows `cancel_all` would touch (queued, downloading, paused).
    pub fn active_count(&self) -> usize {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            })
            .count()
    }

    /// Whether anything is actually transferring (queued or downloading).
    /// Paused items don't count: closing over only-paused downloads quits
    /// instead of hiding to a "background" notification.
    pub fn has_transferring(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Queued | DownloadStatus::Downloading))
    }

    /// Whether any item failed or was cancelled.
    pub fn has_failed(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled))
    }

    /// Whether any item genuinely failed. User-cancelled rows need no
    /// error banner: cancelling was deliberate, and Retry Failed in the
    /// menu still resurrects them via `has_failed`.
    pub fn has_errored(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed))
    }

    fn any_status(&self, pred: impl Fn(DownloadStatus) -> bool) -> bool {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .any(|it| pred(it.status()))
    }

    fn queue_file() -> std::path::PathBuf {
        // Test seam only (debug builds run the suite against temp files):
        // release builds always use the real location, so a crafted
        // launcher environment can never redirect queue state.
        if cfg!(debug_assertions)
            && let Some(p) = std::env::var_os("GRAB_QUEUE_FILE")
        {
            return std::path::PathBuf::from(p);
        }
        let mut dir = glib::user_data_dir();
        dir.push("grab");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("queue.json")
    }

    /// Move a broken queue file aside (`queue.json.bak`) so its bytes
    /// survive for inspection and the next persist starts fresh.
    fn quarantine_queue() {
        let bak = Self::queue_file().with_extension("json.bak");
        if let Err(e) = std::fs::rename(Self::queue_file(), &bak) {
            tracing::error!("could not quarantine download queue: {e}");
        }
    }

    fn persist_queue(&self) {
        if self.batch.get() > 0 {
            return;
        }
        let mut items = Vec::new();
        for i in 0..self.store.n_items() {
            if let Some(it) = self.store.item(i).and_downcast::<DownloadItem>() {
                // Cancelled rows carry no intent: never persisted.
                if it.status() != DownloadStatus::Cancelled {
                    let segments = self.segment_state.borrow().get(&it.id()).cloned();
                    let selected_files = crate::torrent::get_selection(&it.url().to_string());
                    // Empty means "no folder tracked": omit it so old files
                    // stay clean and old app versions keep reading new ones.
                    // Same for the video source: only Page rows write it.
                    let output_dir = it.output_dir().to_string();
                    let output_dir = (!output_dir.is_empty()).then_some(output_dir);
                    let video_source = self
                        .video_source(it.id())
                        .filter(|s| matches!(s, crate::media_types::VideoSource::Page { .. }));
                    items.push(StoredItem {
                        url: it.url().to_string(),
                        dest_dir: it.dest_dir().to_string(),
                        filename: it.filename().to_string(),
                        status: it.status(),
                        progress: it.progress(),
                        segments,
                        selected_files,
                        output_dir,
                        video_source,
                    });
                }
            }
        }
        let data = StoredQueue {
            version: QUEUE_VERSION,
            items,
        };
        let text = match serde_json::to_string_pretty(&data) {
            Ok(text) => text,
            Err(e) => {
                tracing::error!("could not serialize download queue: {e}");
                return;
            }
        };
        let tmp = Self::queue_file().with_extension("json.tmp");
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            Ok(())
        };
        match write_tmp() {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, Self::queue_file()) {
                    tracing::error!("could not replace download queue: {e}");
                    return;
                }
                if let Some(parent) = Self::queue_file().parent()
                    && let Ok(dir) = std::fs::File::open(parent)
                {
                    let _ = dir.sync_all();
                }
            }
            Err(e) => tracing::error!("could not persist download queue: {e}"),
        }
    }

    /// Validate one persisted queue entry for restore: trust-checked
    /// output dir, URL, filename, absolute dest, well-shaped bitmap.
    /// Invalid entries warn-skip (never strand engines); valid ones
    /// collect for the apply phase. Pure part of `restore_queue` —
    /// validation must never insert or spawn (see its two-phase note).
    fn validate_stored_item(item: StoredItem) -> Option<PendingRestore> {
        // The recorded engine folder is only trusted when it sits
        // directly inside the row's own dest; otherwise it stays
        // unknown and delete falls back to the recomputed path.
        let output_dir = item.output_dir.clone().filter(|dir| {
            let folder = std::path::PathBuf::from(dir);
            folder.is_absolute()
                && folder
                    .parent()
                    .is_some_and(|p| p == std::path::Path::new(&item.dest_dir))
        });
        // Mirror restore_existing's cheap validations now, so the
        // apply phase below cannot fail (and strand engines) partway.
        if normalize_url(&item.url).is_err() {
            tracing::warn!("skipping queue entry with bad URL");
            return None;
        }
        if !sane_filename(&item.filename) {
            tracing::warn!(
                "skipping queue entry: Invalid filename in queue: {}",
                item.filename
            );
            return None;
        }
        if !std::path::Path::new(&item.dest_dir).is_absolute() {
            tracing::warn!(
                "skipping queue entry: Invalid destination in queue: {}",
                item.dest_dir
            );
            return None;
        }
        // A stored bitmap resumes segmented; anything
        // misshapen is dropped (single-stream fallback stays
        // correct via the spawn-time file checks).
        let segments = match &item.segments {
            Some(s)
                if s.total > 0
                    && s.total <= MAX_SEGMENTED_TOTAL
                    && s.done.len() == s.total.div_ceil(piece_len(s.total)) as usize =>
            {
                Some(s.clone())
            }
            _ => None,
        };
        Some(PendingRestore {
            item,
            output_dir,
            segments,
        })
    }

    /// Load the persisted queue (cap: 1000 items / 10 MB), then resume.
    /// Unusable files are moved to `queue.json.bak` (not deleted), so a
    /// single bad write can never silently wipe the whole queue.
    pub fn restore_queue(self: &Rc<Self>) {
        if Self::queue_file().exists() {
            const MAX_QUEUE_BYTES: u64 = 10_000_000;
            const MAX_QUEUE_ITEMS: usize = 1000;
            if std::fs::metadata(Self::queue_file())
                .map(|m| m.len() > MAX_QUEUE_BYTES)
                .unwrap_or(true)
            {
                tracing::warn!("quarantining oversized download queue");
                Self::quarantine_queue();
                return;
            };
            let Ok(text) = std::fs::read_to_string(Self::queue_file()) else {
                return;
            };
            let Ok(queue) = serde_json::from_str::<StoredQueue>(&text) else {
                tracing::warn!("quarantining unreadable download queue");
                Self::quarantine_queue();
                return;
            };
            if queue.version == 0 || queue.version > QUEUE_VERSION {
                tracing::warn!("quarantining download queue version {}", queue.version);
                Self::quarantine_queue();
                return;
            }
            // Over-cap queues keep every resumable item first, then the
            // newest history: active rows are user intent, Done rows are not.
            let mut items = queue.items;
            if items.len() > MAX_QUEUE_ITEMS {
                tracing::warn!(
                    "truncating download queue ({} items, keeping active first)",
                    items.len()
                );
                let (mut active, done): (Vec<StoredItem>, Vec<StoredItem>) = items
                    .into_iter()
                    .partition(|it| !matches!(it.status, DownloadStatus::Done));
                active.truncate(MAX_QUEUE_ITEMS);
                let skip = done.len().saturating_sub(MAX_QUEUE_ITEMS - active.len());
                items = active
                    .into_iter()
                    .chain(done.into_iter().skip(skip))
                    .collect();
            }
            self.batch.set(self.batch.get() + 1);
            // Two phases: every insert below can spawn an engine via
            // start_next, so collect all restore decisions first and only
            // then apply them — a mid-loop validation failure can no longer
            // leave half-spawned engines behind.
            let mut pending = Vec::with_capacity(items.len());
            for item in items {
                if let Some(p) = Self::validate_stored_item(item) {
                    pending.push(p);
                }
            }
            for p in pending {
                match p.item.status {
                    DownloadStatus::Done => {
                        self.insert_history(
                            p.item.url,
                            p.item.dest_dir,
                            p.item.filename,
                            p.item.progress,
                            p.output_dir,
                        );
                    }
                    status => {
                        // Re-stage the intake file selection BEFORE
                        // restore_existing: the live map is in-memory only,
                        // and insert → start_next → spawn_torrent reads it
                        // synchronously. Staging after the insert is too
                        // late — session adoption would see None against the
                        // persisted filter and fail the row on the
                        // only_files mismatch arm.
                        if let Some(sel) = p.item.selected_files.clone()
                            && let Ok(url) = normalize_url(&p.item.url)
                        {
                            crate::torrent::stage_selection(&url, sel);
                        }
                        match self.restore_existing(
                            &p.item.url,
                            &p.item.dest_dir,
                            &p.item.filename,
                            status,
                            p.segments,
                            p.item.video_source.clone(),
                        ) {
                            Ok(restored) => {
                                // Re-attach the recorded engine folder.
                                if let Some(dir) = p.output_dir.clone() {
                                    restored.set_output_dir(dir);
                                }
                            }
                            Err(e) => tracing::warn!("skipping queue entry: {e}"),
                        }
                    }
                }
            }
            self.batch.set(self.batch.get().saturating_sub(1));
            // Drop archived .torrent files no row references anymore
            // (removed rows keep theirs until now; explicit deletes drop
            // theirs at once, Finished engines drop theirs on completion).
            let referenced: std::collections::HashSet<String> = (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent_url(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            // Same for staged file selections: rows that are gone need no
            // filter on a future re-add (which stages fresh at intake).
            crate::torrent::prune_selections(&referenced);
            // Session persistence re-adds every remembered torrent when the
            // engine is created — including entries whose rows vanished in
            // a crash between row removal and the session delete. Sweep
            // those orphans now that the queue is restored: the keep-set
            // is every live torrent row's info-hash (Done rows finished and
            // already left the session).
            // No session yet: the sweep would no-op, so skip the store walk.
            if crate::torrent::session_handle().is_some() {
                let keep: std::collections::HashSet<String> = (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .filter(|it| {
                        it.status() != DownloadStatus::Done && crate::torrent::is_torrent(&it.url())
                    })
                    .filter_map(|it| crate::torrent::info_hash_for_url(&it.url()))
                    .collect();
                tokio_rt().spawn(async move {
                    crate::torrent::sweep_session_orphans(&keep).await;
                });
            }
            self.persist_queue();
            self.changed();
        }
    }

    /// Abort running tasks and persist the queue for the next launch.
    pub fn shutdown(&self) {
        self.draining.set(true);
        let handles: Vec<_> = self.running.borrow_mut().drain().map(|(_, h)| h).collect();
        for handle in &handles {
            handle.abort();
        }
        // Engine tasks touch only the tokio runtime (never the main thread),
        // so joining them here is prompt and deadlock-free.
        tokio_rt().block_on(async {
            for handle in handles {
                let _ = handle.await;
            }
        });
        let ids: Vec<u64> = self.segment_state.borrow().keys().cloned().collect();
        for id in ids {
            if let Some(item) = self.find(id)
                && let Some(st) = self.segment_state.borrow_mut().get_mut(&id)
            {
                truncate_to_prefix(&item.file_path(), st);
                st.forget_beyond_prefix();
            }
        }
        // Persist BEFORE returning: the bitmaps are what let the next launch
        // resume segmented instead of restarting. Pump tails exit silently
        // once draining is set, so this is the only persist that matters.
        self.persist_queue();
    }
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
