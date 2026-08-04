use std::fs;
use std::path::{Path, PathBuf};

/// User-starred files/folders (an app-local favorites list, matching
/// Nautilus's "Starred" — not the same thing as a real macOS Finder tag),
/// persisted as one absolute path per line in
/// `~/Library/Application Support/Sepia/starred`.
pub struct Starred {
    file: PathBuf,
    entries: Vec<PathBuf>,
}

impl Starred {
    pub fn load() -> Self {
        let file = starred_file();
        let entries = fs::read_to_string(&file)
            .map(|contents| contents.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect())
            .unwrap_or_default();
        Self { file, entries }
    }

    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub fn is_starred(&self, path: &Path) -> bool {
        self.entries.iter().any(|p| p == path)
    }

    /// Adds `path` if not already starred, else removes it. Returns the
    /// new starred state.
    pub fn toggle(&mut self, path: PathBuf) -> bool {
        let now_starred = if let Some(pos) = self.entries.iter().position(|p| p == &path) {
            self.entries.remove(pos);
            false
        } else {
            self.entries.push(path);
            true
        };
        self.save();
        now_starred
    }

    fn save(&self) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let contents: String = self.entries.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
        let _ = fs::write(&self.file, contents);
    }
}

fn starred_file() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/starred")
}
