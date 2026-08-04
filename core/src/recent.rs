use std::fs;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 50;

/// Recently-opened files (via `open::open_with_default_app`), most recent
/// first, persisted as one absolute path per line in
/// `~/Library/Application Support/Sepia/recent`. Unlike Bookmarks, this
/// is app-local rather than the freedesktop shared recently-used
/// registry Nautilus reads from — sepia doesn't participate in that
/// system-wide registry.
pub struct RecentFiles {
    file: PathBuf,
    entries: Vec<PathBuf>,
}

impl RecentFiles {
    pub fn load() -> Self {
        let file = recent_file();
        let entries = fs::read_to_string(&file)
            .map(|contents| contents.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect())
            .unwrap_or_default();
        Self { file, entries }
    }

    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// Moves `path` to the front (adding it if new), capped at
    /// `MAX_ENTRIES`.
    pub fn record(&mut self, path: PathBuf) {
        self.entries.retain(|p| p != &path);
        self.entries.insert(0, path);
        self.entries.truncate(MAX_ENTRIES);
        self.save();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let contents: String = self.entries.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
        let _ = fs::write(&self.file, contents);
    }
}

fn recent_file() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/recent")
}
