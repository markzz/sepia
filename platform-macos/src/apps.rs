use std::path::{Path, PathBuf};
use std::process::Command;

use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

pub struct AppInfo {
    pub name: String,
    pub path: PathBuf,
}

fn file_url(path: &Path) -> objc2::rc::Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn app_info_from_url(url: &NSURL) -> Option<AppInfo> {
    let path = PathBuf::from(url.path()?.to_string());
    // Strips the ".app" extension — e.g. "Preview.app" -> "Preview".
    let name = path.file_stem()?.to_string_lossy().into_owned();
    Some(AppInfo { name, path })
}

/// The app macOS would use to open `path` by default (matches what
/// double-clicking it, or `open`, would launch).
pub fn default_app_for(path: &Path) -> Option<AppInfo> {
    let workspace = NSWorkspace::sharedWorkspace();
    let url = workspace.URLForApplicationToOpenURL(&file_url(path))?;
    app_info_from_url(&url)
}

/// Every app capable of opening `path`, for an "Open With…" picker.
pub fn apps_for(path: &Path) -> Vec<AppInfo> {
    let workspace = NSWorkspace::sharedWorkspace();
    let urls = workspace.URLsForApplicationsToOpenURL(&file_url(path));
    urls.iter().filter_map(|u| app_info_from_url(&u)).collect()
}

/// Opens `path` with a specific app (by its `.app` bundle path), via
/// `open -a`.
pub fn open_with(path: &Path, app_path: &Path) -> Result<(), String> {
    let status = Command::new("/usr/bin/open").arg("-a").arg(app_path).arg(path).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Couldn't open {} with {}", path.display(), app_path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_default_and_compatible_apps_for_a_png() {
        let path = std::env::temp_dir().join(format!("sepia-apps-test-{}.png", std::process::id()));
        // A minimal valid 1x1 PNG, so LaunchServices has a real content
        // type to match against.
        let png_bytes: [u8; 69] = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00,
            0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03,
            0x00, 0x01, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60,
            0x82,
        ];
        std::fs::write(&path, png_bytes).unwrap();

        let default_app = default_app_for(&path);
        let apps = apps_for(&path);

        std::fs::remove_file(&path).ok();

        assert!(default_app.is_some(), "expected a default app for a PNG");
        assert!(!apps.is_empty(), "expected at least one compatible app for a PNG");
    }
}
