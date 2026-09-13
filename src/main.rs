mod application;
mod download;
mod preferences;
mod settings;
mod torrent;
mod window;

use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;

pub const APP_ID: &str = "io.github.houssemko.Grab";

/// Locale for translated UI strings: compiled .mo catalogs under the
/// install prefix (or GRAB_LOCALEDIR override). Without catalogs gettext
/// returns the English msgids unchanged (dev/test).
fn init_locale() {
    // Explicit override, Flatpak (/app), or the cargo build prefix.
    // bindtextdomain tolerates a missing dir; without .mo catalogs
    // gettext returns the English msgids unchanged (dev/test).
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

fn ensure_schema_dir() {
    if std::env::var_os("GSETTINGS_SCHEMA_DIR").is_some() {
        return;
    }
    let dir = env!("GRAB_SCHEMA_DIR");
    if std::path::Path::new(&format!("{dir}/gschemas.compiled")).exists() {
        // SAFETY: single-threaded startup, before any GSettings use.
        unsafe { std::env::set_var("GSETTINGS_SCHEMA_DIR", dir) };
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();
    ensure_schema_dir();
    init_locale();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    application::setup(&app);
    app.run()
}
