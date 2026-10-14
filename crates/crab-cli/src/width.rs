//! ANSI-aware terminal width handling.
//!
//! Rendering code often needs to know how wide a string actually looks on
//! screen, which is not the same as its byte or `char` count once SGR color
//! codes (`\x1b[36m`, `\x1b[0m`, ...) are embedded in it. v0 only understands
//! color codes: every non-escape char counts as one column.

/// The SGR reset sequence, used to close any styling still open.
const RESET: &str = "\x1b[0m";

/// Number of visible columns in `s`, ignoring ANSI escape sequences.
///
/// Escape sequences start at `'\x1b'` and run through their terminating `'m'`
/// (SGR color codes); they contribute no width. Every other char counts as
/// width 1.
///
/// TODO: wide (CJK) and zero-width/combining characters are counted as 1
/// column each; a proper Unicode width table is needed once they matter.
pub fn visible_width(s: &str) -> usize {
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
            width += 1;
        }
    }
    width
}

/// Truncate `s` to at most `max` visible columns.
///
/// ANSI escape sequences are copied through without counting toward the
/// limit, so styling that opened before the cut is still present in the
/// result. If any escape sequence was seen and the result does not already end
/// with a reset, `"\x1b[0m"` is appended so the caller's styling cannot leak
/// into the rest of the line.
pub fn truncate_to_width(s: &str, max: usize) -> String {
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
        if width >= max {
            break;
        }
        out.push(c);
        width += 1;
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
}
