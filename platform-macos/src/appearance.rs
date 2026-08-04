use objc2::MainThreadMarker;
use objc2_app_kit::{NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication};
use objc2_foundation::NSArray;

/// Whether the system is currently in Dark mode, via `NSApplication`'s
/// `effectiveAppearance` — the same source of truth Finder and every
/// other native app follow, so this stays in sync with whatever the user
/// has set in System Settings (light/dark/auto).
pub fn is_dark_mode(mtm: MainThreadMarker) -> bool {
    let app = NSApplication::sharedApplication(mtm);
    let appearance = app.effectiveAppearance();
    // Safety: these are just opaque `NSString` constants provided by
    // AppKit — reading the static pointers is fine, there's no aliasing
    // or lifetime concern here.
    let (aqua, dark_aqua) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark_aqua]);
    appearance.bestMatchFromAppearancesWithNames(&names).is_some_and(|best| &*best == dark_aqua)
}
