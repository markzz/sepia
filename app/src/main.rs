mod file_entry_object;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{
    gdk, gio, glib, Align, AlertDialog, Application, ApplicationWindow, Box as GtkBox, Button,
    ColumnView, ColumnViewColumn, Entry, GridView, Image, Justification, Label, ListBox,
    MultiSelection, Notebook, Orientation, Paned, ScrolledWindow, SelectionMode, Stack,
    Window as GtkWindow,
};

use file_entry_object::FileEntryObject;
use sepia_core::{display_path, loader, ops, NavigationHistory};

const APP_ID: &str = "com.markzz.sepia";

thread_local! {
    // Whichever window last had focus, so the (single, app-wide) menu bar
    // and its accelerators apply to the right window in a multi-window
    // session. Updated by each window's `is-active` handler in
    // `build_window`. A plain `thread_local`, not an `AppState` field —
    // GTK is single-threaded anyway, and this avoids a reference cycle
    // that storing it on every `AppState` would create (each window's
    // state pointing back to the cell that, while focused, points to it).
    static ACTIVE_STATE: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
    // Real per-file-type icons/thumbnails (via NSWorkspace/QuickLook) are
    // fetched asynchronously and cached here by path — every list/grid
    // cell for a given file shares one fetch and one texture, regardless
    // of which view or how many times it scrolls in and out.
    static ICON_CACHE: RefCell<HashMap<PathBuf, gdk::Texture>> = RefCell::new(HashMap::new());
    // Avoids firing a second concurrent fetch for a path that's already
    // in flight — GTK can rebind the same cell to the same item more
    // than once in quick succession during initial layout.
    static ICON_IN_FLIGHT: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
    // Bounds how many `request_thumbnail` calls are outstanding at once.
    // Without this, opening a folder with 70+ images fires 70+
    // simultaneous requests at QuickLook's XPC thumbnail service —
    // observed to make things *slower* in aggregate (heavy contention)
    // than a modest, steady queue.
    static ICON_QUEUE: RefCell<std::collections::VecDeque<QueuedIconRequest>> = RefCell::new(std::collections::VecDeque::new());
    static ICON_ACTIVE_FETCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // Index into `GRID_ICON_SIZES`/`LIST_ICON_SIZES` — a single, app-wide
    // zoom level shared by every window/tab (matching Nautilus), not
    // per-folder. Seeded from `Preferences` by the first window built;
    // `Preferences::load()` in any later window picks up whatever was
    // last saved, so multi-window stays consistent without needing this
    // to be synced live across windows.
    static GRID_ZOOM_INDEX: std::cell::Cell<usize> = const { std::cell::Cell::new(DEFAULT_ZOOM_INDEX) };
    // Type-ahead selection: letters typed with a file view focused (no
    // modifiers, no text entry involved) accumulate here and jump the
    // selection to the first name starting with the buffer. Resets after
    // a pause, matching Finder/Nautilus's own type-ahead convention.
    static TYPE_AHEAD: RefCell<(String, std::time::Instant)> = RefCell::new((String::new(), std::time::Instant::now()));
}
const MAX_CONCURRENT_ICON_FETCHES: usize = 4;
const GRID_ICON_SIZES: [i32; 5] = [32, 48, 64, 96, 128];
const GRID_CONTAINER_WIDTHS: [i32; 5] = [64, 84, 100, 132, 164];
const LIST_ICON_SIZES: [i32; 5] = [12, 16, 20, 28, 36];
const TYPE_AHEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Appends `ch` to the type-ahead buffer (clearing it first if the user
/// paused longer than `TYPE_AHEAD_TIMEOUT`), then jumps `view`'s
/// selection to the first item in `model` whose name starts with the
/// resulting buffer, case-insensitively.
fn handle_type_ahead(model: &impl IsA<gio::ListModel>, scroll_to: impl Fn(u32), ch: char) {
    let query = TYPE_AHEAD.with(|cell| {
        let mut buf = cell.borrow_mut();
        let now = std::time::Instant::now();
        if now.duration_since(buf.1) > TYPE_AHEAD_TIMEOUT {
            buf.0.clear();
        }
        buf.0.push(ch.to_ascii_lowercase());
        buf.1 = now;
        buf.0.clone()
    });
    for i in 0..model.n_items() {
        if let Some(entry) = model.item(i).and_downcast::<FileEntryObject>() {
            if entry.name().to_lowercase().starts_with(&query) {
                scroll_to(i);
                return;
            }
        }
    }
}
const DEFAULT_ZOOM_INDEX: usize = 1;

struct QueuedIconRequest {
    path: PathBuf,
    list_item: gtk::ListItem,
    icon: Image,
    original_entry: FileEntryObject,
}

/// Sets `icon`'s image: an instant themed placeholder (folder/app/generic
/// document, same as before), then — for files, not folders —
/// asynchronously upgrades to the real per-type icon or, for types
/// QuickLook can preview (images, video, PDFs, etc.), an actual content
/// thumbnail, matching Finder. Cached by path, so this only costs
/// anything the first time a given file is shown. `list_item` is
/// re-checked before applying the result: GTK recycles list/grid cells,
/// so by the time an async fetch completes, this cell may already be
/// bound to a different file.
fn apply_file_icon(icon: &Image, list_item: &gtk::ListItem, entry: &FileEntryObject, path: PathBuf) {
    icon.set_icon_name(Some(if entry.is_dir() && entry.name().to_ascii_lowercase().ends_with(".app") {
        "application-x-executable"
    } else if entry.is_dir() {
        "folder"
    } else {
        "text-x-generic"
    }));
    // Folders don't have a QuickLook-able "content" — just the themed
    // folder icon above, matching Finder's own list/grid views.
    if entry.is_dir() {
        return;
    }
    if let Some(texture) = ICON_CACHE.with(|cache| cache.borrow().get(&path).cloned()) {
        icon.set_paintable(Some(&texture));
        return;
    }
    if !ICON_IN_FLIGHT.with(|set| set.borrow_mut().insert(path.clone())) {
        return;
    }
    ICON_QUEUE.with(|queue| {
        queue.borrow_mut().push_back(QueuedIconRequest {
            path,
            list_item: list_item.clone(),
            icon: icon.clone(),
            original_entry: entry.clone(),
        })
    });
    pump_icon_queue();
}

/// Dispatches queued icon/thumbnail fetches up to
/// `MAX_CONCURRENT_ICON_FETCHES` at a time; each completion frees its
/// slot and calls back in here to keep the queue draining.
fn pump_icon_queue() {
    while ICON_ACTIVE_FETCHES.get() < MAX_CONCURRENT_ICON_FETCHES {
        let Some(request) = ICON_QUEUE.with(|queue| queue.borrow_mut().pop_front()) else {
            return;
        };
        ICON_ACTIVE_FETCHES.set(ICON_ACTIVE_FETCHES.get() + 1);
        let QueuedIconRequest { path, list_item, icon, original_entry } = request;
        sepia_platform_macos::icons::request_thumbnail(path.clone(), 64.0, 2.0, move |png_bytes| {
            ICON_ACTIVE_FETCHES.set(ICON_ACTIVE_FETCHES.get().saturating_sub(1));
            ICON_IN_FLIGHT.with(|set| set.borrow_mut().remove(&path));
            let png_bytes = png_bytes.or_else(|| sepia_platform_macos::icons::icon_for_file(&path));
            if let Some(texture) =
                png_bytes.and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok())
            {
                ICON_CACHE.with(|cache| cache.borrow_mut().insert(path.clone(), texture.clone()));
                if list_item.item().and_downcast::<FileEntryObject>().as_ref() == Some(&original_entry) {
                    icon.set_paintable(Some(&texture));
                }
            }
            pump_icon_queue();
        });
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ClipboardMode {
    Copy,
    Cut,
}

/// What a sidebar row navigates to: a normal folder, or one of the
/// virtual (non-folder-backed) listings.
#[derive(Clone)]
enum SidebarTarget {
    Path(PathBuf),
    Recent,
    Starred,
    Network,
}

/// A tab showing a virtual listing (Recent or Starred) instead of a real
/// directory — `Tab::current_dir` is left as whatever it last was, so
/// navigating away or reloading falls back to something sensible.
#[derive(Clone, Copy, PartialEq)]
enum VirtualView {
    Recent,
    Starred,
}

/// A plain `Vec`-based undo/redo stack. GTK/GLib has no built-in equivalent
/// to `NSUndoManager`, but `sepia_core::ops::undo::UndoStep` is already
/// toolkit-agnostic (each step's `perform()` returns its own inverse), so
/// undo and redo are both just "pop a step, perform it, push the result on
/// the other stack" — this is genuinely a full multi-level undo/redo, not
/// a simplification. Shared across all tabs, matching Finder/Nautilus (undo
/// is a single app-wide stack, not per-tab).
#[derive(Default)]
struct UndoManager {
    undo_stack: Vec<(String, ops::undo::UndoStep)>,
    redo_stack: Vec<(String, ops::undo::UndoStep)>,
}

impl UndoManager {
    fn push(&mut self, name: impl Into<String>, step: ops::undo::UndoStep) {
        self.undo_stack.push((name.into(), step));
        self.redo_stack.clear();
    }
}

/// One tab's worth of browsing state: its own location, history, file-view
/// content, selection, and filesystem watcher. Identified by a stable `id`
/// rather than its position in `AppState::tabs`, since position shifts as
/// tabs open and close.
struct Tab {
    id: u64,
    current_dir: PathBuf,
    store: gio::ListStore,
    history: NavigationHistory,
    list_selection: MultiSelection,
    grid_selection: MultiSelection,
    view_stack: Stack,
    watcher: loader::DirectoryWatcher,
    tab_label: Label,
    /// `Some(query)` while this tab is showing search results instead of a
    /// normal directory listing (search is per-tab, like Finder).
    search_query: Option<String>,
    /// The live Spotlight query backing `search_query`, if any. Dropping
    /// this (by replacing or clearing it) stops the query and removes its
    /// notification observers — happens automatically when search is
    /// exited, a new search supersedes it, or the tab itself is closed.
    search_handle: Option<sepia_platform_macos::spotlight::SpotlightQuery>,
    /// `Some` while this tab is showing a virtual listing (Recent or
    /// Starred) rather than a normal directory listing.
    virtual_view: Option<VirtualView>,
}

struct AppState {
    window: ApplicationWindow,
    notebook: Notebook,
    tabs: Vec<Tab>,
    active_tab: u64,
    next_tab_id: u64,
    path_bar: GtkBox,
    back_button: Button,
    forward_button: Button,
    /// `None` entries are inert placeholder rows (Network, Trash —
    /// present for visual parity with Nautilus, not yet backed by real
    /// functionality).
    sidebar_targets: Vec<Option<SidebarTarget>>,
    /// Set right after `build_sidebar` returns — `None` only during that
    /// brief construction window. Lets global actions (e.g.
    /// `app.remove_bookmark`) trigger a sidebar rebuild without needing
    /// the `ListBox` threaded through their call site.
    sidebar_list_box: Option<ListBox>,
    clipboard: Option<(Vec<PathBuf>, ClipboardMode)>,
    undo_manager: UndoManager,
    show_hidden: bool,
    search_entry: Entry,
    view_toggle_button: Button,
    bookmarks: sepia_core::bookmarks::Bookmarks,
    preferences: sepia_core::preferences::Preferences,
    recent: sepia_core::recent::RecentFiles,
    starred: sepia_core::starred::Starred,
    recent_servers: sepia_core::network::RecentServers,
    folder_view_state: sepia_core::folder_view_state::FolderViewState,
}

impl AppState {
    fn tab(&self, id: u64) -> &Tab {
        self.tabs.iter().find(|t| t.id == id).expect("tab id must exist")
    }

    fn tab_mut(&mut self, id: u64) -> &mut Tab {
        self.tabs.iter_mut().find(|t| t.id == id).expect("tab id must exist")
    }

    fn active(&self) -> &Tab {
        self.tab(self.active_tab)
    }
}

/// Loads a directory into the tab identified by `tab_id`, without touching
/// its back/forward history. Only refreshes the shared toolbar/path bar if
/// that tab happens to be the active one.
fn load_dir(state: &Rc<RefCell<AppState>>, tab_id: u64, dir: PathBuf) {
    let show_hidden = state.borrow().show_hidden;
    let entries: Vec<_> = match loader::list_dir(&dir) {
        Ok(entries) => entries.into_iter().filter(|entry| show_hidden || !entry.name.starts_with('.')).collect(),
        Err(error) => {
            show_error(state, &format!("Couldn't read {}: {error}", dir.display()));
            Vec::new()
        }
    };

    {
        let state_ref = state.borrow();
        let tab = state_ref.tab(tab_id);
        let objects: Vec<FileEntryObject> = entries.iter().map(FileEntryObject::new).collect();
        let old_count = tab.store.n_items();
        tab.store.splice(0, old_count, &objects);
        let title = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        tab.tab_label.set_label(&title);
    }

    {
        let mut state_mut = state.borrow_mut();
        let is_grid = state_mut
            .folder_view_state
            .get(&dir)
            .unwrap_or(state_mut.preferences.default_view == sepia_core::preferences::DefaultView::Grid);
        let tab = state_mut.tab_mut(tab_id);
        tab.watcher.watch(&dir);
        tab.current_dir = dir.clone();
        tab.view_stack.set_visible_child_name(if is_grid { "grid" } else { "list" });
        // A real directory is fundamentally incompatible with a virtual
        // listing being active — cleared here (rather than only at
        // `navigate`'s call sites) so `go_back`/`go_forward`, which call
        // `load_dir` directly, can't leave a stale `virtual_view` behind.
        tab.virtual_view = None;
    }

    if state.borrow().active_tab == tab_id {
        refresh_toolbar(state, &dir);
    }
}

/// Refreshes a tab's content in place: re-runs its search if it's
/// currently showing search results, re-shows Recent/Starred if it's
/// showing one of those, otherwise reloads its current directory.
fn refresh_tab(state: &Rc<RefCell<AppState>>, tab_id: u64) {
    let query = state.borrow().tab(tab_id).search_query.clone();
    if let Some(query) = query {
        run_search(state, tab_id, query);
        return;
    }
    let virtual_view = state.borrow().tab(tab_id).virtual_view;
    match virtual_view {
        Some(VirtualView::Recent) => show_virtual_listing_for_tab(state, tab_id, VirtualView::Recent),
        Some(VirtualView::Starred) => show_virtual_listing_for_tab(state, tab_id, VirtualView::Starred),
        None => {
            let dir = state.borrow().tab(tab_id).current_dir.clone();
            load_dir(state, tab_id, dir);
        }
    }
}

/// `refresh_tab` for whichever tab is currently active — the common case
/// after a file operation triggered by the user.
fn refresh_active_view(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    refresh_tab(state, id);
}

fn show_recent(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    stop_tab_search(state, id);
    state.borrow_mut().tab_mut(id).virtual_view = Some(VirtualView::Recent);
    show_virtual_listing_for_tab(state, id, VirtualView::Recent);
}

fn show_starred(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    stop_tab_search(state, id);
    state.borrow_mut().tab_mut(id).virtual_view = Some(VirtualView::Starred);
    show_virtual_listing_for_tab(state, id, VirtualView::Starred);
}

/// Populates `tab_id`'s store with a virtual listing (Recent or Starred),
/// using the same "search hit" `FileEntryObject` representation as search
/// results (full path + location detail), since these items can live
/// anywhere on disk rather than under one common root. Entries that no
/// longer exist (deleted since being recorded) are silently skipped.
fn show_virtual_listing_for_tab(state: &Rc<RefCell<AppState>>, tab_id: u64, view: VirtualView) {
    let title = match view {
        VirtualView::Recent => "Recent",
        VirtualView::Starred => "Starred",
    };
    let entries: Vec<PathBuf> = {
        let state_ref = state.borrow();
        match view {
            VirtualView::Recent => state_ref.recent.entries().to_vec(),
            VirtualView::Starred => state_ref.starred.entries().to_vec(),
        }
    };
    let objects: Vec<FileEntryObject> = entries
        .iter()
        .filter(|p| p.exists())
        .map(|path| {
            let is_dir = path.is_dir();
            let detail = path.parent().map(display_path).unwrap_or_default();
            FileEntryObject::new_search_hit(path, is_dir, detail)
        })
        .collect();

    {
        let state_ref = state.borrow();
        let tab = state_ref.tab(tab_id);
        let old_count = tab.store.n_items();
        tab.store.splice(0, old_count, &objects);
        tab.tab_label.set_label(title);
    }

    if state.borrow().active_tab == tab_id {
        let (window, path_bar) = {
            let state_ref = state.borrow();
            (state_ref.window.clone(), state_ref.path_bar.clone())
        };
        window.set_title(Some(&format!("Sepia — {title}")));
        while let Some(child) = path_bar.first_child() {
            path_bar.remove(&child);
        }
        path_bar.append(&Label::builder().label(title).xalign(0.0).build());
    }
}

/// Refreshes the window title, path bar, and back/forward sensitivity to
/// reflect the active tab's location and history.
fn refresh_toolbar(state: &Rc<RefCell<AppState>>, dir: &Path) {
    let (window, path_bar, back_button, forward_button, search_entry, can_back, can_forward, query) = {
        let state_ref = state.borrow();
        let tab = state_ref.active();
        (
            state_ref.window.clone(),
            state_ref.path_bar.clone(),
            state_ref.back_button.clone(),
            state_ref.forward_button.clone(),
            state_ref.search_entry.clone(),
            tab.history.can_go_back(),
            tab.history.can_go_forward(),
            tab.search_query.clone(),
        )
    };
    window.set_title(Some(&format!("Sepia — {}", display_path(dir))));
    rebuild_path_bar(&path_bar, state, dir);
    back_button.set_sensitive(can_back);
    forward_button.set_sensitive(can_forward);

    // Remembered for `NewWindowLocation::LastLocation` on the next launch.
    {
        let mut state_mut = state.borrow_mut();
        if state_mut.preferences.last_location.as_deref() != Some(dir) {
            state_mut.preferences.last_location = Some(dir.to_path_buf());
            state_mut.preferences.save();
        }
    }

    // Sync the (shared) search entry to the active tab's own search state.
    // Guarded with an equality check so switching back to an already-
    // matching tab doesn't needlessly re-fire `connect_changed`.
    match query {
        Some(query) => {
            search_entry.set_visible(true);
            if search_entry.text() != query {
                search_entry.set_text(&query);
            }
        }
        None => search_entry.set_visible(false),
    }

    sync_view_toggle_icon(state);
}

/// The active tab's current directory.
fn active_dir(state: &Rc<RefCell<AppState>>) -> PathBuf {
    state.borrow().active().current_dir.clone()
}

/// Navigates the active tab to a new directory, recording the current one
/// in that tab's history.
/// Clears a tab's search query and stops its live Spotlight query (if any)
/// — must happen together, otherwise a result notification arriving after
/// search has ostensibly been exited would clobber the normal listing that
/// replaced it.
fn stop_tab_search(state: &Rc<RefCell<AppState>>, tab_id: u64) {
    let mut state_mut = state.borrow_mut();
    let tab = state_mut.tab_mut(tab_id);
    tab.search_query = None;
    tab.search_handle = None;
}

/// Navigates the active tab to a new directory, recording the current one
/// in that tab's history. Also exits search or a virtual view (Recent/
/// Starred) if active, even when `dir` equals the tab's stale
/// `current_dir` — clicking the current-location breadcrumb while
/// searching/viewing Recent is how you get back to a normal listing
/// without otherwise changing directories.
fn navigate(state: &Rc<RefCell<AppState>>, dir: PathBuf) {
    let id = state.borrow().active_tab;
    let current = state.borrow().tab(id).current_dir.clone();
    let was_searching = state.borrow().tab(id).search_query.is_some();
    let was_virtual = state.borrow().tab(id).virtual_view.is_some();
    if dir == current && !was_searching && !was_virtual {
        return;
    }
    if was_searching {
        stop_tab_search(state, id);
    }
    // `load_dir` below also clears `virtual_view` unconditionally; no
    // separate handling needed here beyond the early-return guard above.
    if dir != current {
        state.borrow_mut().tab_mut(id).history.record(current);
    }
    sepia_platform_macos::quicklook::hide_preview();
    load_dir(state, id, dir);
}

fn go_up(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    let parent = state.borrow().tab(id).current_dir.parent().map(PathBuf::from);
    if let Some(parent) = parent {
        navigate(state, parent);
    }
}

fn go_back(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    let current = state.borrow().tab(id).current_dir.clone();
    let target = state.borrow_mut().tab_mut(id).history.go_back(current);
    if let Some(target) = target {
        load_dir(state, id, target);
    }
}

fn go_forward(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    let current = state.borrow().tab(id).current_dir.clone();
    let target = state.borrow_mut().tab_mut(id).history.go_forward(current);
    if let Some(target) = target {
        load_dir(state, id, target);
    }
}

/// Rebuilds the path bar's breadcrumb buttons for `dir`. Takes `path_bar`
/// directly (rather than fetching it from `state`) so no `RefCell` borrow
/// needs to be held while this runs.
fn rebuild_path_bar(path_bar: &GtkBox, state: &Rc<RefCell<AppState>>, dir: &Path) {
    while let Some(child) = path_bar.first_child() {
        path_bar.remove(&child);
    }

    let mut accumulated = PathBuf::new();
    for component in dir.components() {
        accumulated.push(component.as_os_str());
        let label = component.as_os_str().to_string_lossy().into_owned();
        let label = if label.is_empty() { "/".to_string() } else { label };

        let button = Button::builder().label(&label).build();
        let target = accumulated.clone();
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            move |_| navigate(&state, target.clone())
        ));
        path_bar.append(&button);
    }
}

/// The path of the *first* selected item in whichever view (list or grid)
/// is currently visible in the active tab. File ops still act on a single
/// item for now even though multiple can be selected — a reasonable first
/// cut, extendable to batch operations later.
fn selected_path(state: &Rc<RefCell<AppState>>) -> Option<PathBuf> {
    let state_ref = state.borrow();
    let tab = state_ref.active();
    let selection = if tab.view_stack.visible_child_name().as_deref() == Some("grid") {
        &tab.grid_selection
    } else {
        &tab.list_selection
    };
    let bitset = selection.selection();
    if bitset.is_empty() {
        return None;
    }
    tab.store
        .item(bitset.minimum())
        .and_downcast::<FileEntryObject>()
        .map(|entry| entry.resolved_path(&tab.current_dir))
}

/// All paths currently selected in whichever view is visible in the active
/// tab — used as the drag source's payload (unlike `selected_path`, which
/// file ops use and deliberately only look at the first selection).
/// A drag `ContentProvider` offering `paths` both as a `gdk::FileList`
/// (what GTK's own drop targets, e.g. sepia's other tabs/windows, expect)
/// and as `text/uri-list` bytes. The latter matters for dragging *out*
/// of sepia into a native, non-GTK app (Slack, Mail, Finder, etc.):
/// GTK's macOS backend only bridges a drag's advertised MIME types to
/// native NSPasteboard file types by looking for `text/uri-list`
/// specifically — a bare `GdkFileList` GValue, with no MIME type of its
/// own, falls back to an internal-only pasteboard type real apps can't
/// read, so nothing appears to happen when dropped on one.
fn file_drag_content_provider(paths: &[PathBuf]) -> gdk::ContentProvider {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    let file_list = gdk::FileList::from_array(&files);
    let file_list_provider = gdk::ContentProvider::for_value(&file_list.to_value());

    let uri_list = files.iter().map(|f| f.uri().to_string()).collect::<Vec<String>>().join("\r\n");
    let uri_provider = gdk::ContentProvider::for_bytes("text/uri-list", &glib::Bytes::from_owned(uri_list.into_bytes()));

    gdk::ContentProvider::new_union(&[file_list_provider, uri_provider])
}

fn selected_paths(state: &Rc<RefCell<AppState>>) -> Vec<PathBuf> {
    let state_ref = state.borrow();
    let tab = state_ref.active();
    let selection = if tab.view_stack.visible_child_name().as_deref() == Some("grid") {
        &tab.grid_selection
    } else {
        &tab.list_selection
    };
    let bitset = selection.selection();
    let mut positions = Vec::new();
    if let Some((mut iter, first)) = gtk::BitsetIter::init_first(&bitset) {
        positions.push(first);
        while let Some(next) = iter.next() {
            positions.push(next);
        }
    }
    positions
        .into_iter()
        // Resolved through `selection` itself (whichever of list/grid
        // this is), not `tab.store` directly — the list view's
        // selection wraps a `SortListModel` sitting in front of
        // `store`, so a raw store lookup would silently return the
        // wrong item whenever sorted by anything other than insertion
        // order.
        .filter_map(|position| selection.item(position).and_downcast::<FileEntryObject>())
        .map(|entry| entry.resolved_path(&tab.current_dir))
        .collect()
}

/// Copies dropped external files (from Finder, or another sidebar/file-list
/// drop) into `dest_dir`. Always copies rather than moves — GDK doesn't
/// reliably expose modifier-key state for cross-app drops on the
/// destination side, and copy is the safer default.
// GTK4's Quartz/macOS backend has a bug where file:// URIs synthesized from
// an external (e.g. Finder) drag have their scheme colon percent-encoded
// (`file%3A///...` instead of `file:///...`), which gio::File::path() fails
// to resolve. Repair the scheme and re-parse as a fallback.
fn repair_dropped_file_path(file: &gio::File) -> Option<PathBuf> {
    let uri = file.uri();
    let fixed = format!("file:{}", uri.strip_prefix("file%3A")?);
    gio::File::for_uri(&fixed).path()
}

fn accept_dropped_files(state: &Rc<RefCell<AppState>>, files: Vec<gio::File>, dest_dir: PathBuf) {
    let queue: Vec<PathBuf> = files
        .into_iter()
        .filter_map(|file| file.path().or_else(|| repair_dropped_file_path(&file)))
        .collect();
    if queue.is_empty() {
        return;
    }
    paste_next(state, queue, dest_dir, ClipboardMode::Copy, Vec::new());
}

fn show_error(state: &Rc<RefCell<AppState>>, message: &str) {
    let window = state.borrow().window.clone();
    AlertDialog::builder()
        .message("Sepia")
        .detail(message)
        .build()
        .show(Some(&window));
}

fn action_new_folder(state: &Rc<RefCell<AppState>>) {
    let dir = active_dir(state);
    match ops::new_folder(&dir) {
        Ok(created) => {
            state.borrow_mut().undo_manager.push(
                "New Folder",
                ops::undo::UndoStep::Remove {
                    path: created,
                    restore: ops::undo::Restore::EmptyFolder,
                },
            );
            refresh_active_view(state);
        }
        Err(error) => show_error(state, &format!("Couldn't create a new folder: {error}")),
    }
}

fn show_rename_dialog(state: &Rc<RefCell<AppState>>, path: PathBuf) {
    let current_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let window = state.borrow().window.clone();

    let dialog = GtkWindow::builder()
        .transient_for(&window)
        .modal(true)
        .title("Rename")
        .default_width(320)
        .resizable(false)
        .build();

    let entry = Entry::new();
    entry.set_text(&current_name);

    let label = Label::new(Some(&format!("New name for \u{201c}{current_name}\u{201d}:")));
    label.set_xalign(0.0);

    let cancel_button = Button::with_label("Cancel");
    let ok_button = Button::with_label("Rename");
    ok_button.add_css_class("suggested-action");
    let button_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(Align::End)
        .build();
    button_box.append(&cancel_button);
    button_box.append(&ok_button);

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&label);
    content.append(&entry);
    content.append(&button_box);
    dialog.set_child(Some(&content));

    let confirm = glib::clone!(
        #[strong]
        state,
        #[strong]
        dialog,
        #[strong]
        entry,
        #[strong]
        path,
        move || {
            let new_name = entry.text().to_string();
            dialog.close();
            if new_name.is_empty() || new_name == current_name {
                return;
            }
            match ops::rename(&path, &new_name) {
                Ok(renamed) => {
                    state.borrow_mut().undo_manager.push(
                        "Rename",
                        ops::undo::UndoStep::Rename {
                            from: renamed,
                            to: path.clone(),
                        },
                    );
                    refresh_active_view(&state);
                }
                Err(error) => show_error(&state, &format!("Couldn't rename: {error}")),
            }
        }
    );
    ok_button.connect_clicked(glib::clone!(
        #[strong]
        confirm,
        move |_| confirm()
    ));
    entry.connect_activate(move |_| confirm());
    cancel_button.connect_clicked(glib::clone!(
        #[strong]
        dialog,
        move |_| dialog.close()
    ));

    dialog.present();
}

fn action_rename_selected(state: &Rc<RefCell<AppState>>) {
    if let Some(path) = selected_path(state) {
        show_rename_dialog(state, path);
    }
}

/// Computes one item's new name for a given batch-rename mode. Shared by
/// the dialog's live preview and its actual execution, so the preview is
/// always exactly what running it will produce. `mode` is one of
/// `"replace"`, `"add"`, `"format"`.
#[allow(clippy::too_many_arguments)]
fn compute_batch_rename(
    path: &Path,
    index: usize,
    mode: &str,
    find: &str,
    replace: &str,
    add_text: &str,
    add_before: bool,
    format_name: &str,
    format_start: i64,
) -> String {
    let old_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match mode {
        "add" => {
            if add_before {
                format!("{add_text}{old_name}")
            } else {
                match path.extension().and_then(|e| e.to_str()) {
                    Some(ext) => {
                        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        format!("{stem}{add_text}.{ext}")
                    }
                    None => format!("{old_name}{add_text}"),
                }
            }
        }
        "format" => {
            let counter = format_start + index as i64;
            match path.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("{format_name} {counter}.{ext}"),
                None => format!("{format_name} {counter}"),
            }
        }
        _ => {
            if find.is_empty() {
                old_name
            } else {
                old_name.replace(find, replace)
            }
        }
    }
}

/// Batch Rename dialog (Finder-style: Replace Text / Add Text / Format
/// with a counter), with a live preview of every item's new name. Only
/// meaningful for 2+ items — the caller is expected to check that.
fn show_batch_rename_dialog(state: &Rc<RefCell<AppState>>, paths: Vec<PathBuf>) {
    let window = state.borrow().window.clone();

    let dialog = GtkWindow::builder()
        .transient_for(&window)
        .modal(true)
        .title(format!("Rename {} Items", paths.len()))
        .default_width(420)
        .default_height(420)
        .build();

    let mode_replace = gtk::CheckButton::with_label("Replace Text");
    let mode_add = gtk::CheckButton::with_label("Add Text");
    let mode_format = gtk::CheckButton::with_label("Format");
    mode_add.set_group(Some(&mode_replace));
    mode_format.set_group(Some(&mode_replace));
    mode_replace.set_active(true);
    let mode_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
    mode_box.append(&mode_replace);
    mode_box.append(&mode_add);
    mode_box.append(&mode_format);

    let find_entry = Entry::builder().placeholder_text("Find").hexpand(true).build();
    let replace_entry = Entry::builder().placeholder_text("Replace with").hexpand(true).build();
    let replace_fields = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
    replace_fields.append(&find_entry);
    replace_fields.append(&replace_entry);

    let add_entry = Entry::builder().placeholder_text("Text to add").hexpand(true).build();
    let add_before = gtk::CheckButton::with_label("Before name");
    let add_after = gtk::CheckButton::with_label("After name");
    add_after.set_group(Some(&add_before));
    add_after.set_active(true);
    let add_fields = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
    add_fields.append(&add_entry);
    add_fields.append(&add_before);
    add_fields.append(&add_after);

    let format_name_entry = Entry::builder().placeholder_text("Name").hexpand(true).build();
    let format_start_entry = Entry::builder().text("1").width_chars(6).build();
    let format_fields = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
    format_fields.append(&format_name_entry);
    format_fields.append(&Label::new(Some("starting at")));
    format_fields.append(&format_start_entry);

    let fields_stack = Stack::new();
    fields_stack.add_named(&replace_fields, Some("replace"));
    fields_stack.add_named(&add_fields, Some("add"));
    fields_stack.add_named(&format_fields, Some("format"));
    fields_stack.set_visible_child_name("replace");

    let preview_list = ListBox::new();
    let preview_scroll = ScrolledWindow::builder().vexpand(true).child(&preview_list).build();

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&mode_box);
    content.append(&fields_stack);
    content.append(&Label::builder().label("Preview").xalign(0.0).build());
    content.append(&preview_scroll);

    let cancel_button = Button::with_label("Cancel");
    let rename_button = Button::with_label("Rename");
    rename_button.add_css_class("suggested-action");
    let button_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(Align::End)
        .build();
    button_box.append(&cancel_button);
    button_box.append(&rename_button);
    content.append(&button_box);

    dialog.set_child(Some(&content));

    let read_mode = glib::clone!(
        #[strong]
        mode_replace,
        #[strong]
        mode_add,
        move || {
            if mode_replace.is_active() {
                "replace"
            } else if mode_add.is_active() {
                "add"
            } else {
                "format"
            }
        }
    );

    let update_preview: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[strong]
        paths,
        #[strong]
        preview_list,
        #[strong]
        find_entry,
        #[strong]
        replace_entry,
        #[strong]
        add_entry,
        #[strong]
        add_before,
        #[strong]
        format_name_entry,
        #[strong]
        format_start_entry,
        #[strong]
        read_mode,
        move || {
            while let Some(child) = preview_list.first_child() {
                preview_list.remove(&child);
            }
            let mode = read_mode();
            let find = find_entry.text();
            let replace = replace_entry.text();
            let add_text = add_entry.text();
            let add_before_active = add_before.is_active();
            let format_name = format_name_entry.text();
            let format_start: i64 = format_start_entry.text().parse().unwrap_or(1);

            for (i, path) in paths.iter().enumerate() {
                let old_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let new_name = compute_batch_rename(
                    path,
                    i,
                    mode,
                    &find,
                    &replace,
                    &add_text,
                    add_before_active,
                    &format_name,
                    format_start,
                );
                let row = Label::builder().label(format!("{old_name}  →  {new_name}")).xalign(0.0).build();
                preview_list.append(&row);
            }
        }
    ));

    mode_replace.connect_toggled(glib::clone!(
        #[strong]
        fields_stack,
        #[strong]
        update_preview,
        move |button| {
            if button.is_active() {
                fields_stack.set_visible_child_name("replace");
                update_preview();
            }
        }
    ));
    mode_add.connect_toggled(glib::clone!(
        #[strong]
        fields_stack,
        #[strong]
        update_preview,
        move |button| {
            if button.is_active() {
                fields_stack.set_visible_child_name("add");
                update_preview();
            }
        }
    ));
    mode_format.connect_toggled(glib::clone!(
        #[strong]
        fields_stack,
        #[strong]
        update_preview,
        move |button| {
            if button.is_active() {
                fields_stack.set_visible_child_name("format");
                update_preview();
            }
        }
    ));
    for entry in [&find_entry, &replace_entry, &add_entry, &format_name_entry, &format_start_entry] {
        entry.connect_changed(glib::clone!(
            #[strong]
            update_preview,
            move |_| update_preview()
        ));
    }
    for check in [&add_before, &add_after] {
        check.connect_toggled(glib::clone!(
            #[strong]
            update_preview,
            move |_| update_preview()
        ));
    }

    update_preview();

    cancel_button.connect_clicked(glib::clone!(
        #[strong]
        dialog,
        move |_| dialog.close()
    ));

    rename_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[strong]
        dialog,
        #[strong]
        paths,
        #[strong]
        find_entry,
        #[strong]
        replace_entry,
        #[strong]
        add_entry,
        #[strong]
        add_before,
        #[strong]
        format_name_entry,
        #[strong]
        format_start_entry,
        #[strong]
        read_mode,
        move |_| {
            let mode = read_mode();
            let find = find_entry.text();
            let replace = replace_entry.text();
            let add_text = add_entry.text();
            let add_before_active = add_before.is_active();
            let format_name = format_name_entry.text();
            let format_start: i64 = format_start_entry.text().parse().unwrap_or(1);

            let new_names: Vec<String> = paths
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    compute_batch_rename(
                        path,
                        i,
                        mode,
                        &find,
                        &replace,
                        &add_text,
                        add_before_active,
                        &format_name,
                        format_start,
                    )
                })
                .collect();

            let mut seen = std::collections::HashSet::new();
            for name in &new_names {
                if !seen.insert(name.clone()) {
                    show_error(&state, &format!("Multiple items would be named \u{201c}{name}\u{201d} — adjust the pattern and try again."));
                    return;
                }
            }

            let mut steps = Vec::new();
            let mut error = None;
            for (path, new_name) in paths.iter().zip(new_names.iter()) {
                let old_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if new_name.is_empty() || *new_name == old_name {
                    continue;
                }
                match ops::rename(path, new_name) {
                    Ok(renamed) => steps.push(ops::undo::UndoStep::Rename {
                        from: renamed,
                        to: path.clone(),
                    }),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }

            dialog.close();
            if !steps.is_empty() {
                state.borrow_mut().undo_manager.push("Batch Rename", ops::undo::UndoStep::Batch(steps));
            }
            if let Some(error) = error {
                show_error(&state, &format!("Couldn't rename all items: {error}"));
            }
            refresh_active_view(&state);
        }
    ));

    dialog.present();
}

fn action_batch_rename(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if paths.len() < 2 {
        return;
    }
    show_batch_rename_dialog(state, paths);
}

/// Compress dialog, matching Nautilus's own (verified against
/// `nautilus-compress-dialog.blp`/`.c`): archive name, a compression
/// method dropdown (ZIP / Encrypted ZIP / TAR.XZ / TAR.ZST / 7Z — the
/// same list Nautilus offers, plus TAR.ZST), and a password section
/// revealed only for Encrypted ZIP. Not undoable: the created archive has
/// no natural "recreate on redo" story with the existing `Restore`
/// variants (unlike New Folder, which redoes as an empty folder — an
/// archive's contents can't be regenerated that way).
fn show_compress_dialog(state: &Rc<RefCell<AppState>>, paths: Vec<PathBuf>) {
    let dest_dir = active_dir(state);
    let default_name = if let [only] = paths.as_slice() {
        only.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Archive".to_string())
    } else {
        "Archive".to_string()
    };

    let window = state.borrow().window.clone();
    let dialog = GtkWindow::builder()
        .transient_for(&window)
        .modal(true)
        .title("Compress")
        .default_width(360)
        .resizable(false)
        .build();

    let name_entry = Entry::new();
    name_entry.set_text(&default_name);

    let formats = [
        sepia_core::archive::CompressFormat::Zip,
        sepia_core::archive::CompressFormat::EncryptedZip,
        sepia_core::archive::CompressFormat::TarGz,
        sepia_core::archive::CompressFormat::TarXz,
        sepia_core::archive::CompressFormat::TarZst,
        sepia_core::archive::CompressFormat::SevenZip,
    ];
    let format_labels: Vec<&str> = formats.iter().map(|f| f.label()).collect();
    let format_dropdown = gtk::DropDown::from_strings(&format_labels);

    let password_entry = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    let confirm_entry = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    let password_box = GtkBox::builder().orientation(Orientation::Vertical).spacing(6).visible(false).build();
    password_box.append(&Label::builder().label("Password:").xalign(0.0).build());
    password_box.append(&password_entry);
    password_box.append(&Label::builder().label("Confirm Password:").xalign(0.0).build());
    password_box.append(&confirm_entry);

    format_dropdown.connect_selected_notify(glib::clone!(
        #[strong]
        password_box,
        #[strong]
        formats,
        move |dropdown| {
            let selected = formats.get(dropdown.selected() as usize).copied();
            password_box.set_visible(selected == Some(sepia_core::archive::CompressFormat::EncryptedZip));
        }
    ));

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&Label::builder().label("Archive Name:").xalign(0.0).build());
    content.append(&name_entry);
    content.append(&Label::builder().label("Compression Method:").xalign(0.0).build());
    content.append(&format_dropdown);
    content.append(&password_box);

    let cancel_button = Button::with_label("Cancel");
    let compress_button = Button::with_label("Compress");
    compress_button.add_css_class("suggested-action");
    let button_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(Align::End)
        .build();
    button_box.append(&cancel_button);
    button_box.append(&compress_button);
    content.append(&button_box);

    dialog.set_child(Some(&content));

    cancel_button.connect_clicked(glib::clone!(
        #[strong]
        dialog,
        move |_| dialog.close()
    ));

    compress_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[strong]
        dialog,
        #[strong]
        paths,
        #[strong]
        dest_dir,
        #[strong]
        name_entry,
        #[strong]
        format_dropdown,
        #[strong]
        password_entry,
        #[strong]
        confirm_entry,
        #[strong]
        formats,
        move |_| {
            let name = name_entry.text().to_string();
            if name.is_empty() {
                return;
            }
            let format = formats
                .get(format_dropdown.selected() as usize)
                .copied()
                .unwrap_or(sepia_core::archive::CompressFormat::Zip);

            let passphrase = if format == sepia_core::archive::CompressFormat::EncryptedZip {
                let password = password_entry.text().to_string();
                let confirm = confirm_entry.text().to_string();
                if password.is_empty() {
                    show_error(&state, "Enter a password for the encrypted zip.");
                    return;
                }
                if password != confirm {
                    show_error(&state, "Passwords don't match.");
                    return;
                }
                Some(password)
            } else {
                None
            };

            // Built directly from `name` + the format's (possibly
            // multi-part, e.g. "tar.xz") extension rather than via
            // `ops::unique_destination`, which assumes a single-component
            // extension and would mishandle these.
            let extension = format.extension();
            let mut dest = dest_dir.join(format!("{name}.{extension}"));
            let mut n = 2;
            while dest.exists() {
                dest = dest_dir.join(format!("{name} {n}.{extension}"));
                n += 1;
            }

            dialog.close();
            match sepia_core::archive::compress(&paths, &dest, format, passphrase.as_deref()) {
                Ok(()) => refresh_active_view(&state),
                Err(error) => show_error(&state, &format!("Couldn't compress: {error}")),
            }
        }
    ));

    dialog.present();
}

/// Runs a user script from the Scripts folder against the current
/// selection (and current directory, for scripts that don't need a
/// selection at all — e.g. one that generates a new file).
fn action_run_script(state: &Rc<RefCell<AppState>>, script_path: PathBuf) {
    let selected = selected_paths(state);
    let current_dir = active_dir(state);
    match sepia_core::scripts::run_script(&script_path, &selected, &current_dir) {
        Ok(()) => refresh_active_view(state),
        Err(error) => show_error(state, &error),
    }
}

fn action_open_scripts_folder(state: &Rc<RefCell<AppState>>) {
    let dir = sepia_core::scripts::ensure_scripts_dir();
    if let Err(error) = sepia_core::open::open_with_default_app(&dir) {
        show_error(state, &error);
    }
}

fn action_compress(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    show_compress_dialog(state, paths);
}

/// Handles double-click/Enter activation of a file-view item: navigates
/// into directories (existing behavior), and extracts archives in place
/// (matching Finder, where double-clicking a zip extracts it rather than
/// "opening" it in some app) — anything else is a no-op, since sepia has
/// no general file-opening implementation yet.
/// Takes the already-resolved `FileEntryObject` rather than a raw store
/// position — the list view's position is relative to the sorted model
/// (`SortListModel`), not the backing `gio::ListStore` directly, so the
/// caller has to resolve it through whichever model the position
/// actually came from.
fn on_item_activated(state: &Rc<RefCell<AppState>>, entry: FileEntryObject) {
    let (is_dir, path) = {
        let state_ref = state.borrow();
        let tab = state_ref.active();
        (entry.is_dir(), entry.resolved_path(&tab.current_dir))
    };
    if is_dir && !sepia_core::open::is_app_bundle(&path) {
        navigate(state, path);
    } else if sepia_core::archive::is_archive(&path) {
        match sepia_core::archive::extract(&path) {
            Ok(_dest) => refresh_active_view(state),
            Err(error) => show_error(state, &format!("Couldn't extract: {error}")),
        }
    } else {
        match sepia_core::open::open_with_default_app(&path) {
            Ok(()) => {
                state.borrow_mut().recent.record(path);
                // Reflects the reordered/new entry if Recent happens to
                // be the view currently open.
                refresh_active_view(state);
            }
            Err(error) => show_error(state, &error),
        }
    }
}

/// Extracts the selected archive into a new sibling folder (same
/// non-undoable rationale as `action_compress`). A no-op if the selection
/// isn't a single recognized archive.
fn action_extract(state: &Rc<RefCell<AppState>>) {
    let Some(path) = selected_path(state) else {
        return;
    };
    if !sepia_core::archive::is_archive(&path) {
        return;
    }
    match sepia_core::archive::extract(&path) {
        Ok(_dest) => refresh_active_view(state),
        Err(error) => show_error(state, &format!("Couldn't extract: {error}")),
    }
}

fn scaled_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut size = bytes as f64;
    let mut unit_index = 0;
    while size >= 1000.0 && unit_index < UNITS.len() - 1 {
        size /= 1000.0;
        unit_index += 1;
    }
    format!("{size:.1} {}", UNITS[unit_index])
}

fn format_size(bytes: u64) -> String {
    if bytes < 1000 {
        scaled_size(bytes)
    } else {
        format!("{} ({bytes} bytes)", scaled_size(bytes))
    }
}

/// Kind text/list-view Size column formatting share this: neither stats
/// the filesystem (folder sizes aren't computed eagerly — matching
/// Finder's own non-recursive default — and Kind is derived purely from
/// the name, same source as the list/grid view's icon selection).
fn kind_text(name: &str, is_dir: bool) -> String {
    if is_dir && name.to_ascii_lowercase().ends_with(".app") {
        "Application".to_string()
    } else if is_dir {
        "Folder".to_string()
    } else {
        match Path::new(name).extension() {
            Some(ext) => format!("{} Document", ext.to_string_lossy().to_uppercase()),
            None => "Document".to_string(),
        }
    }
}

/// Recursively sums a folder's total size and item count (all descendants,
/// not just direct children). Synchronous — fine for a deliberate,
/// occasional action like Get Info, unlike live search.
fn dir_stats(path: &Path) -> (u64, u64) {
    let mut total_bytes = 0u64;
    let mut item_count = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            item_count += 1;
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    let (sub_bytes, sub_count) = dir_stats(&entry.path());
                    total_bytes += sub_bytes;
                    item_count += sub_count;
                } else {
                    total_bytes += metadata.len();
                }
            }
        }
    }
    (total_bytes, item_count)
}

fn format_time(time: Option<std::time::SystemTime>) -> String {
    match time {
        Some(t) => {
            let datetime: chrono::DateTime<chrono::Local> = t.into();
            datetime.format("%B %-d, %Y at %-I:%M %p").to_string()
        }
        None => "Unknown".to_string(),
    }
}

/// A narrower rendering of the same timestamp for the list view's Date
/// Modified column — `format_time`'s full "August 3, 2026 at 11:34 PM"
/// is Get-Info-dialog-appropriate but too wide for a table column.
fn format_time_compact(time: Option<std::time::SystemTime>) -> String {
    match time {
        Some(t) => {
            let datetime: chrono::DateTime<chrono::Local> = t.into();
            datetime.format("%-m/%-d/%y, %-I:%M %p").to_string()
        }
        None => "\u{2014}".to_string(),
    }
}

fn permission_summary(mode: u32) -> String {
    fn describe(bits: u32) -> &'static str {
        match bits & 0o7 {
            0 => "No Access",
            1 => "Execute Only",
            2 => "Write Only",
            3 => "Write & Execute",
            4 => "Read Only",
            5 | 7 => "Read & Execute",
            6 => "Read & Write",
            _ => unreachable!(),
        }
    }
    format!(
        "Owner: {}, Group: {}, Everyone: {}",
        describe(mode >> 6),
        describe(mode >> 3),
        describe(mode)
    )
}

fn info_row(label_text: &str, value_text: &str) -> GtkBox {
    let row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
    let label = Label::builder().label(label_text).xalign(1.0).width_chars(9).build();
    label.add_css_class("dim-label");
    let value = Label::builder().label(value_text).xalign(0.0).wrap(true).hexpand(true).build();
    row.append(&label);
    row.append(&value);
    row
}

/// Clears and rebuilds `tags_list`'s rows from `path`'s current real
/// Finder tags (via `sepia_platform_macos::tags`, backed by the same
/// `com.apple.metadata:_kMDItemUserTags` extended attribute Finder itself
/// reads/writes — not the app-local "Starred" favorites list). Each row
/// has a remove button; used by both the initial Get Info population and
/// after any add/remove.
fn rebuild_tags_list(tags_list: &GtkBox, state: &Rc<RefCell<AppState>>, path: PathBuf) {
    while let Some(child) = tags_list.first_child() {
        tags_list.remove(&child);
    }
    for tag in sepia_platform_macos::tags::get_tags(&path) {
        let row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
        row.append(&Label::builder().label(&tag.name).xalign(0.0).hexpand(true).build());
        let remove_button = Button::from_icon_name("window-close-symbolic");
        remove_button.add_css_class("flat");
        remove_button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[strong]
            path,
            #[strong]
            tags_list,
            move |_| {
                let mut names: Vec<String> =
                    sepia_platform_macos::tags::get_tags(&path).into_iter().map(|t| t.name).collect();
                names.retain(|n| n != &tag.name);
                if let Err(error) = sepia_platform_macos::tags::set_tags(&path, &names) {
                    show_error(&state, &format!("Couldn't update tags: {error}"));
                    return;
                }
                rebuild_tags_list(&tags_list, &state, path.clone());
            }
        ));
        row.append(&remove_button);
        tags_list.append(&row);
    }
}

/// A Finder-style Get Info window: name (editable — renames on Enter,
/// matching Finder), kind, size, location, timestamps, and a read-only
/// Unix permissions summary. Non-modal, like Finder's own Get Info
/// windows — you can keep browsing with it open.
fn show_get_info_dialog(state: &Rc<RefCell<AppState>>, path: PathBuf) {
    let Ok(metadata) = std::fs::metadata(&path) else {
        show_error(state, &format!("Couldn't get info for {}", path.display()));
        return;
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let is_dir = metadata.is_dir();
    let is_app_bundle = sepia_core::open::is_app_bundle(&path);

    let (size_text, kind_text) = if is_app_bundle {
        let (bytes, _) = dir_stats(&path);
        (format_size(bytes), "Application".to_string())
    } else if is_dir {
        let (bytes, count) = dir_stats(&path);
        (
            format!("{} — {count} item{}", format_size(bytes), if count == 1 { "" } else { "s" }),
            "Folder".to_string(),
        )
    } else {
        let kind = path
            .extension()
            .map(|ext| format!("{} Document", ext.to_string_lossy().to_uppercase()))
            .unwrap_or_else(|| "Document".to_string());
        (format_size(metadata.len()), kind)
    };

    let where_text = path.parent().map(display_path).unwrap_or_default();
    let created_text = format_time(metadata.created().ok());
    let modified_text = format_time(metadata.modified().ok());
    let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
    let permissions_text = permission_summary(mode);

    let window = state.borrow().window.clone();

    let dialog = GtkWindow::builder()
        .transient_for(&window)
        .title(format!("{name} Info"))
        .default_width(340)
        .resizable(false)
        .build();

    let icon = Image::from_icon_name(if is_app_bundle {
        "application-x-executable"
    } else if is_dir {
        "folder"
    } else {
        "text-x-generic"
    });
    icon.set_pixel_size(64);

    let name_entry = Entry::new();
    name_entry.set_text(&name);
    name_entry.set_halign(Align::Center);

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&icon);
    content.append(&name_entry);
    content.append(&gtk::Separator::new(Orientation::Horizontal));
    content.append(&info_row("Kind:", &kind_text));
    content.append(&info_row("Size:", &size_text));
    content.append(&info_row("Where:", &where_text));
    content.append(&info_row("Created:", &created_text));
    content.append(&info_row("Modified:", &modified_text));
    content.append(&gtk::Separator::new(Orientation::Horizontal));
    content.append(&info_row("Permissions:", &permissions_text));
    content.append(&gtk::Separator::new(Orientation::Horizontal));

    content.append(&Label::builder().label("Tags:").xalign(0.0).build());
    let tags_list = GtkBox::builder().orientation(Orientation::Vertical).spacing(4).build();
    content.append(&tags_list);
    let tag_add_entry = Entry::builder().placeholder_text("Add a tag…").build();
    content.append(&tag_add_entry);

    dialog.set_child(Some(&content));

    rebuild_tags_list(&tags_list, state, path.clone());
    tag_add_entry.connect_activate(glib::clone!(
        #[strong]
        state,
        #[strong]
        path,
        #[strong]
        tags_list,
        move |entry| {
            let new_tag = entry.text().to_string();
            if new_tag.is_empty() {
                return;
            }
            entry.set_text("");
            let mut names: Vec<String> = sepia_platform_macos::tags::get_tags(&path).into_iter().map(|t| t.name).collect();
            if !names.contains(&new_tag) {
                names.push(new_tag);
                if let Err(error) = sepia_platform_macos::tags::set_tags(&path, &names) {
                    show_error(&state, &format!("Couldn't update tags: {error}"));
                    return;
                }
            }
            rebuild_tags_list(&tags_list, &state, path.clone());
        }
    ));

    name_entry.connect_activate(glib::clone!(
        #[strong]
        state,
        #[strong]
        path,
        move |entry| {
            let new_name = entry.text().to_string();
            if new_name.is_empty() || new_name == name {
                return;
            }
            match ops::rename(&path, &new_name) {
                Ok(renamed) => {
                    state.borrow_mut().undo_manager.push(
                        "Rename",
                        ops::undo::UndoStep::Rename {
                            from: renamed,
                            to: path.clone(),
                        },
                    );
                    refresh_active_view(&state);
                }
                Err(error) => show_error(&state, &format!("Couldn't rename: {error}")),
            }
        }
    ));

    dialog.present();
}

fn action_get_info(state: &Rc<RefCell<AppState>>) {
    for path in selected_paths(state) {
        show_get_info_dialog(state, path);
    }
}

/// Toggles starred state for every selected item independently (so a
/// mixed selection ends up all-starred rather than each item flipping to
/// the opposite of its own prior state).
fn action_toggle_starred(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    {
        let mut state_mut = state.borrow_mut();
        let should_star = paths.iter().any(|p| !state_mut.starred.is_starred(p));
        for path in paths {
            let currently = state_mut.starred.is_starred(&path);
            if currently != should_star {
                state_mut.starred.toggle(path);
            }
        }
    }
    // Reflects the change immediately if Starred happens to be the view
    // currently open.
    refresh_active_view(state);
}

/// "Connect to Server" (Cmd+K, matching Finder's own convention): a URL
/// entry (smb://, afp://, nfs://, ftp://) plus a list of recently-
/// connected servers for quick reconnect. Connecting shells out to
/// macOS's own `open`, which handles the actual mount and any credential
/// prompts — sepia doesn't implement network mounting itself. The newly
/// mounted share then shows up in the sidebar like any other volume, via
/// the periodic volume-list poll set up in `build_ui`.
fn show_connect_to_server_dialog(state: &Rc<RefCell<AppState>>) {
    let window = state.borrow().window.clone();
    let recent: Vec<String> = state.borrow().recent_servers.entries().to_vec();

    let dialog = GtkWindow::builder()
        .transient_for(&window)
        .modal(true)
        .title("Connect to Server")
        .default_width(380)
        .resizable(false)
        .build();

    let entry = Entry::builder().placeholder_text("smb://server/share").hexpand(true).build();

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&Label::builder().label("Server Address:").xalign(0.0).build());
    content.append(&entry);

    if !recent.is_empty() {
        content.append(&Label::builder().label("Recent Servers:").xalign(0.0).margin_top(8).build());
        let recent_list = ListBox::new();
        recent_list.add_css_class("boxed-list");
        for url in &recent {
            recent_list.append(
                &Label::builder()
                    .label(url)
                    .xalign(0.0)
                    .margin_top(4)
                    .margin_bottom(4)
                    .margin_start(8)
                    .margin_end(8)
                    .build(),
            );
        }
        recent_list.connect_row_activated(glib::clone!(
            #[strong]
            entry,
            #[strong]
            recent,
            move |_, row| {
                if let Some(url) = recent.get(row.index() as usize) {
                    entry.set_text(url);
                }
            }
        ));
        let scroll = ScrolledWindow::builder().height_request(120).child(&recent_list).build();
        content.append(&scroll);
    }

    let cancel_button = Button::with_label("Cancel");
    let connect_button = Button::with_label("Connect");
    connect_button.add_css_class("suggested-action");
    let button_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(Align::End)
        .build();
    button_box.append(&cancel_button);
    button_box.append(&connect_button);
    content.append(&button_box);

    dialog.set_child(Some(&content));

    cancel_button.connect_clicked(glib::clone!(
        #[strong]
        dialog,
        move |_| dialog.close()
    ));

    let confirm = glib::clone!(
        #[strong]
        state,
        #[strong]
        dialog,
        #[strong]
        entry,
        move || {
            let url = entry.text().to_string();
            if url.is_empty() {
                return;
            }
            match sepia_core::network::connect(&url) {
                Ok(()) => {
                    state.borrow_mut().recent_servers.record(url);
                    dialog.close();
                }
                Err(error) => show_error(&state, &error),
            }
        }
    );
    connect_button.connect_clicked(glib::clone!(
        #[strong]
        confirm,
        move |_| confirm()
    ));
    entry.connect_activate(move |_| confirm());

    dialog.present();
}

fn action_connect_to_server(state: &Rc<RefCell<AppState>>) {
    show_connect_to_server_dialog(state);
}

fn show_about_dialog(state: &Rc<RefCell<AppState>>) {
    let window = state.borrow().window.clone();
    let dialog = gtk::AboutDialog::builder()
        .transient_for(&window)
        .modal(true)
        .program_name("Sepia")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("An alternative file browser for macOS, because Finder sucks.")
        .logo_icon_name("system-file-manager")
        .license_type(gtk::License::Gpl20Only)
        .copyright("© 2026 Mark King")
        .build();
    dialog.present();
}

/// A non-modal Preferences window covering the settings sepia actually
/// has: where new windows open, what view new tabs default to, and
/// whether hidden files are shown (mirrors the existing Cmd+. toggle for
/// discoverability). Each control saves immediately on change, matching
/// macOS Preferences conventions (no separate "Apply"/"OK").
fn show_preferences_window(state: &Rc<RefCell<AppState>>) {
    let (window, show_hidden, new_window_location, default_view) = {
        let state_ref = state.borrow();
        (
            state_ref.window.clone(),
            state_ref.show_hidden,
            state_ref.preferences.new_window_location,
            state_ref.preferences.default_view,
        )
    };

    let dialog = GtkWindow::builder()
        .transient_for(&window)
        .title("Preferences")
        .default_width(320)
        .resizable(false)
        .build();

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(16)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();

    let location_label = Label::builder().label("New windows open at:").xalign(0.0).build();
    location_label.add_css_class("heading");
    let home_radio = gtk::CheckButton::with_label("Home");
    let last_radio = gtk::CheckButton::with_label("Last Opened Folder");
    last_radio.set_group(Some(&home_radio));
    match new_window_location {
        sepia_core::preferences::NewWindowLocation::Home => home_radio.set_active(true),
        sepia_core::preferences::NewWindowLocation::LastLocation => last_radio.set_active(true),
    }
    let location_box = GtkBox::builder().orientation(Orientation::Vertical).spacing(4).build();
    location_box.append(&home_radio);
    location_box.append(&last_radio);

    home_radio.connect_toggled(glib::clone!(
        #[strong]
        state,
        move |button| {
            if button.is_active() {
                let mut state_mut = state.borrow_mut();
                state_mut.preferences.new_window_location = sepia_core::preferences::NewWindowLocation::Home;
                state_mut.preferences.save();
            }
        }
    ));
    last_radio.connect_toggled(glib::clone!(
        #[strong]
        state,
        move |button| {
            if button.is_active() {
                let mut state_mut = state.borrow_mut();
                state_mut.preferences.new_window_location = sepia_core::preferences::NewWindowLocation::LastLocation;
                state_mut.preferences.save();
            }
        }
    ));

    let view_label = Label::builder().label("New tabs open in:").xalign(0.0).build();
    view_label.add_css_class("heading");
    let list_radio = gtk::CheckButton::with_label("List View");
    let grid_radio = gtk::CheckButton::with_label("Icon View");
    grid_radio.set_group(Some(&list_radio));
    match default_view {
        sepia_core::preferences::DefaultView::List => list_radio.set_active(true),
        sepia_core::preferences::DefaultView::Grid => grid_radio.set_active(true),
    }
    let view_box = GtkBox::builder().orientation(Orientation::Vertical).spacing(4).build();
    view_box.append(&list_radio);
    view_box.append(&grid_radio);

    list_radio.connect_toggled(glib::clone!(
        #[strong]
        state,
        move |button| {
            if button.is_active() {
                let mut state_mut = state.borrow_mut();
                state_mut.preferences.default_view = sepia_core::preferences::DefaultView::List;
                state_mut.preferences.save();
            }
        }
    ));
    grid_radio.connect_toggled(glib::clone!(
        #[strong]
        state,
        move |button| {
            if button.is_active() {
                let mut state_mut = state.borrow_mut();
                state_mut.preferences.default_view = sepia_core::preferences::DefaultView::Grid;
                state_mut.preferences.save();
            }
        }
    ));

    let hidden_check = gtk::CheckButton::with_label("Show Hidden Files");
    hidden_check.set_active(show_hidden);
    hidden_check.connect_toggled(glib::clone!(
        #[strong]
        state,
        move |button| {
            let currently = state.borrow().show_hidden;
            if button.is_active() != currently {
                action_toggle_hidden(&state);
            }
        }
    ));

    content.append(&location_label);
    content.append(&location_box);
    content.append(&view_label);
    content.append(&view_box);
    content.append(&gtk::Separator::new(Orientation::Horizontal));
    content.append(&hidden_check);

    dialog.set_child(Some(&content));
    dialog.present();
}

fn action_move_to_trash(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    let mut steps = Vec::new();
    let mut error = None;
    for path in &paths {
        match sepia_platform_macos::trash::trash(path) {
            Ok(trashed) => steps.push(ops::undo::UndoStep::Move {
                from: trashed,
                to: path.clone(),
            }),
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    if !steps.is_empty() {
        state.borrow_mut().undo_manager.push("Move to Trash", ops::undo::UndoStep::Batch(steps));
    }
    refresh_active_view(state);
    if let Some(error) = error {
        show_error(state, &format!("Couldn't move to Trash: {error}"));
    }
}

fn action_delete_permanently(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    let detail = if let [only] = paths.as_slice() {
        let name = only.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("\u{201c}{name}\u{201d} will be deleted immediately. You can't undo this action.")
    } else {
        format!("{} items will be deleted immediately. You can't undo this action.", paths.len())
    };
    let window = state.borrow().window.clone();
    let dialog = AlertDialog::builder()
        .message("Delete Permanently?")
        .detail(detail)
        .buttons(["Cancel", "Delete"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    dialog.choose(Some(&window), gio::Cancellable::NONE, move |result| {
        if result != Ok(1) {
            return;
        }
        let mut error = None;
        for path in &paths {
            if let Err(e) = ops::delete_permanently(path) {
                error = Some(e);
                break;
            }
        }
        refresh_active_view(&state);
        if let Some(error) = error {
            show_error(&state, &format!("Couldn't delete: {error}"));
        }
    });
}

fn action_copy_selection(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if !paths.is_empty() {
        state.borrow_mut().clipboard = Some((paths, ClipboardMode::Copy));
    }
}

fn action_cut_selection(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if !paths.is_empty() {
        state.borrow_mut().clipboard = Some((paths, ClipboardMode::Cut));
    }
}

/// Performs one copy or move and returns its undo inverse. Pure — no
/// state/undo-push/refresh side effects, since `paste_next` batches those
/// across the whole queue.
fn perform_paste(src: &Path, dest: &Path, mode: ClipboardMode) -> Result<ops::undo::UndoStep, String> {
    match mode {
        ClipboardMode::Copy => ops::copy_recursive(src, dest),
        ClipboardMode::Cut => ops::move_path(src, dest),
    }
    .map_err(|e| e.to_string())?;
    Ok(match mode {
        ClipboardMode::Copy => ops::undo::UndoStep::Remove {
            path: dest.to_path_buf(),
            restore: ops::undo::Restore::CopyFrom(src.to_path_buf()),
        },
        ClipboardMode::Cut => ops::undo::UndoStep::Move {
            from: dest.to_path_buf(),
            to: src.to_path_buf(),
        },
    })
}

fn finish_paste_queue(state: &Rc<RefCell<AppState>>, mode: ClipboardMode, steps: Vec<ops::undo::UndoStep>) {
    if !steps.is_empty() {
        let name = match mode {
            ClipboardMode::Copy => "Copy",
            ClipboardMode::Cut => "Move",
        };
        state.borrow_mut().undo_manager.push(name, ops::undo::UndoStep::Batch(steps));
    }
    if mode == ClipboardMode::Cut {
        state.borrow_mut().clipboard = None;
    }
    refresh_active_view(state);
}

/// Pastes/copies `queue` into `dest_dir` one item at a time, accumulating
/// `steps` into a single batched undo entry. Sequential (not a plain
/// `for` loop) because each name-conflict dialog is asynchronous —
/// kicking them all off at once would race on multiple simultaneous
/// dialogs. Shared by clipboard paste and drag-and-drop import.
fn paste_next(
    state: &Rc<RefCell<AppState>>,
    mut queue: Vec<PathBuf>,
    dest_dir: PathBuf,
    mode: ClipboardMode,
    mut steps: Vec<ops::undo::UndoStep>,
) {
    let Some(src) = queue.pop() else {
        finish_paste_queue(state, mode, steps);
        return;
    };
    let Some(file_name) = src.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        paste_next(state, queue, dest_dir, mode, steps);
        return;
    };
    let dest = dest_dir.join(&file_name);
    if !dest.exists() {
        match perform_paste(&src, &dest, mode) {
            Ok(step) => steps.push(step),
            Err(error) => show_error(state, &format!("Couldn't paste: {error}")),
        }
        paste_next(state, queue, dest_dir, mode, steps);
        return;
    }

    let window = state.borrow().window.clone();
    let dialog = AlertDialog::builder()
        .message("Item Already Exists")
        .detail(format!(
            "\u{201c}{file_name}\u{201d} already exists in this location. \
             Do you want to replace it, keep both, or skip it?"
        ))
        .buttons(["Skip", "Keep Both", "Replace"])
        .cancel_button(0)
        .build();
    let state = state.clone();
    dialog.choose(Some(&window), gio::Cancellable::NONE, move |result| match result {
        Ok(2) => {
            // Replace
            if let Err(error) = ops::delete_permanently(&dest) {
                show_error(&state, &format!("Couldn't replace: {error}"));
                paste_next(&state, queue, dest_dir, mode, steps);
                return;
            }
            match perform_paste(&src, &dest, mode) {
                Ok(step) => steps.push(step),
                Err(error) => show_error(&state, &format!("Couldn't paste: {error}")),
            }
            paste_next(&state, queue, dest_dir, mode, steps);
        }
        Ok(1) => {
            // Keep Both
            let unique = ops::unique_destination(&dest_dir, &file_name);
            match perform_paste(&src, &unique, mode) {
                Ok(step) => steps.push(step),
                Err(error) => show_error(&state, &format!("Couldn't paste: {error}")),
            }
            paste_next(&state, queue, dest_dir, mode, steps);
        }
        // Skip (or the dialog was otherwise dismissed)
        _ => paste_next(&state, queue, dest_dir, mode, steps),
    });
}

fn action_paste_clipboard(state: &Rc<RefCell<AppState>>) {
    let Some((sources, mode)) = state.borrow().clipboard.clone() else {
        return;
    };
    if sources.is_empty() {
        return;
    }
    let dest_dir = active_dir(state);
    paste_next(state, sources, dest_dir, mode, Vec::new());
}

/// Opens the currently selected item(s) with their default app, matching
/// double-click — used by the context menu's "Open" item, which (unlike
/// `on_item_activated`) can apply to a whole multi-selection at once.
fn action_open_selected(state: &Rc<RefCell<AppState>>) {
    for path in selected_paths(state) {
        match sepia_core::open::open_with_default_app(&path) {
            Ok(()) => state.borrow_mut().recent.record(path),
            Err(error) => {
                show_error(state, &error);
                return;
            }
        }
    }
    refresh_active_view(state);
}

/// "Move To…"/"Copy To…": prompts for a destination folder (unlike Cut/
/// Copy+Paste, this is a single action with no clipboard involved),
/// via the current selection.
fn show_move_or_copy_to_dialog(state: &Rc<RefCell<AppState>>, mode: ClipboardMode) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    let window = state.borrow().window.clone();
    let title = match mode {
        ClipboardMode::Copy => "Copy To",
        ClipboardMode::Cut => "Move To",
    };
    let dialog = gtk::FileDialog::builder().title(title).build();
    dialog.select_folder(
        Some(&window),
        gio::Cancellable::NONE,
        glib::clone!(
            #[strong]
            state,
            move |result| {
                let Ok(folder) = result else {
                    return;
                };
                let Some(dest_dir) = folder.path() else {
                    return;
                };
                paste_next(&state, paths, dest_dir, mode, Vec::new());
            }
        ),
    );
}

fn action_move_to(state: &Rc<RefCell<AppState>>) {
    show_move_or_copy_to_dialog(state, ClipboardMode::Cut);
}

fn action_copy_to(state: &Rc<RefCell<AppState>>) {
    show_move_or_copy_to_dialog(state, ClipboardMode::Copy);
}

/// The current selection, if it's exactly one directory that isn't an
/// app bundle — shared by the context menu (to decide whether to show
/// the directory-specific "Open" submenu) and the New Tab/New Window/
/// Console actions below (which only make sense for such a selection).
fn single_selected_dir(state: &Rc<RefCell<AppState>>) -> Option<PathBuf> {
    let paths = selected_paths(state);
    if let [only] = paths.as_slice() {
        (only.is_dir() && !sepia_core::open::is_app_bundle(only)).then(|| only.clone())
    } else {
        None
    }
}

fn action_open_selected_in_new_tab(state: &Rc<RefCell<AppState>>) {
    if let Some(dir) = single_selected_dir(state) {
        new_tab(state, &state_cell_for(state), dir);
    }
}

fn action_open_selected_in_new_window(state: &Rc<RefCell<AppState>>) {
    if let Some(dir) = single_selected_dir(state) {
        if let Some(app) = state.borrow().window.application() {
            build_window(&app, Some(dir));
        }
    }
}

fn action_open_selected_in_console(state: &Rc<RefCell<AppState>>) {
    if let Some(dir) = single_selected_dir(state) {
        if let Err(error) = sepia_core::open::open_terminal_at(&dir) {
            show_error(state, &error);
        }
    }
}

fn action_email_selected(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if let Err(error) = sepia_core::email::email_files(&paths) {
        show_error(state, &error);
    }
}

/// Backing action for the sidebar Trash row's "Empty Trash…" — like
/// Delete Permanently, gated behind an explicit confirmation rather than
/// a single click.
fn action_empty_trash(state: &Rc<RefCell<AppState>>) {
    let Some(trash_dir) = sepia_core::open::trash_dir() else {
        return;
    };
    let window = state.borrow().window.clone();
    let dialog = AlertDialog::builder()
        .message("Empty Trash?")
        .detail("Items in the Trash will be deleted immediately. You can't undo this action.")
        .buttons(["Cancel", "Empty Trash"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    dialog.choose(Some(&window), gio::Cancellable::NONE, move |result| {
        if result != Ok(1) {
            return;
        }
        if let Err(error) = ops::empty_trash(&trash_dir) {
            show_error(&state, &format!("Couldn't empty Trash: {error}"));
        }
        refresh_active_view(&state);
    });
}

/// Backing action for a bookmark row's "Remove from Bookmarks" —
/// parameterized (the bookmark's path as a string) since it's one
/// action shared by every bookmark row, not a per-row action group.
fn action_remove_bookmark(state: &Rc<RefCell<AppState>>, path: PathBuf) {
    state.borrow_mut().bookmarks.remove(&path);
    if let Some(list_box) = state.borrow().sidebar_list_box.clone() {
        rebuild_sidebar_rows(state, &list_box);
    }
}

/// Right-click context menu for a file-view item, matching Nautilus's own
/// (verified against `nautilus-files-view-context-menus.ui`): Open (with
/// the actual default app named), Open With…, Cut/Copy/Move To…/Copy
/// To…, Rename…/Compress…/Email…/Move to Trash, Get Info. For a single
/// selected *directory*, "Open" instead becomes a submenu — Open/Open in
/// New Tab/Open in New Window, then Open With…/Open in Console — also
/// matching Nautilus exactly (its `open_in_view_submenu`). Items already
/// in the Trash get Delete Permanently instead of the normal move/copy/
/// file actions (no Restore — macOS's Finder doesn't expose "Put Back"
/// as a real scriptable AppleScript command, only as GUI automation of
/// its own Trash window, too fragile to rely on; dragging an item out of
/// Trash, browsed as a normal folder, already works as a manual restore).
///
/// Every item is a global `app.*` action (reading the current selection
/// fresh at activation time, same as the menu bar's own actions) rather
/// than a per-click `SimpleActionGroup` inserted directly on the popover
/// — that combination turned out to silently break activation for every
/// item in the menu on this GTK4/quartz setup (confirmed via an isolated
/// repro unrelated to any of sepia's own menu content: a real GTK bug,
/// not something wrong with the menu itself). "Open With…" is the one
/// exception that still needs per-click data (a dynamic, per-file-type
/// app list) — handled via a single parameterized `app.open_with` action
/// instead, with each item passing its app's bundle path as the target.
fn show_context_menu(state: &Rc<RefCell<AppState>>, parent: &impl IsA<gtk::Widget>, x: f64, y: f64) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    let single_dir = single_selected_dir(state);
    let all_in_trash = paths.iter().all(|p| sepia_core::open::is_in_trash(p));

    let open_label = paths
        .first()
        .and_then(|p| sepia_platform_macos::apps::default_app_for(p))
        .map(|app| format!("Open with {}", app.name))
        .unwrap_or_else(|| "Open".to_string());

    let open_with_menu = gio::Menu::new();
    if let Some(first) = paths.first() {
        for app in sepia_platform_macos::apps::apps_for(first) {
            let item = gio::MenuItem::new(Some(&app.name), None);
            item.set_action_and_target_value(
                Some("app.open_with"),
                Some(&app.path.to_string_lossy().to_string().to_variant()),
            );
            open_with_menu.append_item(&item);
        }
    }

    let menu = gio::Menu::new();

    let open_section = gio::Menu::new();
    if single_dir.is_some() {
        let open_submenu = gio::Menu::new();
        let top = gio::Menu::new();
        top.append(Some("Open"), Some("app.open_selected"));
        top.append(Some("Open in New Tab"), Some("app.open_selected_in_new_tab"));
        top.append(Some("Open in New Window"), Some("app.open_selected_in_new_window"));
        open_submenu.append_section(None, &top);
        let bottom = gio::Menu::new();
        bottom.append_submenu(Some("Open With…"), &open_with_menu);
        bottom.append(Some("Open in Console"), Some("app.open_selected_in_console"));
        open_submenu.append_section(None, &bottom);
        open_section.append_submenu(Some("Open"), &open_submenu);
    } else {
        open_section.append(Some(&open_label), Some("app.open_selected"));
        open_section.append_submenu(Some("Open With…"), &open_with_menu);
    }
    menu.append_section(None, &open_section);

    if !all_in_trash {
        let move_copy_section = gio::Menu::new();
        move_copy_section.append(Some("Cut"), Some("app.cut_selection"));
        move_copy_section.append(Some("Copy"), Some("app.copy_selection"));
        move_copy_section.append(Some("Move To…"), Some("app.move_to"));
        move_copy_section.append(Some("Copy To…"), Some("app.copy_to"));
        menu.append_section(None, &move_copy_section);
    }

    let file_actions_section = gio::Menu::new();
    file_actions_section.append(Some("Rename…"), Some("app.rename_selected"));
    if all_in_trash {
        file_actions_section.append(Some("Delete Permanently…"), Some("app.delete_permanently"));
    } else {
        file_actions_section.append(Some("Compress…"), Some("app.compress"));
        file_actions_section.append(Some("Email…"), Some("app.email_selected"));
        file_actions_section.append(Some("Move to Trash"), Some("app.move_to_trash"));
    }
    menu.append_section(None, &file_actions_section);

    let properties_section = gio::Menu::new();
    properties_section.append(Some("Get Info"), Some("app.get_info"));
    menu.append_section(None, &properties_section);

    // Routed through an invisible `GtkMenuButton` rather than a bare
    // `PopoverMenu` shown via manual `set_parent()`/`popup()` — the
    // latter, even with no per-click action group involved at all,
    // wasn't confirmed to actually activate anything on this backend;
    // only the MenuButton-driven construction was.
    // Parented to the tab's view stack, not the per-cell widget that was
    // actually clicked — `set_parent` makes this a real layout
    // participant, and the per-cell widget is a tight `GtkBox` row
    // (icon + label) that a third child visibly disturbs, widening the
    // row. The view stack isn't laying out visible row content, so an
    // invisible 0x0 child there doesn't affect anything on screen. The
    // click point is translated from the cell's coordinate space into
    // the view stack's accordingly.
    let view_stack = state.borrow().active().view_stack.clone();
    let point = parent.compute_point(&view_stack, &gtk::graphene::Point::new(x as f32, y as f32));
    let (stack_x, stack_y) = point.map(|p| (p.x() as i32, p.y() as i32)).unwrap_or((x as i32, y as i32));

    let menu_button = gtk::MenuButton::new();
    menu_button.set_has_frame(false);
    menu_button.set_size_request(0, 0);
    menu_button.set_can_focus(false);
    menu_button.set_can_target(false);
    // A completely empty custom child, instead of leaving the button's
    // default icon/label content unset, is what actually suppresses its
    // built-in dropdown-arrow indicator (which otherwise renders
    // regardless of has_frame/size_request).
    menu_button.set_child(Some(&GtkBox::new(Orientation::Horizontal, 0)));
    menu_button.set_parent(&view_stack);
    menu_button.set_menu_model(Some(&menu));
    if let Some(popover) = menu_button.popover() {
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(stack_x, stack_y, 1, 1)));
        popover.connect_closed(glib::clone!(
            #[strong]
            menu_button,
            move |_| menu_button.unparent()
        ));
    }
    menu_button.popup();
}

fn perform_undo(state: &Rc<RefCell<AppState>>) {
    let popped = state.borrow_mut().undo_manager.undo_stack.pop();
    let Some((name, step)) = popped else {
        return;
    };
    match step.perform() {
        Ok(inverse) => {
            state.borrow_mut().undo_manager.redo_stack.push((name, inverse));
            refresh_active_view(state);
        }
        Err(error) => show_error(state, &format!("Couldn't undo: {error}")),
    }
}

fn perform_redo(state: &Rc<RefCell<AppState>>) {
    let popped = state.borrow_mut().undo_manager.redo_stack.pop();
    let Some((name, step)) = popped else {
        return;
    };
    match step.perform() {
        Ok(inverse) => {
            state.borrow_mut().undo_manager.undo_stack.push((name, inverse));
            refresh_active_view(state);
        }
        Err(error) => show_error(state, &format!("Couldn't redo: {error}")),
    }
}

/// Syncs the toolbar's combined view-toggle button icon to the active
/// tab's current mode. Called after anything that can change either —
/// switching views directly, or switching to a tab with a different mode.
fn sync_view_toggle_icon(state: &Rc<RefCell<AppState>>) {
    let (button, is_grid) = {
        let state_ref = state.borrow();
        (
            state_ref.view_toggle_button.clone(),
            state_ref.active().view_stack.visible_child_name().as_deref() == Some("grid"),
        )
    };
    button.set_icon_name(if is_grid { "view-grid-symbolic" } else { "view-list-symbolic" });
}

/// Persists `is_grid` as the active tab's current folder's own view mode
/// — matches Finder, where changing a folder's view sticks for that
/// folder from then on. Skipped while showing search results or a
/// virtual listing (Recent/Starred), where `current_dir` isn't a
/// meaningful "folder being viewed".
fn persist_active_view_mode(state: &Rc<RefCell<AppState>>, is_grid: bool) {
    let mut state_mut = state.borrow_mut();
    let tab = state_mut.active();
    if tab.search_query.is_some() || tab.virtual_view.is_some() {
        return;
    }
    let dir = tab.current_dir.clone();
    state_mut.folder_view_state.set(dir, is_grid);
}

fn action_view_list(state: &Rc<RefCell<AppState>>) {
    state.borrow().active().view_stack.set_visible_child_name("list");
    persist_active_view_mode(state, false);
    sync_view_toggle_icon(state);
}

fn action_view_grid(state: &Rc<RefCell<AppState>>) {
    state.borrow().active().view_stack.set_visible_child_name("grid");
    persist_active_view_mode(state, true);
    sync_view_toggle_icon(state);
}

/// Toggles the active tab between list and grid view — the toolbar's
/// combined view-toggle button, standing in for Nautilus's `Adw.SplitButton`.
fn action_toggle_view(state: &Rc<RefCell<AppState>>) {
    let is_grid = state.borrow().active().view_stack.visible_child_name().as_deref() == Some("grid");
    if is_grid {
        action_view_list(state);
    } else {
        action_view_grid(state);
    }
}

fn action_select_all(state: &Rc<RefCell<AppState>>) {
    let state_ref = state.borrow();
    let tab = state_ref.active();
    let selection = if tab.view_stack.visible_child_name().as_deref() == Some("grid") {
        &tab.grid_selection
    } else {
        &tab.list_selection
    };
    selection.select_all();
}

/// Hidden-file visibility is a global preference (matching Finder), so
/// toggling it reloads every open tab, not just the active one.
fn action_toggle_hidden(state: &Rc<RefCell<AppState>>) {
    {
        let mut state_mut = state.borrow_mut();
        state_mut.show_hidden ^= true;
        state_mut.preferences.show_hidden = state_mut.show_hidden;
        state_mut.preferences.save();
    }
    let ids: Vec<u64> = state.borrow().tabs.iter().map(|t| t.id).collect();
    for id in ids {
        refresh_tab(state, id);
    }
}

/// Toggles the search entry for the active tab. Hiding it (either by
/// toggling again or pressing Escape) exits search and returns to a normal
/// directory listing.
fn action_toggle_search(state: &Rc<RefCell<AppState>>) {
    let (search_entry, currently_visible) = {
        let state_ref = state.borrow();
        (state_ref.search_entry.clone(), state_ref.search_entry.get_visible())
    };
    if currently_visible {
        search_entry.set_text("");
        search_entry.set_visible(false);
        let id = state.borrow().active_tab;
        stop_tab_search(state, id);
        refresh_active_view(state);
    } else {
        search_entry.set_visible(true);
        search_entry.grab_focus();
    }
}

/// Toggles the Quick Look preview panel for the current selection
/// (Space bar, matching Finder). Hides it if already open.
fn action_toggle_quick_look(state: &Rc<RefCell<AppState>>) {
    let paths = selected_paths(state);
    if paths.is_empty() {
        return;
    }
    sepia_platform_macos::quicklook::toggle_preview(&paths);
}

/// Keeps an already-open Quick Look panel in sync as the selection
/// changes (matching Finder) — a no-op if the panel isn't open. Wired to
/// both view's selection models' `selection-changed` signal.
fn sync_quick_look_selection(state_cell: &StateCell) {
    let Some(state) = state_cell.borrow().clone() else {
        return;
    };
    let paths = selected_paths(&state);
    sepia_platform_macos::quicklook::refresh_preview(&paths);
}

/// Kicks off a recursive search of `tab_id`'s current directory on a
/// background thread (so a large tree doesn't freeze the UI), replacing
/// any previously in-flight search. Results are picked up by the polling
/// timer set up in `build_ui` and applied via `apply_search_results`.
/// Starts (or restarts) a live Spotlight search of `tab_id`'s current
/// directory. Assigning the new handle to `tab.search_handle` drops
/// whatever query was previously running for this tab, which stops it —
/// see `SpotlightQuery`'s `Drop` impl.
fn run_search(state: &Rc<RefCell<AppState>>, tab_id: u64, query: String) {
    let root = state.borrow().tab(tab_id).current_dir.clone();
    let state_for_callback = state.clone();
    let handle = sepia_platform_macos::spotlight::SpotlightQuery::start(&root, &query, move |hits| {
        apply_search_results(&state_for_callback, tab_id, hits);
    });
    state.borrow_mut().tab_mut(tab_id).search_handle = Some(handle);
}

fn apply_search_results(
    state: &Rc<RefCell<AppState>>,
    tab_id: u64,
    hits: Vec<sepia_platform_macos::spotlight::SearchHit>,
) {
    let root = state.borrow().tab(tab_id).current_dir.clone();
    let objects: Vec<FileEntryObject> = hits
        .iter()
        .map(|hit| {
            let detail = hit
                .path
                .parent()
                .and_then(|parent| parent.strip_prefix(&root).ok())
                .map(|rel| {
                    if rel.as_os_str().is_empty() {
                        ".".to_string()
                    } else {
                        rel.display().to_string()
                    }
                })
                .unwrap_or_default();
            FileEntryObject::new_search_hit(&hit.path, hit.is_dir, detail)
        })
        .collect();

    let state_ref = state.borrow();
    let tab = state_ref.tab(tab_id);
    let old_count = tab.store.n_items();
    tab.store.splice(0, old_count, &objects);
}

/// Placeholder for menu items mirroring Nautilus's interface (New Window,
/// sort, visible columns, preferences, etc.) that aren't implemented yet
/// — present so the interface is complete, inert until built out.
fn action_noop(_state: &Rc<RefCell<AppState>>) {}

fn action_zoom_in(state: &Rc<RefCell<AppState>>) {
    adjust_zoom(state, 1);
}

fn action_zoom_out(state: &Rc<RefCell<AppState>>) {
    adjust_zoom(state, -1);
}

/// Moves the app-wide zoom level by `delta` steps (clamped to the size
/// tables), persists it, then forces every open tab *in this window* to
/// rebind its visible cells at the new size — `items_changed` on a
/// store's existing range is the standard GTK4 way to say "these items'
/// presentation changed" without touching the underlying data.
fn adjust_zoom(state: &Rc<RefCell<AppState>>, delta: i32) {
    let new_index = GRID_ZOOM_INDEX.with(|zoom| {
        let clamped = (zoom.get() as i32 + delta).clamp(0, GRID_ICON_SIZES.len() as i32 - 1) as usize;
        zoom.set(clamped);
        clamped
    });
    {
        let mut state_mut = state.borrow_mut();
        state_mut.preferences.zoom_index = new_index;
        state_mut.preferences.save();
    }
    // Splicing in genuinely new `FileEntryObject`s (not the same
    // references) — GTK's list views treat a splice of the exact same
    // objects as a no-op, since nothing about the *items* changed as far
    // as the model can tell, so neither that nor a bare `items_changed`
    // actually forces a rebind of already-visible cells.
    for tab in &state.borrow().tabs {
        let items: Vec<FileEntryObject> = (0..tab.store.n_items())
            .filter_map(|i| tab.store.item(i).and_downcast::<FileEntryObject>())
            .map(|entry| entry.duplicate())
            .collect();
        let count = items.len() as u32;
        if count > 0 {
            tab.store.splice(0, count, &items);
        }
    }
}

/// Opens a new tab at the active tab's current location (matching Finder's
/// Cmd+T convention, rather than resetting to Home).
fn action_new_tab(state: &Rc<RefCell<AppState>>, state_cell: &StateCell) {
    let dir = active_dir(state);
    new_tab(state, state_cell, dir);
}

fn action_close_tab(state: &Rc<RefCell<AppState>>) {
    let id = state.borrow().active_tab;
    close_tab(state, id);
}

fn action_next_tab(state: &Rc<RefCell<AppState>>) {
    state.borrow().notebook.next_page();
}

fn action_prev_tab(state: &Rc<RefCell<AppState>>) {
    state.borrow().notebook.prev_page();
}

/// Removes the tab identified by `id`, unless it's the last remaining tab
/// (closing the window's only tab should quit or do nothing, not leave a
/// window with zero content — Cmd+Q covers "close the app" already).
/// Active-tab bookkeeping is left to `Notebook`'s own `switch-page` signal,
/// which GTK emits automatically when the current page is removed.
fn close_tab(state: &Rc<RefCell<AppState>>, id: u64) {
    let (notebook, page_widget) = {
        let state_ref = state.borrow();
        if state_ref.tabs.len() <= 1 {
            return;
        }
        (state_ref.notebook.clone(), state_ref.tab(id).view_stack.clone())
    };
    if let Some(page_num) = notebook.page_num(&page_widget) {
        notebook.remove_page(Some(page_num));
    }
    state.borrow_mut().tabs.retain(|t| t.id != id);
    notebook.set_show_tabs(state.borrow().tabs.len() > 1);
}

/// Creates a new tab showing `dir`, appends it to the notebook, and makes
/// it the active tab.
fn new_tab(state: &Rc<RefCell<AppState>>, state_cell: &StateCell, dir: PathBuf) {
    let content = build_tab_content(state_cell);
    if state.borrow().preferences.default_view == sepia_core::preferences::DefaultView::Grid {
        content.view_stack.set_visible_child_name("grid");
    }

    let tab_label = Label::new(None);
    let close_button = Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    let label_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .build();
    label_box.append(&tab_label);
    label_box.append(&close_button);

    let id = {
        let mut state_mut = state.borrow_mut();
        let id = state_mut.next_tab_id;
        state_mut.next_tab_id += 1;
        id
    };

    let notebook = state.borrow().notebook.clone();
    notebook.append_page(&content.view_stack, Some(&label_box));
    // Native GTK drag-to-reorder — nothing in the app depends on
    // `AppState::tabs`'s vector order matching the notebook's visual tab
    // order (tabs are always looked up by id, and the notebook's own
    // page order is authoritative for next/prev/switch-page), so this
    // needs no bookkeeping on our side.
    notebook.set_tab_reorderable(&content.view_stack, true);

    close_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| close_tab(&state, id)
    ));

    let tab = Tab {
        id,
        current_dir: PathBuf::new(),
        store: content.store,
        history: NavigationHistory::default(),
        list_selection: content.list_selection,
        grid_selection: content.grid_selection,
        view_stack: content.view_stack,
        watcher: loader::DirectoryWatcher::new(),
        tab_label,
        search_query: None,
        search_handle: None,
        virtual_view: None,
    };
    state.borrow_mut().tabs.push(tab);
    notebook.set_show_tabs(state.borrow().tabs.len() > 1);

    let page_widget = state.borrow().tab(id).view_stack.clone();
    if let Some(page_num) = notebook.page_num(&page_widget) {
        notebook.set_current_page(Some(page_num));
    }
    // `set_current_page` above only emits `switch-page` when it's an actual
    // change (e.g. not for this tab if it's the first page ever added), so
    // this is set directly too as a fallback.
    state.borrow_mut().active_tab = id;
    load_dir(state, id, dir);
}

/// Appends one sidebar row: a label, plus an inert eject button for volume
/// rows (unmounting isn't wired up yet).
fn append_sidebar_row(list_box: &ListBox, icon_name: &str, label: &str, with_eject: bool) {
    let row_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    row_box.append(&Image::from_icon_name(icon_name));
    row_box.append(&Label::builder().label(label).xalign(0.0).hexpand(true).build());
    if with_eject {
        let eject = Button::from_icon_name("media-eject-symbolic");
        eject.add_css_class("flat");
        eject.set_tooltip_text(Some("Eject"));
        eject.connect_clicked(|_| {});
        row_box.append(&eject);
    }
    list_box.append(&row_box);
}

/// Shows a one-item context menu ("Empty Trash…"/"Remove from
/// Bookmarks"-style) for a sidebar row, via an invisible `MenuButton`
/// parented to the sidebar's own `ListBox` rather than the row itself
/// (which would otherwise visibly disturb that row's layout) — the same
/// construction the file-view context menu uses, for the same reason:
/// a bare `Popover`/`PopoverMenu` shown by hand doesn't reliably
/// activate its action on this backend, but a `GtkMenuButton`-driven
/// popover does.
fn show_sidebar_row_menu(
    list_box: &ListBox,
    row_box: &GtkBox,
    action_name: &str,
    target: Option<&glib::Variant>,
    label: &str,
    x: f64,
    y: f64,
) {
    let menu = gio::Menu::new();
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(&format!("app.{action_name}")), target);
    menu.append_item(&item);

    let point = row_box.compute_point(list_box, &gtk::graphene::Point::new(x as f32, y as f32));
    let (list_x, list_y) = point.map(|p| (p.x() as i32, p.y() as i32)).unwrap_or((x as i32, y as i32));

    let menu_button = gtk::MenuButton::new();
    menu_button.set_has_frame(false);
    menu_button.set_size_request(0, 0);
    menu_button.set_can_focus(false);
    menu_button.set_can_target(false);
    menu_button.set_child(Some(&GtkBox::new(Orientation::Horizontal, 0)));
    menu_button.set_parent(list_box);
    menu_button.set_menu_model(Some(&menu));
    if let Some(popover) = menu_button.popover() {
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(list_x, list_y, 1, 1)));
        popover.connect_closed(glib::clone!(
            #[strong]
            menu_button,
            move |_| menu_button.unparent()
        ));
    }
    menu_button.popup();
}

/// Appends one bookmark row: a folder icon, its name, and a right-click
/// "Remove from Bookmarks" menu — unlike `append_sidebar_row`'s rows,
/// these are backed by the persisted, user-editable `Bookmarks` list.
fn append_bookmark_row(_state: &Rc<RefCell<AppState>>, list_box: &ListBox, label: &str, path: &Path) {
    let path = path.to_path_buf();
    let row_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    row_box.append(&Image::from_icon_name("folder-symbolic"));
    row_box.append(&Label::builder().label(label).xalign(0.0).hexpand(true).build());

    let click = gtk::GestureClick::new();
    click.set_button(3);
    click.connect_pressed(glib::clone!(
        #[strong]
        list_box,
        #[strong]
        row_box,
        #[strong]
        path,
        move |_, _, x, y| {
            let target = path.to_string_lossy().to_string().to_variant();
            show_sidebar_row_menu(&list_box, &row_box, "remove_bookmark", Some(&target), "Remove from Bookmarks", x, y);
        }
    ));
    row_box.add_controller(click);

    list_box.append(&row_box);
}

/// Appends the Trash row: browsing it navigates into `~/.Trash` like any
/// other folder, and a right-click offers "Empty Trash…" (permanent,
/// so — like Delete Permanently — it's gated behind an explicit
/// confirmation rather than being a single click or shortcut away).
fn append_trash_row(_state: &Rc<RefCell<AppState>>, list_box: &ListBox, _trash_dir: &Path) {
    let row_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    row_box.append(&Image::from_icon_name("user-trash-symbolic"));
    row_box.append(&Label::builder().label("Trash").xalign(0.0).hexpand(true).build());

    let click = gtk::GestureClick::new();
    click.set_button(3);
    click.connect_pressed(glib::clone!(
        #[strong]
        list_box,
        #[strong]
        row_box,
        move |_, _, x, y| {
            show_sidebar_row_menu(&list_box, &row_box, "empty_trash", None, "Empty Trash\u{2026}", x, y);
        }
    ));
    row_box.add_controller(click);

    list_box.append(&row_box);
}

/// Clears and rebuilds every sidebar row: quick-access locations (some
/// real, some inert placeholders for not-yet-implemented features),
/// bookmarks (persisted, user-editable — seeded with the standard macOS
/// user folders on first run), then mounted volumes — sections separated
/// by a plain rule rather than a selectable row, via `set_header_func`.
/// Called both for the initial sidebar build and after any bookmark
/// add/remove.
fn rebuild_sidebar_rows(state: &Rc<RefCell<AppState>>, list_box: &ListBox) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
    let trash_dir = sepia_core::open::trash_dir().expect("HOME must be set");
    // Usually already present on macOS, but not guaranteed until something
    // has actually been trashed — create it so the sidebar row (and
    // browsing into it) works on a fresh account too.
    let _ = std::fs::create_dir_all(&trash_dir);

    let top_locations: Vec<(&str, &str, Option<SidebarTarget>)> = vec![
        ("go-home-symbolic", "Home", Some(SidebarTarget::Path(home.clone()))),
        ("document-open-recent-symbolic", "Recent", Some(SidebarTarget::Recent)),
        ("starred-symbolic", "Starred", Some(SidebarTarget::Starred)),
        ("network-server-symbolic", "Network", Some(SidebarTarget::Network)),
    ]
    .into_iter()
    .filter(|(_, _, target)| match target {
        Some(SidebarTarget::Path(p)) => p.is_dir(),
        _ => true,
    })
    .collect();

    let bookmarks: Vec<PathBuf> = state.borrow().bookmarks.entries().to_vec();

    let volumes: Vec<(String, PathBuf)> = sepia_platform_macos::volumes::list_volumes()
        .into_iter()
        .map(|v| (v.name, v.path))
        .collect();

    let mut sidebar_targets: Vec<Option<SidebarTarget>> = Vec::new();
    let mut section_starts: Vec<i32> = vec![0];

    for (icon, label, target) in &top_locations {
        append_sidebar_row(list_box, icon, label, false);
        sidebar_targets.push(target.clone());
    }
    append_trash_row(state, list_box, &trash_dir);
    sidebar_targets.push(Some(SidebarTarget::Path(trash_dir.clone())));

    if !bookmarks.is_empty() {
        section_starts.push(sidebar_targets.len() as i32);
        for path in &bookmarks {
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            append_bookmark_row(state, list_box, &label, path);
            sidebar_targets.push(Some(SidebarTarget::Path(path.clone())));
        }
    }

    if !volumes.is_empty() {
        section_starts.push(sidebar_targets.len() as i32);
        for (label, path) in &volumes {
            // The boot volume (raw path "/") isn't ejectable.
            let ejectable = path.as_path() != Path::new("/");
            append_sidebar_row(list_box, "drive-harddisk-symbolic", label, ejectable);
            sidebar_targets.push(Some(SidebarTarget::Path(path.clone())));
        }
    }

    list_box.set_header_func(move |row, before| {
        if before.is_some() && section_starts.contains(&row.index()) {
            row.set_header(Some(&gtk::Separator::new(Orientation::Horizontal)));
        } else {
            row.set_header(None::<&gtk::Widget>);
        }
    });

    state.borrow_mut().sidebar_targets = sidebar_targets;
}

/// Builds the sidebar's scaffolding (scrolled window, list box, row
/// activation) and populates it via `rebuild_sidebar_rows`.
fn build_sidebar(state: &Rc<RefCell<AppState>>) -> (ScrolledWindow, ListBox) {
    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::Single);

    rebuild_sidebar_rows(state, &list_box);

    list_box.connect_row_activated(glib::clone!(
        #[strong]
        state,
        move |_, row| {
            let index = row.index();
            let target = state.borrow().sidebar_targets.get(index as usize).cloned().flatten();
            match target {
                Some(SidebarTarget::Path(path)) => navigate(&state, path),
                Some(SidebarTarget::Recent) => show_recent(&state),
                Some(SidebarTarget::Starred) => show_starred(&state),
                Some(SidebarTarget::Network) => show_connect_to_server_dialog(&state),
                None => {}
            }
        }
    ));

    let scrolled = ScrolledWindow::builder()
        .width_request(180)
        .vexpand(true)
        .child(&list_box)
        .build();
    (scrolled, list_box)
}

/// Forward-reference cell letting closures set up before `AppState` exists
/// (e.g. inside `build_tab_content`, which runs for the very first tab
/// before there's anywhere to store an `Rc<RefCell<AppState>>` yet) resolve
/// it lazily once it's later constructed and the cell is populated.
type StateCell = Rc<RefCell<Option<Rc<RefCell<AppState>>>>>;

/// Wraps an already-fully-built `state` in a `StateCell` — for the (rare)
/// case of calling a function that expects the forward-reference pattern
/// (e.g. `new_tab`, for functions like `build_tab_content` it eventually
/// calls) when there's no real bootstrapping happening and a `state`
/// already exists, e.g. invoking "New Tab" from the app-wide menu bar
/// against whichever window is currently active.
fn state_cell_for(state: &Rc<RefCell<AppState>>) -> StateCell {
    Rc::new(RefCell::new(Some(state.clone())))
}

/// The widgets that make up one tab's file-view content: a `Stack` toggling
/// between list and grid views, each backed by its own selection model over
/// a shared `ListStore`.
struct TabContent {
    store: gio::ListStore,
    list_selection: MultiSelection,
    grid_selection: MultiSelection,
    view_stack: Stack,
}

/// Builds one tab's list/grid view widgets, wired for navigation,
/// selection, rubber-banding, and drag-and-drop (both as a source and as an
/// import target for files dropped from Finder). Uses `state_cell` rather
/// than a concrete `state` so this can build the very first tab's widgets
/// before `AppState` itself exists yet — every closure here only resolves
/// `state_cell` lazily, when the event actually fires, by which point
/// `AppState` always exists.
///
/// Hidden (non-current) `Notebook` pages don't receive pointer/activation
/// events in GTK, so any callback that fires here is guaranteed to belong
/// to the currently *active* tab — callbacks can safely read
/// `state.borrow().active()` rather than having to identify which tab they
/// belong to.
fn build_tab_content(state_cell: &StateCell) -> TabContent {
    let store = gio::ListStore::new::<FileEntryObject>();
    // The sorter itself is only set later, once `column_view` (and its
    // columns' individual sorters) exist — `gtk::ColumnView` maintains
    // its own `ColumnViewSorter` automatically as column headers are
    // clicked, so wiring `sort_model.set_sorter(column_view.sorter())`
    // makes every future `store.splice(...)` (on reload, search, etc.)
    // land pre-sorted with no extra code at those call sites.
    let sort_model = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
    let selection_model = gtk::MultiSelection::new(Some(sort_model.clone()));
    selection_model.connect_selection_changed(glib::clone!(
        #[strong]
        state_cell,
        move |_, _, _| sync_quick_look_selection(&state_cell)
    ));

    // Filled in with the real `ColumnView` right after it's constructed
    // below; the factory's `connect_setup` closure only runs later (when
    // items actually render), by which point this is populated. Needed for
    // the rubber-band/drag-source workaround below.
    let column_view_cell: Rc<RefCell<Option<ColumnView>>> = Rc::new(RefCell::new(None));

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(glib::clone!(
        #[strong]
        column_view_cell,
        #[strong]
        state_cell,
        move |_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("factory item must be a ListItem");
            let container = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
            let icon = Image::new();
            icon.set_pixel_size(16);
            let label = Label::new(None);
            label.set_xalign(0.0);
            container.append(&icon);
            container.append(&label);

            // GTK bug workaround (same one Nautilus itself works around —
            // see https://gitlab.gnome.org/GNOME/gtk/-/issues/5670): a
            // `GtkDragSource` attached to the view competes with the view's
            // own rubber-band gesture for the same item press, and loses.
            // Disabling rubber-band for the duration of a press-on-an-item
            // lets the drag source claim it instead; background presses
            // never disable it in the first place, so rubber-band select
            // there is unaffected.
            let click = gtk::GestureClick::new();
            click.connect_pressed(glib::clone!(
                #[strong]
                column_view_cell,
                move |_, _, _, _| {
                    if let Some(view) = column_view_cell.borrow().as_ref() {
                        view.set_enable_rubberband(false);
                    }
                }
            ));
            click.connect_released(glib::clone!(
                #[strong]
                column_view_cell,
                move |_, _, _, _| {
                    if let Some(view) = column_view_cell.borrow().as_ref() {
                        view.set_enable_rubberband(true);
                    }
                }
            ));
            click.connect_stopped(glib::clone!(
                #[strong]
                column_view_cell,
                move |_| {
                    if let Some(view) = column_view_cell.borrow().as_ref() {
                        view.set_enable_rubberband(true);
                    }
                }
            ));
            container.add_controller(click);

            // Drag source, attached per-cell (matching Nautilus's own
            // `on_item_drag_prepare`, not the view as a whole — that's what
            // makes `list_item.item()`/`.position()` below meaningful, and
            // lets the sequence be explicitly claimed for *this* cell).
            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
            // Capture phase (not the default Bubble): the drag source needs
            // first crack at the press, ahead of the rubber-band-workaround
            // click gesture above — matching Nautilus's own setup exactly.
            drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);
            drag_source.connect_prepare(glib::clone!(
                #[strong]
                list_item,
                #[strong]
                column_view_cell,
                #[strong]
                state_cell,
                move |source, _x, _y| {
                    let position = list_item.position();
                    if position == gtk::INVALID_LIST_POSITION {
                        return None;
                    }
                    let Some(view) = column_view_cell.borrow().clone() else {
                        return None;
                    };
                    let Some(model) = view.model() else {
                        return None;
                    };
                    // Anticipate selection for the pressed item, unless it's
                    // already part of a larger selection (in which case
                    // dragging should carry the whole selection).
                    if !model.is_selected(position) {
                        model.select_item(position, true);
                    }
                    source.set_state(gtk::EventSequenceState::Claimed);

                    let Some(state) = state_cell.borrow().clone() else {
                        return None;
                    };
                    let paths = selected_paths(&state);
                    if paths.is_empty() {
                        return None;
                    }
                    Some(file_drag_content_provider(&paths))
                }
            ));
            container.add_controller(drag_source);

            // Right-click context menu. Matches Nautilus: right-clicking
            // an item outside the current selection replaces the
            // selection with just that item; right-clicking within an
            // existing multi-selection preserves it.
            let right_click = gtk::GestureClick::new();
            right_click.set_button(3);
            right_click.connect_pressed(glib::clone!(
                #[strong]
                list_item,
                #[strong]
                column_view_cell,
                #[strong]
                state_cell,
                #[strong]
                container,
                move |gesture, _n_press, x, y| {
                    let position = list_item.position();
                    if position == gtk::INVALID_LIST_POSITION {
                        return;
                    }
                    let Some(view) = column_view_cell.borrow().clone() else {
                        return;
                    };
                    let Some(model) = view.model() else {
                        return;
                    };
                    if !model.is_selected(position) {
                        model.select_item(position, true);
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let Some(state) = state_cell.borrow().clone() else {
                        return;
                    };
                    show_context_menu(&state, &container, x, y);
                }
            ));
            container.add_controller(right_click);

            list_item.set_child(Some(&container));
        }
    ));
    factory.connect_bind(glib::clone!(
        #[strong]
        state_cell,
        move |_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("factory item must be a ListItem");
        let entry = list_item
            .item()
            .and_downcast::<FileEntryObject>()
            .expect("bound item must be a FileEntryObject");
        let container = list_item
            .child()
            .and_downcast::<GtkBox>()
            .expect("list item child must be a Box");
        let icon = container
            .first_child()
            .and_downcast::<Image>()
            .expect("list cell's first child must be an Image");
        let label = container
            .last_child()
            .and_downcast::<Label>()
            .expect("list cell's last child must be a Label");
        icon.set_pixel_size(LIST_ICON_SIZES[GRID_ZOOM_INDEX.with(|z| z.get())]);
        if let Some(state) = state_cell.borrow().clone() {
            let path = entry.resolved_path(&state.borrow().active().current_dir);
            apply_file_icon(&icon, list_item, &entry, path);
        }
        let text = match entry.detail() {
            Some(detail) => format!("{}   —   {}", entry.name(), detail),
            None => entry.name(),
        };
        label.set_label(&text);
    }
    ));

    let name_column = ColumnViewColumn::new(Some("Name"), Some(factory));
    name_column.set_expand(true);
    // Folders group before files regardless of the active sort column —
    // matches Finder/Nautilus list-view convention.
    fn dir_first(a: &FileEntryObject, b: &FileEntryObject) -> std::cmp::Ordering {
        b.is_dir().cmp(&a.is_dir())
    }
    fn entry_of(obj: &glib::Object) -> FileEntryObject {
        obj.downcast_ref::<FileEntryObject>().expect("sorter item must be a FileEntryObject").clone()
    }
    let name_sorter =
        gtk::CustomSorter::new(|a, b| {
            let (a, b) = (entry_of(a), entry_of(b));
            dir_first(&a, &b).then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase())).into()
        });
    name_column.set_sorter(Some(&name_sorter));

    let size_factory = gtk::SignalListItemFactory::new();
    size_factory.connect_setup(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("factory item must be a ListItem");
        list_item.set_child(Some(&Label::builder().xalign(1.0).build()));
    });
    size_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("factory item must be a ListItem");
        let entry = list_item.item().and_downcast::<FileEntryObject>().expect("bound item must be a FileEntryObject");
        let label = list_item.child().and_downcast::<Label>().expect("size cell child must be a Label");
        label.set_label(&if entry.is_dir() { "\u{2014}".to_string() } else { scaled_size(entry.size()) });
    });
    let size_column = ColumnViewColumn::new(Some("Size"), Some(size_factory));
    let size_sorter = gtk::CustomSorter::new(|a, b| {
        let (a, b) = (entry_of(a), entry_of(b));
        dir_first(&a, &b).then_with(|| a.size().cmp(&b.size())).into()
    });
    size_column.set_sorter(Some(&size_sorter));

    let modified_factory = gtk::SignalListItemFactory::new();
    modified_factory.connect_setup(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("factory item must be a ListItem");
        list_item.set_child(Some(&Label::builder().xalign(0.0).build()));
    });
    modified_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("factory item must be a ListItem");
        let entry = list_item.item().and_downcast::<FileEntryObject>().expect("bound item must be a FileEntryObject");
        let label = list_item.child().and_downcast::<Label>().expect("modified cell child must be a Label");
        label.set_label(&format_time_compact(entry.modified()));
    });
    let modified_column = ColumnViewColumn::new(Some("Date Modified"), Some(modified_factory));
    let modified_sorter = gtk::CustomSorter::new(|a, b| {
        let (a, b) = (entry_of(a), entry_of(b));
        dir_first(&a, &b).then_with(|| a.modified().cmp(&b.modified())).into()
    });
    modified_column.set_sorter(Some(&modified_sorter));

    let kind_factory = gtk::SignalListItemFactory::new();
    kind_factory.connect_setup(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("factory item must be a ListItem");
        list_item.set_child(Some(&Label::builder().xalign(0.0).build()));
    });
    kind_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("factory item must be a ListItem");
        let entry = list_item.item().and_downcast::<FileEntryObject>().expect("bound item must be a FileEntryObject");
        let label = list_item.child().and_downcast::<Label>().expect("kind cell child must be a Label");
        label.set_label(&kind_text(&entry.name(), entry.is_dir()));
    });
    let kind_column = ColumnViewColumn::new(Some("Kind"), Some(kind_factory));
    let kind_sorter = gtk::CustomSorter::new(|a, b| {
        let (a, b) = (entry_of(a), entry_of(b));
        dir_first(&a, &b)
            .then_with(|| kind_text(&a.name(), a.is_dir()).cmp(&kind_text(&b.name(), b.is_dir())))
            .into()
    });
    kind_column.set_sorter(Some(&kind_sorter));

    let column_view = ColumnView::new(Some(selection_model.clone()));
    column_view.append_column(&name_column);
    column_view.append_column(&size_column);
    column_view.append_column(&modified_column);
    column_view.append_column(&kind_column);
    column_view.set_enable_rubberband(true);
    // `ColumnView` maintains its own sorter from whichever column's
    // header was last clicked (and in which direction) — feeding that
    // into the `SortListModel` inserted earlier is what makes header
    // clicks actually reorder the list, and keeps it sorted through
    // every future reload automatically.
    sort_model.set_sorter(column_view.sorter().as_ref());
    column_view.sort_by_column(Some(&name_column), gtk::SortType::Ascending);
    *column_view_cell.borrow_mut() = Some(column_view.clone());

    let type_ahead_key = gtk::EventControllerKey::new();
    type_ahead_key.connect_key_pressed(glib::clone!(
        #[strong]
        selection_model,
        #[strong]
        column_view,
        move |_controller, keyval, _keycode, modifiers| {
            // Plain characters only — anything held with Cmd/Ctrl/Alt is
            // a shortcut, not type-ahead, and arrows/Enter/etc. already
            // have their own meaning in this view.
            if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK) {
                return glib::Propagation::Proceed;
            }
            let Some(ch) = keyval.to_unicode().filter(|c| c.is_alphanumeric()) else {
                return glib::Propagation::Proceed;
            };
            handle_type_ahead(
                &selection_model,
                glib::clone!(
                    #[strong]
                    column_view,
                    move |pos| {
                        column_view.scroll_to(pos, None, gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS, None)
                    }
                ),
                ch,
            );
            glib::Propagation::Stop
        }
    ));
    column_view.add_controller(type_ahead_key);

    let file_scroll = ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&column_view)
        .build();

    // Grid (icon) view. Uses its own selection model over the same
    // `store` — sharing one `MultiSelection` between two simultaneously
    // constructed view widgets isn't a supported pattern, so list and grid
    // don't share live selection state for now (a reasonable first cut).
    let grid_selection_model = gtk::MultiSelection::new(Some(store.clone()));
    grid_selection_model.connect_selection_changed(glib::clone!(
        #[strong]
        state_cell,
        move |_, _, _| sync_quick_look_selection(&state_cell)
    ));
    // Same rubber-band/drag-source workaround as the list view, see above.
    let grid_view_cell: Rc<RefCell<Option<GridView>>> = Rc::new(RefCell::new(None));

    let grid_factory = gtk::SignalListItemFactory::new();
    grid_factory.connect_setup(glib::clone!(
        #[strong]
        grid_view_cell,
        #[strong]
        state_cell,
        move |_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("factory item must be a ListItem");
            let container = GtkBox::builder()
                .orientation(Orientation::Vertical)
                .spacing(4)
                .width_request(84)
                .build();
            let icon = Image::new();
            icon.set_pixel_size(48);
            let label = Label::new(None);
            label.set_wrap(true);
            label.set_justify(Justification::Center);
            label.set_lines(2);
            label.set_max_width_chars(12);
            container.append(&icon);
            container.append(&label);

            let click = gtk::GestureClick::new();
            click.connect_pressed(glib::clone!(
                #[strong]
                grid_view_cell,
                move |_, _, _, _| {
                    if let Some(view) = grid_view_cell.borrow().as_ref() {
                        view.set_enable_rubberband(false);
                    }
                }
            ));
            click.connect_released(glib::clone!(
                #[strong]
                grid_view_cell,
                move |_, _, _, _| {
                    if let Some(view) = grid_view_cell.borrow().as_ref() {
                        view.set_enable_rubberband(true);
                    }
                }
            ));
            click.connect_stopped(glib::clone!(
                #[strong]
                grid_view_cell,
                move |_| {
                    if let Some(view) = grid_view_cell.borrow().as_ref() {
                        view.set_enable_rubberband(true);
                    }
                }
            ));
            container.add_controller(click);

            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
            // Capture phase (not the default Bubble): the drag source needs
            // first crack at the press, ahead of the rubber-band-workaround
            // click gesture above — matching Nautilus's own setup exactly.
            drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);
            drag_source.connect_prepare(glib::clone!(
                #[strong]
                list_item,
                #[strong]
                grid_view_cell,
                #[strong]
                state_cell,
                move |source, _x, _y| {
                    let position = list_item.position();
                    if position == gtk::INVALID_LIST_POSITION {
                        return None;
                    }
                    let Some(view) = grid_view_cell.borrow().clone() else {
                        return None;
                    };
                    let Some(model) = view.model() else {
                        return None;
                    };
                    if !model.is_selected(position) {
                        model.select_item(position, true);
                    }
                    source.set_state(gtk::EventSequenceState::Claimed);

                    let Some(state) = state_cell.borrow().clone() else {
                        return None;
                    };
                    let paths = selected_paths(&state);
                    if paths.is_empty() {
                        return None;
                    }
                    Some(file_drag_content_provider(&paths))
                }
            ));
            container.add_controller(drag_source);

            // Right-click context menu — same behavior as list view, see
            // the comment there.
            let right_click = gtk::GestureClick::new();
            right_click.set_button(3);
            right_click.connect_pressed(glib::clone!(
                #[strong]
                list_item,
                #[strong]
                grid_view_cell,
                #[strong]
                state_cell,
                #[strong]
                container,
                move |gesture, _n_press, x, y| {
                    let position = list_item.position();
                    if position == gtk::INVALID_LIST_POSITION {
                        return;
                    }
                    let Some(view) = grid_view_cell.borrow().clone() else {
                        return;
                    };
                    let Some(model) = view.model() else {
                        return;
                    };
                    if !model.is_selected(position) {
                        model.select_item(position, true);
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let Some(state) = state_cell.borrow().clone() else {
                        return;
                    };
                    show_context_menu(&state, &container, x, y);
                }
            ));
            container.add_controller(right_click);

            list_item.set_child(Some(&container));
        }
    ));
    grid_factory.connect_bind(glib::clone!(
        #[strong]
        state_cell,
        move |_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("factory item must be a ListItem");
            let entry = list_item
                .item()
                .and_downcast::<FileEntryObject>()
                .expect("bound item must be a FileEntryObject");
            let container = list_item
                .child()
                .and_downcast::<GtkBox>()
                .expect("list item child must be a Box");
            let icon = container
                .first_child()
                .and_downcast::<Image>()
                .expect("grid cell's first child must be an Image");
            let label = container
                .last_child()
                .and_downcast::<Label>()
                .expect("grid cell's last child must be a Label");
            let zoom = GRID_ZOOM_INDEX.with(|z| z.get());
            container.set_width_request(GRID_CONTAINER_WIDTHS[zoom]);
            icon.set_pixel_size(GRID_ICON_SIZES[zoom]);
            if let Some(state) = state_cell.borrow().clone() {
                let path = entry.resolved_path(&state.borrow().active().current_dir);
                apply_file_icon(&icon, list_item, &entry, path);
            }
            label.set_label(&entry.name());
        }
    ));
    let grid_view = GridView::new(Some(grid_selection_model.clone()), Some(grid_factory));
    grid_view.set_enable_rubberband(true);
    *grid_view_cell.borrow_mut() = Some(grid_view.clone());

    let type_ahead_key = gtk::EventControllerKey::new();
    type_ahead_key.connect_key_pressed(glib::clone!(
        #[strong]
        grid_selection_model,
        #[strong]
        grid_view,
        move |_controller, keyval, _keycode, modifiers| {
            if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK) {
                return glib::Propagation::Proceed;
            }
            let Some(ch) = keyval.to_unicode().filter(|c| c.is_alphanumeric()) else {
                return glib::Propagation::Proceed;
            };
            handle_type_ahead(
                &grid_selection_model,
                glib::clone!(
                    #[strong]
                    grid_view,
                    move |pos| grid_view.scroll_to(pos, gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS, None)
                ),
                ch,
            );
            glib::Propagation::Stop
        }
    ));
    grid_view.add_controller(type_ahead_key);

    let grid_scroll = ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&grid_view)
        .build();

    let view_stack = Stack::new();
    view_stack.add_named(&file_scroll, Some("list"));
    view_stack.add_named(&grid_scroll, Some("grid"));

    // Activation (double-click / Enter) navigates into directories. Only
    // the active tab's view can be receiving this signal (see doc comment
    // above), so it's safe to just read `state.active()`.
    column_view.connect_activate(glib::clone!(
        #[strong]
        state_cell,
        #[strong]
        selection_model,
        move |_column_view, position| {
            let Some(state) = state_cell.borrow().clone() else {
                return;
            };
            if let Some(entry) = selection_model.item(position).and_downcast::<FileEntryObject>() {
                on_item_activated(&state, entry);
            }
        }
    ));
    grid_view.connect_activate(glib::clone!(
        #[strong]
        state_cell,
        #[strong]
        grid_selection_model,
        move |_grid_view, position| {
            let Some(state) = state_cell.borrow().clone() else {
                return;
            };
            if let Some(entry) = grid_selection_model.item(position).and_downcast::<FileEntryObject>() {
                on_item_activated(&state, entry);
            }
        }
    ));

    // Drop target: dropping files onto the file list itself (e.g. from
    // Finder) imports them into this tab's current directory.
    for view in [column_view.clone().upcast::<gtk::Widget>(), grid_view.clone().upcast()] {
        let file_drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        file_drop.connect_drop(glib::clone!(
            #[strong]
            state_cell,
            move |_target, value, _x, _y| {
                let Some(state) = state_cell.borrow().clone() else {
                    return false;
                };
                let Ok(file_list) = value.get::<gdk::FileList>() else {
                    return false;
                };
                let files = file_list.files();
                if files.is_empty() {
                    return false;
                }
                let dest_dir = active_dir(&state);
                accept_dropped_files(&state, files, dest_dir);
                true
            }
        ));
        view.add_controller(file_drop);
    }

    TabContent {
        store,
        list_selection: selection_model,
        grid_selection: grid_selection_model,
        view_stack,
    }
}

fn main() {
    // Without a real .app bundle (no Info.plist/CFBundleName), GTK's
    // native macOS app menu falls back to the process name — the
    // lowercase Cargo binary name, "sepia" — for its title. This is
    // what it actually reads instead.
    glib::set_prgname(Some("Sepia"));
    let app = Application::builder().application_id(APP_ID).build();
    // Menu bar + `app.*` actions are built once, at startup — not per
    // window (`connect_activate` fires again, harmlessly, if the app is
    // re-activated with an existing window, e.g. clicking the Dock icon;
    // `connect_startup` fires exactly once).
    app.connect_startup(build_menu_bar_and_actions);
    app.connect_activate(|app| build_window(app, None));
    app.run();
}

/// Builds the (single, shared) menu bar and every `app.*` action, once,
/// at app startup — not per-window. Each action's handler resolves
/// `ACTIVE_STATE` at invocation time rather than closing over one fixed
/// `AppState`, so menu bar commands and accelerators always apply to
/// whichever window last had focus (updated by each window's
/// `is-active` handler in `build_window`).
/// Matches GTK's theme to the system's current Light/Dark appearance
/// setting — a no-op (cheap: one property read, no allocation) if it
/// already matches, so calling this on a timer to catch live system
/// changes doesn't do any real work most ticks.
fn sync_dark_mode() {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let is_dark = sepia_platform_macos::appearance::is_dark_mode(mtm);
    let target = if is_dark { gtk::InterfaceColorScheme::Dark } else { gtk::InterfaceColorScheme::Light };
    if settings.gtk_interface_color_scheme() != target {
        settings.set_gtk_interface_color_scheme(target);
    }
}

fn build_menu_bar_and_actions(app: &Application) {
    sync_dark_mode();
    // The system can switch light/dark while sepia is running (manually,
    // or on an auto-switch schedule) — polling `effectiveAppearance`
    // every couple of seconds is cheap (an in-process AppKit query, not
    // a subprocess) and there's no simpler push notification exposed to
    // a plain GTK/GLib main loop here.
    glib::timeout_add_seconds_local(2, || {
        sync_dark_mode();
        glib::ControlFlow::Continue
    });

    macro_rules! add_action {
        ($name:expr, $handler:expr) => {{
            let action = gio::SimpleAction::new($name, None);
            action.connect_activate(move |_, _| {
                if let Some(state) = ACTIVE_STATE.with(|cell| cell.borrow().clone()) {
                    $handler(&state);
                }
            });
            app.add_action(&action);
        }};
    }
    add_action!("new_folder", action_new_folder);
    add_action!("rename_selected", action_rename_selected);
    add_action!("batch_rename", action_batch_rename);
    add_action!("compress", action_compress);
    add_action!("open_scripts_folder", action_open_scripts_folder);

    // Scripts menu: one action per executable file in the scripts
    // folder (created if missing), each running that script against
    // whichever window is currently active. Discovered once at
    // startup — like any native menu bar, it doesn't live-refresh if
    // you add a script while sepia is running; relaunch to pick up new
    // ones.
    sepia_core::scripts::ensure_scripts_dir();
    let scripts = sepia_core::scripts::list_scripts();
    let scripts_section = gio::Menu::new();
    if scripts.is_empty() {
        scripts_section.append(Some("No Scripts Installed"), None);
    } else {
        for (i, script) in scripts.iter().enumerate() {
            let action_name = format!("run_script_{i}");
            let action = gio::SimpleAction::new(&action_name, None);
            let script_path = script.path.clone();
            action.connect_activate(move |_, _| {
                if let Some(state) = ACTIVE_STATE.with(|cell| cell.borrow().clone()) {
                    action_run_script(&state, script_path.clone());
                }
            });
            app.add_action(&action);
            scripts_section.append(Some(&script.name), Some(&format!("app.{action_name}")));
        }
    }
    let scripts_folder_section = gio::Menu::new();
    scripts_folder_section.append(Some("Open Scripts Folder…"), Some("app.open_scripts_folder"));
    let scripts_menu = gio::Menu::new();
    scripts_menu.append_section(None, &scripts_section);
    scripts_menu.append_section(None, &scripts_folder_section);

    add_action!("extract", action_extract);
    add_action!("get_info", action_get_info);
    add_action!("toggle_starred", action_toggle_starred);
    add_action!("move_to_trash", action_move_to_trash);
    add_action!("delete_permanently", action_delete_permanently);
    // Context-menu-only actions (no menu-bar/accelerator equivalent).
    // These are registered globally rather than in a per-click
    // `SimpleActionGroup` inserted on the popover — that combination
    // (`insert_action_group` on a dynamically-shown popover) turned out
    // to silently break activation for every action in the menu on this
    // GTK4/quartz setup, confirmed via a minimal, isolated repro
    // completely unrelated to any of sepia's own menu content. Reading
    // the current selection fresh at activation time (like every other
    // `app.*` action already does) sidesteps needing a per-click group
    // at all.
    add_action!("open_selected", action_open_selected);
    add_action!("open_selected_in_new_tab", action_open_selected_in_new_tab);
    add_action!("open_selected_in_new_window", action_open_selected_in_new_window);
    add_action!("open_selected_in_console", action_open_selected_in_console);
    add_action!("move_to", action_move_to);
    add_action!("copy_to", action_copy_to);
    add_action!("email_selected", action_email_selected);
    add_action!("empty_trash", action_empty_trash);
    {
        // "Open With…" needs one action per installed app, discovered
        // fresh per right-click — a single parameterized action (the
        // app's bundle path as its string parameter) instead of
        // registering a variable number of actions.
        let open_with_action = gio::SimpleAction::new("open_with", Some(glib::VariantTy::STRING));
        open_with_action.connect_activate(|_, parameter| {
            let Some(app_path) = parameter.and_then(|v| v.get::<String>()) else {
                return;
            };
            if let Some(state) = ACTIVE_STATE.with(|cell| cell.borrow().clone()) {
                for path in selected_paths(&state) {
                    let _ = sepia_platform_macos::apps::open_with(&path, Path::new(&app_path));
                }
            }
        });
        app.add_action(&open_with_action);
    }
    {
        // "Remove from Bookmarks" — one action shared by every bookmark
        // row, parameterized by the bookmark's path.
        let remove_bookmark_action = gio::SimpleAction::new("remove_bookmark", Some(glib::VariantTy::STRING));
        remove_bookmark_action.connect_activate(|_, parameter| {
            let Some(path) = parameter.and_then(|v| v.get::<String>()) else {
                return;
            };
            if let Some(state) = ACTIVE_STATE.with(|cell| cell.borrow().clone()) {
                action_remove_bookmark(&state, PathBuf::from(path));
            }
        });
        app.add_action(&remove_bookmark_action);
    }
    add_action!("cut_selection", action_cut_selection);
    add_action!("copy_selection", action_copy_selection);
    add_action!("paste_clipboard", action_paste_clipboard);
    add_action!("undo", perform_undo);
    add_action!("redo", perform_redo);
    add_action!("select_all", action_select_all);
    add_action!("toggle_hidden", action_toggle_hidden);
    add_action!("toggle_search", action_toggle_search);
    add_action!("view_list", action_view_list);
    add_action!("view_grid", action_view_grid);
    add_action!("go_back", go_back);
    add_action!("go_forward", go_forward);
    add_action!("go_up", go_up);
    add_action!("connect_to_server", action_connect_to_server);
    add_action!("close_tab", action_close_tab);
    add_action!("next_tab", action_next_tab);
    add_action!("prev_tab", action_prev_tab);
    add_action!("zoom_in", action_zoom_in);
    add_action!("zoom_out", action_zoom_out);

    // Placeholder actions: present in the menu bar for visual parity with
    // Nautilus, not yet implemented. `action_noop` is shared since none of
    // them do anything.
    for name in [
        "show_all_tabs",
        "sort_by_name",
        "sort_by_size",
        "sort_by_date_modified",
        "sort_by_kind",
        "visible_columns",
        "captions",
        "keyboard_shortcuts",
    ] {
        add_action!(name, action_noop);
    }
    add_action!("about", show_about_dialog);
    add_action!("preferences", show_preferences_window);

    let new_tab_action = gio::SimpleAction::new("new_tab", None);
    new_tab_action.connect_activate(move |_, _| {
        if let Some(state) = ACTIVE_STATE.with(|cell| cell.borrow().clone()) {
            let state_cell = state_cell_for(&state);
            action_new_tab(&state, &state_cell);
        }
    });
    app.add_action(&new_tab_action);

    let new_window_action = gio::SimpleAction::new("new_window", None);
    new_window_action.connect_activate(glib::clone!(
        #[strong]
        app,
        move |_, _| build_window(&app, None)
    ));
    app.add_action(&new_window_action);

    let sort_by_menu = gio::Menu::new();
    sort_by_menu.append(Some("Name"), Some("app.sort_by_name"));
    sort_by_menu.append(Some("Size"), Some("app.sort_by_size"));
    sort_by_menu.append(Some("Date Modified"), Some("app.sort_by_date_modified"));
    sort_by_menu.append(Some("Kind"), Some("app.sort_by_kind"));

    let file_menu = gio::Menu::new();
    file_menu.append(Some("New Window"), Some("app.new_window"));
    file_menu.append(Some("New Tab"), Some("app.new_tab"));
    file_menu.append(Some("Close Tab"), Some("app.close_tab"));
    file_menu.append(Some("New Folder"), Some("app.new_folder"));
    file_menu.append(Some("Get Info"), Some("app.get_info"));
    file_menu.append(Some("Add to Starred"), Some("app.toggle_starred"));
    file_menu.append(Some("Compress"), Some("app.compress"));
    file_menu.append(Some("Extract"), Some("app.extract"));
    file_menu.append_submenu(Some("Scripts"), &scripts_menu);
    file_menu.append(Some("Move to Trash"), Some("app.move_to_trash"));
    file_menu.append(Some("Delete Permanently"), Some("app.delete_permanently"));

    let edit_menu = gio::Menu::new();
    edit_menu.append(Some("Undo"), Some("app.undo"));
    edit_menu.append(Some("Redo"), Some("app.redo"));
    edit_menu.append(Some("Rename"), Some("app.rename_selected"));
    edit_menu.append(Some("Batch Rename…"), Some("app.batch_rename"));
    edit_menu.append(Some("Cut"), Some("app.cut_selection"));
    edit_menu.append(Some("Copy"), Some("app.copy_selection"));
    edit_menu.append(Some("Paste"), Some("app.paste_clipboard"));
    edit_menu.append(Some("Select All"), Some("app.select_all"));

    let view_menu = gio::Menu::new();
    view_menu.append(Some("as List"), Some("app.view_list"));
    view_menu.append(Some("as Icons"), Some("app.view_grid"));
    view_menu.append(Some("Zoom In"), Some("app.zoom_in"));
    view_menu.append(Some("Zoom Out"), Some("app.zoom_out"));
    view_menu.append_submenu(Some("Sort By"), &sort_by_menu);
    view_menu.append(Some("Visible Columns…"), Some("app.visible_columns"));
    view_menu.append(Some("Captions…"), Some("app.captions"));
    view_menu.append(Some("Show Hidden Files"), Some("app.toggle_hidden"));
    view_menu.append(Some("Find in Folder"), Some("app.toggle_search"));

    let go_menu = gio::Menu::new();
    go_menu.append(Some("Back"), Some("app.go_back"));
    go_menu.append(Some("Forward"), Some("app.go_forward"));
    go_menu.append(Some("Enclosing Folder"), Some("app.go_up"));
    go_menu.append(Some("Connect to Server…"), Some("app.connect_to_server"));
    go_menu.append(Some("Next Tab"), Some("app.next_tab"));
    go_menu.append(Some("Previous Tab"), Some("app.prev_tab"));

    let window_menu = gio::Menu::new();
    window_menu.append(Some("Show All Tabs"), Some("app.show_all_tabs"));

    // Preferences and About aren't listed here: GTK's macOS backend
    // auto-relocates the standard `app.preferences`/`app.about` actions
    // into the native app menu (confirmed via System Events — it already
    // shows "Settings…"/"About sepia" there and triggers these same
    // actions), so listing them again here would just duplicate them.
    let help_menu = gio::Menu::new();
    help_menu.append(Some("Keyboard Shortcuts"), Some("app.keyboard_shortcuts"));

    let menu_bar = gio::Menu::new();
    menu_bar.append_submenu(Some("File"), &file_menu);
    menu_bar.append_submenu(Some("Edit"), &edit_menu);
    menu_bar.append_submenu(Some("View"), &view_menu);
    menu_bar.append_submenu(Some("Go"), &go_menu);
    menu_bar.append_submenu(Some("Window"), &window_menu);
    menu_bar.append_submenu(Some("Help"), &help_menu);
    app.set_menubar(Some(&menu_bar));

    app.set_accels_for_action("app.new_window", &["<Meta>n"]);
    app.set_accels_for_action("app.zoom_in", &["<Meta>equal"]);
    app.set_accels_for_action("app.zoom_out", &["<Meta>minus"]);
    app.set_accels_for_action("app.preferences", &["<Meta>comma"]);
    app.set_accels_for_action("app.new_tab", &["<Meta>t"]);
    app.set_accels_for_action("app.close_tab", &["<Meta>w"]);
    app.set_accels_for_action("app.next_tab", &["<Meta><Shift>bracketright"]);
    app.set_accels_for_action("app.prev_tab", &["<Meta><Shift>bracketleft"]);
    app.set_accels_for_action("app.new_folder", &["<Meta><Shift>n"]);
    app.set_accels_for_action("app.move_to_trash", &["<Meta>BackSpace"]);
    app.set_accels_for_action("app.delete_permanently", &["<Meta><Alt>BackSpace"]);
    app.set_accels_for_action("app.rename_selected", &["F2"]);
    app.set_accels_for_action("app.get_info", &["<Meta>i"]);
    app.set_accels_for_action("app.cut_selection", &["<Meta>x"]);
    app.set_accels_for_action("app.copy_selection", &["<Meta>c"]);
    app.set_accels_for_action("app.paste_clipboard", &["<Meta>v"]);
    app.set_accels_for_action("app.select_all", &["<Meta>a"]);
    app.set_accels_for_action("app.toggle_hidden", &["<Meta>period"]);
    app.set_accels_for_action("app.toggle_search", &["<Meta>f"]);
    app.set_accels_for_action("app.undo", &["<Meta>z"]);
    app.set_accels_for_action("app.redo", &["<Meta><Shift>z"]);
    app.set_accels_for_action("app.view_list", &["<Meta>1"]);
    app.set_accels_for_action("app.view_grid", &["<Meta>2"]);
    app.set_accels_for_action("app.go_back", &["<Meta>bracketleft"]);
    app.set_accels_for_action("app.go_forward", &["<Meta>bracketright"]);
    app.set_accels_for_action("app.go_up", &["<Meta>Up"]);
    app.set_accels_for_action("app.connect_to_server", &["<Meta>k"]);
}

/// Builds one browser window: its own tabs, sidebar, toolbar, and
/// background timers (directory watchers, volume polling), all fully
/// independent of any other open window. Called once for the initial
/// window, and again for every "New Window"/"Open in New Window".
/// `initial_dir` overrides the usual startup-location preference — used
/// by "Open in New Window" to open directly at a specific folder.
fn build_window(app: &Application, initial_dir: Option<PathBuf>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Sepia")
        .default_width(900)
        .default_height(600)
        .build();

    // Forward-reference cell resolved once `AppState` is constructed below;
    // see `build_tab_content`'s doc comment for why this indirection exists.
    let state_cell: StateCell = Rc::new(RefCell::new(None));

    let back_button = Button::from_icon_name("go-previous-symbolic");
    let forward_button = Button::from_icon_name("go-next-symbolic");
    let new_tab_button = Button::from_icon_name("tab-new-symbolic");

    // Verified against nautilus-toolbar.blp / nautilus-view-controls.blp
    // (../nautilus): search toggle sits right after the path bar (icon
    // "nautilus-folder-search-symbolic", a custom Nautilus icon — using
    // the standard "edit-find-symbolic" here instead), followed by New
    // Folder, then the view-mode `Adw.SplitButton`. There is no separate
    // "kebab" actions menu in the real toolbar — that was a misread on my
    // part; removed.
    let search_button = Button::from_icon_name("edit-find-symbolic");
    search_button.set_tooltip_text(Some("Search Current Folder"));
    let new_folder_button = Button::from_icon_name("folder-new-symbolic");
    new_folder_button.set_tooltip_text(Some("New Folder"));

    // Combined view-mode toggle (icon reflects the active tab's current
    // mode; click switches to the other) plus a separate chevron opening
    // the view-options popover — together these stand in for Nautilus's
    // single `Adw.SplitButton`, which GTK4 has no direct equivalent for.
    // Popover contents match nautilus-view-controls.blp's `view_menu`
    // exactly: Zoom Out/In, Sort (submenu here vs. dynamic there), Show
    // Hidden Files, Visible Columns (list view), Captions (grid view).
    let view_toggle_button = Button::from_icon_name("view-list-symbolic");
    let sort_by_menu = gio::Menu::new();
    sort_by_menu.append(Some("Name"), Some("app.sort_by_name"));
    sort_by_menu.append(Some("Size"), Some("app.sort_by_size"));
    sort_by_menu.append(Some("Date Modified"), Some("app.sort_by_date_modified"));
    sort_by_menu.append(Some("Kind"), Some("app.sort_by_kind"));
    let view_options_menu = gio::Menu::new();
    view_options_menu.append(Some("Zoom Out"), Some("app.zoom_out"));
    view_options_menu.append(Some("Zoom In"), Some("app.zoom_in"));
    view_options_menu.append_submenu(Some("Sort By"), &sort_by_menu);
    view_options_menu.append(Some("Show Hidden Files"), Some("app.toggle_hidden"));
    view_options_menu.append(Some("Visible Columns…"), Some("app.visible_columns"));
    view_options_menu.append(Some("Captions…"), Some("app.captions"));
    let view_options_button = gtk::MenuButton::builder()
        .icon_name("pan-down-symbolic")
        .menu_model(&view_options_menu)
        .tooltip_text("View Options")
        .build();

    let search_entry = Entry::builder()
        .placeholder_text("Search this folder")
        .hexpand(true)
        .visible(false)
        .build();
    // Path bar lives inline in the single toolbar row (matching Nautilus's
    // header bar exactly, per the reference screenshots) rather than as a
    // separate row underneath — `hexpand` lets it fill the flexible middle
    // space, pushing the trailing buttons to the right edge.
    let path_bar = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(2)
        .hexpand(true)
        .build();

    let toolbar = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    toolbar.append(&back_button);
    toolbar.append(&forward_button);
    toolbar.append(&path_bar);
    toolbar.append(&search_button);
    toolbar.append(&search_entry);
    toolbar.append(&new_folder_button);
    toolbar.append(&view_toggle_button);
    toolbar.append(&view_options_button);
    toolbar.append(&new_tab_button);

    new_folder_button.connect_clicked(glib::clone!(
        #[strong]
        state_cell,
        move |_| {
            if let Some(state) = state_cell.borrow().clone() {
                action_new_folder(&state);
            }
        }
    ));
    view_toggle_button.connect_clicked(glib::clone!(
        #[strong]
        state_cell,
        move |_| {
            if let Some(state) = state_cell.borrow().clone() {
                action_toggle_view(&state);
            }
        }
    ));

    // Tabs. Each page is a tab's own `Stack` (list/grid content) built by
    // `build_tab_content`; the tab strip is hidden until a second tab
    // exists, matching Finder's convention of not showing tabs for a
    // single-tab window.
    let notebook = Notebook::new();
    notebook.set_show_tabs(false);
    notebook.set_scrollable(true);
    notebook.set_vexpand(true);
    notebook.set_hexpand(true);

    let content_pane = GtkBox::builder().orientation(Orientation::Vertical).build();
    content_pane.append(&toolbar);
    content_pane.append(&notebook);

    let preferences = sepia_core::preferences::Preferences::load();
    let show_hidden = preferences.show_hidden;
    GRID_ZOOM_INDEX.with(|zoom| zoom.set(preferences.zoom_index.min(GRID_ICON_SIZES.len() - 1)));

    let state = Rc::new(RefCell::new(AppState {
        window: window.clone(),
        notebook: notebook.clone(),
        tabs: Vec::new(),
        active_tab: 0,
        next_tab_id: 0,
        path_bar,
        back_button: back_button.clone(),
        forward_button: forward_button.clone(),
        sidebar_targets: Vec::new(),
        sidebar_list_box: None,
        clipboard: None,
        undo_manager: UndoManager::default(),
        show_hidden,
        search_entry: search_entry.clone(),
        view_toggle_button: view_toggle_button.clone(),
        bookmarks: sepia_core::bookmarks::Bookmarks::load({
            let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
            vec![
                home.join("Documents"),
                home.join("Music"),
                home.join("Pictures"),
                home.join("Movies"),
                home.join("Downloads"),
            ]
        }),
        preferences,
        recent: sepia_core::recent::RecentFiles::load(),
        starred: sepia_core::starred::Starred::load(),
        recent_servers: sepia_core::network::RecentServers::load(),
        folder_view_state: sepia_core::folder_view_state::FolderViewState::load(),
    }));
    *state_cell.borrow_mut() = Some(state.clone());

    notebook.connect_switch_page(glib::clone!(
        #[strong]
        state,
        move |_notebook, page_widget, _page_num| {
            let id = {
                let state_ref = state.borrow();
                state_ref
                    .tabs
                    .iter()
                    .find(|t| t.view_stack.upcast_ref::<gtk::Widget>() == page_widget)
                    .map(|t| t.id)
            };
            if let Some(id) = id {
                state.borrow_mut().active_tab = id;
                let dir = state.borrow().tab(id).current_dir.clone();
                refresh_toolbar(&state, &dir);
            }
        }
    ));

    let (sidebar, list_box) = build_sidebar(&state);
    state.borrow_mut().sidebar_list_box = Some(list_box.clone());

    let paned = Paned::new(Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar));
    paned.set_end_child(Some(&content_pane));
    paned.set_position(180);
    paned.set_resize_start_child(false);

    window.set_child(Some(&paned));

    // Space bar toggles Quick Look, matching Finder. Capture phase: the
    // file view's own key handling otherwise swallows Space before it
    // would bubble up to a default-phase window-level controller (same
    // issue as the rubber-band/drag-source conflict elsewhere in this
    // file). Explicitly skipped while the search entry has focus, so
    // typing a literal space still works there.
    let quick_look_key = gtk::EventControllerKey::new();
    quick_look_key.set_propagation_phase(gtk::PropagationPhase::Capture);
    quick_look_key.connect_key_pressed(glib::clone!(
        #[strong]
        state,
        #[strong]
        search_entry,
        move |_controller, key, _keycode, _modifiers| {
            if key != gdk::Key::space || search_entry.has_focus() {
                return glib::Propagation::Proceed;
            }
            action_toggle_quick_look(&state);
            glib::Propagation::Stop
        }
    ));
    window.add_controller(quick_look_key);

    back_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| go_back(&state)
    ));
    forward_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| go_forward(&state)
    ));
    new_tab_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[strong]
        state_cell,
        move |_| action_new_tab(&state, &state_cell)
    ));
    search_button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| action_toggle_search(&state)
    ));

    // Search-as-you-type, debounced: each keystroke cancels any pending
    // search-start timer and schedules a new one, so a fast typist doesn't
    // spawn a background walk per keystroke — only ~200ms after they pause.
    let search_debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    search_entry.connect_changed(glib::clone!(
        #[strong]
        state,
        #[strong]
        search_debounce,
        move |entry| {
            if let Some(source_id) = search_debounce.borrow_mut().take() {
                source_id.remove();
            }
            let query = entry.text().to_string();
            let id = state.borrow().active_tab;
            if query.is_empty() {
                stop_tab_search(&state, id);
                refresh_active_view(&state);
                return;
            }
            state.borrow_mut().tab_mut(id).search_query = Some(query.clone());
            let source_id = glib::timeout_add_local_once(
                std::time::Duration::from_millis(200),
                glib::clone!(
                    #[strong]
                    state,
                    #[strong]
                    search_debounce,
                    move || {
                        // The timer already fired, so GLib has freed this
                        // source on its own — clear the stored id first so
                        // a later keystroke doesn't try to `.remove()` an
                        // id that no longer exists (which panics).
                        *search_debounce.borrow_mut() = None;
                        run_search(&state, id, query);
                    }
                ),
            );
            *search_debounce.borrow_mut() = Some(source_id);
        }
    ));

    let search_escape = gtk::EventControllerKey::new();
    search_escape.connect_key_pressed(glib::clone!(
        #[strong]
        state,
        move |_controller, key, _keycode, _modifiers| {
            if key == gdk::Key::Escape {
                action_toggle_search(&state);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    search_entry.add_controller(search_escape);

    // Drop target: dropping files onto a sidebar row moves/copies them into
    // that location.
    let sidebar_drop = gtk::DropTarget::new(
        gdk::FileList::static_type(),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    sidebar_drop.connect_drop(glib::clone!(
        #[strong]
        state,
        #[strong]
        list_box,
        move |_target, value, _x, y| {
            let Ok(file_list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let files = file_list.files();
            if files.is_empty() {
                return false;
            }
            let Some(row) = list_box.row_at_y(y as i32) else {
                // Dropped in the empty space below the last row: add as a
                // bookmark instead, matching Nautilus's own convention for
                // dropping a folder onto the sidebar's blank area. Only
                // handles a single dropped folder — anything else is a
                // no-op here.
                if let [file] = files.as_slice() {
                    if let Some(path) = file.path().or_else(|| repair_dropped_file_path(file)) {
                        if path.is_dir() {
                            state.borrow_mut().bookmarks.add(path);
                            rebuild_sidebar_rows(&state, &list_box);
                            return true;
                        }
                    }
                }
                return false;
            };
            let target = state.borrow().sidebar_targets.get(row.index() as usize).cloned().flatten();
            let Some(SidebarTarget::Path(dest_dir)) = target else {
                return false;
            };
            accept_dropped_files(&state, files, dest_dir);
            true
        }
    ));
    list_box.add_controller(sidebar_drop);

    // Tracks focus so the app-wide menu bar (built once, in
    // `build_menu_bar_and_actions`) routes to this window whenever it's
    // the active one.
    window.connect_is_active_notify(glib::clone!(
        #[strong]
        state,
        move |window| {
            if window.is_active() {
                ACTIVE_STATE.with(|cell| *cell.borrow_mut() = Some(state.clone()));
            }
        }
    ));
    ACTIVE_STATE.with(|cell| *cell.borrow_mut() = Some(state.clone()));

    let start_dir = initial_dir.unwrap_or_else(|| {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
        let state_ref = state.borrow();
        match state_ref.preferences.new_window_location {
            sepia_core::preferences::NewWindowLocation::LastLocation => {
                state_ref.preferences.last_location.clone().filter(|p| p.is_dir()).unwrap_or(home)
            }
            sepia_core::preferences::NewWindowLocation::Home => home,
        }
    });
    new_tab(&state, &state_cell, start_dir);

    // Polls each tab's filesystem watcher on a fixed tick, coalescing
    // bursts of external changes (e.g. a large Finder copy) into a single
    // reload per affected tab instead of one per event.
    glib::timeout_add_local(
        std::time::Duration::from_millis(500),
        glib::clone!(
            #[strong]
            state,
            move || {
                let changed_ids: Vec<u64> = {
                    let state_ref = state.borrow();
                    state_ref
                        .tabs
                        .iter()
                        .filter(|t| t.watcher.poll_changed())
                        .map(|t| t.id)
                        .collect()
                };
                for id in changed_ids {
                    refresh_tab(&state, id);
                }
                glib::ControlFlow::Continue
            }
        ),
    );

    // Polls the mounted-volumes list so newly connected network shares
    // (via Connect to Server) and inserted/removed drives show up in the
    // sidebar without requiring some unrelated sidebar-rebuild trigger
    // (e.g. adding a bookmark) to happen first.
    let last_volume_paths: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(
        sepia_platform_macos::volumes::list_volumes().into_iter().map(|v| v.path).collect(),
    ));
    glib::timeout_add_local(
        std::time::Duration::from_secs(2),
        glib::clone!(
            #[strong]
            state,
            #[strong]
            list_box,
            #[strong]
            last_volume_paths,
            move || {
                let current: Vec<PathBuf> = sepia_platform_macos::volumes::list_volumes().into_iter().map(|v| v.path).collect();
                if *last_volume_paths.borrow() != current {
                    *last_volume_paths.borrow_mut() = current;
                    rebuild_sidebar_rows(&state, &list_box);
                }
                glib::ControlFlow::Continue
            }
        ),
    );

    window.present();
}
