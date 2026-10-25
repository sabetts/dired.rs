//! The log behind `C-h e` (`view-echo-area-messages`).

use std::collections::VecDeque;

/// Emacs's default `message-log-max`.
pub const MAX_LINES: usize = 1000;

/// A bounded log of echo-area messages.
#[derive(Debug, Default)]
pub struct MessageLog {
    lines: VecDeque<String>,
}

impl MessageLog {
    /// Append a message, splitting multi-line output into separate log lines.
    pub fn push(&mut self, message: &str) {
        if message.is_empty() {
            return;
        }
        for line in message.split('\n') {
            self.lines.push_back(line.to_string());
            if self.lines.len() > MAX_LINES {
                self.lines.pop_front();
            }
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_messages_in_order_and_splits_lines() {
        let mut log = MessageLog::default();
        log.push("first");
        log.push("second\nthird");
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
    }

    #[test]
    fn ignores_empty_messages_and_can_clear() {
        let mut log = MessageLog::default();
        log.push("");
        assert!(log.is_empty());
        log.push("one");
        log.clear();
        assert!(log.is_empty());
    }

    #[test]
    fn truncates_to_the_most_recent_lines() {
        let mut log = MessageLog::default();
        for i in 0..(MAX_LINES + 2) {
            log.push(&i.to_string());
        }
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], "2");
        assert_eq!(lines[MAX_LINES - 1], (MAX_LINES + 1).to_string());
    }
}
