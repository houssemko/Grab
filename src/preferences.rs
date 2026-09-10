use adw::prelude::*;
use gtk4::gio;
use gtk4::prelude::*;
use libadwaita as adw;

pub fn show(parent: &impl gtk4::glib::object::IsA<gtk4::Widget>, settings: &gio::Settings) {
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .build();

    let page = adw::PreferencesPage::builder()
        .title("Downloads")
        .icon_name("folder-download-symbolic")
        .build();

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
        .tooltip_text("Choose download folder")
        .valign(gtk4::Align::Center)
        .build();
    let reset_btn = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Use system default")
        .valign(gtk4::Align::Center)
        .build();
    reset_btn.update_property(&[gtk4::accessible::Property::Label("Use system default")]);
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
                        if s2.set_string("download-dir", &dir).is_ok() {
                            l2.set_text(&dir);
                        }
                    }
                }
            });
        });
    }
    {
        let s = settings.clone();
        let l = dest_label.clone();
        reset_btn.connect_clicked(move |_| {
            if s.set_string("download-dir", "").is_ok() {
                l.set_text("(System Downloads folder)");
            }
        });
    }

    let net_group = adw::PreferencesGroup::builder()
        .title("Network and Queue")
        .build();

    let concurrent = adw::SpinRow::builder()
        .title("Simultaneous downloads")
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 10.0, 1.0, 1.0, 0.0))
        .build();
    settings
        .bind("max-concurrent", &concurrent, "value")
        .build();
    net_group.add(&concurrent);

    let connections = adw::SpinRow::builder()
        .title("Connections per download")
        .subtitle("Parallel connections for large files (1 = single stream)")
        .adjustment(&gtk4::Adjustment::new(4.0, 1.0, 16.0, 1.0, 1.0, 0.0))
        .build();
    connections.set_tooltip_text(Some("Files under ~16 MB always use one connection"));
    settings.bind("connections", &connections, "value").build();
    net_group.add(&connections);

    let limit = adw::EntryRow::builder().title("Speed limit").build();
    limit.set_tooltip_text(Some("Per download, e.g. 500K, 2M; empty means unlimited"));
    limit.set_input_purpose(gtk4::InputPurpose::FreeForm);
    settings.bind("speed-limit", &limit, "text").build();
    // Flag junk immediately instead of failing rows at spawn time.
    {
        let l = limit.clone();
        let mark = move |row: &adw::EntryRow| {
            let t = row.text().to_string();
            let t = t.trim();
            let ok = t.is_empty() || t == "0" || crate::download::parse_rate(t).is_some();
            if ok {
                l.remove_css_class("error");
            } else {
                l.add_css_class("error");
            }
        };
        mark(&limit);
        limit.connect_changed(mark);
    }
    net_group.add(&limit);

    let retries = adw::SpinRow::builder()
        .title("Retries")
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 99.0, 1.0, 5.0, 0.0))
        .build();
    settings.bind("retries", &retries, "value").build();
    net_group.add(&retries);

    let timeout = adw::SpinRow::builder()
        .title("Timeout (seconds)")
        .adjustment(&gtk4::Adjustment::new(30.0, 5.0, 300.0, 5.0, 30.0, 0.0))
        .build();
    settings.bind("timeout", &timeout, "value").build();
    net_group.add(&timeout);

    let ua = adw::EntryRow::builder()
        .title("User agent (optional)")
        .build();
    settings.bind("user-agent", &ua, "text").build();
    net_group.add(&ua);

    let notif_group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();
    let notif = adw::SwitchRow::builder()
        .title("Notify when downloads finish")
        .build();
    settings
        .bind("show-notifications", &notif, "active")
        .build();
    notif_group.add(&notif);
    let bg_notif = adw::SwitchRow::builder()
        .title("Notify for background downloads")
        .subtitle("When closing with downloads still running")
        .build();
    settings
        .bind("notify-background", &bg_notif, "active")
        .build();
    notif_group.add(&bg_notif);

    let power_group = adw::PreferencesGroup::builder().title("Power").build();
    let inhibit = adw::SwitchRow::builder()
        .title("Prevent sleep during downloads")
        .subtitle("Block suspend while downloads are queued or running")
        .build();
    settings.bind("inhibit-suspend", &inhibit, "active").build();
    power_group.add(&inhibit);

    page.add(&dest_group);
    page.add(&net_group);
    page.add(&power_group);
    page.add(&notif_group);
    dialog.add(&page);
    dialog.present(Some(parent));
}
