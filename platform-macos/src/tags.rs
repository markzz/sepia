use std::path::Path;

use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSString, NSURL, NSURLTagNamesKey};

/// One Finder tag as read back from a file: its name, and whatever color
/// (1-7) macOS has associated with that name in its own tag registry —
/// `None` for uncolored. Confirmed empirically (not just documentation):
/// the color isn't something callers can dictate through this API. Setting
/// a tag's raw string to e.g. `"Red\n1"` doesn't stick — macOS silently
/// re-derives and appends its own `\n<index>` suffix based on a system-wide
/// name→color mapping (configured via Finder's own Tags preferences), so
/// sepia only ever writes plain names and treats color as read-only,
/// display-only information.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tag {
    pub name: String,
    pub color: Option<u8>,
}

impl Tag {
    /// Finder stores color as a `\n<digit>` suffix on the raw name string
    /// in the `com.apple.metadata:_kMDItemUserTags` extended attribute
    /// (which `NSURLTagNamesKey` reads for us, already decoded from the
    /// underlying binary plist).
    fn parse(raw: &str) -> Self {
        match raw.rsplit_once('\n') {
            Some((name, color)) if color.len() == 1 && color.chars().all(|c| c.is_ascii_digit()) => {
                let color: u8 = color.parse().unwrap_or(0);
                Tag { name: name.to_string(), color: (color != 0).then_some(color) }
            }
            _ => Tag { name: raw.to_string(), color: None },
        }
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// Reads `path`'s current Finder tags. Returns an empty list if the file
/// has none, doesn't exist, or the read otherwise fails.
pub fn get_tags(path: &Path) -> Vec<Tag> {
    let url = file_url(path);
    let key = unsafe { NSURLTagNamesKey };
    let keys = NSArray::from_slice(&[key]);
    let Ok(values) = url.resourceValuesForKeys_error(&keys) else {
        return Vec::new();
    };
    let Some(names) = values.objectForKey(key) else {
        return Vec::new();
    };
    let Ok(names) = names.downcast::<NSArray>() else {
        return Vec::new();
    };
    // Safety: `NSURLTagNamesKey`'s value is documented (and verified
    // empirically) to always be an array of NSString.
    let names: &NSArray<NSString> = unsafe { names.cast_unchecked() };
    names.iter().map(|s| Tag::parse(&s.to_string())).collect()
}

/// Replaces `path`'s Finder tags with `names` entirely — plain names only
/// (see `Tag`'s doc comment for why sepia doesn't attempt to set colors).
pub fn set_tags(path: &Path, names: &[String]) -> Result<(), String> {
    let url = file_url(path);
    let encoded: Vec<Retained<NSString>> = names.iter().map(|n| NSString::from_str(n)).collect();
    let refs: Vec<&NSString> = encoded.iter().map(|s| s.as_ref()).collect();
    let array = NSArray::from_slice(&refs);
    let key = unsafe { NSURLTagNamesKey };
    unsafe { url.setResourceValue_forKey_error(Some(&array), key) }.map_err(|e| e.localizedDescription().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_tag_names() {
        let path = std::env::temp_dir().join(format!("sepia-tags-test-{}", std::process::id()));
        std::fs::write(&path, b"test").unwrap();

        let names = vec!["Red".to_string(), "Custom".to_string()];
        set_tags(&path, &names).expect("set_tags should succeed");
        let read_back = get_tags(&path);

        std::fs::remove_file(&path).ok();

        assert_eq!(read_back.iter().map(|t| t.name.clone()).collect::<Vec<_>>(), names);
    }
}
