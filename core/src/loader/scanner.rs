use std::io;
use std::fs;
use std::path::Path;

use crate::model::FileEntry;

/// Lists `dir`'s contents. Distinguishes "genuinely empty" from "couldn't
/// even be read" (e.g. macOS denying access to `~/.Trash` without Full
/// Disk Access granted) — the latter would otherwise look identical to an
/// empty folder.
pub fn list_dir(dir: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries: Vec<FileEntry> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            // `DirEntry::metadata` doesn't follow symlinks (same as
            // `file_type`), so this doesn't change `is_dir`'s meaning —
            // just gets size/modified from the same stat for free.
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
                size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}
