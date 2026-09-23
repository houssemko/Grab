mod application;
mod cookies;
mod download;
mod engine_msg;
mod file_names;
mod install_help;
mod install_progress;
mod media_types;
mod net_types;
mod preferences;
mod runtime;
mod settings;
mod torrent;
mod ui_util;
// Video-page extraction + resolver worker (src/video.rs): consumed by the
// dialog, preferences, queue persistence and the engine. A few helpers
// stay ahead of use (expiry checks for a future no-re-resolve fast path);
// expect dead_code until they wire up. Remove this attribute if it ever
// goes unfulfilled.
#[expect(dead_code)]
mod video;
mod video_tools;
mod video_types;
mod window;

use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;

pub const APP_ID: &str = "io.github.houssemko.Grab";

/// Metainfo catalog for the About dialog, embedded at compile time.
/// from_appdata reads only GResource paths, and the cargo OUT_DIR the
/// bundle used to be loaded from does not exist at runtime (Flatpak
/// included), which crashed About — so the bytes ride inside the binary.
static GRESOURCE_DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/grab.gresource"));

/// Locale for translated UI strings: compiled .mo catalogs under the
/// install prefix (or GRAB_LOCALEDIR override). Without catalogs gettext
/// returns the English msgids unchanged (dev/test).
fn init_locale() {
    // First existing dir wins: explicit override, Flatpak (/app), or
    // the cargo build prefix. A missing dir is harmless (gettext falls
    // back to the English msgids), so no further fallback is needed.
    let compiled = env!("GRAB_LOCALEDIR").to_string();
    let dir = std::env::var("GRAB_LOCALEDIR")
        .ok()
        .into_iter()
        .chain(["/app/share/locale".to_string(), compiled])
        .find(|d| std::path::Path::new(d).exists())
        .unwrap_or_default();
    gettextrs::bindtextdomain("grab", dir).ok();
    gettextrs::textdomain("grab").ok();
}

/// Whether `dir` compiles to a schema source providing our schema.
/// Uses an explicit source (never the default one): the default source is
/// cached process-wide, so merely querying it before fixing the environment
/// would freeze a useless preset in place.
fn schema_in_dir(dir: &str) -> bool {
    gio::SettingsSchemaSource::from_directory(dir, None::<&gio::SettingsSchemaSource>, true)
        .ok()
        .and_then(|source| source.lookup(APP_ID, false))
        .is_some()
}

fn ensure_schema_dir() {
    // A preset GSETTINGS_SCHEMA_DIR (flatpak override, distro packaging) is
    // only honored if it actually provides our schema — otherwise fall
    // through to our own install tree instead of aborting later with
    // "Settings schema is not installed".
    if let Some(preset) = std::env::var_os("GSETTINGS_SCHEMA_DIR")
        && preset.to_string_lossy().split(':').any(schema_in_dir)
    {
        return;
    }
    // Compile-time dir (cargo OUT_DIR): only exists for `cargo run`. Portable
    // fallback: the install tree this binary lives in (<prefix>/bin/grab ->
    // <prefix>/share/glib-2.0/schemas), so a tarball extracted anywhere works
    // instead of aborting with "Settings schema is not installed".
    let mut candidates = vec![env!("GRAB_SCHEMA_DIR").to_string()];
    if let Ok(exe) = std::env::current_exe()
        && let Some(prefix) = exe.parent().and_then(|b| b.parent())
    {
        candidates.push(
            prefix
                .join("share/glib-2.0/schemas")
                .to_string_lossy()
                .into_owned(),
        );
    }
    if let Some(dir) = candidates
        .into_iter()
        .find(|d| std::path::Path::new(&format!("{d}/gschemas.compiled")).exists())
    {
        // SAFETY: single-threaded startup, before any GSettings use.
        unsafe { std::env::set_var("GSETTINGS_SCHEMA_DIR", dir) };
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();
    ensure_schema_dir();
    // Registered before any dialog can open; a corrupt bundle only loses
    // release notes, the About action itself guards the missing case.
    if let Ok(res) = gio::Resource::from_data(&glib::Bytes::from_static(GRESOURCE_DATA)) {
        gio::resources_register(&res);
    }
    init_locale();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    application::setup(&app);
    app.run()
}
