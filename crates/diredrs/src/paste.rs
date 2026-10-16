//! Detecting pasted input on terminals that do not report bracketed paste.
//!
//! On Windows, crossterm reads input with `ReadConsoleInput`, so a paste
//! arrives as a burst of individual key events rather than an `Event::Paste`
//! (crossterm issue #737). A multi-line paste would otherwise be dispatched as
//! a run of commands. We detect the burst by peeking the console input queue:
//! after one key is read, a paste leaves many more records already queued.

/// Number of queued console input records (beyond the key just read) that
/// indicate a paste rather than typing. A single physical keystroke leaves only
/// its own key-up queued (~1 record), so this avoids that false positive while
/// catching any paste of two or more characters. A single-character paste is
/// indistinguishable from a keystroke and is not detected.
pub const PASTE_MIN_PENDING: usize = 3;

/// Number of input records currently queued in the console input buffer.
///
/// Returns 0 on non-Windows platforms (which deliver a real `Event::Paste`),
/// and 0 when the console is unavailable (redirected input, no console).
#[cfg(windows)]
pub fn pending_events() -> usize {
    use crossterm_winapi::{Console, Handle};
    let Ok(handle) = Handle::current_in_handle() else {
        return 0;
    };
    Console::from(handle)
        .number_of_console_input_events()
        .map(|n| n as usize)
        .unwrap_or(0)
}

#[cfg(not(windows))]
pub fn pending_events() -> usize {
    0
}

/// Whether a text-bearing key should start a paste: either we are still inside
/// the latch window of a paste already in progress, or the console queue is
/// backed up with a burst of records.
pub fn is_paste(pending: usize, latched: bool, key_has_text: bool) -> bool {
    key_has_text && (latched || pending >= PASTE_MIN_PENDING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_backed_up_queue() {
        assert!(is_paste(PASTE_MIN_PENDING, false, true));
        assert!(is_paste(50, false, true));
        // A single keystroke (its own key-up queued) is not a paste.
        assert!(!is_paste(0, false, true));
        assert!(!is_paste(1, false, true));
    }

    #[test]
    fn latch_keeps_a_split_paste_going() {
        assert!(is_paste(0, true, true));
        // Non-text keys are never treated as paste text.
        assert!(!is_paste(50, false, false));
        assert!(!is_paste(0, true, false));
    }
}
