//! Application lifecycle: startup (actions once), activate (window),
//! open (URLs/files), shutdown (stop downloads, persist queue).

use crate::download::DownloadManager;
use crate::window::{self, show_add_dialog};
use crate::{preferences, APP_ID};
use adw::prelude::*;
use gtk4::gio;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

struct State {
    manager: Rc<DownloadManager>,
    settings: gio::Settings,
    toasts: Rc<adw::ToastOverlay>,
    window: adw::ApplicationWindow,
}

pub fn setup(app: &adw::Application) {
    let state: Rc<RefCell<Option<Rc<State>>>> = Rc::new(RefCell::new(None));

    {
        let st = Rc::clone(&state);
        app.connect_startup(move |app| {
            let settings = gio::Settings::new(APP_ID);
            let store = gio::ListStore::new::<crate::download::DownloadItem>();
            let manager = DownloadManager::new(store, settings.clone());
            manager.restore_queue();

            let toasts = Rc::new(adw::ToastOverlay::new());
            let win = window::build_window(app, manager.clone(), settings.clone(), toasts.clone());
            st.borrow_mut().replace(Rc::new(State {
                manager: manager.clone(),
                settings,
                toasts,
                window: win,
            }));

            register_actions(app, &st);
            app.set_accels_for_action("app.add-download", &["<Control>n"]);
            app.set_accels_for_action("app.quit", &["<Control>q"]);
            app.set_accels_for_action("app.preferences", &["<Control>comma"]);
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_activate(move |app| {
            app.withdraw_notification(window::BACKGROUND_NOTIF_ID);
            if let Some(s) = st.borrow().as_ref() {
                s.window.present();
            }
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_open(move |_, files, _| {
            let s = match st.borrow().as_ref().cloned() {
                Some(s) => s,
                None => return,
            };
            for f in files {
                if let Ok(uri) = f.uri().parse::<url::Url>() {
                    if matches!(uri.scheme(), "http" | "https" | "ftp") {
                        if let Err(e) = s.manager.enqueue(uri.as_str(), None, None) {
                            s.toasts.add_toast(adw::Toast::new(&e));
                        }
                        continue;
                    }
                }
                if let Some(path) = f.path() {
                    const MAX_LIST_BYTES: u64 = 1_000_000;
                    const MAX_LIST_LINES: usize = 1000;
                    if std::fs::metadata(&path)
                        .map(|m| m.len() > MAX_LIST_BYTES)
                        .unwrap_or(true)
                    {
                        continue;
                    }
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        for line in text
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .take(MAX_LIST_LINES)
                        {
                            if let Err(e) = s.manager.enqueue(line, None, None) {
                                s.toasts.add_toast(adw::Toast::new(&e));
                            }
                        }
                    }
                }
            }
            s.window.present();
        });
    }

    {
        let st = Rc::clone(&state);
        app.connect_shutdown(move |_| {
            if let Some(s) = st.borrow().as_ref() {
                s.manager.shutdown();
            }
        });
    }
}

fn register_actions(app: &adw::Application, st: &Rc<RefCell<Option<Rc<State>>>>) {
    let entries = [
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("add-download")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        show_add_dialog(s.manager.clone());
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("cancel-all")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        s.manager.cancel_all();
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("retry-failed")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        s.manager.retry_failed();
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("open-folder")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        let dir = s.manager.effective_download_dir();
                        window::open_folder(std::path::Path::new(&dir), &s.toasts);
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("preferences")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref().cloned() {
                        preferences::show(&s.window, &s.settings);
                    }
                })
                .build()
        },
        {
            let st = Rc::clone(st);
            gio::ActionEntry::builder("about")
                .activate(move |_, _, _| {
                    if let Some(s) = st.borrow().as_ref() {
                        let about = adw::AboutDialog::builder()
                            .application_name("Grab")
                            .application_icon(APP_ID)
                            .version(env!("CARGO_PKG_VERSION"))
                            .developer_name("Grab Contributors")
                            .license_type(gtk4::License::MitX11)
                            .website("https://github.com/houssemko/grab")
                            .issue_url("https://github.com/houssemko/grab/issues")
                            .comments("A GNOME download manager")
                            .build();
                        about.present(Some(&s.window));
                    }
                })
                .build()
        },
        gio::ActionEntry::builder("quit")
            .activate(|app: &adw::Application, _, _| app.quit())
            .build(),
        gio::ActionEntry::builder("present")
            .activate(|app: &adw::Application, _, _| app.activate())
            .build(),
    ];
    app.add_action_entries(entries);
}
