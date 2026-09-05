//! Main window: code-built libadwaita UI (no Blueprint compiler needed).
//!
//! `AdwApplicationWindow > AdwToastOverlay > AdwToolbarView > AdwViewStack`
//! with an empty-state `AdwStatusPage` and a `ListBox` of download rows.
//! Rows update via `notify::` handlers with weak widget refs — no threads,
//! no factory recycling bugs.

use crate::download::{DownloadManager, DownloadStatus};
use adw::prelude::*;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

fn icon_button(icon: &str, tooltip: &str) -> gtk4::Button {
    gtk4::Button::builder()
        .icon_name(icon)
        .css_classes(["flat"])
        .tooltip_text(tooltip)
        .valign(gtk4::Align::Center)
        .build()
}

/// Reveal a downloaded file in the file manager (Nautilus opens with the
/// file selected). Uses GtkFileLauncher: portal-aware, so it also works
/// sandboxed in Flatpak. Failures always surface as a toast — launching
/// must never fail silently.
pub fn reveal_file(path: &std::path::Path, toasts: &adw::ToastOverlay) {
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
    let t = toasts.clone();
    let what = path.to_string_lossy().into_owned();
    glib::spawn_future_local(async move {
        if let Err(e) = launcher
            .open_containing_folder_future(None::<&gtk4::Window>)
            .await
        {
            t.add_toast(adw::Toast::new(&format!(
                "Could not show {what} in the file manager: {e}"
            )));
        }
    });
}

/// Open a folder itself in the file manager.
pub fn open_folder(path: &std::path::Path, toasts: &adw::ToastOverlay) {
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
    let t = toasts.clone();
    let what = path.to_string_lossy().into_owned();
    glib::spawn_future_local(async move {
        if let Err(e) = launcher.launch_future(None::<&gtk4::Window>).await {
            t.add_toast(adw::Toast::new(&format!("Could not open {what}: {e}")));
        }
    });
}

fn refresh_row(
    item: &crate::download::DownloadItem,
    detail: &gtk4::Label,
    progress: &gtk4::ProgressBar,
    spinner: &adw::Spinner,
    toggle_btn: &gtk4::Button,
    stop_btn: &gtk4::Button,
    reveal_btn: &gtk4::Button,
) {
    let frac = item.progress().clamp(0.0, 1.0);
    progress.set_fraction(frac);
    let active = item.status() == DownloadStatus::Downloading;
    spinner.set_visible(active);
    detail.set_text(&item.detail());

    let running = matches!(
        item.status(),
        DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
    );
    toggle_btn.set_visible(running);
    stop_btn.set_visible(running);
    reveal_btn.set_visible(item.status() == DownloadStatus::Done);

    if item.status() == DownloadStatus::Paused {
        toggle_btn.set_icon_name("media-playback-start-symbolic");
        toggle_btn.set_tooltip_text(Some("Resume"));
    } else {
        toggle_btn.set_icon_name("media-playback-pause-symbolic");
        toggle_btn.set_tooltip_text(Some("Pause"));
    }
}

fn build_row(
    item: &crate::download::DownloadItem,
    manager: &Rc<DownloadManager>,
    toasts: &Rc<adw::ToastOverlay>,
) -> gtk4::ListBoxRow {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
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
    let reveal_btn = icon_button("folder-open-symbolic", "Show in Folder");
    let remove_btn = icon_button("user-trash-symbolic", "Remove");

    top.append(&name);
    top.append(&status);
    top.append(&spinner);
    top.append(&toggle_btn);
    top.append(&stop_btn);
    top.append(&reveal_btn);
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

    // Live updates: weak refs only, so rows never leak after removal.
    let w = |w: &gtk4::Widget| w.downgrade();
    let (w_detail, w_prog, w_spin, w_tog, w_stop, w_reveal, w_status, w_name) = (
        w(detail.upcast_ref()),
        w(progress.upcast_ref()),
        w(spinner.upcast_ref()),
        w(toggle_btn.upcast_ref()),
        w(stop_btn.upcast_ref()),
        w(reveal_btn.upcast_ref()),
        w(status.upcast_ref()),
        w(name.upcast_ref()),
    );
    let updater = move |it: &crate::download::DownloadItem| {
        if let (Some(d), Some(p), Some(s), Some(t), Some(x), Some(o), Some(st), Some(n)) = (
            w_detail.upgrade(),
            w_prog.upgrade(),
            w_spin.upgrade(),
            w_tog.upgrade(),
            w_stop.upgrade(),
            w_reveal.upgrade(),
            w_status.upgrade(),
            w_name.upgrade(),
        ) {
            let d: gtk4::Label = d.downcast().unwrap();
            let p: gtk4::ProgressBar = p.downcast().unwrap();
            let s: adw::Spinner = s.downcast().unwrap();
            let t: gtk4::Button = t.downcast().unwrap();
            let x: gtk4::Button = x.downcast().unwrap();
            let o: gtk4::Button = o.downcast().unwrap();
            let st: gtk4::Label = st.downcast().unwrap();
            let n: gtk4::Label = n.downcast().unwrap();
            n.set_text(&it.filename());
            st.set_text(it.status().label());
            refresh_row(it, &d, &p, &s, &t, &x, &o);
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
        &detail,
        &progress,
        &spinner,
        &toggle_btn,
        &stop_btn,
        &reveal_btn,
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
        let t = Rc::clone(toasts);
        reveal_btn.connect_clicked(move |_| {
            if let Some(it) = m.find(id) {
                reveal_file(&it.file_path(), &t);
            }
        });
    }
    {
        let m = Rc::clone(manager);
        remove_btn.connect_clicked(move |_| m.remove(id));
    }
    row
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

    // App menu.
    let menu = gio::Menu::new();
    menu.append(Some("New Download"), Some("app.add-download"));
    let section = gio::Menu::new();
    section.append(Some("Cancel All"), Some("app.cancel-all"));
    section.append(Some("Retry Failed"), Some("app.retry-failed"));
    section.append(Some("Open Download Folder"), Some("app.open-folder"));
    menu.append_section(None, &section);
    let section2 = gio::Menu::new();
    section2.append(Some("Preferences"), Some("app.preferences"));
    section2.append(Some("About"), Some("app.about"));
    menu.append_section(None, &section2);
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .build();
    header.pack_start(&menu_btn);

    let add_btn = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["suggested-action"])
        .tooltip_text("New Download (Ctrl+N)")
        .build();
    {
        let m = Rc::clone(&manager);
        let t = Rc::clone(&toasts);
        add_btn.connect_clicked(move |_| show_add_dialog(m.clone(), t.clone()));
    }
    header.pack_end(&add_btn);

    // Content: empty status vs download list.
    let stack = adw::ViewStack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("folder-download-symbolic")
        .title("No Downloads Yet")
        .description("Add a download to get started. Files are fetched with wget.")
        .build();
    stack.add_named(&empty, Some("empty"));

    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    stack.add_named(&scroll, Some("list"));

    let rows: Rc<RefCell<HashMap<u64, gtk4::ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    {
        let m = Rc::clone(&manager);
        let l = list.clone();
        let r = Rc::clone(&rows);
        let s = stack.clone();
        let t = Rc::clone(&toasts);
        manager
            .store()
            .connect_items_changed(move |store, _, _, _| {
                // Membership sync (lists are small; full scan is cheapest correct).
                let mut present = std::collections::HashSet::new();
                for i in 0..store.n_items() {
                    if let Some(it) = store
                        .item(i)
                        .and_downcast::<crate::download::DownloadItem>()
                    {
                        present.insert(it.id());
                        if !r.borrow().contains_key(&it.id()) {
                            let row = build_row(&it, &m, &t);
                            r.borrow_mut().insert(it.id(), row.clone());
                            l.append(&row);
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
                        l.remove(&row);
                    }
                }
                s.set_visible_child_name(if store.n_items() > 0 { "list" } else { "empty" });
            });
    }
    // Initial population.
    for i in 0..manager.store().n_items() {
        if let Some(it) = manager
            .store()
            .item(i)
            .and_downcast::<crate::download::DownloadItem>()
        {
            let row = build_row(&it, &manager, &toasts);
            rows.borrow_mut().insert(it.id(), row.clone());
            list.append(&row);
        }
    }
    stack.set_visible_child_name(if manager.store().n_items() > 0 {
        "list"
    } else {
        "empty"
    });

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(toasts.as_ref()));

    // Keep action sensitivity fresh.
    {
        let app_weak = app.downgrade();
        let m = Rc::clone(&manager);
        manager.set_on_change(move || {
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
        });
    }

    window
}

/// "New Download" dialog.
pub fn show_add_dialog(manager: Rc<DownloadManager>, _toasts: Rc<adw::ToastOverlay>) {
    let dialog = adw::Dialog::builder().title("New Download").build();
    dialog.set_content_width(420);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    page.add(&group);

    let url_row = adw::EntryRow::builder()
        .title("URL")
        .text("")
        .show_apply_button(false)
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
        // Parent the portal dialog on the AdwDialog's root once presented.
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
        let d = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            d.close();
        });
    }
    {
        let m = manager.clone();
        let dd = dest_dir.clone();
        let url_row = url_row.clone();
        let file_row = file_row.clone();
        let error_label = error_label.clone();
        let dialog = dialog.clone();
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
                    dialog.close();
                }
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                }
            }
        });
    }
    {
        let m = manager.clone();
        let dd = dest_dir.clone();
        url_row.connect_apply(move |row| {
            let url = row.text().trim().to_string();
            if url.is_empty() {
                return;
            }
            match m.enqueue(&url, Some(&dd.borrow()), None) {
                Ok(_) => {}
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                }
            }
        });
    }

    // Present on the active window.
    if let Some(app) = gio::Application::default().and_downcast::<adw::Application>() {
        if let Some(win) = app.active_window() {
            dialog.present(Some(&win));
        }
    }
}
