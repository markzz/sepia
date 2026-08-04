use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{
    NSArray, NSMetadataItemPathKey, NSMetadataQuery, NSMetadataQueryDidFinishGatheringNotification,
    NSMetadataQueryDidUpdateNotification, NSNotification, NSNotificationCenter, NSObjectProtocol,
    NSOperationQueue, NSPredicate, NSString, NSURL,
};

pub struct SearchHit {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// A live Spotlight query scoped to one folder's subtree. Keeps running —
/// delivering an updated snapshot on every gather/update notification, not
/// just once — until dropped, at which point the query is stopped and its
/// notification observers removed.
pub struct SpotlightQuery {
    query: Retained<NSMetadataQuery>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl SpotlightQuery {
    /// Starts a query for file/folder names containing `name_query`
    /// (case- and diacritic-insensitive), scoped to `root`'s subtree.
    /// `on_results` is called on the main thread — both for the initial
    /// gather and for every live update after — with the current full
    /// result set.
    pub fn start(root: &Path, name_query: &str, on_results: impl Fn(Vec<SearchHit>) + 'static) -> Self {
        let query = NSMetadataQuery::new();

        let format = NSString::from_str("kMDItemFSName CONTAINS[cd] %@");
        let arg = NSString::from_str(name_query);
        let args = NSArray::from_slice(&[&*arg]);
        // Safety: `args` holds a single NSString, matching the single `%@`
        // in `format`.
        let args: &NSArray = unsafe { args.cast_unchecked() };
        let predicate = unsafe { NSPredicate::predicateWithFormat_argumentArray(&format, Some(args)) };
        query.setPredicate(Some(&predicate));

        let root_str = NSString::from_str(&root.to_string_lossy());
        let root_url = NSURL::fileURLWithPath(&root_str);
        let scopes = NSArray::from_slice(&[&*root_url]);
        // Safety: `setSearchScopes` accepts an array of NSURL (or
        // NSString) scope entries, which `scopes` is.
        let scopes: &NSArray = unsafe { scopes.cast_unchecked() };
        unsafe { query.setSearchScopes(scopes) };

        let query_for_callback = query.clone();
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            on_results(collect_results(&query_for_callback));
        });

        let center = NSNotificationCenter::defaultCenter();
        let main_queue = NSOperationQueue::mainQueue();

        // Safety: the block only captures an `Rc`-free closure over
        // `Retained` (thread-safe refcounting) state and a plain `Fn`
        // callback; `main_queue` guarantees it only ever runs on the main
        // thread, the same thread that constructed it and that owns the
        // GTK/GLib main loop this callback ultimately feeds into.
        let observer1 = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSMetadataQueryDidFinishGatheringNotification),
                Some(&query),
                Some(&main_queue),
                &block,
            )
        };
        let observer2 = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSMetadataQueryDidUpdateNotification),
                Some(&query),
                Some(&main_queue),
                &block,
            )
        };

        query.startQuery();

        Self {
            query,
            observers: vec![observer1, observer2],
        }
    }
}

impl Drop for SpotlightQuery {
    fn drop(&mut self) {
        self.query.stopQuery();
        let center = NSNotificationCenter::defaultCenter();
        for observer in &self.observers {
            unsafe { center.removeObserver(observer.as_ref()) };
        }
    }
}

fn collect_results(query: &NSMetadataQuery) -> Vec<SearchHit> {
    let count = query.resultCount();
    let mut hits = Vec::with_capacity(count as usize);
    for i in 0..count {
        let Ok(item) = query.resultAtIndex(i).downcast::<objc2_foundation::NSMetadataItem>() else {
            continue;
        };
        let Some(path_value) = item.valueForAttribute(unsafe { NSMetadataItemPathKey }) else {
            continue;
        };
        let Ok(path_str) = path_value.downcast::<NSString>() else {
            continue;
        };
        let path = PathBuf::from(path_str.to_string());
        let is_dir = path.is_dir();
        hits.push(SearchHit { path, is_dir });
    }
    hits
}
