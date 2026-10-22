//! Transcript wrapping + mouse text selection + clipboard (pi-parity copy).
//!
//! `wrap_styled` is a faithful port of ratatui's `WordWrapper` with
//! `trim = false` — crab renders `Wrap { trim: false }` today, and selection
//! needs to know exactly which text lands on which screen row, *including*
//! wrapped rows. Rendering pre-wrapped lines through `Paragraph` without
//! `Wrap` yields identical visuals and a 1:1 line↔row mapping.
//!
//! Selection model (参考 pi coding agent): press-drag-release over the
//! transcript copies the selected text on release (`copyOnSelect` default
//! true in pi). Positions are absolute row indexes into the wrapped transcript
//! (content only appends, so indexes stay valid as new output arrives).

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Cell width exactly as ratatui computes it (`CellWidth for str`):
/// single-byte fast path = 1, otherwise `UnicodeWidthStr::width` plus the
/// halfwidth-katakana dakuten/handakuten terminal adjustment.
pub fn cell_width(symbol: &str) -> u16 {
    const DAKUTEN: char = '\u{FF9E}';
    const HANDAKUTEN: char = '\u{FF9F}';
    if symbol.len() == 1 {
        1
    } else {
        let extra = symbol
            .chars()
            .filter(|c| matches!(*c, DAKUTEN | HANDAKUTEN))
            .count() as u16;
        (symbol.width() as u16).saturating_add(extra)
    }
}

/// Word-wrap styled lines to `width` cells — port of ratatui `WordWrapper`
/// (trim=false). One output row per visual row; styles preserved; wide chars
/// never split across rows; whitespace after a wrap point is consumed.
pub fn wrap_styled(lines: &[Line], width: usize) -> Vec<Line<'static>> {
    let maxw = width.max(1) as u16;
    let mut out = Vec::new();
    for line in lines {
        let before = out.len();
        let graphemes: Vec<(String, u16, Style)> = line
            .spans
            .iter()
            .flat_map(|s| s.styled_graphemes(Style::default()))
            .map(|g| (g.symbol.to_string(), cell_width(g.symbol), g.style))
            .collect();
        wrap_one(&graphemes, maxw, &mut out);
        // Every input line yields at least one row (blank lines stay blank).
        if out.len() == before {
            out.push(Line::raw(""));
        }
    }
    out
}

fn wrap_one(g: &[(String, u16, Style)], maxw: u16, out: &mut Vec<Line>) {
    let mut pending_line: Vec<(String, u16, Style)> = Vec::new();
    let mut pending_word: Vec<(String, u16, Style)> = Vec::new();
    let mut pending_ws: Vec<(String, u16, Style)> = Vec::new();
    let (mut line_w, mut word_w, mut ws_w) = (0u16, 0u16, 0u16);
    let mut non_ws_prev = false;

    for (symbol, w, style) in g {
        let is_ws = symbol == "\u{200b}"
            || (symbol.chars().all(char::is_whitespace) && symbol != "\u{00a0}");
        // Symbols wider than the line are dropped (ratatui behavior).
        if w > &maxw {
            continue;
        }
        let word_found = non_ws_prev && is_ws;
        // trim=false: a pending word+whitespace that overflows on its own flushes.
        let untrimmed_overflow = pending_line.is_empty() && word_w + ws_w + w > maxw;
        if word_found || untrimmed_overflow {
            // trim=false: whitespace always joins the line.
            pending_line.append(&mut pending_ws);
            line_w += ws_w;
            pending_line.append(&mut pending_word);
            line_w += word_w;
            ws_w = 0;
            word_w = 0;
        }

        let line_full = line_w >= maxw;
        let word_overflow = *w > 0 && line_w + ws_w + word_w >= maxw;
        if line_full || word_overflow {
            let mut remaining = maxw.saturating_sub(line_w);
            push_row(out, std::mem::take(&mut pending_line), maxw);
            line_w = 0;
            // Consume whitespace that still fits on the finished row.
            while let Some((_, ww, _)) = pending_ws.first() {
                if *ww > remaining {
                    break;
                }
                ws_w -= ww;
                remaining -= ww;
                pending_ws.remove(0);
            }
            // Whitespace right after a wrap is dropped.
            if is_ws && pending_ws.is_empty() {
                non_ws_prev = !is_ws;
                continue;
            }
        }

        if is_ws {
            ws_w += w;
            pending_ws.push((symbol.clone(), *w, *style));
        } else {
            word_w += w;
            pending_word.push((symbol.clone(), *w, *style));
        }
        non_ws_prev = !is_ws;
    }

    // Flush the tail (trim=false keeps trailing whitespace).
    pending_line.append(&mut pending_ws);
    pending_line.append(&mut pending_word);
    push_row(out, pending_line, maxw);
}

/// Emit one wrapped row, hard-splitting it if it still exceeds `maxw`.
///
/// ratatui's WordWrapper can hand the renderer rows wider than the area (a
/// word-boundary flush lands past the edge; the renderer then clips the
/// overflow and a character silently disappears off screen — crab inherited
/// this bug from `Paragraph`'s wrap). Pre-wrapping lets us refuse to lose
/// characters: over-wide rows split instead. Everywhere else the row geometry
/// is identical to ratatui's (see `wrap_matches_ratatui_on_a_corpus`).
fn push_row(out: &mut Vec<Line>, g: Vec<(String, u16, Style)>, maxw: u16) {
    if g.is_empty() {
        return;
    }
    let mut row: Vec<(String, u16, Style)> = Vec::new();
    let mut w = 0u16;
    for item in g {
        if w + item.1 > maxw && !row.is_empty() {
            out.push(graphemes_to_line(std::mem::take(&mut row)));
            w = 0;
        }
        w += item.1;
        row.push(item);
    }
    out.push(graphemes_to_line(row));
}

fn graphemes_to_line(g: Vec<(String, u16, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (symbol, _, style) in g {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(&symbol),
            _ => spans.push(Span::styled(symbol, style)),
        }
    }
    Line::from(spans)
}

/// Plain text of a rendered row.
#[cfg(test)]
pub fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// A position in the wrapped transcript: absolute row + display column.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// An in-progress or completed selection. `anchor` is where the press landed;
/// `focus` follows the drag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub anchor: Pos,
    pub focus: Pos,
}

/// What the last draw rendered: the selectable rows (pre-highlight) plus the
/// screen geometry they occupy. Absolute row indexes make positions stable
/// as new output appends; a width change invalidates them (caller clears).
#[derive(Clone)]
pub struct SelectGrid {
    pub rows: Vec<Line<'static>>,
    /// First visible row (scroll applied).
    pub top: usize,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl SelectGrid {
    /// Map a screen cell to a transcript position, or None when outside the
    /// rendered text. Columns past end-of-line clamp to end-of-line.
    pub fn hit(&self, col: u16, row: u16) -> Option<Pos> {
        if col < self.x || col >= self.x + self.width || row < self.y || row >= self.y + self.height
        {
            return None;
        }
        let line = self.top + (row - self.y) as usize;
        let row_line = self.rows.get(line)?;
        Some(Pos {
            line,
            col: clamp_col(row_line, (col - self.x) as usize),
        })
    }
}

impl Selection {
    /// (start, end) with start <= end, ordering by row then column.
    pub fn normalized(&self) -> (Pos, Pos) {
        if self.focus < self.anchor {
            (self.focus, self.anchor)
        } else {
            (self.anchor, self.focus)
        }
    }
}

/// Column range of the selection on one row, if the row is (partially) covered.
fn row_range(sel: &Selection, row: usize, row_width: usize) -> Option<(usize, usize)> {
    let (a, b) = sel.normalized();
    if row < a.line || row > b.line {
        return None;
    }
    let start = if row == a.line {
        a.col.min(row_width)
    } else {
        0
    };
    let end = if row == b.line {
        b.col.min(row_width)
    } else {
        row_width
    };
    (start < end).then_some((start, end))
}

/// Extract the selected text: whole rows between the endpoints, columns
/// sliced on display-cell boundaries, rows joined by `\n`.
pub fn extract(rows: &[Line], sel: &Selection) -> String {
    let mut parts = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some((start, end)) = row_range(sel, i, row_width_cells(row)) else {
            continue;
        };
        let mut part = String::new();
        let mut col = 0usize;
        for (symbol, w, _) in row_graphemes(row) {
            let g_end = col + w as usize;
            // Include graphemes overlapping the selected cell range; a wide
            // char straddling the boundary comes along whole.
            if g_end > start && col < end {
                part.push_str(&symbol);
            }
            col = g_end;
        }
        parts.push(part);
    }
    parts.join("\n")
}

/// Apply `extra` style (reversed) to the selection's column ranges in place.
pub fn highlight(rows: &mut [Line], sel: &Selection, extra: Style) {
    for (i, row) in rows.iter_mut().enumerate() {
        let Some((start, end)) = row_range(sel, i, row_width_cells(row)) else {
            continue;
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        for (symbol, w, style) in row_graphemes(row) {
            let g_end = col + w as usize;
            let inside = g_end > start && col < end;
            let style = if inside { style.patch(extra) } else { style };
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push_str(&symbol),
                _ => spans.push(Span::styled(symbol, style)),
            }
            col = g_end;
        }
        *row = Line::from(spans);
    }
}

/// Clamp a raw click column onto the row: past end-of-line means end-of-line.
pub fn clamp_col(row: &Line, col: usize) -> usize {
    col.min(row_width_cells(row))
}

fn row_width_cells(row: &Line) -> usize {
    row_graphemes(row).iter().map(|(_, w, _)| *w as usize).sum()
}

/// (symbol, cell width, style) per grapheme, mirroring how the row renders.
fn row_graphemes(row: &Line) -> Vec<(String, u16, Style)> {
    row.spans
        .iter()
        .flat_map(|s| s.styled_graphemes(Style::default()))
        .map(|g| (g.symbol.to_string(), cell_width(g.symbol), g.style))
        .collect()
}

// ---------------------------------------------------------------- clipboard

/// pi's OSC52 ceiling: encoded payloads larger than this are rejected.
const MAX_OSC52_ENCODED: usize = 100_000;

/// Copy text to the system clipboard, pi-style: `pbcopy` on macOS (reliable
/// even where OSC52 is unsupported, e.g. Terminal.app), OSC52 elsewhere.
/// Returns the mechanism used for the notice.
pub fn copy_text(text: &str) -> Result<&'static str, String> {
    #[cfg(target_os = "macos")]
    if copy_via_pbcopy(text) {
        return Ok("clipboard");
    }
    match osc52(text) {
        Some(seq) => {
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(seq.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| format!("osc52 write: {e}"))?;
            Ok("clipboard (osc52)")
        }
        None => Err("selection too large for clipboard".into()),
    }
}

#[cfg(target_os = "macos")]
fn copy_via_pbcopy(text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return false;
    };
    let ok = child
        .stdin
        .as_mut()
        .map(|stdin| stdin.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    ok && child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Build the OSC52 escape sequence, or None when the payload exceeds the cap.
fn osc52(text: &str) -> Option<String> {
    let encoded = base64_encode(text.as_bytes());
    (encoded.len() <= MAX_OSC52_ENCODED).then(|| format!("\x1B]52;c;{encoded}\x07"))
}

/// Standard RFC 4648 base64 (no external crate).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn plain(s: &str) -> Line<'static> {
        Line::raw(s.to_string())
    }

    fn row_texts(lines: Vec<Line>) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    /// Parity oracle: render through real ratatui (`Paragraph` + `Wrap { trim: false }`)
    /// and read back the buffer rows. `wrap_styled` must match this exactly —
    /// including ratatui's quirks — because the transcript renders with it today.
    fn ratatui_wrap_rows(text: &str, width: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let height = 64;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let p = ratatui::widgets::Paragraph::new(text)
                    .wrap(ratatui::widgets::Wrap { trim: false });
                f.render_widget(p, f.area());
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut rows: Vec<String> = (0..height)
            .map(|y| {
                let mut s = String::new();
                let mut skip_next = false;
                for x in 0..width {
                    let cell = buf.cell((x, y)).unwrap();
                    // ratatui pads a wide char's continuation cell with a blank;
                    // it is not selectable text.
                    if skip_next {
                        skip_next = false;
                        continue;
                    }
                    s.push_str(cell.symbol());
                    if cell_width(cell.symbol()) == 2 {
                        skip_next = true;
                    }
                }
                s.trim_end().to_string()
            })
            .collect();
        while rows.last().is_some_and(|r| r.is_empty()) {
            rows.pop();
        }
        rows
    }

    fn assert_parity(text: &str, width: u16) {
        // Row-trailing whitespace is invisible on screen (blank cells either
        // way), and the oracle's trim_end erases it — trim both sides.
        let mine: Vec<String> = row_texts(wrap_styled(&[plain(text)], width as usize))
            .iter()
            .map(|s| s.trim_end().to_string())
            .collect();
        let mut mine = mine;
        while mine.last().is_some_and(|r| r.is_empty()) {
            mine.pop();
        }
        let theirs = ratatui_wrap_rows(text, width);
        assert_eq!(mine, theirs, "wrap parity broken for {text:?} @ {width}");
    }

    #[test]
    fn no_row_exceeds_width_and_no_chars_are_lost() {
        // ratatui 的渲染端会把超宽行尾裁掉（字符在屏幕上消失，见
        // push_row 文档）；预换行后这两个不变式必须恒成立。
        let cases = [
            "混合 mixed 文本 text 混排",
            "一二三四五六七八",
            "word word word",
            "a b c d e f g h",
        ];
        for text in cases {
            for width in 3..18 {
                let rows = wrap_styled(&[plain(text)], width);
                assert!(
                    rows.iter()
                        .all(|r| (row_width_cells(r) as u16) <= width as u16),
                    "row wider than {width} for {text:?}: {:?}",
                    row_texts(rows.clone())
                );
                // 非空白字符一个不许丢；空白字符允许落在换行点被吞（ratatui
                // 既有行为：换行点后的空白不进下一行，屏幕上也不可见）。
                let squeezed: String = row_texts(rows)
                    .concat()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let expected: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                assert_eq!(squeezed, expected, "chars lost @ {width} for {text:?}");
            }
        }
    }

    #[test]
    fn wrap_matches_ratatui_on_a_corpus() {
        for (text, width) in [
            ("hello world from crab", 11),
            ("abcdefghij", 4),
            ("aa bb cc", 5),
            ("  indented text here", 12),
            ("trailing space ", 8),
            ("a", 1),
            ("multiple  double   spaces", 10),
            ("word word word", 4),
            ("一二三四五六七八", 5),
            ("x".repeat(40).as_str(), 13),
            ("", 5),
        ] {
            assert_parity(text, width);
        }
    }

    // ---- wrapping ----

    #[test]
    fn wraps_on_word_boundaries() {
        let rows = wrap_styled(&[plain("hello world from crab")], 11);
        assert_eq!(row_texts(rows), vec!["hello world", "from crab"]);
    }

    #[test]
    fn long_word_is_hard_split() {
        let rows = wrap_styled(&[plain("abcdefghij")], 4);
        assert_eq!(row_texts(rows), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn cjk_breaks_between_chars_without_splitting_wide_chars() {
        // 与 ratatui 不同（故意）：ratatui 的末行可超限并被渲染端裁掉字符，
        // crab 预换行保证每行 ≤ width、永不丢字。
        let rows = wrap_styled(&[plain("一二三五六七八")], 5);
        let texts = row_texts(rows);
        assert_eq!(texts, vec!["一二", "三五", "六七", "八"]);
        assert_eq!(texts.concat(), "一二三五六七八");
    }

    #[test]
    fn leading_whitespace_is_kept_on_first_row() {
        // trim=false: 行首空白保留。
        let rows = wrap_styled(&[plain("  indented text here")], 12);
        assert_eq!(row_texts(rows)[0], "  indented");
    }

    #[test]
    fn blank_line_stays_blank() {
        let rows = wrap_styled(&[plain(""), plain("x")], 10);
        assert_eq!(row_texts(rows), vec!["", "x"]);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(wrap_styled(&[], 10).is_empty());
    }

    #[test]
    fn styles_survive_wrapping() {
        let line = Line::from(vec![
            Span::styled("red ", Style::default().fg(ratatui::style::Color::Red)),
            Span::styled(
                "blue words",
                Style::default().fg(ratatui::style::Color::Blue),
            ),
        ]);
        let rows = wrap_styled(&[line], 9);
        assert_eq!(rows.len(), 2);
        // row0 "red blue" 含两色；row1 "words" 只有 blue（跨行处红色已结束）。
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(ratatui::style::Color::Red))
        );
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(ratatui::style::Color::Blue))
        );
        assert!(
            rows[1]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(ratatui::style::Color::Blue))
        );
    }

    #[test]
    fn wrap_matches_ratatui_word_wrapper_shape() {
        // 与 ratatui WordWrapper(trim=false) 的行数结构对齐的关键案例：
        // 词恰好顶到边界时不产生空行；尾随空白不丢。
        let rows = wrap_styled(&[plain("aa bb cc")], 5);
        assert_eq!(row_texts(rows), vec!["aa bb", "cc"]);
    }

    // ---- selection ----

    #[test]
    fn extract_whole_rows_and_column_slice() {
        let rows = wrap_styled(&[plain("hello world"), plain("second row")], 20);
        let sel = Selection {
            anchor: Pos { line: 0, col: 6 },
            focus: Pos { line: 1, col: 6 },
        };
        assert_eq!(extract(&rows, &sel), "world\nsecond");
    }

    #[test]
    fn extract_handles_reversed_anchor() {
        let rows = wrap_styled(&[plain("abcdef")], 20);
        let sel = Selection {
            anchor: Pos { line: 0, col: 4 },
            focus: Pos { line: 0, col: 1 },
        };
        assert_eq!(extract(&rows, &sel), "bcd");
    }

    #[test]
    fn extract_includes_wide_char_straddling_boundary() {
        let rows = wrap_styled(&[plain("ab 汉字 de")], 20);
        // 选区从 col 3 开始（"ab "之后）：宽字符占据 3..5，整体被包含。
        let sel = Selection {
            anchor: Pos { line: 0, col: 3 },
            focus: Pos { line: 0, col: 10 },
        };
        assert_eq!(extract(&rows, &sel), "汉字 de");
    }

    #[test]
    fn highlight_marks_only_the_range() {
        let mut rows = wrap_styled(&[plain("hello world")], 20);
        let sel = Selection {
            anchor: Pos { line: 0, col: 0 },
            focus: Pos { line: 0, col: 5 },
        };
        highlight(
            &mut rows,
            &sel,
            Style::default().add_modifier(Modifier::REVERSED),
        );
        let reversed = |s: &Span| s.style.add_modifier == Modifier::REVERSED;
        let text: String = rows[0]
            .spans
            .iter()
            .map(|s| {
                if reversed(s) {
                    s.content.to_uppercase()
                } else {
                    s.content.to_string()
                }
            })
            .collect();
        assert_eq!(text, "HELLO world");
    }

    #[test]
    fn clamp_col_past_end_of_line() {
        let row = plain("abc");
        assert_eq!(clamp_col(&row, 99), 3);
        assert_eq!(clamp_col(&row, 2), 2);
    }

    // ---- clipboard ----

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_sequence_shape_and_cap() {
        let seq = osc52("hello").unwrap();
        assert!(seq.starts_with("\x1B]52;c;"), "{seq:?}");
        assert!(seq.ends_with('\u{7}'));
        assert!(seq.contains(&base64_encode(b"hello")));
        assert!(osc52(&"x".repeat(200_000)).is_none());
    }

    #[test]
    fn cell_width_dakuten_adjustment() {
        assert_eq!(cell_width("ｶﾞ"), 2);
        assert_eq!(cell_width("a"), 1);
        assert_eq!(cell_width("あ"), 2);
    }
}
