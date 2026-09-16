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

    let keep_date = adw::SwitchRow::builder()
        .title(gettext("Keep server file date"))
        .subtitle(gettext("Use the Last-Modified header for finished files"))
        .build();
    settings
        .bind(crate::settings::key::KEEP_SERVER_DATE, &keep_date, "active")
        .build();
    net_group.add(&keep_date);

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
    let seed_ratio = adw::SpinRow::builder()
        .title(gettext("Seed to ratio"))
        .subtitle(gettext(
            "Stop seeding after uploading this multiple of the download size. 0 means unlimited.",
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
        .title(gettext("Seed for (minutes)"))
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
        .title(gettext("Network"))
        .build();
    let dht = adw::SwitchRow::builder()
        .title(gettext("Use DHT"))
        .subtitle(gettext(
            "Find peers through the distributed hash table. Applies after restart.",
        ))
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
    let trackers = adw::EntryRow::builder()
        .title(gettext("Extra trackers"))
        .tooltip_text(gettext(
            "Comma-separated tracker URLs added to every download",
        ))
        .build();
    settings
        .bind(crate::settings::key::TORRENT_TRACKERS, &trackers, "text")
        .build();
    torrent_net_group.add(&trackers);
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
    torrent_net_group.add(&listen_port);

    torrent_page.add(&share_group);
    torrent_page.add(&torrent_net_group);
    dialog.add(&torrent_page);

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
    fn refresh_video_tools(row: &adw::ActionRow, btn: &gtk4::Button, spin: &gtk4::Spinner) {
        spin.stop();
        spin.set_visible(false);
        let probed = crate::video::resolve_libraries().ok().map(|libs| {
            let yt = tool_version(&libs.youtube, "--version")
                .unwrap_or_else(|| libs.youtube.display().to_string());
            let ff = tool_version(&libs.ffmpeg, "-version")
                .unwrap_or_else(|| libs.ffmpeg.display().to_string());
            (yt, ff)
        });
        match probed {
            Some((yt, ff)) => {
                row.set_subtitle(
                    &gettext("Ready • {yt} • {ff}")
                        .replace("{yt}", &yt)
                        .replace("{ff}", &ff),
                );
                btn.set_label(&gettext("Update"));
            }
            None => {
                row.set_subtitle(&gettext("Not installed"));
                btn.set_label(&gettext("Install"));
            }
        }
    }
    let video_page = adw::PreferencesPage::builder()
        .title(gettext("Video"))
        .icon_name("video-x-generic-symbolic")
        .build();
    let video_quality_group = adw::PreferencesGroup::builder()
        .title(gettext("Quality"))
        .build();
    let video_labels = crate::video::quality_labels();
    let video_refs: Vec<&str> = video_labels.iter().map(String::as_str).collect();
    let video_quality = adw::ComboRow::builder()
        .title(gettext("Preferred quality"))
        .subtitle(gettext("Used for new video downloads"))
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
    let video_audio = adw::SwitchRow::builder()
        .title(gettext("Audio only"))
        .subtitle(gettext("New video downloads skip the video track"))
        .build();
    settings
        .bind(
            crate::settings::key::VIDEO_AUDIO_ONLY,
            &video_audio,
            "active",
        )
        .build();
    video_quality_group.add(&video_audio);
    // Audio-only makes the quality row moot.
    video_audio.connect_active_notify({
        let row = video_quality.clone();
        move |sw| row.set_sensitive(!sw.is_active())
    });
    video_quality.set_sensitive(!video_audio.is_active());
    let video_auth_group = adw::PreferencesGroup::builder()
        .title(gettext("Authentication"))
        .description(gettext("Age gates and member-only pages"))
        .build();
    let cookies_current = settings.cookies_path();
    let cookies_label = gtk4::Label::builder()
        .label(if cookies_current.is_empty() {
            gettext("(None)")
        } else {
            cookies_current.clone()
        })
        .css_classes(["dimmed", "caption"])
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .hexpand(true)
        .halign(gtk4::Align::Start)
        .build();
    let cookies_btn = gtk4::Button::builder()
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose cookies file"))
        .valign(gtk4::Align::Center)
        .build();
    let cookies_clear_btn = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Clear cookies file"))
        .valign(gtk4::Align::Center)
        .build();
    cookies_clear_btn.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Clear cookies file",
    ))]);
    let cookies_row = adw::ActionRow::builder()
        .title(gettext("Cookies file"))
        .build();
    cookies_row.add_suffix(&cookies_label);
    cookies_row.add_suffix(&cookies_clear_btn);
    cookies_row.add_suffix(&cookies_btn);
    video_auth_group.add(&cookies_row);
    {
        let s = settings.clone();
        let (l, row) = (cookies_label.clone(), cookies_row.clone());
        let root = parent.root().and_downcast::<gtk4::Window>();
        cookies_btn.connect_clicked(move |_| {
            let filter_text = gtk4::FileFilter::new();
            filter_text.set_name(Some(&gettext("Text files")));
            filter_text.add_mime_type("text/plain");
            filter_text.add_pattern("*.txt");
            let filter_all = gtk4::FileFilter::new();
            filter_all.set_name(Some(&gettext("All files")));
            filter_all.add_pattern("*");
            let filters = gio::ListStore::new::<gtk4::FileFilter>();
            filters.append(&filter_text);
            filters.append(&filter_all);
            let chooser = gtk4::FileDialog::builder()
                .title(gettext("Choose cookies file"))
                .accept_label(gettext("Select File"))
                .filters(&filters)
                .build();
            let (s2, l2, row2) = (s.clone(), l.clone(), row.clone());
            chooser.open(root.as_ref(), gio::Cancellable::NONE, move |res| {
                let path = res
                    .ok()
                    .and_then(|f| f.path())
                    .map(|p| p.to_string_lossy().into_owned());
                let valid = path
                    .as_deref()
                    .is_some_and(crate::video::valid_cookies_file);
                if valid
                    && let Some(dir) = path
                    && s2
                        .set_string(crate::settings::key::COOKIES_PATH, &dir)
                        .is_ok()
                {
                    l2.set_text(&dir);
                    row2.set_subtitle("");
                    row2.remove_css_class("error");
                } else {
                    row2.set_subtitle(&gettext("Could not read that file"));
                    row2.add_css_class("error");
                }
            });
        });
    }
    {
        let s = settings.clone();
        let (l, row) = (cookies_label.clone(), cookies_row.clone());
        cookies_clear_btn.connect_clicked(move |_| {
            if s.set_string(crate::settings::key::COOKIES_PATH, "").is_ok() {
                l.set_text(&gettext("(None)"));
                row.set_subtitle("");
                row.remove_css_class("error");
            }
        });
    }
    let video_tools_group = adw::PreferencesGroup::builder()
        .title(gettext("Support tools"))
        .description(gettext("yt-dlp and ffmpeg resolve video pages"))
        .build();
    let video_tools_row = adw::ActionRow::builder()
        .title(gettext("Video support tools"))
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
    {
        let (row, btn, spin) = (
            video_tools_row.clone(),
            video_tools_btn.clone(),
            video_tools_spin.clone(),
        );
        let dialog_weak = dialog.downgrade();
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
            match probed {
                Some((yt, ff)) => {
                    row.set_subtitle(
                        &gettext("Ready • {yt} • {ff}")
                            .replace("{yt}", &yt)
                            .replace("{ff}", &ff),
                    );
                    btn.set_label(&gettext("Update"));
                }
                None => {
                    row.set_subtitle(&gettext("Not installed"));
                    btn.set_label(&gettext("Install"));
                }
            }
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
        video_tools_btn.connect_clicked(move |_| {
            btn.set_sensitive(false);
            spin.set_visible(true);
            spin.start();
            row.set_subtitle(&gettext("Installing support tools…"));
            let (row_b, btn_b, spin_b) = (row.clone(), btn.clone(), spin.clone());
            let dialog_b = dialog_weak.clone();
            gtk4::glib::spawn_future_local(async move {
                match crate::video::install_libraries().await {
                    Ok(_) => {
                        if dialog_b.upgrade().is_none() {
                            return;
                        }
                        refresh_video_tools(&row_b, &btn_b, &spin_b);
                    }
                    Err(e) => {
                        if dialog_b.upgrade().is_none() {
                            return;
                        }
                        spin_b.stop();
                        spin_b.set_visible(false);
                        row_b.set_subtitle(&e.to_string());
                    }
                }
                btn_b.set_sensitive(true);
            });
        });
    }
    video_page.add(&video_quality_group);
    video_page.add(&video_auth_group);
    video_page.add(&video_tools_group);
    dialog.add(&video_page);
    dialog.present(Some(parent));
}
