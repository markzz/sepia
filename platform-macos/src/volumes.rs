use std::path::PathBuf;

use objc2_foundation::{NSFileManager, NSVolumeEnumerationOptions};

pub struct Volume {
    pub name: String,
    pub path: PathBuf,
}

/// Live-enumerates mounted volumes via `NSFileManager`, using Finder's own
/// display names (e.g. the boot volume's raw path is `/`, but this reports
/// "Macintosh HD" or whatever the user has actually named it).
pub fn list_volumes() -> Vec<Volume> {
    let file_manager = NSFileManager::defaultManager();
    let volumes = file_manager.mountedVolumeURLsIncludingResourceValuesForKeys_options(
        None,
        NSVolumeEnumerationOptions::SkipHiddenVolumes,
    );
    volumes
        .iter()
        .flat_map(|array| array.iter())
        .filter_map(|url| {
            let path_str = url.path()?;
            let path = PathBuf::from(path_str.to_string());
            let name = file_manager.displayNameAtPath(&path_str).to_string();
            Some(Volume { name, path })
        })
        .collect()
}
