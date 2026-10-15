//! Semantic color theme for the terminal UI.
//!
//! A [`Theme`] maps semantic roles (user input, tool name, success, error, ...)
//! onto terminal styles, so call sites never hard-code colors. v0 ships a dark
//! theme only.

use crossterm::style::Stylize;

/// Semantic color tokens for the terminal UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct Theme;

impl Theme {
    /// The default dark theme.
    pub fn dark() -> Self {
        Self
    }

    /// A tool name: cyan.
    pub fn tool_name(&self, s: &str) -> String {
        s.cyan().to_string()
    }

    /// A successful outcome: green.
    pub fn success(&self, s: &str) -> String {
        s.green().to_string()
    }

    /// A failed outcome: red.
    pub fn error(&self, s: &str) -> String {
        s.red().to_string()
    }

    /// Secondary, de-emphasized text: dark grey.
    pub fn dim(&self, s: &str) -> String {
        s.dark_grey().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_keep_text_and_emit_ansi() {
        let t = Theme::dark();
        let cases = [
            ("read", t.tool_name("read")),
            ("done", t.success("done")),
            ("boom", t.error("boom")),
            ("muted", t.dim("muted")),
        ];
        for (text, styled) in cases {
            assert!(styled.contains(text), "{styled:?} must contain {text:?}");
            assert!(
                styled.contains("\x1b["),
                "{styled:?} must contain an ANSI escape"
            );
        }
    }

    #[test]
    fn distinct_roles_have_distinct_codes() {
        let t = Theme::dark();
        assert_ne!(t.tool_name("x"), t.error("x"));
        assert_ne!(t.success("x"), t.error("x"));
        assert_ne!(t.tool_name("x"), t.success("x"));
    }
}
