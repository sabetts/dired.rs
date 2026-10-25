//! The log behind `C-h e` (`view-echo-area-messages`).

use std::collections::VecDeque;

/// Emacs's default `message-log-max`.
pub const MAX_LINES: usize = 1000;

#[derive(Debug)]
struct Entry {
    text: String,
    count: usize,
}

/// A bounded log of echo-area messages.
#[derive(Debug, Default)]
pub struct MessageLog {
    entries: VecDeque<Entry>,
}

impl MessageLog {
    /// Append a message, collapsing a repeat of the previous message into an
    /// `[N times]` suffix like Emacs's `*Messages*` buffer.
    pub fn push(&mut self, message: &str) {
        if message.is_empty() {
            return;
        }
        if let Some(last) = self.entries.back_mut() {
            if last.text == message {
                last.count += 1;
                return;
            }
        }
        self.entries.push_back(Entry {
            text: message.to_string(),
            count: 1,
        });
        self.trim();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Render entries to lines, suffixing repeated messages on their last line.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let start = out.len();
            out.extend(entry.text.split('\n').map(str::to_string));
            if entry.count > 1 {
                if let Some(last) = out.get_mut(start + entry.text.split('\n').count() - 1) {
                    last.push_str(&format!(" [{} times]", entry.count));
                }
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn line_count(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.text.split('\n').count())
            .sum()
    }

    fn trim(&mut self) {
        while self.line_count() > MAX_LINES && self.entries.len() > 1 {
            self.entries.pop_front();
        }
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
            log.lines(),
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn collapses_repeated_messages() {
        let mut log = MessageLog::default();
        log.push("same");
        log.push("same");
        assert_eq!(log.lines(), vec!["same [2 times]".to_string()]);
        log.push("same");
        assert_eq!(log.lines(), vec!["same [3 times]".to_string()]);
        log.push("different");
        assert_eq!(
            log.lines(),
            vec!["same [3 times]".to_string(), "different".to_string()]
        );
    }

    #[test]
    fn suffixes_the_last_line_of_a_repeated_multiline_message() {
        let mut log = MessageLog::default();
        log.push("one\ntwo");
        log.push("one\ntwo");
        assert_eq!(
            log.lines(),
            vec!["one".to_string(), "two [2 times]".to_string()]
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
        let lines = log.lines();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], "2");
        assert_eq!(lines[MAX_LINES - 1], (MAX_LINES + 1).to_string());
    }
}
