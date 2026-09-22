//! Shared install-progress popover for the yt-dlp + ffmpeg tools install.
//!
//! Both the setup dialog (`window.rs`) and Preferences show the same staged
//! install: yt-dlp first, then the ffmpeg toolchain. The installer exposes
//! no byte progress, so each tool gets its own row with an indeterminate
//! pulsing bar; the bars pulse instead of showing fake percentages. A
//! spinner marks the active row and becomes a checkmark (or an error icon)
//! when its stage finishes.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gettextrs::gettext;
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;

/// One tool's row: activity indicator, name, status text, pulsing bar and
/// a wrapping error label that only appears on failure.
struct ToolRow {
    root: gtk4::Box,
    spinner: gtk4::Spinner,
    icon: gtk4::Image,
    status: gtk4::Label,
    bar: gtk4::ProgressBar,
    error: gtk4::Label,
    /// Which bar the shared pulse driver ticks; the row sets/clears it as
    /// its stage starts and finishes.
    active: Rc<RefCell<Option<gtk4::ProgressBar>>>,
}

impl ToolRow {
    /// Build one tool's row. `bar_label` is the accessible name for the
    /// progress bar; the status label next to it already announces state
    /// changes, the bar needs its own name too.
    fn new(name: &str, bar_label: &str, active: &Rc<RefCell<Option<gtk4::ProgressBar>>>) -> Self {
        let spinner = gtk4::Spinner::new();
        // Stopped until the stage starts; still allocates its slot so the
        // row doesn't shift when downloading begins.
        spinner.stop();
        let icon = gtk4::Image::from_icon_name("emblem-ok-symbolic");
        icon.set_visible(false);

        let name_label = gtk4::Label::new(Some(name));
        name_label.set_halign(gtk4::Align::Start);
        name_label.add_css_class("heading");
        let status = gtk4::Label::new(Some(&gettext("Waiting…")));
        status.set_halign(gtk4::Align::Start);
        status.add_css_class("caption");
        status.add_css_class("dimmed");
        let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        titles.set_hexpand(true);
        titles.append(&name_label);
        titles.append(&status);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        header.append(&spinner);
        header.append(&icon);
        header.append(&titles);

        let bar = gtk4::ProgressBar::new();
        bar.update_property(&[gtk4::accessible::Property::Label(bar_label)]);

        let error = gtk4::Label::new(None);
        error.set_wrap(true);
        error.set_max_width_chars(32);
        error.set_halign(gtk4::Align::Start);
        error.set_visible(false);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        root.append(&header);
        root.append(&bar);
        root.append(&error);

        Self {
            root,
            spinner,
            icon,
            status,
            bar,
            error,
            active: active.clone(),
        }
    }

    fn widget(&self) -> gtk4::Box {
        self.root.clone()
    }

    /// Mark the row active: spinner runs and its bar becomes the pulse target.
    fn set_downloading(&self) {
        self.spinner.start();
        self.status.set_text(&gettext("Downloading…"));
        *self.active.borrow_mut() = Some(self.bar.clone());
    }

    /// Mark the row done: checkmark, full bar, probed version when available.
    fn set_installed(&self, version: Option<String>) {
        *self.active.borrow_mut() = None;
        self.spinner.set_visible(false);
        self.icon.set_visible(true);
        self.bar.set_fraction(1.0);
        let text = match version {
            Some(v) => gettext("Installed • {version}").replace("{version}", &v),
            None => gettext("Installed"),
        };
        self.status.set_text(&text);
    }

    /// Mark the row failed: error icon plus the message as wrapping text
    /// (not color alone, so it survives high-contrast and screen readers).
    /// The popover stays open; the caller re-enables the Install button so
    /// the user can retry.
    fn set_failed(&self, message: &str) {
        *self.active.borrow_mut() = None;
        self.spinner.set_visible(false);
        self.icon.set_icon_name(Some("dialog-error-symbolic"));
        self.icon.set_visible(true);
        self.bar.set_visible(false);
        self.status.set_text(&gettext("Failed"));
        self.error.set_text(message);
        self.error.set_visible(true);
    }
}

/// Run the staged yt-dlp + ffmpeg install under a shared progress popover
/// anchored at `button`.
///
/// Each row moves Waiting… → Downloading… → Installed (with the probed
/// version when available). A failure marks its row Failed with the error
/// and leaves the popover open; on success both checkmarks linger briefly
/// before the popover closes.
///
/// `on_error` reports the failure to the caller (e.g. in the tools-row
/// subtitle); `on_success` refreshes the caller's tools state after the
/// popover closes. The button is insensitive for the whole run either way.
pub fn run(
    button: &gtk4::Button,
    on_error: impl Fn(String) + 'static,
    on_success: impl Fn() + 'static,
) {
    button.set_sensitive(false);
    let btn = button.clone();

    // Which bar the shared pulse driver ticks; each row sets/clears it as
    // its stage starts and finishes.
    let active: Rc<RefCell<Option<gtk4::ProgressBar>>> = Rc::new(RefCell::new(None));
    let yt = ToolRow::new("yt-dlp", &gettext("yt-dlp install progress"), &active);
    let ff = ToolRow::new("ffmpeg", &gettext("ffmpeg install progress"), &active);

    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    rows.set_margin_top(18);
    rows.set_margin_bottom(18);
    rows.set_margin_start(18);
    rows.set_margin_end(18);
    rows.append(&yt.widget());
    rows.append(&ff.widget());
    let pop = gtk4::Popover::new();
    pop.set_child(Some(&rows));
    pop.set_parent(&btn);
    pop.popup();

    glib::spawn_future_local(async move {
        // Stage 1: yt-dlp.
        yt.set_downloading();
        // Pulse driver: ticks whichever row is currently downloading, and
        // stops itself once no row is active. Started after the first row
        // goes active so the first tick can't observe an empty slot and
        // stop itself before anything pulses.
        {
            let active = active.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                if let Some(bar) = active.borrow().as_ref() {
                    bar.pulse();
                    ControlFlow::Continue
                } else {
                    ControlFlow::Break
                }
            });
        }
        let yt_path = match crate::video::install_ytdlp().await {
            Ok(path) => path,
            Err(e) => {
                let message = e.to_string();
                yt.set_failed(&message);
                btn.set_sensitive(true);
                on_error(message);
                return;
            }
        };
        let version = crate::video::tool_display_version(yt_path, "--version").await;
        yt.set_installed(version);

        // Stage 2: the ffmpeg toolchain (ffmpeg and ffprobe).
        ff.set_downloading();
        let ff_path = match crate::video::install_ffmpeg().await {
            Ok(path) => path,
            Err(e) => {
                let message = e.to_string();
                ff.set_failed(&message);
                btn.set_sensitive(true);
                on_error(message);
                return;
            }
        };
        let version = crate::video::tool_display_version(ff_path, "-version").await;
        ff.set_installed(version);

        // Linger on the two checkmarks so the completed state registers,
        // then close and hand back to the caller.
        glib::timeout_future(Duration::from_millis(1200)).await;
        pop.popdown();
        on_success();
        btn.set_sensitive(true);
    });
}
