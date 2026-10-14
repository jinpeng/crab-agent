use crab_core::gate::{Confirmer, Decision, Prompt};
use std::io::Write;
use std::sync::Arc;

pub fn parse_answer(s: &str) -> Option<Decision> {
    match s.trim().to_lowercase().as_str() {
        "y" | "yes" => Some(Decision::Allow),
        "n" | "no" => Some(Decision::Deny),
        "a" | "all" => Some(Decision::AllowAll),
        _ => None,
    }
}

/// Terminal confirmer for interactive mode. Synchronous: when a tool call needs
/// confirmation the agent is mid-run and reedline is idle, so a blocking stdin read
/// is fine here.
pub fn terminal_confirmer() -> Confirmer {
    Arc::new(|p: Prompt| {
        loop {
            print!(
                "\n\u{26a0} {} \u{2014} {} \nallow? [y]es/[n]o/[a]ll: ",
                p.tool, p.summary
            );
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return Decision::Deny;
            }
            if let Some(d) = parse_answer(&line) {
                return d;
            }
            println!("please answer y, n, or a");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_core::gate::Decision;

    #[test]
    fn parses_confirmation_answer() {
        assert_eq!(parse_answer("y"), Some(Decision::Allow));
        assert_eq!(parse_answer("n"), Some(Decision::Deny));
        assert_eq!(parse_answer("a"), Some(Decision::AllowAll));
        assert_eq!(parse_answer("yes"), Some(Decision::Allow));
        assert_eq!(parse_answer("garbage"), None);
    }
}
