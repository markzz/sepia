use std::path::{Path, PathBuf};

pub fn display_path(dir: &Path) -> String {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    match home {
        Some(home) if dir == home => "~".to_string(),
        Some(home) => match dir.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => dir.display().to_string(),
        },
        None => dir.display().to_string(),
    }
}
