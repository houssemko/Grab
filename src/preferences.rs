use adw::prelude::*;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::prelude::*;
use libadwaita as adw;

pub fn show(
    parent: &impl gtk4::glib::object::IsA<gtk4::Widget>,
    settings: &crate::settings::AppSettings,
) {
    let dialog = adw::PreferencesDialog::builder()
        .title(gettext("Preferences"))
        .build();

    let page = adw::PreferencesPage::builder()
        .title(gettext("Downloads"))
        .icon_name("folder-download-symbolic")
        .build();

    let dest_group = adw::PreferencesGroup::builder()
        .title(gettext("Destination"))
        .build();
    let current = settings.download_dir();
    let shown = if current.is_empty() {
        gettext("(System Downloads folder)")
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
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose download folder"))
        .valign(gtk4::Align::Center)
        .build();
    let reset_btn = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Use system default"))
        .valign(gtk4::Align::Center)
        .build();
    reset_btn.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Use system default",
    ))]);
    let dest_row = adw::ActionRow::builder()
        .title(gettext("Download folder"))
        .build();
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
                .title(gettext("Choose download folder"))
                .accept_label(gettext("Select Folder"))
                .build();
            let s2 = s.clone();
            let l2 = l.clone();
            chooser.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res
                    && let Some(p) = f.path()
                {
                    let dir = p.to_string_lossy().into_owned();
                    if s2
                        .set_string(crate::settings::key::DOWNLOAD_DIR, &dir)
                        .is_ok()
                    {
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
            if s.set_string(crate::settings::key::DOWNLOAD_DIR, "").is_ok() {
                l.set_text(&gettext("(System Downloads folder)"));
            }
        });
    }

    let net_page = adw::PreferencesPage::builder()
        .title(gettext("Network"))
        .icon_name("network-wired-symbolic")
        .build();

    let net_group = adw::PreferencesGroup::builder()
        .title(gettext("Network and Queue"))
        .build();

    let concurrent = adw::SpinRow::builder()
        .title(gettext("Simultaneous downloads"))
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 10.0, 1.0, 1.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::MAX_CONCURRENT, &concurrent, "value")
        .build();
    net_group.add(&concurrent);

    let connections = adw::SpinRow::builder()
        .title(gettext("Connections per download"))
        .subtitle(gettext(
            "Parallel connections for large files (1 = single stream)",
        ))
        .adjustment(&gtk4::Adjustment::new(4.0, 1.0, 16.0, 1.0, 1.0, 0.0))
        .build();
    connections.set_tooltip_text(Some(&gettext(
        "Files under ~16 MB always use one connection",
    )));
    settings
        .bind(crate::settings::key::CONNECTIONS, &connections, "value")
        .build();
    net_group.add(&connections);

    let limit = adw::EntryRow::builder()
        .title(gettext("Speed limit"))
        .build();
    limit.set_tooltip_text(Some(&gettext(
        "Per download, e.g. 500K, 2M; empty means unlimited",
    )));
    limit.set_input_purpose(gtk4::InputPurpose::FreeForm);
    settings
        .bind(crate::settings::key::SPEED_LIMIT, &limit, "text")
        .build();
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
        .title(gettext("Retries"))
        .adjustment(&gtk4::Adjustment::new(3.0, 1.0, 99.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::RETRIES, &retries, "value")
        .build();
    net_group.add(&retries);

    let timeout = adw::SpinRow::builder()
        .title(gettext("Timeout (seconds)"))
        .adjustment(&gtk4::Adjustment::new(30.0, 5.0, 300.0, 5.0, 30.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::TIMEOUT, &timeout, "value")
        .build();
    net_group.add(&timeout);

    let ua = adw::EntryRow::builder()
        .title(gettext("User agent (optional)"))
        .build();
    settings
        .bind(crate::settings::key::USER_AGENT, &ua, "text")
        .build();
    net_group.add(&ua);

    let notif_group = adw::PreferencesGroup::builder()
        .title(gettext("Notifications"))
        .build();
    let notif = adw::SwitchRow::builder()
        .title(gettext("Notify when downloads finish"))
        .build();
    settings
        .bind(crate::settings::key::SHOW_NOTIFICATIONS, &notif, "active")
        .build();
    notif_group.add(&notif);
    let bg_notif = adw::SwitchRow::builder()
        .title(gettext("Notify for background downloads"))
        .subtitle(gettext("When closing with downloads still running"))
        .build();
    settings
        .bind(crate::settings::key::NOTIFY_BACKGROUND, &bg_notif, "active")
        .build();
    notif_group.add(&bg_notif);

    let power_group = adw::PreferencesGroup::builder()
        .title(gettext("Power"))
        .build();
    let inhibit = adw::SwitchRow::builder()
        .title(gettext("Prevent sleep during downloads"))
        .subtitle(gettext(
            "Block suspend while downloads are queued or running",
        ))
        .build();
    settings
        .bind(crate::settings::key::INHIBIT_SUSPEND, &inhibit, "active")
        .build();
    power_group.add(&inhibit);

    page.add(&dest_group);
    page.add(&power_group);
    page.add(&notif_group);
    dialog.add(&page);

    net_page.add(&net_group);
    dialog.add(&net_page);

    let torrent_page = adw::PreferencesPage::builder()
        .title(gettext("Torrent"))
        .icon_name("emblem-shared-symbolic")
        .build();

    let share_group = adw::PreferencesGroup::builder()
        .title(gettext("Sharing"))
        .build();
    let seed = adw::SwitchRow::builder()
        .title(gettext("Seed finished downloads"))
        .subtitle(gettext("Keep sharing files after they finish downloading"))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_SEED_FINISHED, &seed, "active")
        .build();
    share_group.add(&seed);

    let torrent_net_group = adw::PreferencesGroup::builder()
        .title(gettext("Network"))
        .build();
    let dht = adw::SwitchRow::builder()
        .title(gettext("Use DHT"))
        .subtitle(gettext("Find peers through the distributed hash table"))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_DHT, &dht, "active")
        .build();
    torrent_net_group.add(&dht);
    let peers = adw::SpinRow::builder()
        .title(gettext("Peer limit"))
        .subtitle(gettext(
            "Maximum peers per download. 0 means unlimited. Applies when a download starts.",
        ))
        .adjustment(&gtk4::Adjustment::new(50.0, 0.0, 500.0, 1.0, 10.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_PEER_LIMIT, &peers, "value")
        .build();
    torrent_net_group.add(&peers);

    torrent_page.add(&share_group);
    torrent_page.add(&torrent_net_group);
    dialog.add(&torrent_page);
    dialog.present(Some(parent));
}
