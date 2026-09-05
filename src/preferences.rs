//! Preferences dialog: every row bound straight to GSettings.

use adw::prelude::*;
use gtk4::gio;
use gtk4::prelude::*;
use libadwaita as adw;

/// Sync an int GSettings key with an AdwSpinRow (whose value is f64).
fn bind_spin(settings: &gio::Settings, key: &str, row: &adw::SpinRow) {
    row.set_value(settings.int(key) as f64);
    {
        let s = settings.clone();
        let k = key.to_string();
        row.connect_value_notify(move |r| {
            let _ = s.set_int(&k, r.value() as i32);
        });
    }
    {
        // Weak: this closure lives on the app-lifetime Settings object, so a
        // strong row ref would leak the whole dialog on every open.
        let r = row.downgrade();
        let k = key.to_string();
        let kd = k.clone();
        settings.connect_changed(Some(kd.as_str()), move |s, _| {
            if let Some(r) = r.upgrade() {
                let v = s.int(&k) as f64;
                if (r.value() - v).abs() > f64::EPSILON {
                    r.set_value(v);
                }
            }
        });
    }
}

pub fn show(parent: &impl gtk4::glib::object::IsA<gtk4::Widget>, settings: &gio::Settings) {
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .build();

    let page = adw::PreferencesPage::builder()
        .title("Downloads")
        .icon_name("folder-download-symbolic")
        .build();

    // -- Destination -------------------------------------------------------
    let dest_group = adw::PreferencesGroup::builder()
        .title("Destination")
        .build();
    let current = settings.string("download-dir").to_string();
    let shown = if current.is_empty() {
        "(System Downloads folder)".to_string()
    } else {
        current
    };
    let dest_label = gtk4::Label::builder()
        .label(&shown)
        .css_classes(["dimmed", "caption"])
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .hexpand(true)
        .halign(gtk4::Align::Start)
        .build();
    let dest_btn = gtk4::Button::builder()
        .label("Choose…")
        .valign(gtk4::Align::Center)
        .build();
    let reset_btn = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Use system default")
        .valign(gtk4::Align::Center)
        .build();
    let dest_row = adw::ActionRow::builder().title("Download folder").build();
    dest_row.add_suffix(&dest_label);
    dest_row.add_suffix(&reset_btn);
    dest_row.add_suffix(&dest_btn);
    dest_group.add(&dest_row);
    {
        let s = settings.clone();
        let l = dest_label.clone();
        let root = parent.root().and_downcast::<gtk4::Window>();
        dest_btn.connect_clicked(move |_| {
            let chooser = gtk4::FileDialog::builder()
                .title("Choose download folder")
                .build();
            let s2 = s.clone();
            let l2 = l.clone();
            chooser.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        let dir = p.to_string_lossy().into_owned();
                        // Same convention as bind_spin: a failed write means
                        // dconf itself is broken; never panic out of a dialog.
                        let _ = s2.set_string("download-dir", &dir);
                        l2.set_text(&dir);
                    }
                }
            });
        });
    }
    {
        let s = settings.clone();
        let l = dest_label.clone();
        reset_btn.connect_clicked(move |_| {
            let _ = s.set_string("download-dir", "");
            l.set_text("(System Downloads folder)");
        });
    }

    // -- Limits -------------------------------------------------------------
    let net_group = adw::PreferencesGroup::builder()
        .title("Network and Queue")
        .build();

    let concurrent = adw::SpinRow::builder()
        .title("Simultaneous downloads")
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 10.0, 1.0, 1.0, 0.0))
        .build();
    bind_spin(settings, "max-concurrent", &concurrent);
    net_group.add(&concurrent);

    let limit = adw::EntryRow::builder()
        .title("Speed limit (e.g. 500K, 2M; empty = unlimited)")
        .text(settings.string("speed-limit").as_str())
        .build();
    limit.set_input_purpose(gtk4::InputPurpose::FreeForm);
    settings.bind("speed-limit", &limit, "text").build();
    net_group.add(&limit);

    let retries = adw::SpinRow::builder()
        .title("Retries")
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 99.0, 1.0, 5.0, 0.0))
        .build();
    bind_spin(settings, "retries", &retries);
    net_group.add(&retries);

    let timeout = adw::SpinRow::builder()
        .title("Timeout (seconds)")
        .adjustment(&gtk4::Adjustment::new(30.0, 5.0, 300.0, 5.0, 30.0, 0.0))
        .build();
    bind_spin(settings, "timeout", &timeout);
    net_group.add(&timeout);

    let ua = adw::EntryRow::builder()
        .title("User agent (optional)")
        .text(settings.string("user-agent").as_str())
        .build();
    settings.bind("user-agent", &ua, "text").build();
    net_group.add(&ua);

    // -- Notifications ------------------------------------------------------
    let ui_group = adw::PreferencesGroup::builder().title("Interface").build();
    let notif = adw::SwitchRow::builder()
        .title("Notify when downloads finish")
        .build();
    settings
        .bind("show-notifications", &notif, "active")
        .build();
    ui_group.add(&notif);

    page.add(&dest_group);
    page.add(&net_group);
    page.add(&ui_group);
    dialog.add(&page);
    dialog.present(Some(parent));
}
