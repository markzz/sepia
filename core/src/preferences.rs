use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NewWindowLocation {
    Home,
    LastLocation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DefaultView {
    List,
    Grid,
}

/// App-wide preferences, persisted as simple `key=value` lines in
/// `~/Library/Application Support/Sepia/preferences`.
pub struct Preferences {
    file: PathBuf,
    pub show_hidden: bool,
    pub new_window_location: NewWindowLocation,
    pub default_view: DefaultView,
    /// The active tab's most recent location, kept up to date on every
    /// navigation so a fresh launch can reopen there when
    /// `new_window_location` is `LastLocation`.
    pub last_location: Option<PathBuf>,
    /// Index into the app's fixed zoom-level table (grid icon size/list
    /// icon size move together) — shared by every window/tab, matching
    /// Nautilus's own single, app-wide zoom setting.
    pub zoom_index: usize,
}

impl Preferences {
    pub fn load() -> Self {
        let file = preferences_file();
        let mut prefs = Self {
            file,
            show_hidden: false,
            new_window_location: NewWindowLocation::Home,
            default_view: DefaultView::List,
            last_location: None,
            zoom_index: 1,
        };
        if let Ok(contents) = fs::read_to_string(&prefs.file) {
            for line in contents.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let value = value.trim();
                match key.trim() {
                    "show_hidden" => prefs.show_hidden = value == "true",
                    "new_window_location" => {
                        prefs.new_window_location = if value == "last" {
                            NewWindowLocation::LastLocation
                        } else {
                            NewWindowLocation::Home
                        };
                    }
                    "default_view" => {
                        prefs.default_view = if value == "grid" {
                            DefaultView::Grid
                        } else {
                            DefaultView::List
                        };
                    }
                    "last_location" if !value.is_empty() => {
                        prefs.last_location = Some(PathBuf::from(value));
                    }
                    "zoom_index" => {
                        if let Ok(index) = value.parse() {
                            prefs.zoom_index = index;
                        }
                    }
                    _ => {}
                }
            }
        }
        prefs
    }

    pub fn save(&self) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let contents = format!(
            "show_hidden={}\nnew_window_location={}\ndefault_view={}\nlast_location={}\nzoom_index={}\n",
            self.show_hidden,
            match self.new_window_location {
                NewWindowLocation::Home => "home",
                NewWindowLocation::LastLocation => "last",
            },
            match self.default_view {
                DefaultView::List => "list",
                DefaultView::Grid => "grid",
            },
            self.last_location.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
            self.zoom_index,
        );
        let _ = fs::write(&self.file, contents);
    }
}

fn preferences_file() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/preferences")
}
