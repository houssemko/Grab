//! Row widgets: list-row construction, refresh ticks, pulse
//! gating and the small builders around them. UI module (gtk/adw +
//! leaves): the window builder consumes `build_row`; dialogs use
//! the labels; tests pin `should_pulse` directly.

use crate::download::{DownloadManager, RemovedSnapshot};
use crate::download_pieces::{BLOCK_CELLS, aggregate};
use crate::download_store::DownloadStatus;
use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::Cell;
use std::rc::Rc;

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

/// Default file name for a probed video under the current preferences:
/// title default with the remux target and audio mode applied. The
/// intake sanitizes it and falls back to the URL stem.
pub(crate) fn default_name_for(
    settings: &crate::settings::AppSettings,
    title: &str,
    audio_only: bool,
) -> String {
    let remux = crate::video_prefs::remux_video_active(&settings.remux_video());
    crate::video::default_video_filename(title, audio_only, remux.as_deref())
}

/// ngettext with the "{n}" count slot filled: the msgids keep "{n}"
/// so translators can reposition the count, filled here for display.
pub(crate) fn ngettext_count(singular: &str, plural: &str, n: usize) -> String {
    ngettext(singular, plural, n as u32).replace("{n}", &n.to_string())
}

/// Hidden error caption for a preferences group: callers set its text
/// and show it on failure. One builder so all dialogs stay identical.
pub(crate) fn error_label(group: &adw::PreferencesGroup) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    group.add(&label);
    label
}

/// HIG selection-mode action bar shared by the pickers: Select All /
/// Select None start-packed, the confirm action end-packed as
/// suggested. Returns the bar and its three buttons for the caller to
/// wire and attach to its toolbar. One builder so both dialogs stay
/// identical.
pub(crate) fn selection_action_bar() -> (gtk4::ActionBar, gtk4::Button, gtk4::Button, gtk4::Button)
{
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
    (action_bar, select_all_btn, select_none_btn, add_btn)
}

/// Open `path` with the system's default application for its file type —
/// the same as double-clicking the file in the file manager. Used for
/// double-click/Enter on a finished download row.
fn open_with_default_app(path: &std::path::Path, toasts: &adw::ToastOverlay) {
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
    let t = toasts.clone();
    let what = path.to_string_lossy().into_owned();
    glib::spawn_future_local(async move {
        if let Err(e) = launcher.launch_future(None::<&gtk4::Window>).await {
            t.add_toast(adw::Toast::new(
                &gettext("Could not open {what} with its default application: {e}")
                    .replace("{what}", &what)
                    .replace("{e}", &e.to_string()),
            ));
        }
    });
}

/// Launch `path` with its default handler (`reveal` shows the containing
/// folder with the file selected instead). For a directory the default
/// handler is the file manager.
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

/// Which of the two meanings the row's stop button currently carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopCopy {
    /// A normal download: stopping it discards the bytes.
    Cancel,
    /// A live capture: stopping it keeps the recording.
    StopRecording,
}

/// The stop button is the one control whose meaning inverts — cancelling
/// a normal download discards it, stopping a live capture keeps the
/// recording — so the copy has to follow the row's state rather than
/// staying "Cancel" for both.
///
/// The decision is pure and testable; the wording is not here on purpose.
/// `xgettext` only extracts *literal* `gettext("…")` arguments, so
/// returning strings from this function and translating them at the call
/// site would put every label permanently out of reach of translators.
/// The enum keeps the logic testable while the UI keeps the literals at
/// the translation boundary.
pub(crate) fn stop_copy(is_live: bool) -> StopCopy {
    if is_live {
        StopCopy::StopRecording
    } else {
        StopCopy::Cancel
    }
}

/// Set a row button's icon, tooltip, and screen-reader label from one
/// verb: the three always agree for state-toggle buttons, so binding
/// them keeps the accessible name from drifting off the visual one.
fn set_toggle_verb(btn: &gtk4::Button, icon: &str, tip: &str) {
    btn.set_icon_name(icon);
    btn.set_tooltip_text(Some(tip));
    btn.update_property(&[gtk4::accessible::Property::Label(tip)]);
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
    // Header capitalization for the tooltip, sentence case for the
    // accessible name — an a11y label is a spoken phrase, not a caption.
    let (stop_tip, stop_a11y) = match stop_copy(live_capturing) {
        StopCopy::Cancel => (gettext("Cancel"), gettext("Cancel")),
        StopCopy::StopRecording => (gettext("Stop Recording"), gettext("Stop recording")),
    };
    w.stop_btn.set_tooltip_text(Some(&stop_tip));
    w.stop_btn
        .update_property(&[gtk4::accessible::Property::Label(&stop_a11y)]);
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

    let (icon, tip) = if item.status() == DownloadStatus::Paused {
        ("media-playback-start-symbolic", gettext("Resume"))
    } else {
        ("media-playback-pause-symbolic", gettext("Pause"))
    };
    set_toggle_verb(&w.toggle_btn, icon, &tip);

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

/// Weak refs to one row's refreshable widgets. Named fields instead of
/// a positional tuple: every element has the same type, so a tuple
/// silently accepts a swapped pair and the mistake only shows up as a
/// wrong widget at refresh time.
struct RowWeaks {
    detail: glib::WeakRef<gtk4::Widget>,
    progress: glib::WeakRef<gtk4::Widget>,
    spinner: glib::WeakRef<gtk4::Widget>,
    toggle_btn: glib::WeakRef<gtk4::Widget>,
    stop_btn: glib::WeakRef<gtk4::Widget>,
    queue_btn: glib::WeakRef<gtk4::Widget>,
    retry_btn: glib::WeakRef<gtk4::Widget>,
    reveal_btn: glib::WeakRef<gtk4::Widget>,
    delete_btn: glib::WeakRef<gtk4::Widget>,
    map_revealer: glib::WeakRef<gtk4::Widget>,
    blocks: glib::WeakRef<gtk4::Widget>,
    status: glib::WeakRef<gtk4::Widget>,
    name: glib::WeakRef<gtk4::Widget>,
}

/// Strongly-held row widgets for one refresh tick: the two text labels
/// plus the refresh bundle.
struct LiveRow {
    status: gtk4::Label,
    name: gtk4::Label,
    widgets: RowWidgets,
}

/// Upgrade a row's weak refs to strong typed widgets. `None` when
/// widgets are gone (row destroyed — normal, silent) or mistyped (UI
/// drift — warns here, never panics, per the no-panic-rows rule).
fn upgrade_row(weaks: &RowWeaks, expanded: &Rc<Cell<bool>>) -> Option<LiveRow> {
    let RowWeaks {
        detail: w_detail,
        progress: w_prog,
        spinner: w_spin,
        toggle_btn: w_tog,
        stop_btn: w_stop,
        queue_btn: w_queue,
        retry_btn: w_retry,
        reveal_btn: w_reveal,
        delete_btn: w_del,
        map_revealer: w_rev,
        blocks: w_map,
        status: w_status,
        name: w_name,
    } = weaks;
    let (
        Some(detail_w),
        Some(progress_w),
        Some(spinner_w),
        Some(tog_w),
        Some(stop_w),
        Some(queue_w),
        Some(retry_w),
        Some(reveal_w),
        Some(del_w),
        Some(rev_w),
        Some(map_w),
        Some(status_w),
        Some(name_w),
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
    )
    else {
        return None;
    };
    let (
        Ok(status),
        Ok(name),
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
        status_w.downcast::<gtk4::Label>(),
        name_w.downcast::<gtk4::Label>(),
        detail_w.downcast::<gtk4::Label>(),
        progress_w.downcast::<gtk4::ProgressBar>(),
        spinner_w.downcast::<adw::Spinner>(),
        tog_w.downcast::<gtk4::Button>(),
        stop_w.downcast::<gtk4::Button>(),
        queue_w.downcast::<gtk4::Button>(),
        retry_w.downcast::<gtk4::Button>(),
        reveal_w.downcast::<gtk4::Button>(),
        del_w.downcast::<gtk4::Button>(),
        rev_w.downcast::<gtk4::Revealer>(),
        map_w.downcast::<gtk4::DrawingArea>(),
    )
    else {
        tracing::warn!("Grab: unexpected row widget types; skipping row refresh");
        return None;
    };
    Some(LiveRow {
        status,
        name,
        widgets: RowWidgets {
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
            expanded: Rc::clone(expanded),
        },
    })
}

pub(crate) fn build_row(
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

    // Click the row body: single-click reveals the block map on live
    // rows, double-click opens a finished download. Clicks landing on a
    // button belong to the button: walk up from the pick target and
    // ignore those. Only live rows expand (finished ones have no map),
    // and only with map data to show (resolving or chunk-less rows
    // ignore the click).
    {
        let click = gtk4::GestureClick::new();
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        let rev = map_revealer.clone();
        let blk = blocks.clone();
        let exp = Rc::clone(&expanded);
        let pop_menu = pop.clone();
        click.connect_pressed(move |gesture, n_press, x, y| {
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
            let Some(it) = m.find(id) else {
                return;
            };
            // Double-click (or double-tap) a finished row opens the
            // downloaded file. Only the second press opens: triple-clicks
            // and beyond do nothing.
            if n_press == 2 && it.status() == DownloadStatus::Done {
                open_with_default_app(&it.display_path(), &t);
                return;
            }
            let live = matches!(
                it.status(),
                DownloadStatus::Downloading | DownloadStatus::Paused
            );
            // No map without data: resolving rows and chunk-less videos
            // have nothing to reveal, so the click does nothing. The
            // toggle stays on the first press so a double-click never
            // flips the map twice.
            if n_press == 1 && live && !m.piece_bitmap(id).is_empty() {
                exp.set(!exp.get());
                rev.set_reveal_child(exp.get());
                blk.queue_draw();
            }
        });
        row.add_controller(click);
    }

    // Enter on a focused finished row opens the downloaded file, the
    // keyboard counterpart of double-click; F2 renames the focused row,
    // the keyboard counterpart of the right-click menu. The row itself
    // must hold focus (not a button inside it) so activating a button
    // never opens the file or pops the rename dialog as a side effect.
    {
        let key = gtk4::EventControllerKey::new();
        let m = Rc::clone(manager);
        let t = Rc::clone(toasts);
        key.connect_key_pressed(move |controller, keyval, _, _| {
            let rename = keyval == gtk4::gdk::Key::F2;
            let open = keyval == gtk4::gdk::Key::Return || keyval == gtk4::gdk::Key::KP_Enter;
            if !rename && !open {
                return glib::Propagation::Proceed;
            }
            let Some(row) = controller
                .widget()
                .and_downcast::<gtk4::ListBoxRow>()
                .filter(|r| r.is_focus())
            else {
                return glib::Propagation::Proceed;
            };
            if rename {
                if let Some(it) = m.find(id) {
                    show_rename_dialog(m.clone(), id, it.filename(), &row);
                }
            } else if let Some(it) = m.find(id)
                && it.status() == DownloadStatus::Done
            {
                open_with_default_app(&it.display_path(), &t);
            }
            glib::Propagation::Stop
        });
        row.add_controller(key);
    }

    let w = |w: &gtk4::Widget| w.downgrade();
    let weaks = RowWeaks {
        detail: w(detail.upcast_ref()),
        progress: w(progress.upcast_ref()),
        spinner: w(spinner.upcast_ref()),
        toggle_btn: w(toggle_btn.upcast_ref()),
        stop_btn: w(stop_btn.upcast_ref()),
        queue_btn: w(queue_btn.upcast_ref()),
        retry_btn: w(retry_btn.upcast_ref()),
        reveal_btn: w(reveal_btn.upcast_ref()),
        delete_btn: w(delete_btn.upcast_ref()),
        map_revealer: w(map_revealer.upcast_ref()),
        blocks: w(blocks.upcast_ref()),
        status: w(status.upcast_ref()),
        name: w(name.upcast_ref()),
    };
    let m_sync = Rc::clone(manager);
    let exp_sync = Rc::clone(&expanded);
    let updater = move |it: &crate::download::DownloadItem| {
        let Some(row) = upgrade_row(&weaks, &exp_sync) else {
            return;
        };
        row.name.set_text(&it.filename());
        row.status.set_text(&it.status().label());
        refresh_row(
            it,
            &row.widgets,
            another_queued(&m_sync, it),
            m_sync.is_live_video(it.id()),
        );
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
        let t = Rc::clone(toasts);
        stop_btn.connect_clicked(move |_| {
            // Read the live flag first: `cancel` may change the row's
            // state, and the toast is about the state the user clicked
            // in, not the one they were left in.
            let copy = stop_copy(m.is_live_video(id));
            m.cancel(id);
            // The HIG says not to rely on a tooltip for essential
            // information — it is unavailable on touch. Whether the
            // recording survives is essential, and this is the one
            // control whose meaning inverts, so it gets a real channel.
            //
            // Progress, not completion: at click time the remux has not
            // run and can still fail (a capture stopped before any bytes
            // records nothing and the row goes Failed), so promising a
            // finished save would be a promise the code cannot keep.
            if copy == StopCopy::StopRecording {
                t.add_toast(adw::Toast::new(&gettext("Saving recording…")));
            }
        });
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

    let error_label = error_label(&group);

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

    crate::ui_util::close_on_click(&cancel_btn, &dialog);
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
