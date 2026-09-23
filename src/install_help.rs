//! Guided yt-dlp/ffmpeg installation for tarball/dev builds.
//!
//! Inside Flatpak the Install button is the only viable path (host
//! packages cannot reach the sandbox); outside it users install through
//! their distro. This dialog shows distro-detected terminal commands (or
//! manual links when unknown) with copy buttons, plus Check Again which
//! closes the dialog and re-runs resolution via the caller's callback.

use adw::prelude::*;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

/// Close the dialog, then run a follow-up (Check Again closes and
/// re-runs resolution via the caller's callback).
fn close_then(btn: &gtk4::Button, dialog: &adw::Dialog, on_check: impl Fn() + 'static) {
    let weak = dialog.downgrade();
    btn.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.close();
        }
        on_check();
    });
}

/// Show the install-help dialog. `on_check` runs after Check Again closes
/// the dialog (typically: re-resolve tools and refresh the calling row).
pub fn show(parent: &impl glib::object::IsA<gtk4::Widget>, on_check: impl Fn() + 'static) {
    let dialog = adw::Dialog::builder()
        .title(gettext("Install Support Tools"))
        .build();
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_end_title_buttons(false);
    hb.set_show_start_title_buttons(false);
    let close_btn = gtk4::Button::builder()
        .label(gettext("_Close"))
        .use_underline(true)
        .build();
    let check_btn = gtk4::Button::builder()
        .label(gettext("_Check Again"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&close_btn);
    hb.pack_end(&check_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&check_btn));

    crate::ui_util::close_on_click(&close_btn, &dialog);
    close_then(&check_btn, &dialog, on_check);

    let pkgs = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| crate::video::distro_packages(&text));
    match pkgs {
        Some(pkgs) => {
            let group = adw::PreferencesGroup::builder()
                .title(pkgs.distro.clone())
                .description(gettext(
                    "Run these commands in a terminal, then press Check Again.",
                ))
                .build();
            command_row(&group, "yt-dlp", &pkgs.yt_dlp);
            command_row(&group, "ffmpeg", &pkgs.ffmpeg);
            page.add(&group);
        }
        None => {
            let group = adw::PreferencesGroup::builder()
                .title(gettext("Install both tools manually"))
                .description(gettext(
                    "Grab couldn't detect your distribution. Install both tools, make sure they are on your PATH, then press Check Again.",
                ))
                .build();
            link_row(
                &group,
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp#installation",
            );
            link_row(&group, "ffmpeg", "https://ffmpeg.org/download.html");
            page.add(&group);
        }
    }

    dialog.present(Some(parent));
}

/// One command row: tool name, selectable-feeling command, copy button
/// with a brief checkmark confirmation.
fn command_row(group: &adw::PreferencesGroup, tool: &str, command: &str) {
    let row = adw::ActionRow::builder()
        .title(tool)
        .subtitle(command)
        .build();
    let copy = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Copy command"))
        .valign(gtk4::Align::Center)
        .build();
    copy.update_property(&[gtk4::accessible::Property::Label(&gettext("Copy command"))]);
    row.add_suffix(&copy);
    let cmd = command.to_string();
    copy.connect_clicked(move |b| {
        if let Some(clipboard) = gtk4::gdk::Display::default().map(|d| d.clipboard()) {
            clipboard.set_text(&cmd);
        }
        b.set_icon_name("emblem-ok-symbolic");
        let b2 = b.clone();
        glib::timeout_add_seconds_local(2, move || {
            b2.set_icon_name("edit-copy-symbolic");
            glib::ControlFlow::Break
        });
    });
    group.add(&row);
}

/// One outbound-link row for the manual fallback.
fn link_row(group: &adw::PreferencesGroup, tool: &str, url: &'static str) {
    let row = adw::ActionRow::builder().title(tool).build();
    let open = gtk4::Button::builder()
        .label(gettext("Open"))
        .valign(gtk4::Align::Center)
        .build();
    row.add_suffix(&open);
    open.connect_clicked(move |b| {
        let root = b.root().and_downcast::<gtk4::Window>();
        gtk4::UriLauncher::new(url).launch(root.as_ref(), gio::Cancellable::NONE, |_| {});
    });
    group.add(&row);
}
