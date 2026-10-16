//! Simple multi-line input buffer with cursor tracking.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Input buffer with cursor position.
#[derive(Debug, Clone)]
pub struct InputBuffer {
    /// The text content.
    text: String,
    /// Cursor position as byte offset (always on a char boundary).
    cursor: usize,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    /// Is the buffer empty?
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Take the buffer content, clearing it.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Get the text content.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Get cursor position (byte offset).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Insert a character at cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete character after cursor (Delete key).
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            // Find the char after cursor
            let start = self.cursor;
            let end = self.text[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(self.text.len());
            self.text.drain(start..end);
        }
    }

    /// Delete character before cursor (Backspace key).
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            // Find the char before cursor
            let end = self.cursor;
            let start = self.text[..end]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.drain(start..end);
            self.cursor = start;
        }
    }

    /// Move cursor left by one char.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            let end = self.cursor;
            let start = self.text[..end]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor = start;
        }
    }

    /// Move cursor right by one char.
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            let start = self.cursor;
            let end = self.text[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(self.text.len());
            self.cursor = end;
        }
    }

    /// Move cursor to start of line (or buffer for single-line).
    pub fn move_home(&mut self) {
        // For multi-line, find start of current line
        if let Some(newline_pos) = self.text[..self.cursor].rfind('\n') {
            self.cursor = newline_pos + 1;
        } else {
            self.cursor = 0;
        }
    }

    /// Move cursor to end of line (or buffer for single-line).
    pub fn move_end(&mut self) {
        // For multi-line, find end of current line
        if let Some(newline_pos) = self.text[self.cursor..].find('\n') {
            self.cursor += newline_pos;
        } else {
            self.cursor = self.text.len();
        }
    }

    /// Move cursor left by one word.
    pub fn move_word_left(&mut self) {
        // Skip whitespace, then skip non-whitespace
        let mut pos = self.cursor;
        let bytes = self.text.as_bytes();

        // Skip trailing whitespace
        while pos > 0 && bytes[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        // Skip word chars
        while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        self.cursor = pos;
    }

    /// Move cursor right by one word.
    pub fn move_word_right(&mut self) {
        let mut pos = self.cursor;
        let bytes = self.text.as_bytes();
        let len = bytes.len();

        // Skip current word chars
        while pos < len && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        // Skip whitespace
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        self.cursor = pos;
    }

    /// Render as styled lines for display.
    pub fn to_lines(&self, _width: u16) -> Vec<Line<'static>> {
        if self.text.is_empty() {
            return vec![Line::from(vec![Span::styled(
                "crab> ",
                Style::default().fg(Color::Blue),
            )])];
        }

        // Split into lines, preserving newlines in the text
        let lines: Vec<&str> = self.text.lines().collect();
        if lines.is_empty() {
            return vec![Line::from(vec![Span::styled(
                "crab> ",
                Style::default().fg(Color::Blue),
            )])];
        }

        let mut result = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let prefix = if i == 0 { "crab> " } else { "      " };
            result.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Blue)),
                Span::raw(line.to_string()),
            ]));
        }
        result
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');
        assert_eq!(buf.text(), "abc");
        assert_eq!(buf.cursor(), 3);

        buf.backspace();
        assert_eq!(buf.text(), "ab");
        assert_eq!(buf.cursor(), 2);
    }

    #[test]
    fn cursor_movement() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');

        buf.move_left();
        assert_eq!(buf.cursor(), 2);

        buf.move_left();
        assert_eq!(buf.cursor(), 1);

        buf.move_right();
        assert_eq!(buf.cursor(), 2);
    }

    #[test]
    fn delete_key() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');

        buf.move_left(); // cursor before 'c'
        buf.delete(); // delete 'c'
        assert_eq!(buf.text(), "ab");
    }

    #[test]
    fn word_movement() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello world test");

        buf.move_word_left();
        assert_eq!(buf.cursor(), 12); // start of "test"

        buf.move_word_left();
        assert_eq!(buf.cursor(), 6); // start of "world"

        buf.move_word_right();
        assert_eq!(buf.cursor(), 12); // start of "test" (next word)
    }

    #[test]
    fn home_and_end() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');

        buf.move_home();
        assert_eq!(buf.cursor(), 0);

        buf.move_end();
        assert_eq!(buf.cursor(), 3);
    }

    #[test]
    fn multi_line_home() {
        let mut buf = InputBuffer::new();
        buf.insert_str("line1\nline2\nline3");
        // Cursor at end (position 17)

        buf.move_home(); // Should go to start of "line3"
        assert_eq!(buf.cursor(), 12);

        buf.move_home(); // Should go to start of line3 (no change since we're at start of this line)
        assert_eq!(buf.cursor(), 12);

        // Move to before the newline to test moving to line start
        buf.cursor = 11; // Before '\n' before "line3"
        buf.move_home(); // Should go to start of "line2"
        assert_eq!(buf.cursor(), 6);
    }

    // Helper trait for testing
    trait InsertStr {
        fn insert_str(&mut self, s: &str);
    }

    impl InsertStr for InputBuffer {
        fn insert_str(&mut self, s: &str) {
            for c in s.chars() {
                self.insert_char(c);
            }
        }
    }
}
