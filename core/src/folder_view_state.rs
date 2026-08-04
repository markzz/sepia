use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Per-folder view mode memory (list vs grid), independent of the global
/// "default view for new tabs" preference — matches Finder, where each
/// folder remembers its own view mode once you've changed it there.
/// Persisted as tab-separated `<list|grid>\t<path>` lines (not `key=value`
/// — paths can contain `=`) in
/// `~/Library/Application Support/Sepia/folder_view_state`.
pub struct FolderViewState {
    file: PathBuf,
    entries: HashMap<PathBuf, bool>,
}

impl FolderViewState {
    pub fn load() -> Self {
        let file = folder_view_state_file();
        let entries = fs::read_to_string(&file)
            .map(|contents| {
                contents
                    .lines()
                    .filter_map(|line| {
                        let (mode, path) = line.split_once('\t')?;
                        Some((PathBuf::from(path), mode == "grid"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { file, entries }
    }

    /// `Some(true)` for grid, `Some(false)` for list, `None` if this
    /// folder has never had its view mode explicitly set.
    pub fn get(&self, path: &Path) -> Option<bool> {
        self.entries.get(path).copied()
    }

    pub fn set(&mut self, path: PathBuf, is_grid: bool) {
        self.entries.insert(path, is_grid);
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let contents: String = self
            .entries
            .iter()
            .map(|(path, is_grid)| format!("{}\t{}", if *is_grid { "grid" } else { "list" }, path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::write(&self.file, contents);
    }
}

fn folder_view_state_file() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/folder_view_state")
}
