use crate::download::{DownloadManager, DownloadStatus};
use adw::prelude::*;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub const BACKGROUND_NOTIF_ID: &str = "grab-background";

fn icon_button(icon: &str, tooltip: &str) -> gtk4::Button {
    let b = gtk4::Button::builder()
        .icon_name(icon)
        .css_classes(["flat"])
        .tooltip_text(tooltip)
        .valign(gtk4::Align::Center)
        .build();
    b.update_property(&[gtk4::accessible::Property::Label(tooltip)]);
    b
}

/// Open `path` in the file manager (`reveal` shows the containing folder
/// with the file selected instead of opening the folder itself).
pub fn launch_path(path: &std::path::Path, toasts: &adw::ToastOverlay, reveal: bool) {
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
    let t = toasts.clone();
    let what = path.to_string_lossy().into_owned();
    glib::spawn_future_local(async move {
        let res = if reveal {
            launcher
                .open_containing_folder_future(None::<&gtk4::Window>)
                .await
        } else {
            launcher.launch_future(None::<&gtk4::Window>).await
        };
        if let Err(e) = res {
            let verb = if reveal { "show" } else { "open" };
            t.add_toast(adw::Toast::new(&format!(
                "Could not {verb} {what} in the file manager: {e}"
            )));
        }
    });
}

struct RowWidgets {
    detail: gtk4::Label,
    progress: gtk4::ProgressBar,
    spinner: adw::Spinner,
    toggle_btn: gtk4::Button,
    stop_btn: gtk4::Button,
    queue_btn: gtk4::Button,
    retry_btn: gtk4::Button,
    reveal_btn: gtk4::Button,
    delete_btn: gtk4::Button,
}

/// Whether deferring `item` could hand its slot to someone: another row is
/// waiting queued. With nothing waiting the button would just stop and
/// immediately restart the same download. O(1) off the cached count, so
/// progress ticks can call it freely.
fn another_queued(manager: &DownloadManager, item: &crate::download::DownloadItem) -> bool {
    let n = manager.queued_count();
    n > 1 || (n == 1 && item.status() != DownloadStatus::Queued)
}

fn refresh_row(item: &crate::download::DownloadItem, w: &RowWidgets, defer_available: bool) {
    let frac = item.progress().clamp(0.0, 1.0);
    w.progress.set_fraction(frac);
    let active = item.status() == DownloadStatus::Downloading;
    w.spinner.set_visible(active);
    w.detail.set_text(&item.detail());

    let running = matches!(
        item.status(),
        DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
    );
    w.toggle_btn.set_visible(running);
    w.stop_btn.set_visible(running);
    // Deferring only makes sense while holding a slot that someone else
    // is waiting for; queued rows are already waiting.
    w.queue_btn.set_visible(
        defer_available
            && matches!(
                item.status(),
                DownloadStatus::Downloading | DownloadStatus::Paused
            ),
    );
    // Failed rows otherwise strand: bulk retry lives in the menu/banner,
    // but a single failure deserves its own button.
    w.retry_btn.set_visible(matches!(
        item.status(),
        DownloadStatus::Failed | DownloadStatus::Cancelled
    ));
    let done = item.status() == DownloadStatus::Done;
    w.reveal_btn.set_visible(done);
    w.delete_btn.set_visible(done);

    if item.status() == DownloadStatus::Paused {
        w.toggle_btn.set_icon_name("media-playback-start-symbolic");
        w.toggle_btn.set_tooltip_text(Some("Resume"));
        w.toggle_btn
            .update_property(&[gtk4::accessible::Property::Label("Resume")]);
    } else {
        w.toggle_btn.set_icon_name("media-playback-pause-symbolic");
        w.toggle_btn.set_tooltip_text(Some("Pause"));
        w.toggle_btn
            .update_property(&[gtk4::accessible::Property::Label("Pause")]);
    }
}

fn build_row(
    item: &crate::download::DownloadItem,
    manager: &Rc<DownloadManager>,
    toasts: &Rc<adw::ToastOverlay>,
) -> gtk4::ListBoxRow {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);

    let name = gtk4::Label::builder()
        .label(item.filename())
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(["heading"])
        .build();
    let status = gtk4::Label::builder()
        .label(item.status().label())
        .css_classes(["dimmed", "caption"])
        .valign(gtk4::Align::Center)
        .build();
    let spinner = adw::Spinner::new();
    spinner.set_visible(false);

    let toggle_btn = icon_button("media-playback-pause-symbolic", "Pause");
    let stop_btn = icon_button("process-stop-symbolic", "Cancel");
    let queue_btn = icon_button("go-down-symbolic", "Queue for later");
    let retry_btn = icon_button("view-refresh-symbolic", "Retry");
    let reveal_btn = icon_button("folder-open-symbolic", "Show in Folder");
    let delete_btn = icon_button("user-trash-symbolic", "Move to Trash");
    let remove_btn = icon_button("list-remove-symbolic", "Remove from list");

    top.append(&name);
    top.append(&status);
    top.append(&spinner);
    top.append(&toggle_btn);
    top.append(&stop_btn);
    top.append(&queue_btn);
    top.append(&retry_btn);
    top.append(&reveal_btn);
    top.append(&delete_btn);
    top.append(&remove_btn);

    let detail = gtk4::Label::builder()
        .label(item.detail())
        .halign(gtk4::Align::Start)
        .css_classes(["dimmed", "caption"])
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    let progress = gtk4::ProgressBar::new();
    progress.set_show_text(false);

    outer.append(&top);
    outer.append(&detail);
    outer.append(&progress);

    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&outer));

    let w = |w: &gtk4::Widget| w.downgrade();
    let weaks = (
        w(detail.upcast_ref()),
        w(progress.upcast_ref()),
        w(spinner.upcast_ref()),
        w(toggle_btn.upcast_ref()),
        w(stop_btn.upcast_ref()),
        w(queue_btn.upcast_ref()),
        w(retry_btn.upcast_ref()),
        w(reveal_btn.upcast_ref()),
        w(delete_btn.upcast_ref()),
        w(status.upcast_ref()),
        w(name.upcast_ref()),
    );
    let m_sync = Rc::clone(manager);
    let updater = move |it: &crate::download::DownloadItem| {
        let (
            w_detail,
            w_prog,
            w_spin,
            w_tog,
            w_stop,
            w_queue,
            w_retry,
            w_reveal,
            w_del,
            w_status,
            w_name,
        ) = &weaks;
        if let (
            Some(d),
            Some(p),
            Some(s),
            Some(t),
            Some(x),
            Some(q),
            Some(r),
            Some(o),
            Some(y),
            Some(st),
            Some(n),
        ) = (
            w_detail.upgrade(),
            w_prog.upgrade(),
            w_spin.upgrade(),
            w_tog.upgrade(),
            w_stop.upgrade(),
            w_queue.upgrade(),
            w_retry.upgrade(),
            w_reveal.upgrade(),
            w_del.upgrade(),
            w_status.upgrade(),
            w_name.upgrade(),
        ) {
            let st: gtk4::Label = st.downcast().expect("Grab: status widget is a Label (bug)");
            let n: gtk4::Label = n.downcast().expect("Grab: name widget is a Label (bug)");
            n.set_text(&it.filename());
            st.set_text(it.status().label());
            refresh_row(
                it,
                &RowWidgets {
                    detail: d.downcast().expect("Grab: detail widget is a Label (bug)"),
                    progress: p
                        .downcast()
                        .expect("Grab: progress widget is a ProgressBar (bug)"),
                    spinner: s
                        .downcast()
                        .expect("Grab: spinner widget is a Spinner (bug)"),
                    toggle_btn: t.downcast().expect("Grab: toggle widget is a Button (bug)"),
                    stop_btn: x.downcast().expect("Grab: stop widget is a Button (bug)"),
                    queue_btn: q.downcast().expect("Grab: queue widget is a Button (bug)"),
                    retry_btn: r.downcast().expect("Grab: retry widget is a Button (bug)"),
                    reveal_btn: o.downcast().expect("Grab: reveal widget is a Button (bug)"),
                    delete_btn: y.downcast().expect("Grab: delete widget is a Button (bug)"),
                },
                another_queued(&m_sync, it),
            );
        }
    };
    let u1 = updater.clone();
    item.connect_progress_notify(move |it| u1(it));
    let u2 = updater.clone();
    item.connect_status_notify(move |it| u2(it));
    let u3 = updater.clone();
    item.connect_detail_notify(move |it| u3(it));
    let u4 = updater.clone();
    item.connect_filename_notify(move |it| u4(it));
    refresh_row(
        item,
        &RowWidgets {
            detail: detail.clone(),
            progress: progress.clone(),
            spinner: spinner.clone(),
            toggle_btn: toggle_btn.clone(),
            stop_btn: stop_btn.clone(),
            queue_btn: queue_btn.clone(),
            retry_btn: retry_btn.clone(),
            reveal_btn: reveal_btn.clone(),
            delete_btn: delete_btn.clone(),
        },
        another_queued(manager, item),
    );

    let id = item.id();
    {
        let m = Rc::clone(manager);
        toggle_btn.connect_clicked(move |_| {
            if let Some(it) = m.find(id) {
                if it.status() == DownloadStatus::Paused {
                    m.resume(id);
                } else {
                    m.pause(id);
                }
            }
        });
    }
    {
        let m = Rc::clone(manager);
        stop_btn.connect_clicked(move |_| m.cancel(id));
    }
    {
        let m = Rc::clone(manager);
        retry_btn.connect_clicked(move |_| m.retry(id));
    }
    {
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        queue_btn.connect_clicked(move |_| {
            // Re-check: the button only refreshes on this row's own ticks,
            // so the last waiter may have left since it was shown.
            let Some(it) = m.find(id) else {
                return;
            };
            if another_queued(&m, &it) {
                m.defer(id);
                t.add_toast(adw::Toast::new("Queued for later"));
            } else {
                t.add_toast(adw::Toast::new("No other downloads waiting"));
            }
        });
    }
    {
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        reveal_btn.connect_clicked(move |_| {
            if let Some(it) = m.find(id) {
                launch_path(&it.file_path(), &t, true);
            }
        });
    }
    {
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        delete_btn.connect_clicked(move |_| {
            if let Err(e) = m.delete_download(id) {
                t.add_toast(adw::Toast::new(&e));
            } else {
                t.add_toast(adw::Toast::new("Moved to Trash"));
            }
        });
    }
    {
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        remove_btn.connect_clicked(move |_| {
            let Some(it) = m.find(id) else { return };
            let snapshot = (
                it.url().to_string(),
                it.dest_dir().to_string(),
                it.filename().to_string(),
                it.status(),
                it.progress(),
                it.detail().to_string(),
            );
            let name = snapshot.2.clone();
            m.remove(id);
            let toast = adw::Toast::new(&format!("Removed {name}"));
            toast.set_button_label(Some("Undo"));
            let m2 = Rc::clone(&m);
            toast.connect_button_clicked(move |_| {
                let (url, dir, fname, status, prog, detail) = snapshot.clone();
                m2.unremove(url, dir, fname, status, prog, detail);
            });
            t.add_toast(toast);
        });
    }
    row
}

/// Suspend block held through the desktop portal. `request` is the portal
/// request path while held; `sub` watches its Response so a denial clears
/// the hold instead of pretending to block. Both die with the process,
/// which also releases the lock server-side.
struct InhibitState {
    request: Option<String>,
    sub: Option<gio::SignalSubscription>,
    seq: u64,
    /// An Inhibit round trip is in flight. `request` stays None until its
    /// reply lands, so without this every sync during the round trip would
    /// fire a duplicate request whose path gets overwritten and never
    /// closed. Set synchronously when launching, cleared on every exit.
    pending: bool,
}

/// Ask the portal to block suspend. Stores the request only if still wanted
/// when the reply lands; otherwise closes it at once so no block leaks.
/// Anything failing (no bus, no portal, denied) leaves nothing held.
async fn request_inhibit(
    state: Rc<RefCell<InhibitState>>,
    manager: Rc<DownloadManager>,
    settings: gio::Settings,
) {
    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
    const SUSPEND: u32 = 4;
    // Every exit below clears `pending`: a stuck true would silence all
    // future inhibits, leaving the machine unblocked forever.
    let clear_pending = |state: &Rc<RefCell<InhibitState>>| {
        state.borrow_mut().pending = false;
    };
    let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
        clear_pending(&state);
        tracing::warn!("suspend block unavailable (no session bus)");
        return; // Headless/test: no session bus, nothing to block on.
    };
    let token = {
        let mut st = state.borrow_mut();
        st.seq += 1;
        format!("grab{}", st.seq)
    };
    let options = glib::VariantDict::new(None);
    options.insert("handle_token", token);
    options.insert("reason", "Downloading files");
    // Flags ride positionally (sua{sv}), not in the options dict: the
    // portal rejects the call otherwise.
    let params = glib::variant::ToVariant::to_variant(&(String::new(), SUSPEND, options.end()));
    let Ok(reply) = conn
        .call_future(
            Some(PORTAL),
            DESKTOP_PATH,
            "org.freedesktop.portal.Inhibit",
            "Inhibit",
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await
    else {
        clear_pending(&state);
        tracing::warn!("suspend block request failed");
        return;
    };
    let path = (reply.n_children() == 1)
        .then(|| reply.child_value(0))
        .and_then(|v| v.str().map(String::from));
    let Some(path) = path else {
        clear_pending(&state);
        tracing::warn!("suspend block reply had no request path");
        return;
    };
    // The queue may have idled during the round trip: close at once instead
    // of leaking a block nobody will release.
    if !(settings.boolean("inhibit-suspend") && manager.has_transferring()) {
        release_inhibit(conn, path).await;
        clear_pending(&state);
        return;
    }
    let st2 = Rc::clone(&state);
    let sub = conn.subscribe_to_signal(
        None,
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&path),
        None,
        gio::DBusSignalFlags::NONE,
        move |sig| {
            let denied = sig.parameters.n_children() != 2
                || sig.parameters.child_value(0).get::<u32>() != Some(0);
            if denied {
                let mut st = st2.borrow_mut();
                if st.request.as_deref() == Some(sig.object_path) {
                    st.request = None;
                    drop(st.sub.take());
                }
            }
        },
    );
    let mut st = state.borrow_mut();
    st.request = Some(path.clone());
    st.sub = Some(sub);
    st.pending = false;
    tracing::info!("suspend block held ({path})");
}

/// Release a held portal block. Fire-and-forget: the lock dies with the
/// bus connection anyway, so a failed Close loses nothing.
async fn release_inhibit(conn: gio::DBusConnection, path: String) {
    let _ = conn
        .call_future(
            Some("org.freedesktop.portal.Desktop"),
            &path,
            "org.freedesktop.portal.Request",
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await;
}

/// Tell the desktop we keep running without windows (Background portal):
/// the cross-desktop way to survive window close on strict desktops, and
/// what lists Grab in the system's background-apps settings. Fire-and-forget:
/// a denial changes nothing about the current transfer, it just means the
/// host may still reap us. No autostart requested: relaunch stays the user's
/// choice.
fn request_background() {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let token = format!(
            "grabbg{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let options = glib::VariantDict::new(None);
        options.insert("handle_token", token);
        options.insert(
            "reason",
            "Downloads continue in the background after the window is closed",
        );
        options.insert("autostart", false);
        options.insert("background", true);
        let params = glib::variant::ToVariant::to_variant(&(String::new(), options.end()));
        let _ = conn
            .call_future(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Background",
                "RequestBackground",
                Some(&params),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await;
    });
}

pub fn build_window(
    app: &adw::Application,
    manager: Rc<DownloadManager>,
    settings: gio::Settings,
    toasts: Rc<adw::ToastOverlay>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Grab")
        .default_width(settings.int("window-width").max(400))
        .default_height(settings.int("window-height").max(300))
        .build();

    settings
        .bind("window-width", &window, "default-width")
        .build();
    settings
        .bind("window-height", &window, "default-height")
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Grab", "Download Manager")));

    let menu = gio::Menu::new();
    menu.append(Some("New Download"), Some("app.add-download"));
    let section = gio::Menu::new();
    section.append(Some("Cancel All"), Some("app.cancel-all"));
    section.append(Some("Retry Failed"), Some("app.retry-failed"));
    section.append(Some("Open Download Folder"), Some("app.open-folder"));
    menu.append_section(None, &section);
    let section2 = gio::Menu::new();
    section2.append(Some("Preferences"), Some("app.preferences"));
    section2.append(Some("Keyboard Shortcuts"), Some("app.shortcuts"));
    section2.append(Some("About"), Some("app.about"));
    menu.append_section(None, &section2);
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .build();
    menu_btn.update_property(&[gtk4::accessible::Property::Label("Main Menu")]);
    header.pack_start(&menu_btn);

    let add_btn = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["suggested-action"])
        .tooltip_text("New Download (Ctrl+N)")
        .build();
    add_btn.update_property(&[gtk4::accessible::Property::Label("New Download")]);
    {
        let m = Rc::clone(&manager);
        add_btn.connect_clicked(move |_| show_add_dialog(m.clone()));
    }
    header.pack_end(&add_btn);

    let stack = adw::ViewStack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("folder-download-symbolic")
        .title("No Downloads Yet")
        .description("Add a download to get started.")
        .build();
    let empty_add = gtk4::Button::builder()
        .label("New Download")
        .css_classes(["pill", "suggested-action"])
        .halign(gtk4::Align::Center)
        .build();
    empty.set_child(Some(&empty_add));
    {
        let m = Rc::clone(&manager);
        empty_add.connect_clicked(move |_| show_add_dialog(m.clone()));
    }
    stack.add_named(&empty, Some("empty"));

    fn section_list(title: &str) -> (gtk4::Box, gtk4::ListBox) {
        let label = gtk4::Label::builder()
            .label(title)
            .halign(gtk4::Align::Start)
            .css_classes(["heading"])
            .build();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        section.append(&label);
        section.append(&list);
        (section, list)
    }
    let (active_section, active_list) = section_list("Active");
    let (queued_section, queued_list) = section_list("Queued");
    let (downloaded_section, downloaded_list) = section_list("Downloaded");
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&active_section);
    content.append(&queued_section);
    content.append(&downloaded_section);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    stack.add_named(&scroll, Some("list"));

    fn is_done(it: &crate::download::DownloadItem) -> bool {
        it.status() == crate::download::DownloadStatus::Done
    }
    fn is_queued(it: &crate::download::DownloadItem) -> bool {
        it.status() == crate::download::DownloadStatus::Queued
    }

    let rows: Rc<RefCell<HashMap<u64, gtk4::ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let sync: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        let r = Rc::clone(&rows);
        let add = add_btn.clone();
        let s = stack.clone();
        let l_active = active_list.clone();
        let l_queued = queued_list.clone();
        let l_downloaded = downloaded_list.clone();
        let sec_active = active_section.clone();
        let sec_queued = queued_section.clone();
        let sec_downloaded = downloaded_section.clone();
        Rc::new(move || {
            let store = m.store();
            let mut present = std::collections::HashSet::new();
            let mut n_active = 0;
            let mut n_queued = 0;
            let mut n_downloaded = 0;
            for i in 0..store.n_items() {
                if let Some(it) = store
                    .item(i)
                    .and_downcast::<crate::download::DownloadItem>()
                {
                    present.insert(it.id());
                    if is_done(&it) {
                        n_downloaded += 1;
                    } else if is_queued(&it) {
                        n_queued += 1;
                    } else {
                        n_active += 1;
                    }
                    let existing = r.borrow().get(&it.id()).cloned();
                    let row = if let Some(row) = existing {
                        row
                    } else {
                        let row = build_row(&it, &m, &t);
                        r.borrow_mut().insert(it.id(), row.clone());
                        row
                    };
                    let target = if is_done(&it) {
                        &l_downloaded
                    } else if is_queued(&it) {
                        &l_queued
                    } else {
                        &l_active
                    };
                    if !row.is_ancestor(target) {
                        if let Some(old) = row.parent().and_downcast::<gtk4::ListBox>() {
                            old.remove(&row);
                        }
                        target.append(&row);
                    }
                }
            }
            let stale: Vec<u64> = r
                .borrow()
                .keys()
                .filter(|id| !present.contains(id))
                .cloned()
                .collect();
            for id in stale {
                if let Some(row) = r.borrow_mut().remove(&id) {
                    if let Some(old) = row.parent().and_downcast::<gtk4::ListBox>() {
                        old.remove(&row);
                    }
                }
            }
            sec_active.set_visible(n_active > 0);
            sec_queued.set_visible(n_queued > 0);
            sec_downloaded.set_visible(n_downloaded > 0);
            let has_items = store.n_items() > 0;
            // Header + duplicates the empty-state pill, so show it only with the list.
            add.set_visible(has_items);
            s.set_visible_child_name(if has_items { "list" } else { "empty" });
        })
    };

    // ponytail: hidden window keeps its widget tree (~MBs) while headless; destroy+rebuild if that ever matters.
    let ever_shown = Rc::new(Cell::new(false));
    {
        let m = Rc::clone(&manager);
        window.connect_close_request(move |win| {
            // Only hide to background while bytes are actually moving. Paused
            // items (or none at all) quit normally: notifying "continues in
            // the background" would be a lie with nothing transferring.
            if m.has_transferring() {
                win.set_visible(false);
                request_background();
                if m.background_notifications_enabled() {
                    if let Some(app) = gio::Application::default() {
                        let n = gio::Notification::new("Downloads continue in the background");
                        n.set_default_action_and_target_value("app.present", None);
                        app.send_notification(Some(BACKGROUND_NOTIF_ID), &n);
                    }
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    {
        let armed = Rc::clone(&ever_shown);
        window.connect_map(move |_| armed.set(true));
    }

    let banner = adw::Banner::new("Some downloads failed");
    banner.set_button_label(Some("Retry Failed"));
    {
        let m = Rc::clone(&manager);
        banner.connect_button_clicked(move |_| m.retry_failed());
    }
    banner.set_revealed(false);

    // Sleep inhibition through the desktop portal
    // (`org.freedesktop.portal.Inhibit`, flag 4 = suspend): the cross-desktop
    // path, sandbox-safe with no extra permissions. GtkApplication's inhibit
    // only speaks to GNOME SessionManager, so KDE/Sway/etc would silently
    // never block. No request held while idle; a failed call simply leaves
    // nothing held and the next queue sync retries.
    let inhibit = Rc::new(RefCell::new(InhibitState {
        request: None,
        sub: None,
        seq: 0,
        pending: false,
    }));
    let sync_inhibit: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let s = settings.clone();
        let st = Rc::clone(&inhibit);
        Rc::new(move || {
            let want = s.boolean("inhibit-suspend") && m.has_transferring();
            // Claim the in-flight marker synchronously: without it, every
            // sync during the D-Bus round trip (e.g. each row of a bulk
            // import) would fire a duplicate request whose path the later
            // reply overwrites and never closes.
            let launch = {
                let mut st = st.borrow_mut();
                if want && st.request.is_none() && !st.pending {
                    st.pending = true;
                    true
                } else {
                    false
                }
            };
            if launch {
                let (st2, m2, s2) = (Rc::clone(&st), Rc::clone(&m), s.clone());
                glib::spawn_future_local(async move {
                    request_inhibit(st2, m2, s2).await;
                });
            } else if !want {
                // Separate statements: the first borrow must end before the
                // second begins, or RefCell panics on release.
                let path = st.borrow_mut().request.take();
                drop(st.borrow_mut().sub.take());
                if let Some(path) = path {
                    glib::spawn_future_local(async move {
                        // Bus lookup stays async: a stalled portal must never
                        // stall the main loop from inside a change hook.
                        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                            return;
                        };
                        release_inhibit(conn, path).await;
                    });
                }
            }
        })
    };
    {
        let inhibit = Rc::clone(&sync_inhibit);
        settings.connect_changed(Some("inhibit-suspend"), move |_, _| inhibit());
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(toasts.as_ref()));

    {
        let app_weak = app.downgrade();
        let m = Rc::clone(&manager);
        let sync = Rc::clone(&sync);
        let w = window.downgrade();
        let armed = Rc::clone(&ever_shown);
        let inhibit = Rc::clone(&sync_inhibit);
        let hook: Rc<dyn Fn()> = Rc::new(move || {
            sync();
            inhibit();
            if let Some(app) = app_weak.upgrade() {
                if let Some(a) = app
                    .lookup_action("cancel-all")
                    .and_downcast::<gio::SimpleAction>()
                {
                    a.set_enabled(m.has_active());
                }
                if let Some(a) = app
                    .lookup_action("retry-failed")
                    .and_downcast::<gio::SimpleAction>()
                {
                    a.set_enabled(m.has_failed());
                }
            }
            banner.set_revealed(m.has_errored());
            // Same predicate as close-request: only quit/withdraw when
            // nothing is transferring. Paused rows persist across launches,
            // so counting them here would strand a hidden zombie.
            let idle_hidden = armed.get()
                && !m.has_transferring()
                && w.upgrade().is_some_and(|win| !win.is_visible());
            if idle_hidden {
                if let Some(app) = app_weak.upgrade() {
                    app.withdraw_notification(BACKGROUND_NOTIF_ID);
                    app.quit();
                }
            }
        });
        manager.set_on_change({
            let hook = Rc::clone(&hook);
            move || hook()
        });
        hook();
    }

    window
}

pub fn show_add_dialog(manager: Rc<DownloadManager>) {
    let dialog = adw::Dialog::builder().title("New Download").build();
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    page.add(&group);

    let url_row = adw::EntryRow::builder()
        .title("URL")
        .text("")
        .show_apply_button(true)
        .build();
    url_row.set_input_purpose(gtk4::InputPurpose::Url);
    group.add(&url_row);

    let file_row = adw::EntryRow::builder()
        .title("File name (optional)")
        .text("")
        .build();
    group.add(&file_row);

    let dest_label = gtk4::Label::builder()
        .label(manager.effective_download_dir())
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(["dimmed", "caption"])
        .hexpand(true)
        .build();
    let dest_btn = gtk4::Button::builder()
        .label("Choose…")
        .tooltip_text("Choose download folder")
        .valign(gtk4::Align::Center)
        .build();
    let dest_row = adw::ActionRow::builder().title("Save to").build();
    dest_row.add_suffix(&dest_label);
    dest_row.add_suffix(&dest_btn);
    group.add(&dest_row);

    let dest_dir = Rc::new(RefCell::new(manager.effective_download_dir()));
    {
        let dd = Rc::clone(&dest_dir);
        let dl = dest_label.clone();
        dest_btn.connect_clicked(move |b| {
            let chooser = gtk4::FileDialog::builder()
                .title("Choose download folder")
                .build();
            let root = b.root().and_downcast::<gtk4::Window>();
            let dd2 = Rc::clone(&dd);
            let dl2 = dl.clone();
            chooser.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        let s = p.to_string_lossy().into_owned();
                        dl2.set_text(&s);
                        *dd2.borrow_mut() = s;
                    }
                }
            });
        });
    }

    let error_label = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    group.add(&error_label);
    {
        let el = error_label.clone();
        let ur = url_row.clone();
        url_row.connect_changed(move |_| {
            el.set_visible(false);
            ur.remove_css_class("error");
        });
    }

    let torrent_btn = gtk4::Button::builder()
        .label("Choose…")
        .tooltip_text("Choose a .torrent file")
        .valign(gtk4::Align::Center)
        .build();
    let torrent_row = adw::ActionRow::builder()
        .title("Torrent file")
        .subtitle("Pick a .torrent file instead of a link")
        .activatable_widget(&torrent_btn)
        .build();
    torrent_row.add_suffix(&torrent_btn);
    group.add(&torrent_row);
    {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let dialog = dialog.downgrade();
        let error_label = error_label.clone();
        torrent_btn.connect_clicked(move |_| {
            let m = m.clone();
            let dd = dd.clone();
            let dialog = dialog.clone();
            let error_label = error_label.clone();
            glib::spawn_future_local(async move {
                let filter = gtk4::FileFilter::new();
                filter.set_name(Some("Torrent files"));
                filter.add_mime_type("application/x-bittorrent");
                filter.add_pattern("*.torrent");
                let filters = gio::ListStore::new::<gtk4::FileFilter>();
                filters.append(&filter);
                let picker = gtk4::FileDialog::builder().filters(&filters).build();
                let Ok(file) = picker.open_future(None::<&gtk4::Window>).await else {
                    return; // dismissed
                };
                let name = file
                    .basename()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "download.torrent".to_string());
                let bytes = match file.path().and_then(|p| {
                    std::fs::metadata(&p)
                        .ok()
                        .filter(|md| md.len() <= 10_000_000)
                        .and_then(|_| std::fs::read(&p).ok())
                }) {
                    Some(b) => b,
                    None => {
                        error_label.set_text("Could not read that .torrent file");
                        error_label.set_visible(true);
                        return;
                    }
                };
                let (_tname, entries) = match crate::torrent::torrent_file_list(&bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        error_label.set_text(&e);
                        error_label.set_visible(true);
                        return;
                    }
                };
                if entries.len() <= 1 {
                    match m.enqueue_torrent_file(bytes, &name, Some(&dd.borrow()), None) {
                        Ok(_) => {
                            if let Some(d) = dialog.upgrade() {
                                d.close();
                            }
                        }
                        Err(e) => {
                            error_label.set_text(&e);
                            error_label.set_visible(true);
                        }
                    }
                    return;
                }
                show_torrent_files_dialog(m, dd, Some(dialog), name, bytes, entries);
            });
        });
    }

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_end_title_buttons(true);
    hb.set_show_start_title_buttons(false);
    let cancel_btn = gtk4::Button::builder().label("Cancel").build();
    let add_btn = gtk4::Button::builder()
        .label("Add Download")
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&cancel_btn);
    hb.pack_end(&add_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));

    dialog.set_child(Some(&toolbar));

    {
        let d = dialog.downgrade();
        cancel_btn.connect_clicked(move |_| {
            if let Some(d) = d.upgrade() {
                d.close();
            }
        });
    }
    {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let url_row = url_row.clone();
        let file_row = file_row.clone();
        let error_label = error_label.clone();
        let dialog = dialog.downgrade();
        add_btn.connect_clicked(move |_| {
            let url = url_row.text().trim().to_string();
            let fname = file_row.text().trim().to_string();
            match m.enqueue(
                &url,
                Some(&dd.borrow()),
                if fname.is_empty() {
                    None
                } else {
                    Some(fname.as_str())
                },
            ) {
                Ok(_) => {
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                }
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                    url_row.add_css_class("error");
                }
            }
        });
    }
    {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let file_row = file_row.clone();
        let dialog = dialog.downgrade();
        let error_label = error_label.clone();
        url_row.connect_apply(move |row| {
            let url = row.text().trim().to_string();
            if url.is_empty() {
                return;
            }
            let fname = file_row.text().trim().to_string();
            match m.enqueue(
                &url,
                Some(&dd.borrow()),
                if fname.is_empty() {
                    None
                } else {
                    Some(fname.as_str())
                },
            ) {
                Ok(_) => {
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                }
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                    row.add_css_class("error");
                }
            }
        });
    }

    if let Some(app) = gio::Application::default().and_downcast::<adw::Application>() {
        if let Some(win) = app.active_window() {
            dialog.present(Some(&win));
        } else {
            // No window (e.g. action fired while hidden): present standalone
            // rather than silently dropping the dialog.
            dialog.present(None::<&gtk4::Window>);
        }
    }

    // ponytail: single clipboard read per dialog open; no watch, no polling.
    {
        let url_row = url_row.clone();
        let dialog_weak = dialog.downgrade();
        glib::spawn_future_local(async move {
            let clipboard = gtk4::gdk::Display::default().map(|d| d.clipboard());
            let Some(clipboard) = clipboard else { return };
            let Ok(Some(text)) = clipboard.read_text_future().await else {
                return;
            };
            if dialog_weak.upgrade().is_none() {
                return;
            }
            if !url_row.text().trim().is_empty() {
                return;
            }
            let pasted = text.trim().to_string();
            if let Ok(normalized) = crate::download::normalize_url(&pasted) {
                url_row.set_text(&normalized);
            }
        });
    }
}

/// Multi-file .torrent intake: one switch per file, all on by default.
/// The selection feeds rqbit's `only_files` at add time (no live setter),
/// so it must be chosen here, before the row exists.
pub(crate) fn show_torrent_files_dialog(
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    parent: Option<glib::WeakRef<adw::Dialog>>,
    file_name: String,
    bytes: Vec<u8>,
    entries: Vec<crate::torrent::TorrentFileEntry>,
) {
    let dialog = adw::Dialog::builder().title(&file_name).build();
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Files")
        .description(format!("{} files", entries.len()))
        .build();
    page.add(&group);

    let mut switches = Vec::new();
    for e in &entries {
        let row = adw::SwitchRow::builder()
            .title(&e.path)
            .subtitle(crate::download::fmt_bytes(e.length))
            .active(true)
            .build();
        switches.push(row.clone());
        group.add(&row);
    }
    let error_label = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    group.add(&error_label);

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_end_title_buttons(true);
    hb.set_show_start_title_buttons(false);
    let cancel_btn = gtk4::Button::builder().label("Cancel").build();
    let add_btn = gtk4::Button::builder()
        .label("Add Files")
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&cancel_btn);
    hb.pack_end(&add_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));

    {
        let dialog_weak = dialog.downgrade();
        cancel_btn.connect_clicked(move |_| {
            if let Some(d) = dialog_weak.upgrade() {
                d.close();
            }
        });
    }
    {
        let dialog_weak = dialog.downgrade();
        add_btn.connect_clicked(move |_| {
            let selected: Vec<usize> = switches
                .iter()
                .enumerate()
                .filter(|(_, s)| s.is_active())
                .map(|(i, _)| i)
                .collect();
            if selected.is_empty() {
                error_label.set_text("Select at least one file");
                error_label.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < switches.len()).then_some(selected);
            match manager.enqueue_torrent_file(
                bytes.clone(),
                &file_name,
                Some(&dest_dir.borrow()),
                only,
            ) {
                Ok(_) => {
                    if let Some(d) = dialog_weak.upgrade() {
                        d.close();
                    }
                    if let Some(p) = parent.as_ref().and_then(|w| w.upgrade()) {
                        p.close();
                    }
                }
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                }
            }
        });
    }

    // No gtk Window parent exists here (invoked from an adw::Dialog):
    // present standalone like the no-window fallback above.
    dialog.present(None::<&gtk4::Window>);
}
