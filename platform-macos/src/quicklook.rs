use std::cell::RefCell;
use std::path::PathBuf;

use quicklook::{PreviewItem, QuickLookPanel};

thread_local! {
    // `QLPreviewPanel` wraps an `Objective-C` object and is main-thread-
    // only (not `Send`/`Sync`) — `thread_local` keeps it pinned there,
    // which matches the GTK/GLib main loop this is always called from.
    static PANEL: RefCell<Option<QuickLookPanel>> = const { RefCell::new(None) };
}

/// Toggles the Quick Look preview panel: hides it if already visible
/// (matching Finder's Space-bar-to-close), otherwise shows it previewing
/// `paths`, starting at the first item. A no-op if `paths` is empty or the
/// panel can't be created.
pub fn toggle_preview(paths: &[PathBuf]) {
    PANEL.with(|cell| {
        let mut panel_ref = cell.borrow_mut();
        if panel_ref.is_none() {
            *panel_ref = QuickLookPanel::shared();
        }
        let Some(panel) = panel_ref.as_ref() else {
            return;
        };
        if panel.is_visible() {
            panel.hide();
            return;
        }
        let items: Vec<PreviewItem> = paths.iter().filter_map(|p| PreviewItem::from_file_url(p, None)).collect();
        if items.is_empty() {
            return;
        }
        panel.set_items(items);
        panel.reload_if_dirty();
        panel.set_current_preview_item_index(0);
        panel.show();
    });
}

/// Updates the currently-open panel's items in place (e.g. because the
/// selection changed while it was open) — a no-op if it isn't visible.
pub fn refresh_preview(paths: &[PathBuf]) {
    PANEL.with(|cell| {
        let panel_ref = cell.borrow();
        let Some(panel) = panel_ref.as_ref() else {
            return;
        };
        if !panel.is_visible() {
            return;
        }
        let items: Vec<PreviewItem> = paths.iter().filter_map(|p| PreviewItem::from_file_url(p, None)).collect();
        if items.is_empty() {
            panel.hide();
            return;
        }
        panel.set_items(items);
        panel.reload_if_dirty();
    });
}

pub fn hide_preview() {
    PANEL.with(|cell| {
        if let Some(panel) = cell.borrow().as_ref() {
            panel.hide();
        }
    });
}
