use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Watches a single directory (non-recursively, matching a folder view's
/// scope) and lets the caller poll for pending changes on its own schedule,
/// so bursts of filesystem events (e.g. a large copy) coalesce into one
/// reload instead of one per event.
pub struct DirectoryWatcher {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<notify::Event>>,
    watched: Option<PathBuf>,
}

impl DirectoryWatcher {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )
        .expect("failed to create filesystem watcher");
        Self {
            watcher,
            rx,
            watched: None,
        }
    }

    pub fn watch(&mut self, dir: &Path) {
        if self.watched.as_deref() == Some(dir) {
            return;
        }
        if let Some(old) = self.watched.take() {
            let _ = self.watcher.unwatch(&old);
        }
        if self.watcher.watch(dir, RecursiveMode::NonRecursive).is_ok() {
            self.watched = Some(dir.to_path_buf());
        }
    }

    /// Drains all pending events, returning `true` if any arrived (meaning
    /// the watched directory changed and a reload is warranted).
    pub fn poll_changed(&self) -> bool {
        let mut changed = false;
        while let Ok(res) = self.rx.try_recv() {
            if res.is_ok() {
                changed = true;
            }
        }
        changed
    }
}

impl Default for DirectoryWatcher {
    fn default() -> Self {
        Self::new()
    }
}
