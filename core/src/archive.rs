use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ops::{copy_recursive, unique_destination};

/// Matches Nautilus's own compress dialog options (verified against
/// `nautilus-compress-dialog.c`), plus `.tar.gz` and `.tar.zst`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CompressFormat {
    Zip,
    EncryptedZip,
    TarGz,
    TarXz,
    TarZst,
    SevenZip,
}

impl CompressFormat {
    pub fn extension(self) -> &'static str {
        match self {
            CompressFormat::Zip | CompressFormat::EncryptedZip => "zip",
            CompressFormat::TarGz => "tar.gz",
            CompressFormat::TarXz => "tar.xz",
            CompressFormat::TarZst => "tar.zst",
            CompressFormat::SevenZip => "7z",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CompressFormat::Zip => "ZIP (.zip)",
            CompressFormat::EncryptedZip => "Encrypted ZIP (.zip)",
            CompressFormat::TarGz => "TAR (.tar.gz)",
            CompressFormat::TarXz => "TAR (.tar.xz)",
            CompressFormat::TarZst => "TAR (.tar.zst)",
            CompressFormat::SevenZip => "7Z (.7z)",
        }
    }
}

/// Compresses `sources` into `dest` using `format`. `passphrase` is only
/// used (and required, non-empty) for `CompressFormat::EncryptedZip`.
///
/// Plain ZIP goes through `ditto` (matching Finder's own Compress exactly
/// — it preserves resource forks/extended attributes, which a generic
/// zip tool doesn't). The other formats shell out to their respective
/// standard tool directly; none of them need ditto's multi-item staging
/// trick since `zip`/`tar`/`7z` all natively accept multiple source
/// arguments.
pub fn compress(sources: &[PathBuf], dest: &Path, format: CompressFormat, passphrase: Option<&str>) -> Result<(), String> {
    if sources.is_empty() {
        return Err("Nothing to compress".to_string());
    }
    match format {
        CompressFormat::Zip => compress_with_ditto(sources, dest),
        CompressFormat::EncryptedZip => {
            let passphrase = passphrase
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "A password is required for an encrypted zip".to_string())?;
            // `-P` puts the password on the command line, which is
            // briefly visible to other processes on the same machine via
            // `ps` — Info-ZIP's interactive `-e` prompt avoids this but
            // can't be scripted without a pty. A reasonable first-cut
            // tradeoff, not appropriate for very sensitive data.
            run_command(Command::new("/usr/bin/zip").arg("-r").arg("-P").arg(passphrase).arg(dest).args(sources))
        }
        CompressFormat::TarGz => run_command(Command::new("/usr/bin/tar").arg("-czf").arg(dest).args(sources)),
        CompressFormat::TarXz => run_command(Command::new("/usr/bin/tar").arg("-cJf").arg(dest).args(sources)),
        CompressFormat::TarZst => run_command(Command::new("/usr/bin/tar").arg("--zstd").arg("-cf").arg(dest).args(sources)),
        CompressFormat::SevenZip => {
            let bin = find_7z_binary().ok_or_else(|| "7-Zip isn't installed (try `brew install p7zip`)".to_string())?;
            run_command(Command::new(bin).arg("a").arg(dest).args(sources))
        }
    }
}

fn run_command(cmd: &mut Command) -> Result<(), String> {
    let output = cmd.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn compress_with_ditto(sources: &[PathBuf], dest_zip: &Path) -> Result<(), String> {
    let [first, rest @ ..] = sources else {
        return Err("Nothing to compress".to_string());
    };
    if rest.is_empty() {
        return run_ditto_compress(first, dest_zip, true);
    }

    // Multiple items: stage copies in a temp directory so each becomes
    // its own top-level entry in the zip (matching Finder's multi-item
    // "Compress N Items", which produces one Archive.zip with every item
    // at the top level rather than nested under a wrapper folder).
    let staging = std::env::temp_dir().join(format!("sepia-compress-{}", std::process::id()));
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let result = (|| {
        for src in sources {
            let name = src.file_name().ok_or_else(|| "invalid source name".to_string())?;
            let dest = staging.join(name);
            if src.is_dir() {
                copy_recursive(src, &dest).map_err(|e| e.to_string())?;
            } else {
                std::fs::copy(src, &dest).map_err(|e| e.to_string())?;
            }
        }
        run_ditto_compress(&staging, dest_zip, false)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn run_ditto_compress(source: &Path, dest_zip: &Path, keep_parent: bool) -> Result<(), String> {
    let mut cmd = Command::new("/usr/bin/ditto");
    cmd.arg("-c").arg("-k").arg("--sequesterRsrc");
    if keep_parent {
        cmd.arg("--keepParent");
    }
    cmd.arg(source).arg(dest_zip);
    run_command(&mut cmd)
}

/// Looks for a 7-Zip binary on `PATH` — not bundled with macOS, only
/// present if installed separately (e.g. Homebrew's `p7zip`).
pub fn find_7z_binary() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for candidate in ["7zz", "7z", "7za"] {
            let full = dir.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn archive_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    for suffix in [".tar.xz", ".tar.zst", ".tar.gz", ".tar.bz2"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return Some(stem.to_string());
        }
    }
    path.file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// Extracts `archive` into a new sibling folder — `Archive.zip` extracts
/// to `Archive/`, or `Archive 2/` etc. if that already exists — returning
/// the destination path. Dispatches to `ditto`/`tar`/7-Zip by extension.
pub fn extract(archive: &Path) -> Result<PathBuf, String> {
    let parent = archive.parent().ok_or_else(|| "invalid archive path".to_string())?;
    let stem = archive_stem(archive).ok_or_else(|| "invalid archive name".to_string())?;
    let base_dest = parent.join(&stem);
    let dest = if base_dest.exists() { unique_destination(parent, &stem) } else { base_dest };
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

    let name_lower = archive.to_string_lossy().to_lowercase();
    let result = if name_lower.ends_with(".zip") {
        run_command(Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(archive).arg(&dest))
    } else if name_lower.ends_with(".7z") {
        match find_7z_binary() {
            Some(bin) => run_command(Command::new(bin).arg("x").arg(format!("-o{}", dest.display())).arg(archive)),
            None => Err("7-Zip isn't installed (try `brew install p7zip`)".to_string()),
        }
    } else if [".tar.xz", ".tar.zst", ".tar.gz", ".tar.bz2", ".tar"].iter().any(|ext| name_lower.ends_with(ext)) {
        run_command(Command::new("/usr/bin/tar").arg("-xf").arg(archive).arg("-C").arg(&dest))
    } else {
        Err("Unrecognized archive format".to_string())
    };

    match result {
        Ok(()) => Ok(dest),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(e)
        }
    }
}

pub fn is_archive(path: &Path) -> bool {
    let name = path.to_string_lossy().to_lowercase();
    [".zip", ".tar.xz", ".tar.zst", ".tar.gz", ".tar.bz2", ".tar", ".7z"]
        .iter()
        .any(|ext| name.ends_with(ext))
}
