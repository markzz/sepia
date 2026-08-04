use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSImage, NSWorkspace};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSDictionary, NSError, NSString, NSURL};
use objc2_quick_look_thumbnailing::{
    QLThumbnailGenerationRequest, QLThumbnailGenerationRequestRepresentationTypes, QLThumbnailGenerator,
    QLThumbnailRepresentation,
};

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// Encodes an `NSImage` as PNG bytes via its `CGImage`, avoiding a TIFF
/// round-trip — used for both `NSWorkspace`'s per-type icons and
/// QuickLook's generated thumbnails, which both hand back an `NSImage`.
fn nsimage_to_png(image: &NSImage) -> Option<Vec<u8>> {
    let size = image.size();
    let mut proposed_rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(size.width, size.height));
    let cg_image = unsafe { image.CGImageForProposedRect_context_hints(&mut proposed_rect, None, None) }?;
    let bitmap_rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg_image);
    let empty_props: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> =
        NSDictionary::from_slices::<NSString>(&[], &[]);
    let data = unsafe { bitmap_rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty_props) }?;
    Some(data.to_vec())
}

/// The correct per-type icon for `path` (matches Finder's own — respects
/// LaunchServices file-type associations, custom app icons, folder
/// icons, etc.), as PNG bytes.
pub fn icon_for_file(path: &Path) -> Option<Vec<u8>> {
    let workspace = NSWorkspace::sharedWorkspace();
    let image = workspace.iconForFile(&NSString::from_str(&path.to_string_lossy()));
    nsimage_to_png(&image)
}

thread_local! {
    // `QLThumbnailGenerator`'s completion handler fires on an arbitrary
    // background queue, not the calling thread — `glib::MainContext::
    // invoke` (safe to call from any thread) marshals back to the GTK
    // main loop, but it requires the invoked closure to be `Send`. The
    // callbacks callers actually want to run (updating a `gtk::Image`,
    // etc.) capture non-`Send` GObject types, so they can't travel
    // through `invoke` directly. Instead, the callback is stashed here
    // (thread-local — never leaves the main thread) keyed by an id, and
    // only that id plus the `Send`-safe `Vec<u8>` result cross over.
    static PENDING: RefCell<HashMap<u64, Box<dyn FnOnce(Option<Vec<u8>>)>>> = RefCell::new(HashMap::new());
}
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(0);

/// Asynchronously requests a real content thumbnail (image/video/PDF/etc.
/// preview, not just a generic type icon) for `path`, via QuickLook's
/// thumbnail generator. `callback` runs on the main thread's GLib main
/// loop, with PNG bytes if a thumbnail could be generated (`None` for
/// file types QuickLook has no preview for — the caller should keep
/// showing `icon_for_file`'s placeholder in that case). Must itself be
/// called from the main thread.
pub fn request_thumbnail(path: PathBuf, pixel_size: f64, scale: f64, callback: impl FnOnce(Option<Vec<u8>>) + 'static) {
    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    PENDING.with(|cell| cell.borrow_mut().insert(id, Box::new(callback)));

    let main_context = glib::MainContext::default();
    let url = file_url(&path);
    let size = CGSize::new(pixel_size, pixel_size);
    let request = unsafe {
        QLThumbnailGenerationRequest::initWithFileAtURL_size_scale_representationTypes(
            QLThumbnailGenerationRequest::alloc(),
            &url,
            size,
            scale,
            QLThumbnailGenerationRequestRepresentationTypes::Thumbnail,
        )
    };
    let generator = unsafe { QLThumbnailGenerator::sharedGenerator() };
    let block = block2::RcBlock::new(move |representation: *mut QLThumbnailRepresentation, _error: *mut NSError| {
        let representation: Option<Retained<QLThumbnailRepresentation>> = unsafe { Retained::retain(representation) };
        let png_bytes = representation.and_then(|r| {
            let image = unsafe { r.NSImage() };
            nsimage_to_png(&image)
        });
        main_context.invoke(move || {
            if let Some(callback) = PENDING.with(|cell| cell.borrow_mut().remove(&id)) {
                callback(png_bytes);
            }
        });
    });
    unsafe {
        generator.generateBestRepresentationForRequest_completionHandler(&request, &block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_png(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("sepia-icons-test-{name}-{}.png", std::process::id()));
        // A minimal valid 1x1 PNG — enough for LaunchServices/QuickLook
        // to recognize a real content type.
        let png_bytes: [u8; 69] = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00,
            0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03,
            0x00, 0x01, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60,
            0x82,
        ];
        std::fs::write(&path, png_bytes).unwrap();
        path
    }

    #[test]
    fn icon_for_file_returns_real_png_bytes() {
        let path = write_test_png("icon");
        let icon = icon_for_file(&path);
        std::fs::remove_file(&path).ok();
        let icon = icon.expect("expected an icon for a PNG file");
        assert!(icon.starts_with(&[0x89, 0x50, 0x4E, 0x47]), "result should itself be PNG-encoded");
    }

    #[test]
    fn request_thumbnail_calls_back_on_main_loop() {
        let path = write_test_png("thumb");
        let main_loop = glib::MainLoop::new(None, false);
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));

        request_thumbnail(path.clone(), 64.0, 1.0, glib::clone!(
            #[strong]
            result,
            #[strong]
            main_loop,
            move |bytes| {
                *result.borrow_mut() = Some(bytes);
                main_loop.quit();
            }
        ));

        // Real async work with a real timeout, not a busy-loop — bounded
        // so a genuine failure (QuickLook never calling back) doesn't
        // hang the test suite.
        glib::timeout_add_seconds_local_once(10, glib::clone!(
            #[strong]
            main_loop,
            move || main_loop.quit()
        ));
        main_loop.run();
        std::fs::remove_file(&path).ok();

        let result = result.borrow_mut().take();
        assert!(result.is_some(), "callback never fired within the timeout");
    }
}
