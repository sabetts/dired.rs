//! Free disk space, shown in the Dired header line.

use std::path::Path;

/// Bytes available to the current user on `path`'s filesystem, or `None` when
/// it cannot be determined.
pub fn available_space(path: &Path) -> Option<u64> {
    fs4::available_space(path).ok()
}
