use std::path::{Path, PathBuf};
use std::process::Command;

/// Opens `path` with its default application, via macOS's `open` — the
/// same mechanism Finder uses when you double-click a file.
pub fn open_with_default_app(path: &Path) -> Result<(), String> {
    let status = Command::new("/usr/bin/open").arg(path).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Couldn't open {}", path.display()))
    }
}

/// True if `path` is an application bundle — a directory whose name ends
/// in `.app`, the convention every macOS app follows. Bundles are
/// directories on disk, but Finder (and sepia) treats them as launchable
/// files rather than folders to browse into.
pub fn is_app_bundle(path: &Path) -> bool {
    path.is_dir() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

/// `~/.Trash`, the same location Finder uses.
pub fn trash_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".Trash"))
}

/// True if `path` is already inside the Trash — items there get a
/// different set of file operations (no Cut/Copy/Move to Trash; Restore
/// instead), matching Nautilus's own trash-aware context menu.
pub fn is_in_trash(path: &Path) -> bool {
    trash_dir().is_some_and(|trash| path.starts_with(trash))
}

/// Opens a new Terminal window at `dir` — Terminal.app is registered to
/// treat a folder argument this way (long-standing macOS behavior, not
/// sepia-specific), matching Nautilus's "Open in Console".
pub fn open_terminal_at(dir: &Path) -> Result<(), String> {
    let status =
        Command::new("/usr/bin/open").arg("-a").arg("Terminal").arg(dir).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Couldn't open Terminal at {}", dir.display()))
    }
}
