use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub mod undo;

/// Picks "untitled folder", "untitled folder 2", ... for a new folder in `parent`.
fn unique_new_folder_name(parent: &Path) -> String {
    let base = "untitled folder";
    if !parent.join(base).exists() {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let name = format!("{base} {n}");
        if !parent.join(&name).exists() {
            return name;
        }
        n += 1;
    }
}

pub fn new_folder(parent: &Path) -> io::Result<PathBuf> {
    let path = parent.join(unique_new_folder_name(parent));
    fs::create_dir(&path)?;
    Ok(path)
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let dest = path.with_file_name(new_name);
    fs::rename(path, &dest)?;
    Ok(dest)
}

/// Picks a non-conflicting "name 2", "name 2.ext", "name 3", ... in `dest_dir`
/// for the "Keep Both" conflict resolution.
pub fn unique_destination(dest_dir: &Path, file_name: &str) -> PathBuf {
    let path = Path::new(file_name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(file_name);
    let ext = path.extension().and_then(|s| s.to_str());

    let mut n = 2;
    loop {
        let candidate_name = match ext {
            Some(ext) => format!("{stem} {n}.{ext}"),
            None => format!("{stem} {n}"),
        };
        let candidate = dest_dir.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

pub fn copy_recursive(src: &Path, dest: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(src, dest).map(|_| ())
    }
}

/// Moves `src` to `dest`, falling back to copy-then-delete if they're on
/// different filesystems (`fs::rename` can't cross a filesystem boundary).
pub fn move_path(src: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_recursive(src, dest)?;
            delete_permanently(src)
        }
    }
}

pub fn delete_permanently(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Permanently deletes every direct entry of `trash_dir` — the trash
/// folder itself is left in place, only its contents go.
pub fn empty_trash(trash_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(trash_dir)? {
        delete_permanently(&entry?.path())?;
    }
    Ok(())
}
