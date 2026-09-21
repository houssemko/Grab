use crate::download::{BLOCK_CELLS, DownloadManager, DownloadStatus, RemovedSnapshot, aggregate};
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
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
            let verb = if reveal {
                gettext("show")
            } else {
                gettext("open")
            };
            t.add_toast(adw::Toast::new(
                &gettext("Could not {verb} {what} in the file manager: {e}")
                    .replace("{verb}", &verb)
                    .replace("{what}", &what)
                    .replace("{e}", &e.to_string()),
            ));
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
    map_revealer: gtk4::Revealer,
    blocks: gtk4::DrawingArea,
    expanded: Rc<Cell<bool>>,
}

/// Whether deferring `item` could hand its slot to someone: another row is
/// waiting queued. With nothing waiting the button would just stop and
/// immediately restart the same download. O(1) off the cached count, so
/// progress ticks can call it freely.
fn another_queued(manager: &DownloadManager, item: &crate::download::DownloadItem) -> bool {
    let n = manager.queued_count();
    n > 1 || (n == 1 && item.status() != DownloadStatus::Queued)
}

/// Whether a row's bar is indeterminate: active with no fraction to
/// fill (live captures never report a total; resolving rows sit at
/// zero), so the wall-clock tick advances it instead of progress
/// notifies. Pure for tests — the only pulse logic allowed outside
/// build_row's tick.
pub(crate) fn should_pulse(status: DownloadStatus, is_live: bool, progress: f64) -> bool {
    status == DownloadStatus::Downloading && (is_live || progress <= 0.0)
}

fn refresh_row(
    item: &crate::download::DownloadItem,
    w: &RowWidgets,
    defer_available: bool,
    is_live: bool,
) {
    let frac = item.progress().clamp(0.0, 1.0);
    let active = item.status() == DownloadStatus::Downloading;
    // Unbounded work has no fraction to fill with: live captures never
    // report a total, and any row whose total is still unknown sits at
    // zero. HIG prescribes indeterminate activity there instead of a
    // frozen empty bar — advanced solely by the per-row wall-clock tick
    // in build_row, never here: progress ticks arrive ~20/sec during
    // transfer and would otherwise double the animation cadence.
    if !should_pulse(item.status(), is_live, frac) {
        w.progress.set_fraction(frac);
    }
    w.spinner.set_visible(active);
    w.detail.set_text(&item.detail());

    let running = matches!(
        item.status(),
        DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
    );
    // Live captures can't pause or defer mid-flight (resuming a moved-on
    // stream is meaningless): the toggle and queue buttons hide, and Stop
    // keeps what's recorded instead of discarding it.
    let live_capturing = active && is_live;
    w.toggle_btn.set_visible(running && !live_capturing);
    w.stop_btn.set_visible(running);
    if live_capturing {
        w.stop_btn.set_tooltip_text(Some(&gettext("Stop")));
        w.stop_btn
            .update_property(&[gtk4::accessible::Property::Label(&gettext(
                "Stop recording",
            ))]);
    } else {
        w.stop_btn.set_tooltip_text(Some(&gettext("Cancel")));
        w.stop_btn
            .update_property(&[gtk4::accessible::Property::Label(&gettext("Cancel"))]);
    }
    // Deferring only makes sense while holding a slot that someone else
    // is waiting for; queued rows are already waiting.
    w.queue_btn.set_visible(
        defer_available
            && !live_capturing
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
        w.toggle_btn.set_tooltip_text(Some(&gettext("Resume")));
        w.toggle_btn
            .update_property(&[gtk4::accessible::Property::Label(&gettext("Resume"))]);
    } else {
        w.toggle_btn.set_icon_name("media-playback-pause-symbolic");
        w.toggle_btn.set_tooltip_text(Some(&gettext("Pause")));
        w.toggle_btn
            .update_property(&[gtk4::accessible::Property::Label(&gettext("Pause"))]);
    }

    // Block map: only while pieces are still landing. Other states
    // collapse it so finished rows stay compact.
    let expandable = matches!(
        item.status(),
        DownloadStatus::Downloading | DownloadStatus::Paused
    );
    if !expandable {
        w.expanded.set(false);
    }
    w.map_revealer
        .set_reveal_child(w.expanded.get() && expandable);
    if w.map_revealer.reveals_child() {
        w.blocks.queue_draw();
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

    let toggle_btn = icon_button("media-playback-pause-symbolic", &gettext("Pause"));
    let stop_btn = icon_button("process-stop-symbolic", &gettext("Cancel"));
    let queue_btn = icon_button("go-down-symbolic", &gettext("Queue for later"));
    let retry_btn = icon_button("view-refresh-symbolic", &gettext("Retry"));
    let reveal_btn = icon_button("folder-open-symbolic", &gettext("Show in Folder"));
    let delete_btn = icon_button("user-trash-symbolic", &gettext("Move to Trash"));
    let remove_btn = icon_button("list-remove-symbolic", &gettext("Remove from list"));

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

    // Block map: per-piece completion strip under the progress bar,
    // revealed by clicking the row. A DrawingArea (not hundreds of
    // widgets) keeps thousands of pieces cheap; the textual percent in
    // `detail` stays the screen-reader path.
    let id = item.id();
    let expanded = Rc::new(Cell::new(false));
    let blocks = gtk4::DrawingArea::new();
    blocks.set_content_height(48);
    blocks.set_hexpand(true);
    blocks.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Downloaded blocks",
    ))]);
    {
        let m = Rc::clone(manager);
        blocks.set_draw_func(move |_area, cr, width, height| {
            let cells = aggregate(&m.piece_bitmap(id), BLOCK_CELLS);
            if cells.is_empty() {
                return;
            }
            // Accent for done, washed accent for pending: follows the theme.
            let accent = adw::StyleManager::default().accent_color().to_rgba();
            let (r, g, b) = (
                f64::from(accent.red()),
                f64::from(accent.green()),
                f64::from(accent.blue()),
            );
            let (cols, rows) = (64_usize, 4_usize);
            let (cw, ch) = (width as f64 / cols as f64, height as f64 / rows as f64);
            for (i, done) in cells.iter().enumerate().take(cols * rows) {
                let (col, row) = ((i % cols) as f64, (i / cols) as f64);
                cr.set_source_rgba(r, g, b, if *done { 1.0 } else { 0.18 });
                cr.rectangle(col * cw + 0.5, row * ch + 0.5, cw - 1.0, ch - 1.0);
                let _ = cr.fill();
            }
        });
    }
    let map_revealer = gtk4::Revealer::new();
    map_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
    // HIG separation: a horizontal separator between the progress bar and
    // the blocks, 6px from each (related elements). Inside the revealer
    // so nothing shows while collapsed.
    let map_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    map_box.set_margin_top(6);
    map_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    map_box.append(&blocks);
    map_revealer.set_child(Some(&map_box));

    outer.append(&top);
    outer.append(&detail);
    outer.append(&progress);
    outer.append(&map_revealer);

    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&outer));

    // Right-click menu (rename only): a per-row action group keeps the
    // global menu untouched.
    let rename_menu = gio::Menu::new();
    rename_menu.append(Some(&gettext("Rename…")), Some("row.rename"));
    let pop = gtk4::PopoverMenu::from_model(Some(&rename_menu));
    pop.set_parent(&row);
    {
        let m = Rc::clone(manager);
        let anchor = row.clone();
        let actions = gio::SimpleActionGroup::new();
        let act = gio::SimpleAction::new("rename", None);
        act.connect_activate(move |_, _| {
            if let Some(it) = m.find(id) {
                show_rename_dialog(m.clone(), id, it.filename(), &anchor);
            }
        });
        actions.add_action(&act);
        row.insert_action_group("row", Some(&actions));
    }

    // Click the row body to reveal the block map. Clicks landing on a
    // button belong to the button: walk up from the pick target and
    // ignore those. Only live rows expand (finished ones have no map),
    // and only with map data to show (resolving or chunk-less rows
    // ignore the click).
    {
        let click = gtk4::GestureClick::new();
        let m = Rc::clone(manager);
        let rev = map_revealer.clone();
        let blk = blocks.clone();
        let exp = Rc::clone(&expanded);
        let pop_menu = pop.clone();
        click.connect_pressed(move |gesture, _n_press, x, y| {
            if gesture.current_button() == gtk4::gdk::BUTTON_SECONDARY {
                pop_menu
                    .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                pop_menu.popup();
                return;
            }
            let pick = gesture
                .widget()
                .and_downcast::<gtk4::ListBoxRow>()
                .and_then(|r| r.pick(x, y, gtk4::PickFlags::DEFAULT));
            let mut w = pick;
            while let Some(widget) = w {
                if widget.is::<gtk4::Button>() {
                    return;
                }
                w = widget.parent();
            }
            let live = m.find(id).is_some_and(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Downloading | DownloadStatus::Paused
                )
            });
            // No map without data: resolving rows and chunk-less videos
            // have nothing to reveal, so the click does nothing.
            if live && !m.piece_bitmap(id).is_empty() {
                exp.set(!exp.get());
                rev.set_reveal_child(exp.get());
                blk.queue_draw();
            }
        });
        row.add_controller(click);
    }

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
        w(map_revealer.upcast_ref()),
        w(blocks.upcast_ref()),
        w(status.upcast_ref()),
        w(name.upcast_ref()),
    );
    let m_sync = Rc::clone(manager);
    let exp_sync = Rc::clone(&expanded);
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
            w_rev,
            w_map,
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
            Some(rv),
            Some(mp),
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
            w_rev.upgrade(),
            w_map.upgrade(),
            w_status.upgrade(),
            w_name.upgrade(),
        ) {
            // A mismatched widget type means the UI definition drifted:
            // warn and skip this row's refresh instead of panicking.
            let (
                Ok(st),
                Ok(n),
                Ok(detail),
                Ok(progress),
                Ok(spinner),
                Ok(toggle),
                Ok(stop),
                Ok(queue),
                Ok(retry),
                Ok(reveal),
                Ok(delete),
                Ok(map),
                Ok(blocks),
            ) = (
                st.downcast::<gtk4::Label>(),
                n.downcast::<gtk4::Label>(),
                d.downcast::<gtk4::Label>(),
                p.downcast::<gtk4::ProgressBar>(),
                s.downcast::<adw::Spinner>(),
                t.downcast::<gtk4::Button>(),
                x.downcast::<gtk4::Button>(),
                q.downcast::<gtk4::Button>(),
                r.downcast::<gtk4::Button>(),
                o.downcast::<gtk4::Button>(),
                y.downcast::<gtk4::Button>(),
                rv.downcast::<gtk4::Revealer>(),
                mp.downcast::<gtk4::DrawingArea>(),
            )
            else {
                tracing::warn!("Grab: unexpected row widget types; skipping row refresh");
                return;
            };
            n.set_text(&it.filename());
            st.set_text(&it.status().label());
            refresh_row(
                it,
                &RowWidgets {
                    detail,
                    progress,
                    spinner,
                    toggle_btn: toggle,
                    stop_btn: stop,
                    queue_btn: queue,
                    retry_btn: retry,
                    reveal_btn: reveal,
                    delete_btn: delete,
                    map_revealer: map,
                    blocks,
                    expanded: Rc::clone(&exp_sync),
                },
                another_queued(&m_sync, it),
                m_sync.is_live_video(it.id()),
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
            map_revealer: map_revealer.clone(),
            blocks: blocks.clone(),
            expanded: Rc::clone(&expanded),
        },
        another_queued(manager, item),
        manager.is_live_video(item.id()),
    );
    // Indeterminate activity runs on wall-clock, not progress ticks.
    // Resolving ("Resolving media…") emits no property changes, so a
    // pulse driven by refresh_row alone freezes on one frame — the
    // reported hang. This tick advances only indeterminate bars
    // (active with no fraction, or live) and dies with the row; real
    // fractions keep rendering from progress notifies as before.
    {
        let bar = progress.downgrade();
        let weak_item = item.downgrade();
        let m = Rc::clone(manager);
        glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
            let (Some(bar), Some(it)) = (bar.upgrade(), weak_item.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if should_pulse(it.status(), m.is_live_video(it.id()), it.progress()) {
                bar.pulse();
            }
            glib::ControlFlow::Continue
        });
    }

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
                t.add_toast(adw::Toast::new(&gettext("Queued for later")));
            } else {
                t.add_toast(adw::Toast::new(&gettext("No other downloads waiting")));
            }
        });
    }
    {
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        reveal_btn.connect_clicked(move |_| {
            if let Some(it) = m.find(id) {
                launch_path(&it.display_path(), &t, true);
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
                t.add_toast(adw::Toast::new(&gettext("Moved to Trash")));
            }
        });
    }
    {
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        remove_btn.connect_clicked(move |_| {
            let Some(it) = m.find(id) else { return };
            let snapshot = RemovedSnapshot {
                url: it.url().to_string(),
                dest_dir: it.dest_dir().to_string(),
                filename: it.filename().to_string(),
                status: it.status(),
                progress: it.progress(),
                detail: it.detail().to_string(),
                output_dir: it.output_dir().to_string(),
                segments: m.segments_of(id),
                video_source: m.video_source(id),
            };
            let name = snapshot.filename.clone();
            m.remove(id);
            let toast = adw::Toast::new(&gettext("Removed {name}").replace("{name}", &name));
            toast.set_button_label(Some(&gettext("Undo")));
            let m2 = Rc::clone(&m);
            toast.connect_button_clicked(move |_| {
                m2.unremove(snapshot.clone());
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
    settings: crate::settings::AppSettings,
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
    options.insert("reason", gettext("Downloading files"));
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
    if !(settings.inhibit_suspend() && manager.has_transferring()) {
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
            gettext("Downloads continue in the background after the window is closed"),
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
    settings: crate::settings::AppSettings,
    toasts: Rc<adw::ToastOverlay>,
) -> (adw::ApplicationWindow, gtk4::SearchBar) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Grab")
        .default_width(settings.window_width().max(400))
        .default_height(settings.window_height().max(300))
        .build();

    settings
        .bind(crate::settings::key::WINDOW_WIDTH, &window, "default-width")
        .build();
    settings
        .bind(
            crate::settings::key::WINDOW_HEIGHT,
            &window,
            "default-height",
        )
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Grab"),
        &gettext("Download Manager"),
    )));

    let menu = gio::Menu::new();
    menu.append(Some(&gettext("New Download")), Some("app.add-download"));
    let section = gio::Menu::new();
    section.append(Some(&gettext("Cancel All")), Some("app.cancel-all"));
    section.append(Some(&gettext("Retry Failed")), Some("app.retry-failed"));
    section.append(Some(&gettext("Clear Finished")), Some("app.clear-finished"));
    section.append(
        Some(&gettext("Open Download Folder")),
        Some("app.open-folder"),
    );
    menu.append_section(None, &section);
    let section2 = gio::Menu::new();
    section2.append(Some(&gettext("Preferences")), Some("app.preferences"));
    section2.append(Some(&gettext("Keyboard Shortcuts")), Some("app.shortcuts"));
    section2.append(Some(&gettext("About")), Some("app.about"));
    menu.append_section(None, &section2);
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text(gettext("Main Menu"))
        .build();
    menu_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("Main Menu"))]);
    header.pack_start(&menu_btn);

    let search_toggle = gtk4::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text(gettext("Search (Ctrl+F)"))
        .build();
    search_toggle.update_property(&[gtk4::accessible::Property::Label(&gettext("Search"))]);
    header.pack_end(&search_toggle);

    let add_btn = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["suggested-action"])
        .tooltip_text(gettext("New Download (Ctrl+N)"))
        .build();
    add_btn.update_property(&[gtk4::accessible::Property::Label(&gettext("New Download"))]);
    {
        let m = Rc::clone(&manager);
        add_btn.connect_clicked(move |_| show_add_dialog(m.clone(), None));
    }
    header.pack_end(&add_btn);

    let stack = adw::ViewStack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("folder-download-symbolic")
        .title(gettext("No Downloads Yet"))
        .description(gettext("Add a download to get started"))
        .build();
    let empty_add = gtk4::Button::builder()
        .label(gettext("New Download"))
        .css_classes(["pill", "suggested-action"])
        .halign(gtk4::Align::Center)
        .build();
    empty.set_child(Some(&empty_add));
    {
        let m = Rc::clone(&manager);
        empty_add.connect_clicked(move |_| show_add_dialog(m.clone(), None));
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
    let (active_section, active_list) = section_list(&gettext("Active"));
    let (queued_section, queued_list) = section_list(&gettext("Queued"));
    let (downloaded_section, downloaded_list) = section_list(&gettext("Downloaded"));
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
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
    /// DropDown position for a status (0 = All). Order must match the
    /// model built below.
    fn status_filter_index(s: crate::download::DownloadStatus) -> u32 {
        use crate::download::DownloadStatus::*;
        match s {
            Downloading => 1,
            Paused => 2,
            Queued => 3,
            Done => 4,
            Failed => 5,
            Cancelled => 6,
        }
    }

    // Search + status filter above the sections: rows that don't match
    // are hidden in sync(), and empty sections collapse as usual.
    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let status_sel: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(gettext("Search downloads"))
        .hexpand(true)
        .build();
    let status_names = gtk4::StringList::new(&[]);
    for name in [
        gettext("All"),
        gettext("Downloading"),
        gettext("Paused"),
        gettext("Queued"),
        gettext("Done"),
        gettext("Failed"),
        gettext("Cancelled"),
    ] {
        status_names.append(&name);
    }
    let status_drop = gtk4::DropDown::builder()
        .model(&status_names)
        .selected(0)
        .valign(gtk4::Align::Center)
        .build();
    status_drop.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Filter by status",
    ))]);
    // HIG search pattern: a header toggle reveals a GtkSearchBar beneath
    // the header; it may also hold extra widgets like the status filter.
    let search_bar = gtk4::SearchBar::builder().show_close_button(true).build();
    let filter_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    filter_box.append(&search);
    filter_box.append(&status_drop);
    search_bar.set_child(Some(&filter_box));
    search_bar.connect_entry(&search);
    search_bar.set_key_capture_widget(Some(&window));
    search_toggle
        .bind_property("active", &search_bar, "search-mode-enabled")
        .bidirectional()
        .sync_create()
        .build();
    content.append(&active_section);
    content.append(&queued_section);
    content.append(&downloaded_section);

    let rows: Rc<RefCell<HashMap<u64, gtk4::ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let sync: Rc<dyn Fn()> = {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        let r = Rc::clone(&rows);
        let add = add_btn.clone();
        let search_btn = search_toggle.clone();
        let s = stack.clone();
        let l_active = active_list.clone();
        let l_queued = queued_list.clone();
        let l_downloaded = downloaded_list.clone();
        let sec_active = active_section.clone();
        let sec_queued = queued_section.clone();
        let sec_downloaded = downloaded_section.clone();
        let query = Rc::clone(&query);
        let status_sel = Rc::clone(&status_sel);
        Rc::new(move || {
            let store = m.store();
            let q = query.borrow();
            let sel = status_sel.get();
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
                    let mut shown = sel == 0 || status_filter_index(it.status()) == sel;
                    if shown && !q.is_empty() && !it.filename().to_lowercase().contains(q.as_str())
                    {
                        shown = false;
                    }
                    if shown {
                        if is_done(&it) {
                            n_downloaded += 1;
                        } else if is_queued(&it) {
                            n_queued += 1;
                        } else {
                            n_active += 1;
                        }
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
                    row.set_visible(shown);
                }
            }
            let stale: Vec<u64> = r
                .borrow()
                .keys()
                .filter(|id| !present.contains(id))
                .cloned()
                .collect();
            for id in stale {
                if let Some(row) = r.borrow_mut().remove(&id)
                    && let Some(old) = row.parent().and_downcast::<gtk4::ListBox>()
                {
                    old.remove(&row);
                }
            }
            sec_active.set_visible(n_active > 0);
            sec_queued.set_visible(n_queued > 0);
            sec_downloaded.set_visible(n_downloaded > 0);
            let has_items = store.n_items() > 0;
            // Header + duplicates the empty-state pill, so show it only with the list.
            add.set_visible(has_items);
            search_btn.set_visible(has_items);
            if !has_items {
                // List is gone, so nothing to search: hide the toggle and
                // collapse the bar through the bidirectional binding.
                search_btn.set_active(false);
            }
            s.set_visible_child_name(if has_items { "list" } else { "empty" });
        })
    };

    {
        let sync = Rc::clone(&sync);
        let q = Rc::clone(&query);
        search.connect_search_changed(move |s| {
            *q.borrow_mut() = s.text().to_lowercase();
            sync();
        });
    }
    {
        let sync = Rc::clone(&sync);
        let sel = Rc::clone(&status_sel);
        status_drop.connect_selected_notify(move |d| {
            sel.set(d.selected());
            sync();
        });
    }

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
                if m.background_notifications_enabled()
                    && let Some(app) = gio::Application::default()
                {
                    let n =
                        gio::Notification::new(&gettext("Downloads continue in the background"));
                    n.set_default_action_and_target_value("app.present", None);
                    app.send_notification(Some(BACKGROUND_NOTIF_ID), &n);
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

    let banner = adw::Banner::new(&gettext("Some downloads failed"));
    banner.set_button_label(Some(&gettext("Retry Failed")));
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
    toolbar.add_top_bar(&search_bar);
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
            if idle_hidden && let Some(app) = app_weak.upgrade() {
                app.withdraw_notification(BACKGROUND_NOTIF_ID);
                app.quit();
            }
        });
        manager.set_on_change({
            let hook = Rc::clone(&hook);
            move || hook()
        });
        hook();
    }

    (window, search_bar)
}

/// Rename a completed or queued row. The manager enforces what can be
/// renamed; failures (mid-transfer, torrent, bad name) show inline.
fn show_rename_dialog(
    manager: Rc<DownloadManager>,
    id: u64,
    current: String,
    anchor: &gtk4::ListBoxRow,
) {
    let dialog = adw::Dialog::builder()
        .title(gettext("Rename Download"))
        .build();
    dialog.set_follows_content_size(true);
    dialog.set_content_width(380);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    page.add(&group);

    let name_row = adw::EntryRow::builder()
        .title(gettext("File name"))
        .text(current)
        .activates_default(true)
        .build();
    group.add(&name_row);

    let error_label = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    group.add(&error_label);

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    let cancel_btn = gtk4::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();
    let rename_btn = gtk4::Button::builder()
        .label(gettext("_Rename"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&cancel_btn);
    hb.pack_end(&rename_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&rename_btn));

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
        let dialog = dialog.downgrade();
        let error_label = error_label.clone();
        let name_row = name_row.clone();
        rename_btn.connect_clicked(move |_| match m.rename_download(id, &name_row.text()) {
            Ok(()) => {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
            }
            Err(e) => {
                error_label.set_text(&e);
                error_label.set_visible(true);
                name_row.add_css_class("error");
            }
        });
    }

    name_row.grab_focus();
    dialog.present(anchor.root().as_ref());
}

/// Present a dialog on the active window when there is one, standalone
/// otherwise (e.g. action fired while hidden).
fn present_dialog(dialog: &adw::Dialog) {
    let win = gio::Application::default()
        .and_downcast::<adw::Application>()
        .and_then(|app| app.active_window());
    dialog.present(win.as_ref());
}

/// Widgets of the New Download dialog's video step, living on the
/// details navigation page. Managed as one unit: exactly one state
/// visible at a time (resolving spinner, preview, missing-tools prompt,
/// or load error). The group header itself carries the video identity
/// (title + page URL).
struct VideoStep {
    status: adw::ActionRow,
    group: adw::PreferencesGroup,
    name: adw::EntryRow,
    revert: gtk4::Button,
    quality: adw::ComboRow,
    audio: adw::SwitchRow,
    tools: adw::ActionRow,
    error: adw::ActionRow,
}

/// Queue a probed link as a plain file and close the dialog: the
/// fallback when extraction finds no playable media (or fails) on an
/// unlisted page. Returns false when even the plain intake rejects the
/// URL, so the caller can show the error instead.
fn queue_plain(
    manager: &Rc<DownloadManager>,
    dest: &Rc<RefCell<String>>,
    dialog: &glib::WeakRef<adw::Dialog>,
    file_row: &adw::EntryRow,
    url: &str,
) -> Result<(), String> {
    let typed = file_row.text().trim().to_string();
    let name = (!typed.is_empty()).then_some(typed);
    manager.enqueue(url, Some(&dest.borrow()), name.as_deref())?;
    if let Some(d) = dialog.upgrade() {
        d.close();
    }
    Ok(())
}

/// Desensitize the details-page Add button while a lookup resolves.
/// No-op until the button exists (see `lookup_add`); every terminal
/// lookup state re-enables it.
fn set_lookup_add(cell: &Rc<RefCell<Option<gtk4::Button>>>, enabled: bool) {
    if let Some(button) = cell.borrow().as_ref() {
        button.set_sensitive(enabled);
    }
}

fn hide_video_step(v: &VideoStep) {
    v.status.set_visible(false);
    v.name.set_visible(false);
    v.revert.set_visible(false);
    v.quality.set_visible(false);
    v.audio.set_visible(false);
    v.tools.set_visible(false);
    v.error.set_visible(false);
}

fn show_video_loading(v: &VideoStep) {
    hide_video_step(v);
    v.status.set_visible(true);
}

fn show_video_ready(v: &VideoStep) {
    hide_video_step(v);
    v.name.set_visible(true);
    v.revert.set_visible(true);
    v.quality.set_visible(true);
    v.audio.set_visible(true);
}

fn show_video_tools_missing(v: &VideoStep, message: &str) {
    hide_video_step(v);
    v.tools.set_subtitle(message);
    v.tools.set_visible(true);
}

fn show_video_error(v: &VideoStep, message: &str) {
    hide_video_step(v);
    v.error.set_subtitle(message);
    v.error.set_visible(true);
}

/// Item-count label for a probed collection, kind-aware ("3 stories").
fn playlist_count_label(kind: crate::video::PlaylistKind, count: usize) -> String {
    let template = match kind {
        crate::video::PlaylistKind::Stories => ngettext("{} story", "{} stories", count as u32),
        crate::video::PlaylistKind::Highlights => {
            ngettext("{} highlight", "{} highlights", count as u32)
        }
        crate::video::PlaylistKind::Playlist => ngettext("{} item", "{} items", count as u32),
    };
    template.replace("{}", &count.to_string())
}

/// Playlist probe state: the group header carries the collection
/// identity (title + item count). The name row and format picker stay
/// hidden — renames and format pins don't apply across items — while
/// the audio switch stays visible and seeds the picker for every
/// queued item.
fn show_video_playlist(v: &VideoStep, pl: &crate::video::PlaylistInfo) {
    hide_video_step(v);
    v.group
        .set_title(glib::markup_escape_text(&pl.title).as_str());
    let mut desc = format!(
        "{} • {}",
        playlist_count_label(pl.kind, pl.items.len()),
        glib::markup_escape_text(&pl.page_url)
    );
    if crate::video::playlist_truncated(pl) {
        desc.push_str(" • ");
        desc.push_str(
            &gettext("Showing the first {n} of {total}")
                .replace("{n}", &pl.items.len().to_string())
                .replace("{total}", &pl.total.to_string()),
        );
    }
    v.group.set_description(Some(&desc));
    v.audio.set_visible(true);
}

/// New-download dialog, optionally pre-filled (drag-and-drop / Open With
/// hands a URL in; the normal lookup flow then takes over, including
/// video-page detection, so drops never bypass the media pipeline).
pub fn show_add_dialog(manager: Rc<DownloadManager>, initial_url: Option<&str>) {
    let dialog = adw::Dialog::builder()
        .title(gettext("New Download"))
        .build();
    dialog.set_follows_content_size(true);
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    page.add(&group);

    let url_row = adw::EntryRow::builder()
        .title(gettext("URL"))
        .text("")
        .show_apply_button(true)
        .activates_default(true)
        .build();
    url_row.set_input_purpose(gtk4::InputPurpose::Url);
    group.add(&url_row);

    let file_row = adw::EntryRow::builder()
        .title(gettext("File name (optional)"))
        .text("")
        .activates_default(true)
        .build();
    group.add(&file_row);

    // Video step (details navigation page): preview rows for video-page
    // URLs. All hidden until a lookup runs; exactly one state shows.
    let video_group = adw::PreferencesGroup::new();
    let video_status = adw::ActionRow::builder()
        .title(gettext("Looking up media…"))
        .build();
    let video_spinner = gtk4::Spinner::new();
    video_spinner.start();
    video_status.add_suffix(&video_spinner);
    video_status.set_visible(false);
    video_group.add(&video_status);
    let video_name = adw::EntryRow::builder()
        .title(gettext("File name"))
        .activates_default(false)
        .build();
    let video_revert_btn = gtk4::Button::builder()
        .icon_name("edit-undo-symbolic")
        .css_classes(["flat"])
        .tooltip_text(gettext("Revert to Title"))
        .valign(gtk4::Align::Center)
        .build();
    video_revert_btn.update_property(&[gtk4::accessible::Property::Label(&gettext(
        "Revert to Title",
    ))]);
    video_name.add_suffix(&video_revert_btn);
    video_name.set_visible(false);
    video_revert_btn.set_visible(false);
    video_group.add(&video_name);
    // Format picker, filled per video on resolve: exact pinnable
    // formats, tallest first (the preference preselects the closest
    // row). Starts with a single Automatic row — global preference,
    // no pin — until the first lookup lands, and returns to it when a
    // page lists nothing pinnable.
    let video_quality = adw::ComboRow::builder()
        .title(gettext("Media format"))
        .subtitle(gettext("Uses your preferred quality"))
        .model(&gtk4::StringList::new(&[gettext("Automatic").as_str()]))
        .build();
    video_quality.set_visible(false);
    video_group.add(&video_quality);
    let video_audio = adw::SwitchRow::builder()
        .title(gettext("Audio only"))
        .subtitle(gettext("Skip the video track"))
        .build();
    video_audio.set_visible(false);
    video_group.add(&video_audio);
    let video_tools = adw::ActionRow::builder()
        .title(gettext("Support tools"))
        .build();
    let video_install_btn = gtk4::Button::builder()
        .label(gettext("Install"))
        .tooltip_text(gettext("Download the yt-dlp support tools"))
        .valign(gtk4::Align::Center)
        .build();
    // Whole-row click hits Install (same pattern as the torrent row below).
    video_tools.set_activatable_widget(Some(&video_install_btn));
    video_tools.add_suffix(&video_install_btn);
    video_tools.set_visible(false);
    video_group.add(&video_tools);
    let video_error = adw::ActionRow::builder()
        .title(gettext("Couldn't load the media preview"))
        .build();
    let video_retry_btn = gtk4::Button::builder()
        .label(gettext("Retry"))
        .valign(gtk4::Align::Center)
        .build();
    video_error.add_suffix(&video_retry_btn);
    video_error.set_visible(false);
    video_group.add(&video_error);
    let step = Rc::new(VideoStep {
        status: video_status,
        group: video_group,
        name: video_name,
        revert: video_revert_btn,
        quality: video_quality,
        audio: video_audio,
        tools: video_tools,
        error: video_error,
    });
    // Dialog-local choices: quality is initialized from Preferences
    // (not bound); audio-only is always off by design — no global
    // preference exists. A queued row keeps the choices made here.
    // The format picker opens on the preference-preselected row; exact
    // picks are per lookup, so nothing persists here.
    step.quality.set_selected(0);
    // Audio-only is per-download only: always default off (no global
    // preference exists), and the quality row is moot while it is on.
    step.audio.set_active(false);
    {
        let q = step.quality.clone();
        step.audio.connect_active_notify(move |sw| {
            q.set_sensitive(!sw.is_active());
        });
    }

    let torrent_btn = gtk4::Button::builder()
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose a .torrent file"))
        .valign(gtk4::Align::Center)
        .build();
    let torrent_row = adw::ActionRow::builder()
        .title(gettext("Torrent file"))
        .activatable_widget(&torrent_btn)
        .build();
    torrent_row.add_suffix(&torrent_btn);
    group.add(&torrent_row);

    let dest_label = gtk4::Label::builder()
        .label(manager.effective_download_dir())
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(["dimmed", "caption"])
        .hexpand(true)
        .build();
    let dest_btn = gtk4::Button::builder()
        .label(gettext("Choose…"))
        .tooltip_text(gettext("Choose download folder"))
        .valign(gtk4::Align::Center)
        .build();
    let dest_row = adw::ActionRow::builder().title(gettext("Save to")).build();
    dest_row.add_suffix(&dest_label);
    dest_row.add_suffix(&dest_btn);
    group.add(&dest_row);

    let dest_dir = Rc::new(RefCell::new(manager.effective_download_dir()));
    {
        let dd = Rc::clone(&dest_dir);
        let dl = dest_label.clone();
        dest_btn.connect_clicked(move |b| {
            let chooser = gtk4::FileDialog::builder()
                .title(gettext("Choose download folder"))
                .accept_label(gettext("Select Folder"))
                .build();
            let root = b.root().and_downcast::<gtk4::Window>();
            let dd2 = Rc::clone(&dd);
            let dl2 = dl.clone();
            chooser.select_folder(root.as_ref(), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res
                    && let Some(p) = f.path()
                {
                    let s = p.to_string_lossy().into_owned();
                    dl2.set_text(&s);
                    *dd2.borrow_mut() = s;
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

    // Video resolve machinery: debounced metadata lookup that never blocks
    // the main loop. `video_generation` drops stale completions when the user
    // keeps typing; every async touch re-checks the dialog is still open.
    let video_generation = Rc::new(Cell::new(0u64));
    let video_last_ok = Rc::new(RefCell::new(String::new()));
    let video_info = Rc::new(RefCell::new(None::<crate::video::ProbeResult>));
    // Index-aligned with the format combo rows: exact format ids, or
    // a single `None` for the Automatic row. Reset on every resolve.
    let format_ids: Rc<RefCell<Vec<Option<String>>>> = Rc::new(RefCell::new(vec![None]));
    // Set while the submit path re-arms the apply tick (touching the entry
    // text): the changed handler below must ignore that synthetic edit, or
    // every failed Enter-submit would drop the preview and re-resolve.
    let video_quiet = Rc::new(Cell::new(false));
    // The details-page Add button, desensitized while a lookup is in
    // flight: submit already refuses early adds with a message, but a
    // dead button says so upfront. Populated once the button exists.
    let lookup_add: Rc<RefCell<Option<gtk4::Button>>> = Rc::new(RefCell::new(None));
    let kick_video = {
        let generation = video_generation.clone();
        let last_ok = video_last_ok.clone();
        let info = video_info.clone();
        let step2 = step.clone();
        let url_row2 = url_row.clone();
        let file_row2 = file_row.clone();
        let dialog_weak = dialog.downgrade();
        let formats_kick = format_ids.clone();
        let settings2 = manager.settings().clone();
        let lookup_add_kick = lookup_add.clone();
        let manager_kick = manager.clone();
        let dest_kick = dest_dir.clone();
        Rc::new(move |probe_unlisted: bool| {
            let my = generation.get() + 1;
            generation.set(my);
            let (
                generation_b,
                last_b,
                info_b,
                step_b,
                url_b,
                dialog_b,
                settings_b,
                file_b,
                formats_b,
                lookup_add_b,
                manager_b,
                dest_b,
            ) = (
                generation.clone(),
                last_ok.clone(),
                info.clone(),
                step2.clone(),
                url_row2.clone(),
                dialog_weak.clone(),
                settings2.clone(),
                file_row2.clone(),
                formats_kick.clone(),
                lookup_add_kick.clone(),
                manager_kick.clone(),
                dest_kick.clone(),
            );
            glib::spawn_future_local(async move {
                if dialog_b.upgrade().is_none() {
                    return;
                }
                let url = url_b.text().trim().to_string();
                // Unlisted links probe only on explicit kicks (submit,
                // retry) — never while typing, where every prefix would
                // spawn a doomed extraction. Non-HTTP schemes never
                // probe: magnets and friends belong to their own flows.
                let probing = probe_unlisted
                    && !crate::video::is_video_page(&url)
                    && crate::video::is_http_url(&url);
                if url.is_empty() || (!crate::video::is_video_page(&url) && !probing) {
                    // A stale probe result for another URL must not
                    // linger: without this, navigating back with a fresh
                    // typed URL would show the old preview as ready.
                    if !crate::video::preview_fresh(
                        &info_b.borrow(),
                        last_b.borrow().as_str(),
                        &url,
                    ) {
                        hide_video_step(&step_b);
                        info_b.borrow_mut().take();
                        set_lookup_add(&lookup_add_b, true);
                    }
                    return;
                }
                if crate::video::preview_fresh(&info_b.borrow(), last_b.borrow().as_str(), &url) {
                    show_video_ready(&step_b);
                    set_lookup_add(&lookup_add_b, true);
                    return;
                }
                // Fast local tools check first: missing tools show Install
                // with no spinner round-trip.
                let libs = match crate::video::resolve_libraries() {
                    Ok(libs) => libs,
                    Err(e) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        info_b.borrow_mut().take();
                        show_video_tools_missing(&step_b, &e.to_string());
                        set_lookup_add(&lookup_add_b, true);
                        return;
                    }
                };
                show_video_loading(&step_b);
                set_lookup_add(&lookup_add_b, false);
                // Invalid manual proxy fails the lookup loudly, matching
                // the row behavior: no silent direct extraction.
                let proxy = match crate::download::DownloadOptions::from_settings(&settings_b)
                    .proxy_config()
                {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        show_video_error(&step_b, &e);
                        set_lookup_add(&lookup_add_b, true);
                        return;
                    }
                };
                match crate::video::fetch_video_infos(
                    libs,
                    url.clone(),
                    settings_b.cookies_browser(),
                    settings_b.video_codec_newest(),
                    proxy,
                )
                .await
                {
                    Err(e) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        // Probed links fall back to today's outcome (queue
                        // the file directly) only when extraction says
                        // unsupported — transient failures keep the error
                        // row with retry instead of mistyping the row as
                        // plain forever.
                        if !crate::video::is_video_page(&url)
                            && e.to_string().to_lowercase().contains("unsupported url")
                        {
                            match queue_plain(&manager_b, &dest_b, &dialog_b, &file_b, &url) {
                                Ok(()) => return,
                                Err(pe) => {
                                    info_b.borrow_mut().take();
                                    tracing::warn!(
                                        host = %crate::video::page_host(&url),
                                        error = %pe.to_string(),
                                        "plain fallback failed"
                                    );
                                    show_video_error(&step_b, &pe);
                                    set_lookup_add(&lookup_add_b, true);
                                    return;
                                }
                            }
                        }
                        info_b.borrow_mut().take();
                        tracing::warn!(
                            host = %crate::video::page_host(&url),
                            error = %e.to_string(),
                            "video preview failed"
                        );
                        show_video_error(&step_b, &e.to_string());
                        set_lookup_add(&lookup_add_b, true);
                    }
                    Ok(probe) => {
                        if dialog_b.upgrade().is_none() || generation_b.get() != my {
                            return;
                        }
                        // Resolved but nothing playable, and not a listed
                        // video page: same plain fallback as above.
                        if !probe.fetchable() && !crate::video::is_video_page(&url) {
                            match queue_plain(&manager_b, &dest_b, &dialog_b, &file_b, &url) {
                                Ok(()) => return,
                                Err(pe) => {
                                    info_b.borrow_mut().take();
                                    tracing::warn!(
                                        host = %crate::video::page_host(&url),
                                        error = %pe.to_string(),
                                        "plain fallback failed"
                                    );
                                    show_video_error(&step_b, &pe);
                                    set_lookup_add(&lookup_add_b, true);
                                    return;
                                }
                            }
                        }
                        match probe {
                            crate::video::ProbeResult::Single(v) => {
                                // Group header carries the identity (title + page);
                                // rows below carry the choices. Both sinks parse
                                // Pango markup, so escape: page URLs carry `&`
                                // query separators and titles carry anything.
                                let desc =
                                    match v.duration_string.as_deref().filter(|s| !s.is_empty()) {
                                        Some(d) => format!(
                                            "{} • {}",
                                            glib::markup_escape_text(&v.page_url),
                                            glib::markup_escape_text(d)
                                        ),
                                        None => glib::markup_escape_text(&v.page_url).to_string(),
                                    };
                                step_b
                                    .group
                                    .set_title(glib::markup_escape_text(&v.title).as_str());
                                step_b.group.set_description(Some(&desc));
                                // Seed the file name once: an explicit page-1 name
                                // wins, else the title default. Never clobbers an
                                // edit already made here.
                                if step_b.name.text().trim().is_empty() {
                                    let typed = file_b.text().trim().to_string();
                                    let base = if typed.is_empty() {
                                        let remux = crate::video::remux_video_active(
                                            &settings_b.remux_video(),
                                        );
                                        crate::video::default_video_filename(
                                            &v.title,
                                            &v.id,
                                            step_b.audio.is_active(),
                                            remux.as_deref(),
                                        )
                                    } else {
                                        typed
                                    };
                                    step_b.name.set_text(&base);
                                }
                                *last_b.borrow_mut() = url;
                                // Rebuild the format picker from this resolve:
                                // exact pinnable formats, tallest first, with the
                                // preference preselecting the closest row — or a
                                // single Automatic row (global preference, no pin)
                                // when the page lists nothing pinnable. Selection
                                // resets — a pin from another video must never
                                // carry over.
                                let mut labels = Vec::new();
                                let mut ids: Vec<Option<String>> = Vec::new();
                                for opt in &v.formats {
                                    labels.push(opt.label.clone());
                                    ids.push(Some(opt.id.clone()));
                                }
                                if labels.is_empty() {
                                    labels.push(gettext("Automatic"));
                                    ids.push(None);
                                }
                                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                                step_b
                                    .quality
                                    .set_model(Some(&gtk4::StringList::new(&refs)));
                                *formats_b.borrow_mut() = ids;
                                step_b
                                    .quality
                                    .set_selected(crate::video::default_quality_index(
                                        &v.formats,
                                        &settings_b.video_quality(),
                                    ) as u32);
                                *info_b.borrow_mut() = Some(crate::video::ProbeResult::Single(v));
                                show_video_ready(&step_b);
                                set_lookup_add(&lookup_add_b, true);
                            }
                            crate::video::ProbeResult::Playlist(pl) => {
                                if pl.items.is_empty() {
                                    info_b.borrow_mut().take();
                                    show_video_error(
                                        &step_b,
                                        &gettext("No items found in this playlist"),
                                    );
                                    set_lookup_add(&lookup_add_b, true);
                                    return;
                                }
                                *last_b.borrow_mut() = url;
                                *info_b.borrow_mut() =
                                    Some(crate::video::ProbeResult::Playlist(pl.clone()));
                                show_video_playlist(&step_b, &pl);
                                set_lookup_add(&lookup_add_b, true);
                            }
                        }
                    }
                }
            });
        })
    };
    // Debounced auto-lookup while typing (600 ms idle); Enter applies
    // immediately through the submit path below.
    {
        let generation = video_generation.clone();
        let kick = kick_video.clone();
        let dialog_weak = dialog.downgrade();
        let step2 = step.clone();
        let info2 = video_info.clone();
        let quiet = video_quiet.clone();
        let file_row2 = file_row.clone();
        url_row.connect_changed(move |row| {
            // Synthetic edit from the apply re-arm below: ignore it.
            if quiet.get() {
                return;
            }
            let text = row.text().trim().to_string();
            // The direct-only file row hides in video mode (the details
            // page has its own name row); a non-empty entry is not lost —
            // the resolve seeds the video name from it. A probed preview
            // counts as video mode while its canonical URL still matches.
            let fresh = info2
                .borrow()
                .as_ref()
                .is_some_and(|p| p.page_url() == text);
            file_row2.set_visible(!(crate::video::is_video_page(&text) || fresh));
            // Sync skeleton: leaving video-land (or editing a resolved URL)
            // hides the stale step at once; the debounced kick refills
            // it. `fresh` is deliberately the stricter canonical compare
            // (not the round-trip key the kick uses): a mismatch is
            // always safe to hide, and typing only fires on edit.
            if !crate::video::is_video_page(&text) || !fresh {
                hide_video_step(&step2);
                if !fresh {
                    info2.borrow_mut().take();
                }
            }
            let my = generation.get() + 1;
            generation.set(my);
            let (generation_b, kick_b, dialog_b) =
                (generation.clone(), kick.clone(), dialog_weak.clone());
            glib::spawn_future_local(async move {
                glib::timeout_future(std::time::Duration::from_millis(600)).await;
                if dialog_b.upgrade().is_none() || generation_b.get() != my {
                    return;
                }
                kick_b(false);
            });
        });
    }
    {
        let step2 = step.clone();
        let kick = kick_video.clone();
        let dialog_weak = dialog.downgrade();
        let btn = video_install_btn.clone();
        // Outside Flatpak there is no bundled binary and host packages
        // can't be installed from here: guide through self-install
        // instead of the automatic download.
        if !crate::video::in_flatpak() {
            btn.set_label(&gettext("How to Install"));
            btn.set_tooltip_text(Some(&gettext("Show terminal install instructions")));
        }
        video_install_btn.connect_clicked(move |_| {
            if !crate::video::in_flatpak() {
                let kick_b = kick.clone();
                crate::install_help::show(&btn, move || kick_b(true));
                return;
            }
            // Flatpak: staged auto-install under a progress popover
            // anchored at the button (HIG: feedback lives with its
            // control; stages stand in for percentages that don't exist).
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
            let (btn_b, pop_b, label_b, step_b, kick_b, dialog_b) = (
                btn.clone(),
                pop.clone(),
                pop_label.clone(),
                step2.clone(),
                kick.clone(),
                dialog_weak.clone(),
            );
            glib::spawn_future_local(async move {
                if let Err(e) = crate::video::install_ytdlp().await {
                    pop_b.popdown();
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    step_b.tools.set_subtitle(&e.to_string());
                    btn_b.set_sensitive(true);
                    return;
                }
                label_b.set_text(&gettext("Downloading ffmpeg (2 of 2)…"));
                if let Err(e) = crate::video::install_ffmpeg().await {
                    pop_b.popdown();
                    if dialog_b.upgrade().is_none() {
                        return;
                    }
                    step_b.tools.set_subtitle(&e.to_string());
                    btn_b.set_sensitive(true);
                    return;
                }
                pop_b.popdown();
                if dialog_b.upgrade().is_none() {
                    return;
                }
                btn_b.set_sensitive(true);
                // Re-probe, don't just refresh: an unlisted URL that led
                // here for missing tools has no preview yet.
                kick_b(true);
            });
        });
    }
    {
        let kick = kick_video.clone();
        video_retry_btn.connect_clicked(move |_| kick(true));
    }
    // One-click restore of the title default (audio-aware, like submit).
    {
        let (name, audio, info) = (step.name.clone(), step.audio.clone(), video_info.clone());
        let settings = manager.settings().clone();
        step.revert.connect_clicked(move |_| {
            if let Some(p) = info.borrow().as_ref() {
                let remux = crate::video::remux_video_active(&settings.remux_video());
                name.set_text(&crate::video::default_video_filename(
                    p.title(),
                    p.video_id(),
                    audio.is_active(),
                    remux.as_deref(),
                ));
                name.grab_focus();
            }
        });
    }
    // Toggling the mode re-seeds an untouched name: the resolve-time
    // seed ran under the other mode, so without this the row keeps a
    // video-container name for an audio download (or vice versa). An edited name
    // is never clobbered.
    {
        let (name, audio, info) = (step.name.clone(), step.audio.clone(), video_info.clone());
        let settings = manager.settings().clone();
        audio.connect_active_notify(move |sw| {
            if let Some(p) = info.borrow().as_ref() {
                let active = sw.is_active();
                let current = name.text().to_string();
                let remux = crate::video::remux_video_active(&settings.remux_video());
                if current.trim().is_empty()
                    || current
                        == crate::video::default_video_filename(
                            p.title(),
                            p.video_id(),
                            !active,
                            remux.as_deref(),
                        )
                {
                    name.set_text(&crate::video::default_video_filename(
                        p.title(),
                        p.video_id(),
                        active,
                        remux.as_deref(),
                    ));
                }
            }
        });
    }

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
                filter.set_name(Some(&gettext("Torrent files")));
                filter.add_mime_type("application/x-bittorrent");
                filter.add_pattern("*.torrent");
                let filters = gio::ListStore::new::<gtk4::FileFilter>();
                filters.append(&filter);
                let picker = gtk4::FileDialog::builder()
                    .title(gettext("Choose torrent file"))
                    .accept_label(gettext("Add Torrent"))
                    .filters(&filters)
                    .build();
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
                        error_label.set_text(&gettext("Could not read that .torrent file"));
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
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    let cancel_btn = gtk4::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();
    let add_btn = gtk4::Button::builder()
        .label(gettext("_Add Download"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    hb.pack_start(&cancel_btn);
    hb.pack_end(&add_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));

    // Details page: the video step lives here behind an explicit Continue,
    // so the final Add is unreachable without a resolved preview. The back
    // button is provided by the navigation view.
    let video_page = adw::PreferencesPage::new();
    video_page.add(&step.group);
    let video_toolbar = adw::ToolbarView::new();
    let video_hb = adw::HeaderBar::new();
    // No WM title buttons either end: close paths are the nav back
    // button and Esc, like the sibling dialog headers (which keep an
    // explicit Cancel instead).
    video_hb.set_show_start_title_buttons(false);
    video_hb.set_show_end_title_buttons(false);
    let final_add_btn = gtk4::Button::builder()
        .label(gettext("_Add Download"))
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    video_hb.pack_end(&final_add_btn);
    video_toolbar.add_top_bar(&video_hb);
    video_toolbar.set_content(Some(&video_page));
    // Wire the lookup gate: the kick above desensitizes this button while
    // resolving and re-enables it at every terminal state.
    lookup_add.replace(Some(final_add_btn.clone()));

    let entry_nav_page = adw::NavigationPage::builder()
        .tag("entry")
        .title(gettext("New Download"))
        .can_pop(false)
        .child(&toolbar)
        .build();
    let video_nav_page = adw::NavigationPage::builder()
        .tag("video")
        .title(gettext("Media Details"))
        .child(&video_toolbar)
        .build();
    let nav = adw::NavigationView::new();
    nav.push(&entry_nav_page);

    dialog.set_child(Some(&nav));
    dialog.set_default_widget(Some(&add_btn));

    {
        let d = dialog.downgrade();
        cancel_btn.connect_clicked(move |_| {
            if let Some(d) = d.upgrade() {
                d.close();
            }
        });
    }
    // One submit path for the Add button and URL apply: video pages go
    // through the Page intake (a matching preview is required so the row
    // stores the resolved page, not a stale URL), everything else keeps
    // the direct enqueue.
    let submit = {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let url_row = url_row.clone();
        let file_row = file_row.clone();
        let error_label = error_label.clone();
        let dialog_weak = dialog.downgrade();
        let info = video_info.clone();
        let last_ok = video_last_ok.clone();
        let step2 = step.clone();
        let kick = kick_video.clone();
        let quiet = video_quiet.clone();
        let nav2 = nav.clone();
        let video_nav_page2 = video_nav_page.clone();
        let formats = format_ids.clone();
        let lookup_add_submit = lookup_add.clone();
        move |rearm_apply: bool| {
            let fail = |message: &str| {
                error_label.set_text(message);
                error_label.set_visible(true);
                url_row.add_css_class("error");
                if rearm_apply {
                    // ponytail: libadwaita hides its apply tick before
                    // emitting `apply`; touch the text while focused to
                    // re-arm it. Quiet: the video changed handler must not
                    // treat this as a user edit.
                    quiet.set(true);
                    let current = url_row.text().to_string();
                    url_row.grab_focus();
                    url_row.set_text("");
                    url_row.set_text(&current);
                    quiet.set(false);
                }
            };
            let close = || {
                if let Some(d) = dialog_weak.upgrade() {
                    d.close();
                }
            };
            let url = url_row.text().trim().to_string();
            if crate::video::is_video_page(&url) {
                // Structural guarantee: the final Add lives on the details
                // page, so from the entry page a video URL only ever
                // advances (and starts resolving) — it can never queue
                // without its preview.
                if nav2.visible_page_tag().as_deref() != Some("video") {
                    nav2.push(&video_nav_page2);
                    step2.name.grab_focus();
                    kick(false);
                    return;
                }
                // Same freshness gate as the kick skip above: the stored
                // page URL is canonicalized, so only the round-trip key
                // (which text was resolved) decides.
                let ready =
                    if crate::video::preview_fresh(&info.borrow(), last_ok.borrow().as_str(), &url)
                    {
                        info.borrow().clone()
                    } else {
                        None
                    };
                match ready {
                    Some(probe) => match probe {
                        crate::video::ProbeResult::Single(v) => {
                            let typed = step2.name.text().trim().to_string();
                            let audio_only = step2.audio.is_active();
                            // Default name from the video title and id; the intake
                            // sanitizes it and falls back to the URL stem.
                            let remux =
                                crate::video::remux_video_active(&m.settings().remux_video());
                            let auto = typed.is_empty().then(|| {
                                crate::video::default_video_filename(
                                    &v.title,
                                    &v.id,
                                    audio_only,
                                    remux.as_deref(),
                                )
                            });
                            let name = if typed.is_empty() {
                                auto.as_deref()
                            } else {
                                Some(typed.as_str())
                            };
                            // Exact picks pin the format and carry its height
                            // as the fallback, so a dropped pin still
                            // degrades to the chosen height. Audio-only rows
                            // drop the pin (nothing to pin a track to).
                            // The Automatic row (no pin: pre-resolve, or pages
                            // listing nothing pinnable) falls back to the
                            // global preference. The combo rows and the info
                            // formats share one order.
                            let selected = step2.quality.selected() as usize;
                            let format_id = formats.borrow().get(selected).cloned().flatten();
                            let format_id = if audio_only { None } else { format_id };
                            let quality = match format_id.clone() {
                                Some(id) => v
                                    .formats
                                    .iter()
                                    .find(|opt| opt.id == id)
                                    .map(|opt| {
                                        crate::video::quality_for_height(opt.height).to_string()
                                    })
                                    .unwrap_or_else(|| m.settings().video_quality()),
                                None => m.settings().video_quality(),
                            };
                            match m.enqueue_video(
                                &v.page_url,
                                Some(&dd.borrow()),
                                name,
                                crate::video::VideoChoices {
                                    quality,
                                    audio_only,
                                    video_format_id: format_id,
                                    is_live: v.is_live,
                                    playlist_item_id: None,
                                },
                            ) {
                                Ok(_) => close(),
                                Err(e) => {
                                    show_video_error(&step2, &e);
                                    set_lookup_add(&lookup_add_submit, true);
                                }
                            }
                        }
                        // Collections queue through the item picker: one
                        // row per chosen entry, each re-resolving its own
                        // page at download time.
                        crate::video::ProbeResult::Playlist(pl) => {
                            push_playlist_items_page(
                                &nav2,
                                m.clone(),
                                dd.clone(),
                                dialog_weak.clone(),
                                pl,
                                step2.audio.is_active(),
                            );
                        }
                    },
                    None => {
                        kick(true);
                        show_video_error(
                            &step2,
                            &gettext(
                                "Still looking up the media — wait for the preview, then add.",
                            ),
                        );
                    }
                }
                return;
            }
            // Unlisted http(s) links get one probe for a video path, unless
            // they are obviously direct files (extension sniff — the
            // plain engine downloads those better anyway, with no probe
            // delay): the details page resolves, and either shows the
            // video step or falls back to a plain queue. Anything else
            // skips straight to the plain intake below.
            if crate::video::is_http_url(&url) && !crate::video::is_direct_file_url(&url) {
                if nav2.visible_page_tag().as_deref() != Some("video") {
                    nav2.push(&video_nav_page2);
                    step2.name.grab_focus();
                    kick(true);
                } else if !crate::video::preview_fresh(
                    &info.borrow(),
                    last_ok.borrow().as_str(),
                    &url,
                ) {
                    // Resubmit while already probing: re-kick so the user
                    // gets feedback instead of silence.
                    kick(true);
                    show_video_error(
                        &step2,
                        &gettext("Still looking up the media — wait for the preview, then add."),
                    );
                }
                return;
            }
            // Apply on an empty field stays silent (stray Enter); the Add
            // button surfaces the intake error instead.
            if url.is_empty() && rearm_apply {
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
                Ok(_) => close(),
                Err(e) => fail(&e),
            }
        }
    };
    let submit = Rc::new(submit);
    {
        let s = submit.clone();
        add_btn.connect_clicked(move |_| s(false));
    }
    {
        let s = submit.clone();
        url_row.connect_apply(move |_| s(true));
    }
    // Details page actions run the same submit: on this page a video URL
    // takes the enqueue branch; anything else falls through to direct.
    {
        let s = submit.clone();
        final_add_btn.connect_clicked(move |_| s(false));
    }
    {
        let s = submit.clone();
        step.name.connect_apply(move |_| s(false));
    }
    // Contextual verb: the entry button continues to details for video
    // links and ambiguous http(s) links (probed), and queues obvious
    // direct files plus anything else straight away.
    {
        let b = add_btn.clone();
        let ur = url_row.clone();
        ur.connect_changed(move |row| {
            let text = row.text().trim().to_string();
            if crate::video::is_video_page(&text)
                || (crate::video::is_http_url(&text) && !crate::video::is_direct_file_url(&text))
            {
                b.set_label(&gettext("_Continue"));
            } else {
                b.set_label(&gettext("_Add Download"));
            }
        });
    }

    present_dialog(&dialog);

    // Dropped/opened URLs land here pre-filled: setting the text fires
    // the same changed → debounce → lookup chain as typing, so video
    // pages resolve through the media pipeline (the clipboard read
    // below stands down on non-empty fields by itself).
    if let Some(url) = initial_url.map(str::trim).filter(|u| !u.is_empty())
        && let Ok(normalized) = crate::download::normalize_url(url)
    {
        url_row.set_text(&normalized);
    }

    // Keyboard-first: focus lands in the URL field so typing starts a
    // download with no tab stops (same pattern as the rename dialog).
    url_row.grab_focus();

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
    dialog.set_follows_content_size(true);
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Files"))
        .description(
            ngettext("{} file", "{} files", entries.len() as u32)
                .replace("{}", &entries.len().to_string()),
        )
        .build();
    page.add(&group);

    // HIG selection, not settings: a switch means "a setting is on",
    // a checkbox means "this item is picked". Clicking a row toggles
    // its checkbox.
    let mut checks = Vec::new();
    for e in &entries {
        let check = gtk4::CheckButton::builder().active(true).build();
        check.update_property(&[gtk4::accessible::Property::Label(&e.path)]);
        let row = adw::ActionRow::builder()
            .title(&e.path)
            .subtitle(crate::download::fmt_bytes(e.length))
            .activatable(true)
            .build();
        row.add_prefix(&check);
        {
            let check = check.clone();
            row.connect_activate(move |_| {
                check.set_active(!check.is_active());
            });
        }
        checks.push(check);
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
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    let cancel_btn = gtk4::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();
    hb.pack_start(&cancel_btn);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&page));
    // HIG selection mode: the selection's actions live in a bottom
    // action bar, not the header.
    let action_bar = gtk4::ActionBar::new();
    let select_all_btn = gtk4::Button::builder().label(gettext("Select All")).build();
    let select_none_btn = gtk4::Button::builder()
        .label(gettext("Select None"))
        .build();
    let add_btn = gtk4::Button::builder()
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    action_bar.pack_start(&select_all_btn);
    action_bar.pack_start(&select_none_btn);
    action_bar.pack_end(&add_btn);
    toolbar.add_bottom_bar(&action_bar);
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&add_btn));

    // The action counts the live selection; with nothing selected it
    // reads "Add 0 files" and clicking it shows the error label.
    let refresh_add = Rc::new({
        let add_btn = add_btn.clone();
        let checks = checks.clone();
        move || {
            let n = checks.iter().filter(|c| c.is_active()).count();
            add_btn.set_label(
                &ngettext("_Add {} file", "_Add {} files", n as u32).replace("{}", &n.to_string()),
            );
        }
    });
    for check in &checks {
        let refresh = refresh_add.clone();
        check.connect_toggled(move |_| refresh());
    }
    {
        let checks = checks.clone();
        select_all_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(true);
            }
        });
    }
    {
        let checks = checks.clone();
        select_none_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(false);
            }
        });
    }
    refresh_add();

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
            let selected: Vec<usize> = checks
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_active())
                .map(|(i, _)| i)
                .collect();
            if selected.is_empty() {
                error_label.set_text(&gettext("Select at least one file"));
                error_label.set_visible(true);
                return;
            }
            // All on means no filter: pass None, not every index.
            let only = (selected.len() < checks.len()).then_some(selected);
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

/// Seconds as M:SS / H:MM:SS for picker subtitles.
fn fmt_item_duration(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Item picker for probed playlists, stories and highlights, mirroring
/// [`show_torrent_files_dialog`]: one checkbox per entry, all checked
/// by default (HIG selection: checkboxes pick items, switches flip
/// settings). Each chosen item becomes its own queue row through the Page
/// intake, so formats resolve per item at download time — the probe
/// only listed them. Quality follows the global preference (no pin:
/// pins don't survive across items); the audio-only choice from the
/// New Download dialog applies to every queued row.
/// The picker is a page in the New Download navigation stack (tag
/// "playlist"), not a standalone dialog: one window, one close path,
/// and the navigation header's Back button.
fn push_playlist_items_page(
    nav: &adw::NavigationView,
    manager: Rc<DownloadManager>,
    dest_dir: Rc<RefCell<String>>,
    parent: glib::WeakRef<adw::Dialog>,
    playlist: crate::video::PlaylistInfo,
    audio_only: bool,
) {
    // Same guard as the video page: don't stack a second picker while
    // one is already visible.
    if nav.visible_page_tag().as_deref() == Some("playlist") {
        return;
    }

    let page = adw::PreferencesPage::new();
    let count = playlist.items.len();
    let group = adw::PreferencesGroup::builder()
        .title(playlist_count_label(playlist.kind, count))
        .build();
    if crate::video::playlist_truncated(&playlist) {
        group.set_description(Some(
            &gettext("Showing the first {n} of {total}")
                .replace("{n}", &count.to_string())
                .replace("{total}", &playlist.total.to_string()),
        ));
    }
    page.add(&group);

    let mut checks = Vec::new();
    for item in &playlist.items {
        let check = gtk4::CheckButton::builder().active(true).build();
        check.update_property(&[gtk4::accessible::Property::Label(&item.title)]);
        let row = adw::ActionRow::builder()
            .title(&item.title)
            .activatable(true)
            .build();
        if let Some(d) = item.duration {
            row.set_subtitle(&fmt_item_duration(d));
        }
        row.add_prefix(&check);
        {
            let check = check.clone();
            row.connect_activate(move |_| {
                check.set_active(!check.is_active());
            });
        }
        checks.push(check);
        group.add(&row);
    }
    let error_label = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    group.add(&error_label);

    // Scrolled: big playlists must not size the dialog off-screen, but
    // propagate the natural height (capped) so the dialog grows and
    // shrinks with the item count instead of keeping the previous
    // page's size and scrolling a short list.
    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&page)
        .vexpand(true)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();

    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::new();
    // No WM title buttons either end and no explicit Cancel: the
    // navigation header's Back button (and Esc) close the picker, like
    // the Media Details page. The selection's action lives in the
    // bottom action bar, per HIG selection mode.
    hb.set_show_start_title_buttons(false);
    hb.set_show_end_title_buttons(false);
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&scrolled));
    let action_bar = gtk4::ActionBar::new();
    let select_all_btn = gtk4::Button::builder().label(gettext("Select All")).build();
    let select_none_btn = gtk4::Button::builder()
        .label(gettext("Select None"))
        .build();
    let add_btn = gtk4::Button::builder()
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    action_bar.pack_start(&select_all_btn);
    action_bar.pack_start(&select_none_btn);
    action_bar.pack_end(&add_btn);
    toolbar.add_bottom_bar(&action_bar);
    let picker_page = adw::NavigationPage::builder()
        .tag("playlist")
        .title(&playlist.title)
        .child(&toolbar)
        .build();

    // The action counts the live selection; with nothing selected it
    // reads "Queue 0 items" and clicking it shows the error label,
    // mirroring the torrent picker.
    let refresh_add = Rc::new({
        let add_btn = add_btn.clone();
        let checks = checks.clone();
        move || {
            let n = checks.iter().filter(|c| c.is_active()).count();
            add_btn.set_label(
                &ngettext("_Queue {} item", "_Queue {} items", n as u32)
                    .replace("{}", &n.to_string()),
            );
        }
    });
    for check in &checks {
        let refresh = refresh_add.clone();
        check.connect_toggled(move |_| refresh());
    }
    {
        let checks = checks.clone();
        select_all_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(true);
            }
        });
    }
    {
        let checks = checks.clone();
        select_none_btn.connect_clicked(move |_| {
            for c in &checks {
                c.set_active(false);
            }
        });
    }
    refresh_add();

    {
        let parent_weak = parent.clone();
        add_btn.connect_clicked(move |_| {
            let chosen: Vec<(usize, &crate::video::PlaylistItem)> = playlist
                .items
                .iter()
                .enumerate()
                .filter(|(i, _)| checks[*i].is_active())
                .collect();
            if chosen.is_empty() {
                error_label.set_text(&gettext("Select at least one item"));
                error_label.set_visible(true);
                return;
            }
            // One persist for the whole import, not one per row.
            manager.begin_batch();
            let remux = crate::video::remux_video_active(&manager.settings().remux_video());
            // Story segments are addressable as their own pages: queue
            // those, so each row re-resolves its own segment instead of
            // the tray (tray + format ids downloads the first segment
            // once per row). Attempted unconditionally: highlights and
            // non-story URLs return None here and keep the tray with the
            // persisted entry id as fallback.
            let mut failed: Option<String> = None;
            for (i, item) in &chosen {
                let page_url = crate::video::story_segment_url(&playlist.page_url, &item.id)
                    .unwrap_or_else(|| item.page_url.clone());
                let name = crate::video::default_video_filename(
                    &item.title,
                    &item.id,
                    audio_only,
                    remux.as_deref(),
                );
                if let Err(e) = manager.enqueue_video(
                    &page_url,
                    Some(&dest_dir.borrow()),
                    Some(&name),
                    crate::video::VideoChoices {
                        quality: manager.settings().video_quality(),
                        audio_only,
                        video_format_id: None,
                        // Live streams queued from a playlist take the VOD
                        // path; the worker re-resolves each item page anyway.
                        is_live: false,
                        // Remember the picked entry as fallback: story rows
                        // normally carry segment pages (see
                        // `story_segment_url`) and never need it, but
                        // highlights — and anything unparseable at pick
                        // time — re-resolve the tray, so the worker
                        // selects the picked entry out of it by this id.
                        playlist_item_id: Some(item.id.clone()),
                    },
                ) {
                    failed = Some(e);
                    break;
                }
                // Rows already queued stay queued on a partial failure:
                // uncheck them so a retry only submits the remainder
                // instead of duplicating them (dedupe is by filename).
                checks[*i].set_active(false);
            }
            manager.end_batch();
            if let Some(e) = failed {
                error_label.set_text(&e);
                error_label.set_visible(true);
                return;
            }
            // Complete success closes the whole New Download dialog; a
            // partial failure stays on the picker so the remaining rows
            // can be retried (their checkboxes were unchecked above).
            if let Some(p) = parent_weak.upgrade() {
                p.close();
            }
        });
    }

    // Enter queues the selection while the picker is up; the dialog's
    // previous default widget is restored when the page is popped.
    if let Some(p) = parent.upgrade() {
        let prev_default = p.default_widget();
        p.set_default_widget(Some(&add_btn));
        let parent_weak = parent.clone();
        nav.connect_popped(move |_, popped| {
            if popped.tag().as_deref() == Some("playlist")
                && let Some(p) = parent_weak.upgrade()
            {
                p.set_default_widget(prev_default.as_ref());
            }
        });
    }

    nav.push(&picker_page);
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
