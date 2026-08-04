use std::fs;
use std::path::{Path, PathBuf};

/// Sidebar bookmarks, persisted as one absolute path per line in
/// `~/Library/Application Support/Sepia/bookmarks`. On first run (no file
/// yet), seeded with whichever of `defaults` actually exist.
pub struct Bookmarks {
    file: PathBuf,
    entries: Vec<PathBuf>,
}

impl Bookmarks {
    pub fn load(defaults: Vec<PathBuf>) -> Self {
        let file = bookmarks_file();
        match fs::read_to_string(&file) {
            Ok(contents) => {
                let entries = contents
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from)
                    .collect();
                Self { file, entries }
            }
            Err(_) => {
                let entries: Vec<PathBuf> = defaults.into_iter().filter(|p| p.is_dir()).collect();
                let bookmarks = Self { file, entries };
                bookmarks.save();
                bookmarks
            }
        }
    }

    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// Adds `path` if it isn't already bookmarked. Returns `false` (no-op)
    /// if it was already present.
    pub fn add(&mut self, path: PathBuf) -> bool {
        if self.entries.contains(&path) {
            return false;
        }
        self.entries.push(path);
        self.save();
        true
    }

    pub fn remove(&mut self, path: &Path) {
        self.entries.retain(|p| p != path);
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let contents: String = self
            .entries
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::write(&self.file, contents);
    }
}

fn bookmarks_file() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/bookmarks")
}
