//! Multi-line input buffer with cursor tracking and width-aware wrapping.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::select::wrap_styled;
use crate::width::display_width;

/// Prompt prefix on the first input row (pi-style `crab> `).
const PROMPT: &str = "crab> ";
/// Indent aligning wrapped continuation rows under the prompt text.
const CONTINUATION: &str = "      ";
/// Display width of the prompt/continuation prefix.
const PREFIX_W: usize = 6;

/// The input box rendered for a given width: wrapped rows plus the cursor cell.
#[derive(Debug)]
pub struct InputLayout {
    /// Wrapped rows, prefix included on every row (first `crab> `, continuations indented).
    pub lines: Vec<Line<'static>>,
    /// Wrapped row containing the cursor.
    pub cursor_row: usize,
    /// Cursor column within the row, display cells (prefix included, so ≥ 6).
    pub cursor_col: usize,
}

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

    /// Insert a character at cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a string at cursor position, leaving the cursor after it.
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
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

    /// Wrap the buffer to the input box width, tracking the cursor cell.
    ///
    /// `width` is the full box width including borders. Text wraps inside
    /// them, under the 6-column prompt prefix; wrapped continuation rows are
    /// indented to match, so every row is `prefix + segment`. `cursor_row` /
    /// `cursor_col` locate the cursor in display cells (CJK chars count 2),
    /// so the view can place the terminal cursor without re-deriving the wrap.
    ///
    /// Logical lines come from `split('\n')`, not `lines()`: a trailing
    /// newline then still yields a row, and every cursor position maps to a
    /// visible cell.
    pub fn layout(&self, width: u16) -> InputLayout {
        self.layout_with(width, Color::Blue)
    }

    /// Same as [`layout`](Self::layout), with the prompt-prefix color chosen by
    /// the caller: the box turns green in shell mode (input starting with `!`).
    pub fn layout_with(&self, width: u16, prompt_color: Color) -> InputLayout {
        let inner = width.saturating_sub(2) as usize; // borders
        let text_w = inner.saturating_sub(PREFIX_W).max(1);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut cursor_row = 0;
        let mut cursor_col = PREFIX_W;

        let mut row_offset = 0usize;
        let mut line_start = 0usize;
        for (i, logical) in self.text.split('\n').enumerate() {
            let line_prefix = if i == 0 { PROMPT } else { CONTINUATION };
            let prefix_style = Style::default().fg(prompt_color);
            let wrapped = wrap_styled(&[Line::raw(logical.to_string())], text_w);
            for (j, row) in wrapped.iter().enumerate() {
                // 折行续行用缩进对齐提示符文本，只有框内第一行显示 crab>。
                let prefix = if j == 0 { line_prefix } else { CONTINUATION };
                let mut spans = vec![Span::styled(prefix, prefix_style)];
                spans.extend(row.spans.iter().cloned());
                lines.push(Line::from(spans));
            }

            // Does the cursor sit in this logical line (possibly at its end)?
            let line_end = line_start + logical.len();
            if self.cursor >= line_start && self.cursor <= line_end {
                // Wrapping only the text before the cursor locates its row:
                // every partial row before the last is a row the full line
                // occupies before the cursor. (At a wrap point that consumes
                // whitespace the column can land one cell off — the consumed
                // cell is invisible either way.)
                let upto = &logical[..self.cursor - line_start];
                let partial = wrap_styled(&[Line::raw(upto.to_string())], text_w);
                cursor_row = row_offset + partial.len() - 1;
                let last = partial.last().expect("wrap yields at least one row");
                cursor_col = PREFIX_W
                    + last
                        .spans
                        .iter()
                        .map(|s| display_width(&s.content))
                        .sum::<usize>();
            }
            row_offset += wrapped.len();
            line_start = line_end + 1; // skip the '\n'
        }

        InputLayout {
            lines,
            cursor_row,
            cursor_col,
        }
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
    fn insert_str_inserts_at_cursor() {
        let mut b = InputBuffer::new();
        b.insert_str("foo");
        assert_eq!(b.text(), "foo");
        assert_eq!(b.cursor, 3);
        b.move_home();
        b.insert_str("/skill:tdd ");
        assert_eq!(b.text(), "/skill:tdd foo");
        assert_eq!(b.cursor, "/skill:tdd ".len());
    }

    #[test]
    fn insert_and_backspace() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');
        assert_eq!(buf.text(), "abc");
        assert_eq!(buf.cursor, 3);

        buf.backspace();
        assert_eq!(buf.text(), "ab");
        assert_eq!(buf.cursor, 2);
    }

    #[test]
    fn cursor_movement() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');

        buf.move_left();
        assert_eq!(buf.cursor, 2);

        buf.move_left();
        assert_eq!(buf.cursor, 1);

        buf.move_right();
        assert_eq!(buf.cursor, 2);
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
        assert_eq!(buf.cursor, 12); // start of "test"

        buf.move_word_left();
        assert_eq!(buf.cursor, 6); // start of "world"

        buf.move_word_right();
        assert_eq!(buf.cursor, 12); // start of "test" (next word)
    }

    #[test]
    fn home_and_end() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('c');

        buf.move_home();
        assert_eq!(buf.cursor, 0);

        buf.move_end();
        assert_eq!(buf.cursor, 3);
    }

    #[test]
    fn multi_line_home() {
        let mut buf = InputBuffer::new();
        buf.insert_str("line1\nline2\nline3");
        // Cursor at end (position 17)

        buf.move_home(); // Should go to start of "line3"
        assert_eq!(buf.cursor, 12);

        buf.move_home(); // Should go to start of line3 (no change since we're at start of this line)
        assert_eq!(buf.cursor, 12);

        // Move to before the newline to test moving to line start
        buf.cursor = 11; // Before '\n' before "line3"
        buf.move_home(); // Should go to start of "line2"
        assert_eq!(buf.cursor, 6);
    }

    /// 空缓冲也渲染出一行提示符，光标在提示符后。
    #[test]
    fn layout_empty_buffer_shows_prompt() {
        let buf = InputBuffer::new();
        let layout = buf.layout(80);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_col, PREFIX_W);
    }

    /// 长行按词折行：续行缩进与提示符对齐，行宽不超过文本区宽度。
    #[test]
    fn layout_wraps_long_lines_with_aligned_continuation() {
        let mut buf = InputBuffer::new();
        buf.insert_str("word word word word word word word");
        // width 26 → inner 24 → text width 18.
        let layout = buf.layout(26);
        assert!(layout.lines.len() >= 2, "{}", layout.lines.len());
        for (i, row) in layout.lines.iter().enumerate() {
            let prefix = if i == 0 { PROMPT } else { CONTINUATION };
            assert!(
                row.spans[0].content.starts_with(prefix),
                "row {i} missing prefix"
            );
            let row_w: usize = row.spans.iter().map(|s| display_width(&s.content)).sum();
            assert!(row_w <= 24, "row {i} too wide: {row_w}");
        }
        // 折行点吞掉的空白不可见：去掉空白后字符一个不少。
        let text: String = layout
            .lines
            .iter()
            .flat_map(|r| r.spans[1..].iter().map(|s| s.content.to_string()))
            .collect();
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let expected: String = buf.text().chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(squeezed, expected);
    }

    /// 折行后光标落在正确的折行行/列（含 CJK 宽字符按两列计）。
    #[test]
    fn layout_tracks_cursor_through_wrapping() {
        let mut buf = InputBuffer::new();
        buf.insert_str("word word word word");
        buf.move_home(); // cursor 0
        let layout = buf.layout(20); // text width 12 → "word word" + "word word"
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_col, PREFIX_W);

        buf.move_end(); // cursor at end of line
        let layout = buf.layout(20);
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.cursor_col, PREFIX_W + 9);

        // CJK 宽字符：12 列文本区每行放 6 个汉字，18 字共 3 行。
        let mut cjk = InputBuffer::new();
        cjk.insert_str("输入框输入框输入框输入框输入框输入框");
        cjk.move_end();
        let layout = cjk.layout(20);
        assert_eq!(layout.lines.len(), 3, "36 cols / 12 per row");
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(
            layout.cursor_col,
            PREFIX_W + 12,
            "last row is full: 6 hanzi"
        );
    }

    /// 多行输入：每个逻辑行都占行，尾随换行也产生一行（光标可落上去）。
    #[test]
    fn layout_preserves_logical_lines() {
        let mut buf = InputBuffer::new();
        buf.insert_str("one\ntwo\n");
        let layout = buf.layout(80);
        assert_eq!(layout.lines.len(), 3);
        // 光标在末尾（尾随换行产生的空行）。
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(layout.cursor_col, PREFIX_W);
    }
}
