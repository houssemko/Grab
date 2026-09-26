//! Guided yt-dlp/ffmpeg/quickjs install for tarball/dev builds (Flatpak Install is the only sandbox path).

use adw::prelude::*;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

/// Close the dialog, then run a follow-up.
fn close_then(btn: &gtk4::Button, dialog: &adw::Dialog, on_check: impl Fn() + 'static) {
    let weak = dialog.downgrade();
    btn.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.close();
        }
        on_check();
    });
}

/// Show the install-help dialog; `on_check` re-resolves tools and refreshes the caller.
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
        .and_then(|text| crate::video_tools::distro_packages(&text));
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
            quickjs_row(&group);
            page.add(&group);
        }
        None => {
            let group = adw::PreferencesGroup::builder()
                .title(gettext("Install the tools manually"))
                .description(gettext(
                    "Install yt-dlp and ffmpeg yourself and make sure they are on your PATH; quickjs installs from its row below. Then press Check Again.",
                ))
                .build();
            link_row(
                &group,
                "yt-dlp",
                "https://github.com/yt-dlp/yt-dlp#installation",
            );
            link_row(&group, "ffmpeg", "https://ffmpeg.org/download.html");
            quickjs_row(&group);
            page.add(&group);
        }
    }

    dialog.present(Some(parent));
}

/// quickjs-ng row: Grab's pinned JS runtime for YouTube. No distro packages
/// it, so instead of a terminal command the row installs Grab's own pinned
/// binary with one click (same installer the Flatpak staged prompt uses).
/// Hidden on architectures quickjs-ng doesn't ship; already-installed shows
/// a checkmark instead of a button.
fn quickjs_row(group: &adw::PreferencesGroup) {
    if crate::video_tools::quickjs_download_url().is_none() {
        return;
    }
    let row = adw::ActionRow::builder()
        .title("quickjs")
        .subtitle(gettext("JS runtime for YouTube"))
        .build();
    // Install button → spinner → checkmark; swaps never move the text column.
    let stack = gtk4::Stack::new();
    stack.set_valign(gtk4::Align::Center);
    let install_btn = gtk4::Button::builder()
        .label(gettext("Install"))
        .valign(gtk4::Align::Center)
        .build();
    stack.add_named(&install_btn, Some("install"));
    stack.add_named(&adw::Spinner::new(), Some("spinner"));
    let done = gtk4::Image::from_icon_name("emblem-ok-symbolic");
    done.set_pixel_size(16);
    stack.add_named(&done, Some("done"));
    row.add_suffix(&stack);
    group.add(&row);

    if crate::video_tools::find_quickjs().is_some() {
        row.set_subtitle(&gettext("Installed"));
        stack.set_visible_child_name("done");
        return;
    }
    stack.set_visible_child_name("install");

    let (row_w, stack_w, btn_w) = (row.downgrade(), stack.downgrade(), install_btn.downgrade());
    install_btn.connect_clicked(move |_| {
        let (Some(row), Some(stack), Some(btn)) =
            (row_w.upgrade(), stack_w.upgrade(), btn_w.upgrade())
        else {
            return;
        };
        btn.set_sensitive(false);
        stack.set_visible_child_name("spinner");
        row.set_subtitle(&gettext("Downloading…"));
        glib::spawn_future_local(async move {
            match crate::video::install_quickjs().await {
                Ok(path) => {
                    let version = crate::video_tools::tool_display_version(path, "--version").await;
                    let text = match version {
                        Some(v) => gettext("Installed • {version}").replace("{version}", &v),
                        None => gettext("Installed"),
                    };
                    row.set_subtitle(&text);
                    stack.set_visible_child_name("done");
                }
                Err(e) => {
                    row.set_subtitle(&e.to_string());
                    btn.set_label(&gettext("Retry"));
                    btn.set_sensitive(true);
                    stack.set_visible_child_name("install");
                }
            }
        });
    });
}

/// One command row with copy button and brief checkmark confirmation. The copy
/// button reads the row's current subtitle, so callers can refresh the command
/// later with `set_subtitle`. Returns the row for visibility control.
pub(crate) fn command_row(
    group: &adw::PreferencesGroup,
    title: &str,
    command: &str,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
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
    let row_b = row.clone();
    copy.connect_clicked(move |b| {
        // The subtitle is the command; read it back so callers can refresh it
        // with `set_subtitle` after the row is built.
        let cmd = row_b.subtitle().unwrap_or_default();
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
    row
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
