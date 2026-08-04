use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gtk::glib;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FileEntryObject {
        pub name: RefCell<String>,
        pub is_dir: Cell<bool>,
        pub size: Cell<u64>,
        pub modified: Cell<Option<SystemTime>>,
        /// Set only for search results, which aren't direct children of
        /// whatever directory is currently displayed — `resolved_path`
        /// falls back to `current_dir.join(name)` when this is `None`.
        pub full_path: RefCell<Option<PathBuf>>,
        /// Set only for search results: the hit's location relative to the
        /// search root, shown alongside the name to disambiguate
        /// same-named files from different folders.
        pub detail: RefCell<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileEntryObject {
        const NAME: &'static str = "SepiaFileEntryObject";
        type Type = super::FileEntryObject;
    }

    impl ObjectImpl for FileEntryObject {}
}

glib::wrapper! {
    pub struct FileEntryObject(ObjectSubclass<imp::FileEntryObject>);
}

impl FileEntryObject {
    pub fn new(entry: &sepia_core::model::FileEntry) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().name.replace(entry.name.clone());
        obj.imp().is_dir.set(entry.is_dir);
        obj.imp().size.set(entry.size);
        obj.imp().modified.set(entry.modified);
        obj
    }

    /// Search results and virtual listings (Recent/Starred) don't go
    /// through `loader::list_dir`, so this stats the file itself to
    /// populate size/modified for the Size/Date Modified columns.
    pub fn new_search_hit(path: &Path, is_dir: bool, detail: String) -> Self {
        let obj: Self = glib::Object::new();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let metadata = std::fs::symlink_metadata(path).ok();
        obj.imp().name.replace(name);
        obj.imp().is_dir.set(is_dir);
        obj.imp().size.set(metadata.as_ref().map(|m| m.len()).unwrap_or(0));
        obj.imp().modified.set(metadata.as_ref().and_then(|m| m.modified().ok()));
        obj.imp().full_path.replace(Some(path.to_path_buf()));
        obj.imp().detail.replace(Some(detail));
        obj
    }

    pub fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    pub fn is_dir(&self) -> bool {
        self.imp().is_dir.get()
    }

    pub fn size(&self) -> u64 {
        self.imp().size.get()
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.imp().modified.get()
    }

    pub fn detail(&self) -> Option<String> {
        self.imp().detail.borrow().clone()
    }

    /// A fresh `FileEntryObject` (a distinct GObject, not the same
    /// reference) carrying identical data — needed to force GTK's list
    /// views to actually rebind currently-visible cells: splicing the
    /// exact same object references back into a `gio::ListStore` is
    /// treated as a no-op, since nothing about the *item* changed as far
    /// as the model can tell.
    pub fn duplicate(&self) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().name.replace(self.name());
        obj.imp().is_dir.set(self.is_dir());
        obj.imp().size.set(self.size());
        obj.imp().modified.set(self.modified());
        obj.imp().full_path.replace(self.imp().full_path.borrow().clone());
        obj.imp().detail.replace(self.detail());
        obj
    }

    /// This entry's absolute path. For a normal directory listing that's
    /// `current_dir.join(name)`; for a search result (which may live
    /// several levels below `current_dir`) it's the path captured when the
    /// hit was found.
    pub fn resolved_path(&self, current_dir: &Path) -> PathBuf {
        self.imp()
            .full_path
            .borrow()
            .clone()
            .unwrap_or_else(|| current_dir.join(self.name()))
    }
}
