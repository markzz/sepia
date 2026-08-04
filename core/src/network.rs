use std::fs;
use std::path::PathBuf;
use std::process::Command;

const MAX_ENTRIES: usize = 10;

/// Schemes macOS's NetFS framework can actually mount (what real Finder's
/// own "Connect to Server" supports). Notably excludes `sftp://` — despite
/// looking like a natural fit, it isn't a mountable scheme on macOS; `open`
/// would silently hand it to whatever app is registered as its default
/// handler instead (e.g. a media player that understands `sftp://` stream
/// URLs), which is confusing enough to guard against explicitly rather
/// than deferring to `open` for schemes we haven't verified.
const MOUNTABLE_SCHEMES: [&str; 5] = ["smb", "afp", "nfs", "ftp", "cifs"];

/// Connects to a network server address (`smb://`, `afp://`, `nfs://`,
/// `ftp://`, `cifs://`) via macOS's own `open` — the same mechanism
/// Finder uses for "Connect to Server", which handles the actual mount
/// and any credential prompts natively. Mounted shares subsequently
/// appear under `/Volumes/` like any other volume.
pub fn connect(url: &str) -> Result<(), String> {
    let scheme = url.split_once("://").map(|(scheme, _)| scheme.to_lowercase());
    match scheme.as_deref() {
        Some(scheme) if MOUNTABLE_SCHEMES.contains(&scheme) => {}
        Some(scheme) => {
            return Err(format!(
                "\u{201c}{scheme}://\u{201d} isn't a network protocol macOS can mount. Supported: smb://, afp://, nfs://, ftp://, cifs://."
            ));
        }
        None => return Err("Enter a server address like smb://server/share".to_string()),
    }

    let status = Command::new("/usr/bin/open").arg(url).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Couldn't connect to {url}"))
    }
}

/// Recently-connected server addresses, most recent first, persisted as
/// one per line in `~/Library/Application Support/Sepia/recent_servers`.
pub struct RecentServers {
    file: PathBuf,
    entries: Vec<String>,
}

impl RecentServers {
    pub fn load() -> Self {
        let file = recent_servers_file();
        let entries = fs::read_to_string(&file)
            .map(|contents| contents.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
            .unwrap_or_default();
        Self { file, entries }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn record(&mut self, url: String) {
        self.entries.retain(|u| u != &url);
        self.entries.insert(0, url);
        self.entries.truncate(MAX_ENTRIES);
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let contents = self.entries.join("\n");
        let _ = fs::write(&self.file, contents);
    }
}

fn recent_servers_file() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/Sepia/recent_servers")
}
