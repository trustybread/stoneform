//! Disk facts the window shows: free space and byte counts.

use std::path::Path;

use objc2_foundation::{
    NSArray, NSByteCountFormatter, NSByteCountFormatterCountStyle, NSNumber, NSString, NSURL,
    NSURLVolumeAvailableCapacityForImportantUsageKey,
};

/// Space available for `path`'s volume, counting what macOS can purge for an
/// important download. `path` need not exist yet; its nearest existing folder decides.
pub fn available_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(existing.to_str()?));
    let key = unsafe { NSURLVolumeAvailableCapacityForImportantUsageKey };
    let values = url.resourceValuesForKeys_error(&NSArray::from_slice(&[key])).ok()?;
    let number = values.objectForKey(key)?.downcast::<NSNumber>().ok()?;
    u64::try_from(number.longLongValue()).ok()
}

/// "12.3 GB", as Finder writes sizes.
pub fn format_bytes(count: u64) -> String {
    let count = i64::try_from(count).unwrap_or(i64::MAX);
    NSByteCountFormatter::stringFromByteCount_countStyle(count, NSByteCountFormatterCountStyle::File).to_string()
}
