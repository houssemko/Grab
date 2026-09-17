//! Typed access to the GSettings schema: one method per key, so a
//! renamed key fails at compile time instead of silently reading a default.
//! [`AppSettings`] derefs to [`gio::Settings`], so `bind()` and signal
//! connections keep working; use the [`key`] constants for those.

use gtk4::gio;
use gtk4::prelude::SettingsExt as _;

pub mod key {
    pub const DOWNLOAD_DIR: &str = "download-dir";
    pub const MAX_CONCURRENT: &str = "max-concurrent";
    pub const CONNECTIONS: &str = "connections";
    pub const SPEED_LIMIT: &str = "speed-limit";
    pub const RETRIES: &str = "retries";
    pub const TIMEOUT: &str = "timeout";
    pub const USER_AGENT: &str = "user-agent";
    pub const KEEP_SERVER_DATE: &str = "keep-server-date";
    pub const SHOW_NOTIFICATIONS: &str = "show-notifications";
    pub const NOTIFY_BACKGROUND: &str = "notify-background";
    pub const INHIBIT_SUSPEND: &str = "inhibit-suspend";
    pub const TORRENT_SEED_FINISHED: &str = "torrent-seed-finished";
    pub const TORRENT_DHT: &str = "torrent-dht";
    pub const TORRENT_PEER_LIMIT: &str = "torrent-peer-limit";
    pub const TORRENT_TRACKERS: &str = "torrent-trackers";
    pub const TORRENT_SEED_RATIO: &str = "torrent-seed-ratio";
    pub const TORRENT_SEED_TIME: &str = "torrent-seed-time";
    pub const TORRENT_LISTEN_PORT: &str = "torrent-listen-port";
    pub const VIDEO_QUALITY: &str = "video-quality";
    pub const VIDEO_AUDIO_ONLY: &str = "video-audio-only";
    pub const COOKIES_PATH: &str = "cookies-path";
    pub const COOKIES_BROWSER: &str = "cookies-browser";
    pub const WINDOW_WIDTH: &str = "window-width";
    pub const WINDOW_HEIGHT: &str = "window-height";
}

#[derive(Clone, Debug)]
pub struct AppSettings(gio::Settings);

impl AppSettings {
    pub fn new() -> Self {
        Self(gio::Settings::new(crate::APP_ID))
    }

    pub fn download_dir(&self) -> String {
        self.0.string(key::DOWNLOAD_DIR).to_string()
    }
    pub fn max_concurrent(&self) -> i32 {
        self.0.int(key::MAX_CONCURRENT)
    }
    pub fn connections(&self) -> i32 {
        self.0.int(key::CONNECTIONS)
    }
    pub fn speed_limit(&self) -> String {
        self.0.string(key::SPEED_LIMIT).to_string()
    }
    pub fn retries(&self) -> i32 {
        self.0.int(key::RETRIES)
    }
    pub fn timeout(&self) -> i32 {
        self.0.int(key::TIMEOUT)
    }
    pub fn user_agent(&self) -> String {
        self.0.string(key::USER_AGENT).to_string()
    }
    pub fn keep_server_date(&self) -> bool {
        self.0.boolean(key::KEEP_SERVER_DATE)
    }
    pub fn show_notifications(&self) -> bool {
        self.0.boolean(key::SHOW_NOTIFICATIONS)
    }
    pub fn notify_background(&self) -> bool {
        self.0.boolean(key::NOTIFY_BACKGROUND)
    }
    pub fn inhibit_suspend(&self) -> bool {
        self.0.boolean(key::INHIBIT_SUSPEND)
    }
    pub fn torrent_seed_finished(&self) -> bool {
        self.0.boolean(key::TORRENT_SEED_FINISHED)
    }
    pub fn torrent_dht(&self) -> bool {
        self.0.boolean(key::TORRENT_DHT)
    }
    pub fn torrent_peer_limit(&self) -> i32 {
        self.0.int(key::TORRENT_PEER_LIMIT)
    }
    pub fn torrent_trackers(&self) -> String {
        self.0.string(key::TORRENT_TRACKERS).to_string()
    }
    pub fn torrent_seed_ratio(&self) -> f64 {
        self.0.double(key::TORRENT_SEED_RATIO)
    }
    pub fn torrent_seed_time(&self) -> i32 {
        self.0.int(key::TORRENT_SEED_TIME)
    }
    pub fn torrent_listen_port(&self) -> i32 {
        self.0.int(key::TORRENT_LISTEN_PORT)
    }
    // Read by the New Download video step (per-download defaults).
    pub fn video_quality(&self) -> String {
        self.0.string(key::VIDEO_QUALITY).to_string()
    }
    pub fn video_audio_only(&self) -> bool {
        self.0.boolean(key::VIDEO_AUDIO_ONLY)
    }
    pub fn cookies_browser(&self) -> String {
        self.0.string(key::COOKIES_BROWSER).to_string()
    }

    pub fn window_width(&self) -> i32 {
        self.0.int(key::WINDOW_WIDTH)
    }
    pub fn window_height(&self) -> i32 {
        self.0.int(key::WINDOW_HEIGHT)
    }
}

impl From<gio::Settings> for AppSettings {
    fn from(s: gio::Settings) -> Self {
        Self(s)
    }
}

impl std::ops::Deref for AppSettings {
    type Target = gio::Settings;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
