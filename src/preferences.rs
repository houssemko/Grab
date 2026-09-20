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

    // Power-user knobs live on their own page so the main pages stay
    // approachable. Every advanced row keeps its safe default, so users
    // who never open this page still get the optimal behavior.
    let advanced_page = adw::PreferencesPage::builder()
        .title(gettext("Advanced"))
        .icon_name("applications-engineering-symbolic")
        .build();

    let advanced_general_group = adw::PreferencesGroup::builder()
        .title(gettext("General"))
        .build();
    let advanced_net_group = adw::PreferencesGroup::builder()
        .title(gettext("Network"))
        .build();
    let advanced_media_group = adw::PreferencesGroup::builder()
        .title(gettext("Media"))
        .build();
    let advanced_torrent_group = adw::PreferencesGroup::builder()
        .title(gettext("Torrent"))
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
    let restrict_filenames = adw::SwitchRow::builder()
        .title(gettext("Restrict filenames to ASCII"))
        .subtitle(gettext("Use only ASCII characters in filenames"))
        .build();
    settings
        .bind(
            crate::settings::key::RESTRICT_FILENAMES,
            &restrict_filenames,
            "active",
        )
        .build();
    advanced_general_group.add(&restrict_filenames);
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
        .subtitle(gettext("Parallel connections for large files"))
        .adjustment(&gtk4::Adjustment::new(4.0, 1.0, 16.0, 1.0, 1.0, 0.0))
        .build();
    connections.set_tooltip_text(Some(&gettext(
        "Files under ~16 MB always use one connection",
    )));
    settings
        .bind(crate::settings::key::CONNECTIONS, &connections, "value")
        .build();
    advanced_net_group.add(&connections);

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
    advanced_net_group.add(&retries);

    let retry_sleep = adw::SpinRow::builder()
        .title(gettext("Retry delay"))
        .subtitle(gettext("In seconds"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 60.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::RETRY_SLEEP, &retry_sleep, "value")
        .build();
    advanced_net_group.add(&retry_sleep);

    let sleep_interval = adw::SpinRow::builder()
        .title(gettext("Sleep interval"))
        .subtitle(gettext("In seconds"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 300.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::SLEEP_INTERVAL,
            &sleep_interval,
            "value",
        )
        .build();
    advanced_net_group.add(&sleep_interval);

    let max_sleep_interval = adw::SpinRow::builder()
        .title(gettext("Max sleep interval"))
        .subtitle(gettext("In seconds"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 300.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::MAX_SLEEP_INTERVAL,
            &max_sleep_interval,
            "value",
        )
        .build();
    // The upper bound only means anything alongside an active Sleep
    // interval (yt-dlp rejects --max-sleep-interval on its own), so the
    // row greys out while the minimum is 0.
    sleep_interval
        .bind_property("value", &max_sleep_interval, "sensitive")
        .transform_to(|_: &gtk4::glib::Binding, n: f64| Some(n > 0.0))
        .sync_create()
        .build();
    advanced_net_group.add(&max_sleep_interval);

    let sleep_requests = adw::SpinRow::builder()
        .title(gettext("Sleep between requests"))
        .subtitle(gettext("In seconds"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 300.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::SLEEP_REQUESTS,
            &sleep_requests,
            "value",
        )
        .build();
    advanced_net_group.add(&sleep_requests);

    let socket_timeout = adw::SpinRow::builder()
        .title(gettext("Socket timeout"))
        .subtitle(gettext("In seconds"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 300.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::SOCKET_TIMEOUT,
            &socket_timeout,
            "value",
        )
        .build();
    advanced_net_group.add(&socket_timeout);

    let timeout = adw::SpinRow::builder()
        .title(gettext("Timeout"))
        .subtitle(gettext("In seconds"))
        .adjustment(&gtk4::Adjustment::new(30.0, 5.0, 300.0, 5.0, 30.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::TIMEOUT, &timeout, "value")
        .build();
    advanced_net_group.add(&timeout);

    let throttled_rate = adw::SpinRow::builder()
        .title(gettext("Throttled rate"))
        .subtitle(gettext("In KB/s"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 10000.0, 10.0, 100.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::THROTTLED_RATE,
            &throttled_rate,
            "value",
        )
        .build();
    advanced_net_group.add(&throttled_rate);

    let extractor_retries = adw::SpinRow::builder()
        .title(gettext("Extractor retries"))
        .subtitle(gettext("0 uses the default of 3"))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 20.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::EXTRACTOR_RETRIES,
            &extractor_retries,
            "value",
        )
        .build();
    advanced_net_group.add(&extractor_retries);

    let ua = adw::EntryRow::builder()
        .title(gettext("User agent"))
        .build();
    ua.set_tooltip_text(Some(&gettext("Blank uses the default user agent")));
    settings
        .bind(crate::settings::key::USER_AGENT, &ua, "text")
        .build();
    advanced_net_group.add(&ua);

    let keep_date = adw::SwitchRow::builder()
        .title(gettext("Keep original file dates"))
        .subtitle(gettext("Use the server's date instead of download time"))
        .build();
    settings
        .bind(crate::settings::key::KEEP_SERVER_DATE, &keep_date, "active")
        .build();
    advanced_general_group.add(&keep_date);

    let proxy_mode = adw::ComboRow::builder()
        .title(gettext("Proxy"))
        .subtitle(gettext("Torrents follow SOCKS5 proxies only"))
        .build();
    {
        let labels = crate::download::proxy_mode_labels();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        proxy_mode.set_model(Some(&gtk4::StringList::new(&refs)));
    }
    proxy_mode.set_selected(crate::download::proxy_mode_index(&settings.proxy_mode()) as u32);
    net_group.add(&proxy_mode);
    {
        let row = proxy_mode.downgrade();
        settings.connect_changed(Some(crate::settings::key::PROXY_MODE), move |s, _| {
            if let Some(row) = row.upgrade() {
                row.set_selected(crate::download::proxy_mode_index(
                    &s.string(crate::settings::key::PROXY_MODE),
                ) as u32);
            }
        });
    }
    let proxy_group = adw::PreferencesGroup::builder()
        .title(gettext("Manual proxy"))
        .build();
    let proxy_type = adw::ComboRow::builder().title(gettext("Type")).build();
    {
        let labels = crate::download::proxy_type_labels();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        proxy_type.set_model(Some(&gtk4::StringList::new(&refs)));
    }
    proxy_type.set_selected(crate::download::proxy_type_index(&settings.proxy_type()) as u32);
    proxy_group.add(&proxy_type);
    {
        let row = proxy_type.downgrade();
        settings.connect_changed(Some(crate::settings::key::PROXY_TYPE), move |s, _| {
            if let Some(row) = row.upgrade() {
                row.set_selected(crate::download::proxy_type_index(
                    &s.string(crate::settings::key::PROXY_TYPE),
                ) as u32);
            }
        });
    }
    proxy_type.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::PROXY_TYPE,
                crate::download::proxy_type_value(row.selected() as usize),
            );
        }
    });
    let proxy_host = adw::EntryRow::builder().title(gettext("Host")).build();
    settings
        .bind(crate::settings::key::PROXY_HOST, &proxy_host, "text")
        .build();
    proxy_group.add(&proxy_host);
    let proxy_port = adw::SpinRow::builder()
        .title(gettext("Port"))
        .adjustment(&gtk4::Adjustment::new(9050.0, 1.0, 65535.0, 1.0, 10.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::PROXY_PORT, &proxy_port, "value")
        .build();
    proxy_group.add(&proxy_port);
    // The manual rows only exist in manual mode; the group hides
    // otherwise so System/Direct stay one clean row.
    fn sync_proxy_group(group: &gtk4::glib::WeakRef<adw::PreferencesGroup>, manual: bool) {
        if let Some(group) = group.upgrade() {
            group.set_visible(manual);
        }
    }
    {
        let group = proxy_group.downgrade();
        settings.connect_changed(Some(crate::settings::key::PROXY_MODE), move |s, _| {
            sync_proxy_group(
                &group,
                s.string(crate::settings::key::PROXY_MODE).as_str()
                    == crate::download::PROXY_MODE_MANUAL,
            );
        });
    }
    proxy_mode.connect_selected_notify({
        let s = settings.clone();
        let group = proxy_group.downgrade();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::PROXY_MODE,
                crate::download::proxy_mode_value(row.selected() as usize),
            );
            sync_proxy_group(&group, s.proxy_mode() == crate::download::PROXY_MODE_MANUAL);
        }
    });
    sync_proxy_group(
        &proxy_group.downgrade(),
        settings.proxy_mode() == crate::download::PROXY_MODE_MANUAL,
    );
    net_page.add(&proxy_group);

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
        .title(gettext("Background download notifications"))
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
            "Keep the computer awake while downloads are active",
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

    let torrent_page = adw::PreferencesPage::builder()
        .title(gettext("Torrent"))
        .icon_name("emblem-shared-symbolic")
        .build();

    let share_group = adw::PreferencesGroup::builder()
        .title(gettext("Sharing"))
        .build();
    let seed = adw::SwitchRow::builder()
        .title(gettext("Seed finished downloads"))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_SEED_FINISHED, &seed, "active")
        .build();
    share_group.add(&seed);
    let seed_ratio = adw::SpinRow::builder()
        .title(gettext("Seed to ratio"))
        .subtitle(gettext(
            "Stop seeding at this upload ratio. 0 means unlimited.",
        ))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 100.0, 0.1, 1.0, 0.0))
        .digits(1)
        .build();
    settings
        .bind(
            crate::settings::key::TORRENT_SEED_RATIO,
            &seed_ratio,
            "value",
        )
        .build();
    seed.bind_property("active", &seed_ratio, "sensitive")
        .sync_create()
        .build();
    share_group.add(&seed_ratio);
    let seed_time = adw::SpinRow::builder()
        .title(gettext("Seed time limit"))
        .subtitle(gettext(
            "Stop seeding this many minutes after finishing. 0 means unlimited.",
        ))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 43200.0, 5.0, 60.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_SEED_TIME, &seed_time, "value")
        .build();
    seed.bind_property("active", &seed_time, "sensitive")
        .sync_create()
        .build();
    share_group.add(&seed_time);

    let torrent_net_group = adw::PreferencesGroup::builder()
        .title(gettext("Connectivity"))
        .build();
    let dht = adw::SwitchRow::builder()
        .title(gettext("Use DHT"))
        .subtitle(gettext(
            "Find peers without trackers. Takes effect on restart.",
        ))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_DHT, &dht, "active")
        .build();
    advanced_torrent_group.add(&dht);
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
    advanced_torrent_group.add(&peers);
    let upload_limit = adw::EntryRow::builder()
        .title(gettext("Upload speed limit"))
        .build();
    upload_limit.set_tooltip_text(Some(&gettext(
        "Per torrent, e.g. 500K, 2M; empty means unlimited",
    )));
    upload_limit.set_input_purpose(gtk4::InputPurpose::FreeForm);
    settings
        .bind(
            crate::settings::key::TORRENT_UPLOAD_LIMIT,
            &upload_limit,
            "text",
        )
        .build();
    // Flag junk immediately instead of failing rows at spawn time.
    {
        let l = upload_limit.clone();
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
        mark(&upload_limit);
        upload_limit.connect_changed(mark);
    }
    torrent_net_group.add(&upload_limit);
    let trackers = adw::EntryRow::builder()
        .title(gettext("Extra trackers"))
        .tooltip_text(gettext(
            "Comma-separated tracker URLs added to every download",
        ))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_TRACKERS, &trackers, "text")
        .build();
    advanced_torrent_group.add(&trackers);
    let listen_port = adw::SpinRow::builder()
        .title(gettext("Listen port"))
        .subtitle(gettext(
            "Port for incoming connections. 0 means disabled. Applies when the torrent engine first starts.",
        ))
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 65535.0, 1.0, 100.0, 0.0))
        .build();
    settings
        .bind(
            crate::settings::key::TORRENT_LISTEN_PORT,
            &listen_port,
            "value",
        )
        .build();
    advanced_torrent_group.add(&listen_port);
    let upnp = adw::SwitchRow::builder()
        .title(gettext("UPnP port forwarding"))
        .subtitle(gettext(
            "Ask your router to forward the listen port. Applies when the torrent engine first starts.",
        ))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_UPNP, &upnp, "active")
        .build();
    advanced_torrent_group.add(&upnp);

    torrent_page.add(&torrent_net_group);

    // Video pages resolve through the yt-dlp support tools; this page
    // holds the defaults new video downloads start from, plus tool setup.
    // Runs `binary --version` and reports `None` when it fails.
    fn tool_version(binary: &std::path::Path, version_arg: &str) -> Option<String> {
        let out = std::process::Command::new(binary)
            .arg(version_arg)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let first = String::from_utf8(out.stdout)
            .ok()?
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if first.is_empty() {
            return None;
        }
        // ffmpeg prints a whole sentence ("ffmpeg version n9.0.1 ..."):
        // keep the version token so the row stays readable.
        if binary
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == "ffmpeg")
            && let Some(token) = first.split_whitespace().nth(2)
        {
            return Some(format!("ffmpeg {token}"));
        }
        Some(first)
    }
    /// What the tools-row button does: Install when tools are missing,
    /// Check to probe GitHub for a newer yt-dlp, Update once one is
    /// known. The check-then-act shape keeps a permanent Update button
    /// off fresh installs while leaving on-demand updates one click
    /// away (yt-dlp's pace makes them genuinely useful).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ToolAction {
        Install,
        Check,
        Update,
    }
    fn paint_tools_state(
        row: &adw::ActionRow,
        btn: &gtk4::Button,
        action: &std::rc::Rc<std::cell::Cell<ToolAction>>,
        probed: Option<(String, String)>,
    ) {
        match probed {
            Some((yt, ff)) => {
                row.set_subtitle(
                    &gettext("Ready • {yt} • {ff}")
                        .replace("{yt}", &yt)
                        .replace("{ff}", &ff),
                );
                btn.set_label(&gettext("Check for Updates"));
                action.set(ToolAction::Check);
            }
            None => {
                row.set_subtitle(&gettext("Not installed"));
                btn.set_label(&gettext("Install"));
                action.set(ToolAction::Install);
            }
        }
    }
    fn refresh_video_tools(
        row: &adw::ActionRow,
        btn: &gtk4::Button,
        spin: &gtk4::Spinner,
        action: &std::rc::Rc<std::cell::Cell<ToolAction>>,
    ) {
        spin.stop();
        spin.set_visible(false);
        paint_tools_state(
            row,
            btn,
            action,
            crate::video::resolve_libraries().ok().map(|libs| {
                let yt = tool_version(&libs.youtube, "--version")
                    .unwrap_or_else(|| libs.youtube.display().to_string());
                let ff = tool_version(&libs.ffmpeg, "-version")
                    .unwrap_or_else(|| libs.ffmpeg.display().to_string());
                (yt, ff)
            }),
        );
    }
    let video_page = adw::PreferencesPage::builder()
        .title(gettext("Media"))
        .icon_name("video-x-generic-symbolic")
        .build();
    let video_quality_group = adw::PreferencesGroup::builder()
        .title(gettext("Quality"))
        .build();
    let video_labels = crate::video::quality_labels();
    let video_refs: Vec<&str> = video_labels.iter().map(String::as_str).collect();
    let video_quality = adw::ComboRow::builder()
        .title(gettext("Preferred quality"))
        .subtitle(gettext("Used for new media downloads"))
        .model(&gtk4::StringList::new(&video_refs))
        .build();
    video_quality.set_selected(crate::video::quality_index(&settings.video_quality()) as u32);
    video_quality_group.add(&video_quality);
    // ComboRow holds an index, GSettings a string: sync both ways by hand
    // (weak on the settings side so closed dialogs don't leak).
    {
        let row = video_quality.downgrade();
        settings.connect_changed(Some(crate::settings::key::VIDEO_QUALITY), move |s, _| {
            if let Some(row) = row.upgrade() {
                row.set_selected(crate::video::quality_index(
                    &s.string(crate::settings::key::VIDEO_QUALITY),
                ) as u32);
            }
        });
    }
    video_quality.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::VIDEO_QUALITY,
                crate::video::quality_value(row.selected() as usize),
            );
        }
    });
    let codec_labels = crate::video::codec_priority_labels();
    let codec_refs: Vec<&str> = codec_labels.iter().map(String::as_str).collect();
    let video_codec = adw::ComboRow::builder()
        .title(gettext("Preferred video codec"))
        .subtitle(gettext("Newest codecs first, or widest playback"))
        .model(&gtk4::StringList::new(&codec_refs))
        .build();
    video_codec
        .set_selected(crate::video::codec_priority_index(&settings.video_codec_priority()) as u32);
    advanced_media_group.add(&video_codec);
    {
        let row = video_codec.downgrade();
        settings.connect_changed(
            Some(crate::settings::key::VIDEO_CODEC_PRIORITY),
            move |s, _| {
                if let Some(row) = row.upgrade() {
                    row.set_selected(crate::video::codec_priority_index(
                        &s.string(crate::settings::key::VIDEO_CODEC_PRIORITY),
                    ) as u32);
                }
            },
        );
    }
    video_codec.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::VIDEO_CODEC_PRIORITY,
                crate::video::codec_priority_value(row.selected() as usize),
            );
        }
    });
    let audio_quality = adw::SpinRow::builder()
        .title(gettext("Audio quality"))
        .subtitle(gettext("0 is best, 10 is worst"))
        .adjustment(&gtk4::Adjustment::new(5.0, 0.0, 10.0, 1.0, 5.0, 0.0))
        .build();
    settings
        .bind(crate::settings::key::AUDIO_QUALITY, &audio_quality, "value")
        .build();
    advanced_media_group.add(&audio_quality);
    let subtitle_labels = crate::video::subtitle_language_labels();
    let subtitle_refs: Vec<&str> = subtitle_labels.iter().map(String::as_str).collect();
    let subtitle_lang = adw::ComboRow::builder()
        .title(gettext("Subtitles"))
        .subtitle(gettext("Download subtitles beside the video"))
        .model(&gtk4::StringList::new(&subtitle_refs))
        .build();
    subtitle_lang
        .set_selected(crate::video::subtitle_language_index(&settings.subtitle_language()) as u32);
    video_quality_group.add(&subtitle_lang);
    {
        let row = subtitle_lang.downgrade();
        settings.connect_changed(
            Some(crate::settings::key::SUBTITLE_LANGUAGE),
            move |s, _| {
                if let Some(row) = row.upgrade() {
                    row.set_selected(crate::video::subtitle_language_index(
                        &s.string(crate::settings::key::SUBTITLE_LANGUAGE),
                    ) as u32);
                }
            },
        );
    }
    subtitle_lang.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::SUBTITLE_LANGUAGE,
                crate::video::subtitle_language_value(row.selected() as usize),
            );
        }
    });
    let video_auth_group = adw::PreferencesGroup::builder()
        .title(gettext("Authentication"))
        .description(gettext("Age gates and member-only pages"))
        .build();
    let browser_labels = crate::video::cookies_browser_labels();
    let browser_refs: Vec<&str> = browser_labels.iter().map(String::as_str).collect();
    let browser_row = adw::ComboRow::builder()
        .title(gettext("Cookies from Browser"))
        .subtitle(gettext("Reads this browser's profile directly"))
        .model(&gtk4::StringList::new(&browser_refs))
        .build();
    browser_row
        .set_selected(crate::video::cookies_browser_index(&settings.cookies_browser()) as u32);
    video_auth_group.add(&browser_row);
    {
        let row = browser_row.downgrade();
        settings.connect_changed(Some(crate::settings::key::COOKIES_BROWSER), move |s, _| {
            if let Some(row) = row.upgrade() {
                let selected = crate::video::cookies_browser_index(
                    &s.string(crate::settings::key::COOKIES_BROWSER),
                ) as u32;
                row.set_selected(selected);
            }
        });
    }
    browser_row.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::COOKIES_BROWSER,
                crate::video::cookies_browser_value(row.selected() as usize),
            );
        }
    });
    let video_post_group = adw::PreferencesGroup::builder()
        .title(gettext("Post-processing"))
        .description(gettext("Applied while finishing downloads"))
        .build();
    let embed_subs = adw::SwitchRow::builder()
        .title(gettext("Embed subtitles"))
        .subtitle(gettext("Mux downloaded subtitles into the video file"))
        .build();
    settings
        .bind(crate::settings::key::EMBED_SUBS, &embed_subs, "active")
        .build();
    video_post_group.add(&embed_subs);
    let sponsorblock = adw::SwitchRow::builder()
        .title(gettext("Remove sponsored segments"))
        .subtitle(gettext("Cut SponsorBlock-flagged sponsor segments"))
        .build();
    settings
        .bind(
            crate::settings::key::SPONSORBLOCK_REMOVE,
            &sponsorblock,
            "active",
        )
        .build();
    video_post_group.add(&sponsorblock);
    let sponsorblock_mark = adw::SwitchRow::builder()
        .title(gettext("Mark sponsored segments"))
        .subtitle(gettext(
            "Tag SponsorBlock-flagged sponsor segments as chapters",
        ))
        .build();
    settings
        .bind(
            crate::settings::key::SPONSORBLOCK_MARK,
            &sponsorblock_mark,
            "active",
        )
        .build();
    // Removing segments makes marking them moot: grey the Mark row out
    // while Remove is on.
    sponsorblock
        .bind_property("active", &sponsorblock_mark, "sensitive")
        .invert_boolean()
        .sync_create()
        .build();
    video_post_group.add(&sponsorblock_mark);
    let embed_thumbnail = adw::SwitchRow::builder()
        .title(gettext("Embed thumbnail"))
        .subtitle(gettext("Attach the video thumbnail as cover art"))
        .build();
    settings
        .bind(
            crate::settings::key::EMBED_THUMBNAIL,
            &embed_thumbnail,
            "active",
        )
        .build();
    video_post_group.add(&embed_thumbnail);
    let embed_chapters = adw::SwitchRow::builder()
        .title(gettext("Embed chapters"))
        .subtitle(gettext("Write chapter markers into the finished file"))
        .build();
    settings
        .bind(
            crate::settings::key::EMBED_CHAPTERS,
            &embed_chapters,
            "active",
        )
        .build();
    video_post_group.add(&embed_chapters);
    // ComboRow holds an index, GSettings a string: sync both ways by hand
    // (same pattern as the subtitle language row).
    let remux_labels = crate::video::remux_video_labels();
    let remux_refs: Vec<&str> = remux_labels.iter().map(String::as_str).collect();
    let remux_row = adw::ComboRow::builder()
        .title(gettext("Remux video"))
        .subtitle(gettext(
            "Change the finished file's container without re-encoding",
        ))
        .model(&gtk4::StringList::new(&remux_refs))
        .build();
    remux_row.set_selected(crate::video::remux_video_index(&settings.remux_video()) as u32);
    video_post_group.add(&remux_row);
    {
        let row = remux_row.downgrade();
        settings.connect_changed(Some(crate::settings::key::REMUX_VIDEO), move |s, _| {
            if let Some(row) = row.upgrade() {
                row.set_selected(crate::video::remux_video_index(
                    &s.string(crate::settings::key::REMUX_VIDEO),
                ) as u32);
            }
        });
    }
    remux_row.connect_selected_notify({
        let s = settings.clone();
        move |row| {
            let _ = s.set_string(
                crate::settings::key::REMUX_VIDEO,
                crate::video::remux_video_value(row.selected() as usize),
            );
        }
    });
    let video_live_group = adw::PreferencesGroup::builder()
        .title(gettext("Live"))
        .description(gettext("Live stream recording"))
        .build();
    let live_from_start = adw::SwitchRow::builder()
        .title(gettext("Live from start"))
        .subtitle(gettext("Record live streams from the beginning"))
        .build();
    settings
        .bind(
            crate::settings::key::LIVE_FROM_START,
            &live_from_start,
            "active",
        )
        .build();
    video_live_group.add(&live_from_start);
    let video_tools_group = adw::PreferencesGroup::builder()
        .title(gettext("Support tools"))
        .description(gettext("yt-dlp and ffmpeg resolve media pages"))
        .build();
    let video_tools_row = adw::ActionRow::builder()
        .title(gettext("Media support tools"))
        .build();
    let video_tools_btn = gtk4::Button::builder().valign(gtk4::Align::Center).build();
    video_tools_row.set_activatable_widget(Some(&video_tools_btn));
    video_tools_row.add_suffix(&video_tools_btn);
    let video_tools_spin = gtk4::Spinner::new();
    video_tools_spin.set_visible(false);
    video_tools_row.add_suffix(&video_tools_spin);
    video_tools_group.add(&video_tools_row);
    // Probe off the main thread: spawning cold binaries can jank startup.
    // The row shows Checking until versions (or absence) resolve.
    let tool_action = std::rc::Rc::new(std::cell::Cell::new(ToolAction::Check));
    {
        let (row, btn, spin) = (
            video_tools_row.clone(),
            video_tools_btn.clone(),
            video_tools_spin.clone(),
        );
        let dialog_weak = dialog.downgrade();
        let action = tool_action.clone();
        video_tools_row.set_subtitle(&gettext("Checking…"));
        video_tools_btn.set_sensitive(false);
        video_tools_spin.set_visible(true);
        video_tools_spin.start();
        gtk4::glib::spawn_future_local(async move {
            let probed = gio::spawn_blocking(|| {
                crate::video::resolve_libraries().ok().map(|libs| {
                    let yt = tool_version(&libs.youtube, "--version")
                        .unwrap_or_else(|| libs.youtube.display().to_string());
                    let ff = tool_version(&libs.ffmpeg, "-version")
                        .unwrap_or_else(|| libs.ffmpeg.display().to_string());
                    (yt, ff)
                })
            })
            .await
            .ok()
            .flatten();
            if dialog_weak.upgrade().is_none() {
                return;
            }
            spin.stop();
            spin.set_visible(false);
            paint_tools_state(&row, &btn, &action, probed);
            btn.set_sensitive(true);
        });
    }
    {
        let (row, btn, spin) = (
            video_tools_row.clone(),
            video_tools_btn.clone(),
            video_tools_spin.clone(),
        );
        let dialog_weak = dialog.downgrade();
        let action = tool_action.clone();
        // Outside Flatpak the button guides through self-install; the
        // automatic download stays Flatpak-only.
        if !crate::video::in_flatpak() {
            btn.set_label(&gettext("How to Install"));
            btn.set_tooltip_text(Some(&gettext("Show terminal install instructions")));
        }
        let settings_b = settings.clone();
        video_tools_btn.connect_clicked(move |_| {
            if !crate::video::in_flatpak() {
                let (row_b, btn_b, spin_b) = (row.clone(), btn.clone(), spin.clone());
                let dialog_b = dialog_weak.clone();
                let action_b = action.clone();
                crate::install_help::show(&btn, move || {
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    refresh_video_tools(&row_b, &btn_b, &spin_b, &action_b);
                });
                return;
            }
            if action.get() == ToolAction::Check {
                // One user-initiated probe: compare the installed yt-dlp
                // against the latest GitHub tag, then morph into Update
                // only when something newer exists.
                let (row_b, btn_b, spin_b) = (row.clone(), btn.clone(), spin.clone());
                let dialog_b = dialog_weak.clone();
                let action_b = action.clone();
                let settings_c = settings_b.clone();
                btn.set_sensitive(false);
                spin.set_visible(true);
                spin.start();
                row.set_subtitle(&gettext("Checking for updates…"));
                gtk4::glib::spawn_future_local(async move {
                    // The update probe cannot go through the proxy, so a
                    // proxied check would leak the machine IP to GitHub:
                    // skip loudly instead of checking direct.
                    let proxied = crate::download::DownloadOptions::from_settings(&settings_c)
                        .proxy_config()
                        .map(|p| p.is_some())
                        .unwrap_or(false);
                    if proxied {
                        if dialog_b.upgrade().is_none() {
                            return;
                        }
                        spin_b.stop();
                        spin_b.set_visible(false);
                        row_b.set_subtitle(&gettext(
                            "Update checks are skipped while a proxy is configured",
                        ));
                        btn_b.set_sensitive(true);
                        return;
                    }
                    let current = gio::spawn_blocking(|| {
                        crate::video::resolve_libraries()
                            .ok()
                            .and_then(|libs| tool_version(&libs.youtube, "--version"))
                    })
                    .await
                    .ok()
                    .flatten();
                    let tag = crate::video::latest_ytdlp_tag().await;
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    spin_b.stop();
                    spin_b.set_visible(false);
                    match (current, tag) {
                        (Some(installed), Some(tag))
                            if crate::video::ytdlp_update_available(&installed, &tag) =>
                        {
                            row_b.set_subtitle(
                                &gettext("Update available: {installed} → {latest}")
                                    .replace("{installed}", installed.trim())
                                    .replace("{latest}", tag.trim()),
                            );
                            btn_b.set_label(&gettext("Update"));
                            action_b.set(ToolAction::Update);
                        }
                        (Some(installed), Some(_)) => {
                            let yt = installed.trim().to_string();
                            let ff = gio::spawn_blocking(|| {
                                crate::video::resolve_libraries()
                                    .ok()
                                    .and_then(|libs| tool_version(&libs.ffmpeg, "-version"))
                            })
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                            if dialog_b.upgrade().is_none() {
                                return;
                            }
                            row_b.set_subtitle(
                                &gettext("Up to date • {yt} • {ff}")
                                    .replace("{yt}", &yt)
                                    .replace("{ff}", &ff),
                            );
                            btn_b.set_label(&gettext("Check for Updates"));
                            action_b.set(ToolAction::Check);
                        }
                        _ => {
                            row_b.set_subtitle(&gettext("Couldn't check for updates"));
                            btn_b.set_label(&gettext("Check for Updates"));
                            action_b.set(ToolAction::Check);
                        }
                    }
                    btn_b.set_sensitive(true);
                });
                return;
            }
            btn.set_sensitive(false);
            let pop_label = gtk4::Label::new(Some(&gettext("Downloading yt-dlp (1 of 2)…")));
            pop_label.set_wrap(true);
            pop_label.set_max_width_chars(30);
            let pop_spin = gtk4::Spinner::new();
            pop_spin.start();
            let pop_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
            pop_box.set_margin_top(18);
            pop_box.set_margin_bottom(18);
            pop_box.set_margin_start(18);
            pop_box.set_margin_end(18);
            pop_box.append(&pop_spin);
            pop_box.append(&pop_label);
            let pop = gtk4::Popover::new();
            pop.set_child(Some(&pop_box));
            pop.set_parent(&btn);
            pop.popup();
            let (row_b, btn_b, spin_b, pop_b, label_b) = (
                row.clone(),
                btn.clone(),
                spin.clone(),
                pop.clone(),
                pop_label.clone(),
            );
            let dialog_b = dialog_weak.clone();
            let action_b = action.clone();
            gtk4::glib::spawn_future_local(async move {
                if let Err(e) = crate::video::install_ytdlp().await {
                    pop_b.popdown();
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    row_b.set_subtitle(&e.to_string());
                    btn_b.set_sensitive(true);
                    return;
                }
                label_b.set_text(&gettext("Downloading ffmpeg (2 of 2)…"));
                if let Err(e) = crate::video::install_ffmpeg().await {
                    pop_b.popdown();
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    row_b.set_subtitle(&e.to_string());
                    btn_b.set_sensitive(true);
                    return;
                }
                pop_b.popdown();
                if dialog_b.upgrade().is_none() {
                    return;
                }
                refresh_video_tools(&row_b, &btn_b, &spin_b, &action_b);
                btn_b.set_sensitive(true);
            });
        });
    }
    video_page.add(&video_quality_group);
    video_page.add(&video_auth_group);
    video_page.add(&video_tools_group);
    dialog.add(&video_page);
    // In-page group order: the General/Network/Media/Torrent taxonomy,
    // with each family's related groups adjacent (Media keeps
    // Post-processing and Live; Torrent keeps Sharing).
    advanced_page.add(&advanced_general_group);
    advanced_page.add(&advanced_net_group);
    advanced_page.add(&advanced_media_group);
    advanced_page.add(&video_post_group);
    advanced_page.add(&video_live_group);
    advanced_page.add(&share_group);
    advanced_page.add(&advanced_torrent_group);
    // Page order: Downloads, Media, Torrent, Network, Advanced (Advanced last).
    dialog.add(&torrent_page);
    dialog.add(&net_page);
    dialog.add(&advanced_page);
    dialog.present(Some(parent));
}
