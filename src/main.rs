mod application;
mod download;
mod preferences;
mod window;

use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;

pub const APP_ID: &str = "org.gnome.WgetFrontend";

/// Point GSettings at the build-time compiled schema dir so `cargo run`
/// works without installing. A pre-set GSETTINGS_SCHEMA_DIR or a
/// system-installed schema always wins.
fn ensure_schema_dir() {
    if std::env::var_os("GSETTINGS_SCHEMA_DIR").is_some() {
        return;
    }
    let dir = env!("WGET_SCHEMA_DIR");
    if std::path::Path::new(&format!("{dir}/gschemas.compiled")).exists() {
        // SAFETY: single-threaded startup, before any GSettings use.
        unsafe { std::env::set_var("GSETTINGS_SCHEMA_DIR", dir) };
    }
}

fn main() -> glib::ExitCode {
    ensure_schema_dir();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    application::setup(&app);
    app.run()
}
