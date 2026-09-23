//! Row object: the `DownloadItem` GTK list item, from GObject subclass
//! through wrapper to accessors. Leaf-adjacent module (gtk plus
//! download_store status): the manager, views and tests consume it
//! through the `download` facade.

use gtk4::glib;

mod imp {
    use std::cell::{Cell, RefCell};

    use crate::download_store::DownloadStatus;
    use gtk4::glib;
    use gtk4::glib::object::ObjectExt as _;
    use gtk4::glib::subclass::prelude::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::DownloadItem)]
    pub struct DownloadItem {
        #[property(get, set)]
        pub id: Cell<u64>,
        #[property(get, set)]
        pub url: RefCell<String>,
        #[property(get, set)]
        pub filename: RefCell<String>,
        #[property(get, set)]
        pub dest_dir: RefCell<String>,
        #[property(get, set, builder(DownloadStatus::Queued))]
        pub status: Cell<DownloadStatus>,
        #[property(get, set)]
        pub progress: Cell<f64>,
        #[property(get, set)]
        pub detail: RefCell<String>,
        /// Engine's real output folder for torrents (magnets land in
        /// `dest/<stub>/`; empty means unknown, use `file_path()`).
        /// Recorded at enqueue, persisted in the queue, trashed on delete.
        #[property(get, set)]
        pub output_dir: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DownloadItem {
        const NAME: &'static str = "GrabDownloadItem";
        type Type = super::DownloadItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for DownloadItem {}
}

glib::wrapper! {
    pub struct DownloadItem(ObjectSubclass<imp::DownloadItem>);
}

impl DownloadItem {
    /// Create a list item; prefer [`DownloadManager::enqueue`] which dedupes.
    pub fn new(id: u64, url: &str, filename: &str, dest_dir: &str) -> Self {
        glib::Object::builder()
            .property("id", id)
            .property("url", url)
            .property("filename", filename)
            .property("dest-dir", dest_dir)
            .build()
    }

    /// Full destination path (`dest_dir` joined with `filename`).
    pub fn file_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.dest_dir()).join(self.filename())
    }

    /// Best on-disk guess for reveal: inside the recorded engine folder
    /// when one exists, else the plain destination path.
    pub fn display_path(&self) -> std::path::PathBuf {
        let output_dir = self.output_dir();
        if output_dir.is_empty() {
            self.file_path()
        } else {
            std::path::Path::new(&output_dir).join(self.filename())
        }
    }
}
