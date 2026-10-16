//! ANSI-aware terminal width handling.
//!
//! Rendering code often needs to know how wide a string actually looks on
//! screen, which is not the same as its byte or `char` count once SGR color
//! codes (`\x1b[36m`, `\x1b[0m`, ...) are embedded in it. Character widths
//! come from a Unicode width table: wide (CJK) chars count as two columns,
//! combining and zero-width chars as zero.

/// Display width of `s` in terminal columns, using a Unicode width table.
///
/// Wide (CJK) chars count 2, combining/zero-width chars count 0, everything
/// else counts 1. Use this for anything that positions the cursor or aligns
/// text; [`visible_width`] is the ANSI-stripping variant for plain strings.
pub fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(s)
}

/// The SGR reset sequence, used to close any styling still open.
const RESET: &str = "\x1b[0m";

/// Number of visible columns in `s`, ignoring ANSI escape sequences.
///
/// Escape sequences start at `'\x1b'` and run through their terminating `'m'`
/// (SGR color codes); they contribute no width. All other chars count by
/// their Unicode display width: wide (CJK) chars 2 columns, combining and
/// zero-width chars 0, everything else 1.
pub fn visible_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the escape sequence: everything up to and including 'm'.
            for e in chars.by_ref() {
                if e == 'm' {
                    break;
                }
            }
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Truncate `s` to at most `max` visible columns.
///
/// ANSI escape sequences are copied through without counting toward the
/// limit, so styling that opened before the cut is still present in the
/// result. A wide char that would straddle the cut is dropped whole (the
/// terminal has no half-cell), so the result may end one column short of
/// `max`. If any escape sequence was seen and the result does not already end
/// with a reset, `"\x1b[0m"` is appended so the caller's styling cannot leak
/// into the rest of the line.
pub fn truncate_to_width(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut width = 0;
    let mut saw_ansi = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            saw_ansi = true;
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e == 'm' {
                    break;
                }
            }
            continue;
        }
        if width + UnicodeWidthChar::width(c).unwrap_or(0) > max {
            break;
        }
        out.push(c);
        width += UnicodeWidthChar::width(c).unwrap_or(0);
    }
    if saw_ansi && !out.ends_with(RESET) {
        out.push_str(RESET);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_strips_ansi() {
        assert_eq!(visible_width("\x1b[36mread\x1b[0m"), 4);
        assert_eq!(visible_width("plain"), 5);
    }

    #[test]
    fn truncate_keeps_ansi_intact() {
        let truncated = truncate_to_width("\x1b[36mhello world\x1b[0m", 5);
        assert_eq!(visible_width(&truncated), 5);
        assert!(
            truncated.contains("\x1b[0m"),
            "reset missing: {truncated:?}"
        );
    }

    #[test]
    fn truncate_short_plain_string_is_noop() {
        assert_eq!(truncate_to_width("plain", 10), "plain");
    }

    #[test]
    fn wide_chars_count_two_columns() {
        assert_eq!(visible_width("输入框"), 6);
        assert_eq!(visible_width("a输b"), 4);
        // Combining accent contributes nothing.
        assert_eq!(visible_width("e\u{301}"), 1);
    }

    #[test]
    fn truncate_drops_wide_char_at_the_boundary() {
        // "a输" is 3 columns; cut at 2 drops the whole 输 — half-cells don't exist.
        assert_eq!(truncate_to_width("a输入", 2), "a");
        assert_eq!(truncate_to_width("a输入", 3), "a输");
        assert_eq!(visible_width(&truncate_to_width("输入框x", 5)), 4);
    }
}
