use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One user-installed script: any executable file directly inside
/// `scripts_dir()`. This mirrors Nautilus's own simplest extension
/// mechanism (its "Scripts" folder, distinct from the much more involved
/// nautilus-python/libnautilus-extension C API) — no plugin ABI, no
/// embedded interpreter, just: drop an executable in a folder and it
/// becomes an invocable action given the current selection. Deliberately
/// chosen over dynamic library loading, which has no stable ABI across
/// Rust compiler versions and would be fragile to actually ship.
pub struct Script {
    pub name: String,
    pub path: PathBuf,
}

pub fn scripts_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/Scripts")
}

/// Creates the scripts folder if it doesn't exist yet, so there's always
/// somewhere obvious for the user to add scripts to.
pub fn ensure_scripts_dir() -> PathBuf {
    let dir = scripts_dir();
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Lists executable files directly in the scripts folder, sorted by name.
/// Non-executable files are skipped (matches Nautilus: `chmod +x` is how
/// you "install" a script, no separate registration step).
pub fn list_scripts() -> Vec<Script> {
    let dir = scripts_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut scripts: Vec<Script> = entries
        .flatten()
        .filter(|entry| {
            entry.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            Some(Script { name, path: entry.path() })
        })
        .collect();
    scripts.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    scripts
}

/// Runs `script`, working directory set to `current_dir`, with the
/// current selection available two ways for scripts of any complexity:
/// as positional arguments (`$1 $2 ...` / `"$@"`), and newline-separated
/// in `SEPIA_SELECTED_FILE_PATHS` (handles selections too large for argv,
/// and is easier to parse from most scripting languages). The current
/// directory is also exposed as `SEPIA_CURRENT_DIRECTORY` even when
/// nothing is selected.
pub fn run_script(script: &Path, selected_paths: &[PathBuf], current_dir: &Path) -> Result<(), String> {
    let selected_joined =
        selected_paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
    let status = Command::new(script)
        .args(selected_paths)
        .current_dir(current_dir)
        .env("SEPIA_SELECTED_FILE_PATHS", selected_joined)
        .env("SEPIA_CURRENT_DIRECTORY", current_dir)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {status}", script.display()))
    }
}
