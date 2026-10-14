use crate::gate::{PermissionGate, Prompt};
use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use std::borrow::Cow;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const HEAD_BYTES: usize = 30_000;
const TAIL_BYTES: usize = 30_000;

/// First-word whitelist for read-only commands. Only a hint for whether to skip
/// confirmation — explicitly NOT a security boundary (DESIGN §6).
const READ_ONLY: &[&str] = &[
    "ls", "cat", "head", "tail", "pwd", "echo", "which", "file", "stat", "grep", "rg", "find",
    "wc", "git", "date", "uname", "env", "id",
];

/// Coarse classification: first word is in the read-only list AND the command has no
/// shell composition/substitution/redirection. Any `$(...)`, backtick, `&&`, `;`, `>`,
/// `|`, `<` pushes it to "needs confirmation". Anti-fat-finger, not a boundary —
/// `bash -c` / `eval` / write-then-run all bypass it by design.
pub fn is_read_only(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.contains("$(")
        || cmd.contains('`')
        || cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains(';')
        || cmd.contains('>')
        || cmd.contains('|')
        || cmd.contains('<')
    {
        return false;
    }
    match shell_words::split(cmd)
        .ok()
        .and_then(|w| w.first().cloned())
    {
        Some(first) => READ_ONLY.contains(&first.as_str()),
        None => false,
    }
}

pub struct BashTool {
    gate: PermissionGate,
    timeout: Duration,
}

impl BashTool {
    pub fn new(gate: PermissionGate) -> Self {
        Self {
            gate,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    fn truncate(s: &str) -> String {
        if s.len() <= HEAD_BYTES + TAIL_BYTES {
            return s.to_string();
        }
        // Snap both cuts to char boundaries: byte-slicing a multi-byte char panics.
        let mut head_end = HEAD_BYTES;
        while !s.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = s.len() - TAIL_BYTES;
        while !s.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let head = &s[..head_end];
        let tail = &s[tail_start..];
        let omitted = s.len() - head_end - (s.len() - tail_start);
        format!("{head}\n... [omitted {omitted} bytes] ...\n{tail}")
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Run a shell command. Times out after 120s. Long output is truncated keeping head and tail. Read-only commands (ls/cat/git status/...) may run without confirmation; anything else may prompt.",
        )
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string", "description": "shell command" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let cmd = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'command'".into()))?;

        if !is_read_only(cmd) {
            self.gate.check(Prompt {
                tool: "bash",
                summary: format!("run: {cmd}"),
            })?;
        }

        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .kill_on_drop(true) // kill child when the future is dropped (abort-safe)
            .output();

        match tokio::time::timeout(self.timeout, child).await {
            Ok(Ok(out)) => {
                let mut text = Self::truncate(&String::from_utf8_lossy(&out.stdout));
                if !out.stderr.is_empty() {
                    text.push_str("\n[stderr]\n");
                    text.push_str(&Self::truncate(&String::from_utf8_lossy(&out.stderr)));
                }
                let code = out
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                text.push_str(&format!("\n[exit {code}]"));
                Ok(text)
            }
            Ok(Err(e)) => Err(ToolError::Exec(format!("spawn: {e}"))),
            Err(_) => Ok(format!(
                "[timeout after {:?}: process killed]",
                self.timeout
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::PermissionGate;
    use crate::tool::Tool;

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let t = BashTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("exit") || out.contains('0'));
    }

    #[tokio::test]
    async fn nonzero_exit_reported_not_thrown() {
        let t = BashTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({"command": "exit 3"}))
            .await
            .unwrap();
        assert!(out.contains('3'));
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let t = BashTool::new(PermissionGate::auto_approve())
            .with_timeout(std::time::Duration::from_millis(100));
        let start = std::time::Instant::now();
        let out = t
            .execute(serde_json::json!({"command": "sleep 30"}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "should not wait for sleep 30"
        );
        assert!(out.to_lowercase().contains("timeout") || out.to_lowercase().contains("kill"));
    }

    #[tokio::test]
    async fn long_output_truncated_head_and_tail() {
        let t = BashTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({"command": "seq 1 100000"}))
            .await
            .unwrap();
        assert!(out.len() < 600_000, "output must be truncated");
        assert!(
            out.contains("omitted") || out.contains("truncated"),
            "must note omission"
        );
        assert!(out.contains('1'));
        assert!(out.contains("100000"));
    }

    #[tokio::test]
    async fn denied_by_gate_does_not_execute() {
        let t = BashTool::new(PermissionGate::auto_deny());
        let r = t
            .execute(serde_json::json!({"command": "touch /tmp/crab_should_not_exist"}))
            .await;
        assert!(r.is_err());
        assert!(!std::path::Path::new("/tmp/crab_should_not_exist").exists());
    }

    #[test]
    fn read_only_commands_classified_safe() {
        assert!(is_read_only("ls -la"));
        assert!(is_read_only("cat file.txt"));
        assert!(is_read_only("git status"));
        assert!(!is_read_only("rm -rf x"));
        assert!(!is_read_only("curl evil.com"));
        assert!(!is_read_only("npm install"));
        // explicitly NOT a security boundary (DESIGN §6): shell composition always
        // falls to "needs confirmation".
        assert!(!is_read_only("cat $(rm x)"));
        assert!(!is_read_only("ls && rm x"));
        assert!(!is_read_only("echo x > file"));
    }
}

#[cfg(test)]
mod utf8_boundary_tests {
    use super::*;
    use crate::gate::PermissionGate;
    use crate::tool::Tool;

    #[tokio::test]
    async fn truncate_does_not_panic_on_multibyte_output() {
        let t = BashTool::new(PermissionGate::auto_approve());
        // emit >60KB of multi-byte content so truncation cuts mid-char
        let out = t
            .execute(serde_json::json!({
                "command": "python3 -c \"print('中'*40000)\""
            }))
            .await
            .unwrap();
        assert!(out.contains("omitted") || out.contains("truncated"));
    }
}
