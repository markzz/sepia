use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2_foundation::{NSFileManager, NSString, NSURL};

/// Moves `path` to the Trash via `NSFileManager`, matching Finder's own
/// trash semantics (`~/.Trash`, restorable, works across filesystems).
/// Returns the item's new location in the Trash, for undo/restore support.
pub fn trash(path: &Path) -> Result<PathBuf, String> {
    let file_manager = NSFileManager::defaultManager();
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));

    let mut resulting_url: Option<Retained<NSURL>> = None;
    file_manager
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting_url))
        .map_err(|error| error.localizedDescription().to_string())?;

    resulting_url
        .and_then(|url| url.path())
        .map(|path| PathBuf::from(path.to_string()))
        .ok_or_else(|| "Trash did not report the item's new location".to_string())
}
