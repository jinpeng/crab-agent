#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Edit {
    pub old: String,
    pub new: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EditError {
    #[error("no match for old text. closest: {0}")]
    NoMatch(String),
    #[error("old text matches {count} times (must be unique). first at byte {first_byte}")]
    NotUnique { count: usize, first_byte: usize },
    #[error("edits overlap")]
    Overlap,
}

/// Apply multiple edits to content. Each `old` must match the ORIGINAL text exactly
/// once (literal, no whitespace tolerance) and edits must not overlap. All-or-nothing:
/// any failure yields Err and no partial result (Q14).
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<String, EditError> {
    // 1. Find all match positions per edit; require uniqueness.
    let mut spans: Vec<(usize, usize, &str)> = Vec::new(); // (start, end, new)
    for e in edits {
        let matches: Vec<usize> = content.match_indices(&e.old).map(|(i, _)| i).collect();
        match matches.len() {
            0 => return Err(EditError::NoMatch(closest_hint(content, &e.old))),
            1 => spans.push((matches[0], matches[0] + e.old.len(), &e.new)),
            n => {
                return Err(EditError::NotUnique {
                    count: n,
                    first_byte: matches[0],
                });
            }
        }
    }
    // 2. Sort by start, reject overlaps.
    spans.sort_by_key(|(s, _, _)| *s);
    for w in spans.windows(2) {
        if w[0].1 > w[1].0 {
            return Err(EditError::Overlap);
        }
    }
    // 3. Replace back-to-front so earlier byte offsets stay valid.
    let mut out = content.to_string();
    for (start, end, new) in spans.iter().rev() {
        out.replace_range(*start..*end, new);
    }
    Ok(out)
}

/// `similar` is used ONLY to build an error hint, never to decide where to edit (Q14).
fn closest_hint(content: &str, needle: &str) -> String {
    let mut best = (0.0f32, "");
    for line in content.lines() {
        let r = similar::TextDiff::from_chars(needle, line).ratio();
        if r > best.0 {
            best = (r, line);
        }
    }
    if best.0 > 0.3 {
        format!(
            "nearest line (similarity {:.0}%): `{}`",
            best.0 * 100.0,
            best.1.trim()
        )
    } else {
        "no similar line found".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(pairs: &[(&str, &str)]) -> Vec<Edit> {
        pairs
            .iter()
            .map(|(o, n)| Edit {
                old: o.to_string(),
                new: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn single_unique_match_applies() {
        let r = apply_edits("hello world", &edits(&[("world", "rust")])).unwrap();
        assert_eq!(r, "hello rust");
    }

    #[test]
    fn zero_matches_is_error() {
        let r = apply_edits("hello", &edits(&[("zzz", "x")]));
        assert!(matches!(r, Err(EditError::NoMatch(_))));
    }

    #[test]
    fn non_unique_match_is_error() {
        let r = apply_edits("foo foo foo", &edits(&[("foo", "bar")]));
        assert!(matches!(r, Err(EditError::NotUnique { count: 3, .. })));
    }

    #[test]
    fn multiple_edits_match_against_original() {
        let r = apply_edits("a=1\nb=2\nc=3", &edits(&[("a=1", "a=10"), ("c=3", "c=30")])).unwrap();
        assert_eq!(r, "a=10\nb=2\nc=30");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let r = apply_edits("abcdef", &edits(&[("abc", "X"), ("bcd", "Y")]));
        assert!(matches!(r, Err(EditError::Overlap)));
    }

    #[test]
    fn crlf_preserved_exactly() {
        let r = apply_edits("line1\r\nline2\r\n", &edits(&[("line2", "L2")])).unwrap();
        assert_eq!(r, "line1\r\nL2\r\n");
    }

    #[test]
    fn no_trailing_newline_preserved() {
        let r = apply_edits("no newline at end", &edits(&[("end", "END")])).unwrap();
        assert_eq!(r, "no newline at END");
    }

    #[test]
    fn utf8_multibyte_boundaries() {
        let r = apply_edits("你好，世界。再见", &edits(&[("世界", "Rust")])).unwrap();
        assert_eq!(r, "你好，Rust。再见");
    }

    #[test]
    fn error_includes_closest_candidate_hint() {
        let content = "fn main() {\n    println!(\"hi\");\n}\n";
        let r = apply_edits(
            content,
            &edits(&[("fn main() {\n    println!(\"hello\");\n}", "x")]),
        );
        match r {
            Err(EditError::NoMatch(hint)) => assert!(
                hint.contains("println"),
                "hint should point near closest match"
            ),
            other => panic!("expected NoMatch with hint, got {other:?}"),
        }
    }
}
