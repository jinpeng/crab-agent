//! TUI state model - pure update logic, testable without a terminal.

use crab_core::agent::AgentEvent;
use crab_core::gate::{Decision, Prompt};
use crab_core::types::ToolCallId;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use std::collections::HashMap;

use super::input::InputBuffer;
use crate::status::{RunState, StatusLine};

/// Pending confirmation request from a tool.
#[derive(Debug)]
pub struct PendingConfirm {
    pub prompt: Prompt,
    pub reply: std::sync::mpsc::Sender<Decision>,
}

/// A single bubble in the transcript.
#[derive(Debug, Clone)]
pub enum Bubble {
    /// User message.
    User(String),
    /// Assistant text (streaming or complete).
    Assistant { text: String, complete: bool },
    /// Tool call with optional result.
    Tool {
        id: ToolCallId,
        name: String,
        args: String,
        result: Option<ToolResult>,
    },
    /// System notice (e.g., "[aborted]").
    System(String),
}

/// Result of a tool call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    /// Number of lines truncated (0 = none).
    pub truncated: usize,
}

/// Style constants for TUI rendering.
pub struct Theme;

impl Theme {
    pub fn user() -> Style {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    }

    pub fn assistant() -> Style {
        Style::default().fg(Color::Reset)
    }

    pub fn tool_name() -> Style {
        Style::default().fg(Color::Cyan)
    }

    pub fn success() -> Style {
        Style::default().fg(Color::Green)
    }

    pub fn error() -> Style {
        Style::default().fg(Color::Red)
    }

    pub fn dim() -> Style {
        Style::default().fg(Color::DarkGray)
    }

    pub fn status() -> Style {
        Style::default().fg(Color::DarkGray)
    }
}

/// Scroll state for the transcript.
///
/// `offset` counts rendered (wrapped) lines hidden *below* the viewport, so 0 means
/// "showing the latest output". The `last_*` fields are written by the view on every
/// draw; they let the state clamp scrolling and keep the view anchored.
#[derive(Debug, Clone, Default)]
pub struct ScrollState {
    /// Number of lines scrolled up from the bottom (0 = stick to bottom).
    pub offset: usize,
    /// Whether to auto-follow new content (true iff `offset == 0`).
    pub follow: bool,
    /// Total rendered line count at the last draw.
    pub last_total: usize,
    /// Text width at the last draw (a resize invalidates `last_total`).
    pub last_width: u16,
    /// Transcript viewport height at the last draw.
    pub viewport: usize,
}

impl ScrollState {
    pub fn new() -> Self {
        Self {
            follow: true,
            ..Self::default()
        }
    }

    /// Largest meaningful offset: the whole transcript scrolled past the viewport.
    fn max_offset(&self) -> usize {
        self.last_total.saturating_sub(self.viewport)
    }

    /// Lines to move for PageUp/PageDown: a screenful minus one line of context.
    pub fn page(&self) -> usize {
        self.viewport.saturating_sub(1).max(1)
    }

    /// Scroll up by n lines, stopping at the oldest line.
    pub fn scroll_up(&mut self, n: usize) {
        self.offset = self.offset.saturating_add(n).min(self.max_offset());
        self.follow = self.offset == 0;
    }

    /// Scroll down by n lines. Re-enables follow when reaching the bottom.
    pub fn scroll_down(&mut self, n: usize) {
        self.offset = self.offset.saturating_sub(n);
        self.follow = self.offset == 0;
    }

    /// Jump to the oldest line.
    pub fn scroll_to_top(&mut self) {
        self.offset = self.max_offset();
        self.follow = self.offset == 0;
    }

    /// Jump to the latest output and re-enable follow.
    pub fn scroll_to_bottom(&mut self) {
        self.offset = 0;
        self.follow = true;
    }
}

/// Model picker overlay state (the `/model` menu).
#[derive(Debug, Clone)]
pub struct ModelPicker {
    /// Available provider profile names.
    pub items: Vec<String>,
    /// Currently highlighted index.
    pub selected: usize,
}

impl ModelPicker {
    pub fn new(items: Vec<String>, current: usize) -> Self {
        let selected = current.min(items.len().saturating_sub(1));
        Self { items, selected }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.items.get(self.selected).map(String::as_str)
    }
}

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq)]
pub enum SlashCommand {
    Quit,
    Yolo,
    /// Open the model picker (`/model`) or switch directly (`/model <name>`).
    Model(Option<String>),
    /// Open the interactive model picker (`/models`).
    PickModel,
    /// Load a skill by name: `/skill:tdd [args]`.
    Skill(String, Option<String>),
    /// Unrecognized `/something`.
    Unknown(String),
}

/// Parse a slash command line (leading `/` already stripped of surrounding space).
/// Pure and total: never panics, unknown commands fall through to `Unknown`.
pub fn parse_slash(text: &str) -> Option<SlashCommand> {
    let t = text.trim();
    let rest = t.strip_prefix('/')?;
    let (cmd, arg) = match rest.split_once(|c: char| c.is_whitespace()) {
        Some((c, a)) => (c, Some(a.trim().to_string()).filter(|s| !s.is_empty())),
        None => (rest, None),
    };
    Some(match cmd {
        "quit" | "exit" | "q" => SlashCommand::Quit,
        "yolo" => SlashCommand::Yolo,
        "model" | "m" => SlashCommand::Model(arg),
        "models" => SlashCommand::PickModel,
        other if other.starts_with("skill:") => {
            let name = other["skill:".len()..].trim().to_string();
            SlashCommand::Skill(name, arg)
        }
        other => SlashCommand::Unknown(other.to_string()),
    })
}

/// Build the user message that injects a skill's instructions. `args` (may be
/// empty) is appended as the user request after the skill block.
pub fn skill_message(skill: &crab_core::skills::Skill, args: &str) -> String {
    let content = std::fs::read_to_string(&skill.skill_md).unwrap_or_else(|e| {
        format!("(failed to read {}: {e})", skill.skill_md.display())
    });
    let mut msg = format!(
        "<Skill instructions: {} ({})>\n{}\n</Skill instructions>",
        skill.name,
        skill.skill_md.display(),
        content
    );
    if !args.is_empty() {
        msg.push_str("\n\n");
        msg.push_str(args);
    }
    msg
}

/// Full TUI state.
pub struct TuiState {
    /// Transcript bubbles.
    pub bubbles: Vec<Bubble>,
    /// Input buffer.
    pub input: InputBuffer,
    /// Scroll state.
    pub scroll: ScrollState,
    /// Status line.
    pub status: StatusLine,
    /// Run state (idle/streaming/tool).
    pub run_state: RunState,
    /// When the current run started (drives the spinner + elapsed timer).
    pub run_started: Option<std::time::Instant>,
    /// Pending confirmation, if any.
    pub confirm: Option<PendingConfirm>,
    /// Should quit flag.
    pub should_quit: bool,
    /// Model picker overlay, when open.
    pub picker: Option<ModelPicker>,
    /// Yolo (allow-all) flag, mirrored from the permission gate for the status line.
    pub yolo: bool,
    /// Current provider label for the picker (e.g. "openai/gpt-4o-mini").
    pub provider_label: String,
    /// Cached tool args from AssistantMessage (for showing in tool bubbles).
    tool_args_cache: HashMap<ToolCallId, String>,
}

impl TuiState {
    pub fn new(
        provider_model: String,
        cwd: std::path::PathBuf,
        context_window: Option<u64>,
    ) -> Self {
        let mut status = StatusLine::new(provider_model.clone(), cwd);
        status.set_context(None, context_window);
        Self {
            bubbles: Vec::new(),
            input: InputBuffer::new(),
            scroll: ScrollState::new(),
            status,
            run_state: RunState::Idle,
            run_started: None,
            confirm: None,
            should_quit: false,
            picker: None,
            yolo: false,
            provider_label: provider_model.clone(),
            tool_args_cache: HashMap::new(),
        }
    }

    /// Show a system notice in the transcript.
    pub fn notice(&mut self, msg: impl Into<String>) {
        self.bubbles.push(Bubble::System(msg.into()));
        if self.scroll.follow {
            self.scroll.offset = 0;
        }
    }

    /// Update the provider label shown in the status line (after `/model`).
    pub fn set_provider_label(&mut self, label: String) {
        self.provider_label = label;
    }

    /// Is the agent currently running?
    pub fn is_running(&self) -> bool {
        !matches!(self.run_state, RunState::Idle)
    }

    /// Update the run state and keep the status line in sync.
    fn set_run_state(&mut self, state: RunState) {
        // Start the clock on the idle → busy edge; stop it when back to idle.
        match (self.run_state, state) {
            (RunState::Idle, s) if s != RunState::Idle => {
                self.run_started = Some(std::time::Instant::now());
            }
            (_, RunState::Idle) => self.run_started = None,
            _ => {}
        }
        self.run_state = state;
        self.status.set_state(state);
    }

    /// How long the current run has been going, if any.
    pub fn run_elapsed(&self) -> Option<std::time::Duration> {
        self.run_started.map(|t| t.elapsed())
    }

    /// Record a submitted user message and mark the agent as busy right away,
    /// so the input stays locked until the first agent event arrives.
    pub fn push_user(&mut self, text: &str) {
        self.bubbles.push(Bubble::User(text.to_string()));
        self.scroll.scroll_to_bottom();
        self.set_run_state(RunState::Streaming);
    }

    /// Show a permission prompt and mark the agent as blocked on it.
    pub fn begin_confirm(&mut self, prompt: Prompt, reply: std::sync::mpsc::Sender<Decision>) {
        self.confirm = Some(PendingConfirm { prompt, reply });
        self.set_run_state(RunState::WaitingConfirm);
    }

    /// Answer the pending permission prompt, if any, and resume.
    pub fn answer_confirm(&mut self, decision: Decision) {
        if let Some(pending) = self.confirm.take() {
            let _ = pending.reply.send(decision);
            self.set_run_state(RunState::WaitingTool);
        }
    }

    /// Show a failed run as a system notice and unlock the input.
    pub fn show_error(&mut self, msg: String) {
        self.bubbles.push(Bubble::System(format!("[error] {msg}")));
        self.scroll.scroll_to_bottom();
        self.set_run_state(RunState::Idle);
    }

    /// Apply an agent event to update state.
    pub fn apply_agent_event(&mut self, ev: AgentEvent) {
        use AgentEvent::*;

        match ev {
            TurnStarted { .. } => {
                self.set_run_state(RunState::Streaming);
            }
            TextDelta(s) => {
                self.set_run_state(RunState::Streaming);
                // Append to current assistant bubble or create new one
                if let Some(last) = self.bubbles.last_mut()
                    && let Bubble::Assistant { text, complete } = last
                    && !*complete
                {
                    text.push_str(&s);
                    return;
                }
                // No open assistant bubble, create new
                self.bubbles.push(Bubble::Assistant {
                    text: s,
                    complete: false,
                });
                if self.scroll.follow {
                    self.scroll.offset = 0;
                }
            }
            Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                let used = (prompt_tokens + completion_tokens) as u64;
                self.status.set_tokens_used(used);
                self.status
                    .set_context(Some(used), self.status.context_window());
            }
            AssistantMessage(msg) => {
                // Cache tool args and mark assistant bubble as complete
                for block in &msg.content {
                    if let crab_core::types::ContentBlock::ToolUse { id, name, input } = block {
                        let args_str = compact_json(input);
                        self.tool_args_cache
                            .insert(id.clone(), format!("{name} {args_str}"));
                    }
                }
                // Mark current assistant bubble as complete
                if let Some(last) = self.bubbles.last_mut()
                    && let Bubble::Assistant { complete, .. } = last
                {
                    *complete = true;
                }
            }
            ToolCallStarted { id, name } => {
                self.set_run_state(RunState::WaitingTool);
                let args = self.tool_args_cache.remove(&id).unwrap_or_default();
                self.bubbles.push(Bubble::Tool {
                    id,
                    name,
                    args,
                    result: None,
                });
                if self.scroll.follow {
                    self.scroll.offset = 0;
                }
            }
            ToolCallFinished {
                id,
                result,
                is_error,
            } => {
                self.set_run_state(RunState::Streaming);
                // Find the tool bubble and set result
                for bubble in self.bubbles.iter_mut().rev() {
                    if let Bubble::Tool {
                        id: bid, result: r, ..
                    } = bubble
                        && *bid == id
                    {
                        let (text, truncated) = truncate_result(&result);
                        *r = Some(ToolResult {
                            text,
                            is_error,
                            truncated,
                        });
                        break;
                    }
                }
            }
            Finished { reason } => {
                self.set_run_state(RunState::Idle);
                if reason == crab_core::types::StopReason::Aborted {
                    self.bubbles.push(Bubble::System("[aborted]".into()));
                    if self.scroll.follow {
                        self.scroll.offset = 0;
                    }
                }
            }
        }
    }

    /// Scroll transcript up by n lines.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll.scroll_up(n);
    }

    /// Scroll transcript down by n lines.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll.scroll_down(n);
    }

    /// Scroll to top of transcript (offset is clamped at render time).
    pub fn scroll_to_top(&mut self) {
        self.scroll.scroll_to_top();
    }

    /// Scroll to bottom of transcript.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll.scroll_to_bottom();
    }
}

/// Compact JSON value for display in tool args.
fn compact_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let pairs: Vec<String> = m
                .iter()
                .take(3)
                .map(|(k, v)| format!("{}={}", k, compact_json_value(v)))
                .collect();
            let s = pairs.join(" ");
            if m.len() > 3 { format!("{} ...", s) } else { s }
        }
        _ => v.to_string(),
    }
}

fn compact_json_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => {
            if s.len() > 30 {
                // Find a valid char boundary near position 27
                let end = s.char_indices().take_while(|(i, _)| *i < 27).last();
                let truncate_at = end.map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
                format!("\"{}...\"", &s[..truncate_at])
            } else {
                format!("\"{}\"", s)
            }
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".into(),
        _ => v.to_string(),
    }
}

/// Truncate tool result to reasonable size.
fn truncate_result(s: &str) -> (String, usize) {
    const MAX_LINES: usize = 20;
    let lines: Vec<&str> = s.lines().collect();
    let total = lines.len();
    if total <= MAX_LINES {
        (s.to_string(), 0)
    } else {
        let kept: Vec<&str> = lines.iter().take(MAX_LINES).copied().collect();
        (kept.join("\n"), total - MAX_LINES)
    }
}

impl Bubble {
    /// Render bubble as styled text for the transcript.
    pub fn to_text(&self, _width: usize) -> Text<'static> {
        match self {
            Bubble::User(text) => {
                let lines: Vec<Line> = text
                    .lines()
                    .map(|line| {
                        Line::from(vec![
                            Span::styled("you: ", Theme::user()),
                            Span::raw(line.to_string()),
                        ])
                    })
                    .collect();
                Text::from(lines)
            }
            Bubble::Assistant { text, .. } => {
                let lines: Vec<Line> = text
                    .lines()
                    .map(|line| Line::styled(line.to_string(), Theme::assistant()))
                    .collect();
                Text::from(lines)
            }
            Bubble::Tool {
                name, args, result, ..
            } => {
                let mut lines = vec![Line::from(vec![
                    Span::styled("● ", Theme::tool_name()),
                    Span::styled(name.clone(), Theme::tool_name()),
                    Span::styled(" ", Style::default()),
                    Span::styled(args.clone(), Theme::dim()),
                ])];

                if let Some(r) = result {
                    let marker = if r.is_error { "✗" } else { "✓" };
                    let style = if r.is_error {
                        Theme::error()
                    } else {
                        Theme::success()
                    };
                    let result_text = if r.truncated > 0 {
                        format!("{} (+ {} more lines)", r.text, r.truncated)
                    } else {
                        r.text.clone()
                    };
                    for (i, line) in result_text.lines().take(5).enumerate() {
                        // Marker on the first line only; continuations align under it.
                        let prefix = if i == 0 {
                            format!("  {marker} ")
                        } else {
                            "    ".to_string()
                        };
                        lines.push(Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(line.to_string(), Theme::dim()),
                        ]));
                    }
                }
                Text::from(lines)
            }
            Bubble::System(text) => Text::from(Line::from(vec![Span::styled(
                text.clone(),
                Style::default().fg(Color::Yellow),
            )])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_appends_to_open_assistant_bubble() {
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None);

        state.apply_agent_event(AgentEvent::TextDelta("hello".into()));
        state.apply_agent_event(AgentEvent::TextDelta(" world".into()));

        assert_eq!(state.bubbles.len(), 1);
        if let Bubble::Assistant { text, complete } = &state.bubbles[0] {
            assert_eq!(text, "hello world");
            assert!(!complete);
        } else {
            panic!("expected assistant bubble");
        }
    }

    #[test]
    fn assistant_message_closes_bubble() {
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None);

        state.apply_agent_event(AgentEvent::TextDelta("hi".into()));
        state.apply_agent_event(AgentEvent::AssistantMessage(crab_core::types::Message {
            role: crab_core::types::Role::Assistant,
            content: vec![crab_core::types::ContentBlock::Text { text: "hi".into() }],
        }));

        if let Bubble::Assistant { complete, .. } = &state.bubbles[0] {
            assert!(complete);
        } else {
            panic!("expected assistant bubble");
        }
    }

    #[test]
    fn tool_call_shows_args_from_cache() {
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None);

        // First, the assistant message arrives with tool use
        state.apply_agent_event(AgentEvent::AssistantMessage(crab_core::types::Message {
            role: crab_core::types::Role::Assistant,
            content: vec![crab_core::types::ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "main.rs"}),
            }],
        }));

        // Then ToolCallStarted
        state.apply_agent_event(AgentEvent::ToolCallStarted {
            id: "t1".into(),
            name: "read".into(),
        });

        assert_eq!(state.bubbles.len(), 1);
        if let Bubble::Tool { name, args, .. } = &state.bubbles[0] {
            assert_eq!(name, "read");
            assert!(args.contains("path"));
        } else {
            panic!("expected tool bubble");
        }
    }

    #[test]
    fn tool_finished_shows_result() {
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None);

        state.apply_agent_event(AgentEvent::ToolCallStarted {
            id: "t1".into(),
            name: "bash".into(),
        });
        state.apply_agent_event(AgentEvent::ToolCallFinished {
            id: "t1".into(),
            result: "hello\nworld".into(),
            is_error: false,
        });

        if let Bubble::Tool { result, .. } = &state.bubbles[0] {
            let r = result.as_ref().unwrap();
            assert!(!r.is_error);
            assert_eq!(r.text, "hello\nworld");
        } else {
            panic!("expected tool bubble");
        }
    }

    #[test]
    fn aborted_adds_system_notice() {
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None);

        state.apply_agent_event(AgentEvent::Finished {
            reason: crab_core::types::StopReason::Aborted,
        });

        assert_eq!(state.run_state, RunState::Idle);
        if let Bubble::System(text) = &state.bubbles[0] {
            assert!(text.contains("aborted"));
        } else {
            panic!("expected system bubble");
        }
    }

    #[test]
    fn scroll_state_follows_by_default() {
        let scroll = ScrollState::new();
        assert!(scroll.follow);
        assert_eq!(scroll.offset, 0);
    }

    fn scrolled(total: usize, viewport: usize) -> ScrollState {
        let mut scroll = ScrollState::new();
        scroll.last_total = total;
        scroll.viewport = viewport;
        scroll
    }

    #[test]
    fn scroll_up_disables_follow() {
        let mut scroll = scrolled(100, 10);
        scroll.scroll_up(5);
        assert!(!scroll.follow);
        assert_eq!(scroll.offset, 5);
    }

    #[test]
    fn scroll_up_stops_at_the_oldest_line() {
        let mut scroll = scrolled(30, 10);
        scroll.scroll_up(1000);
        assert_eq!(scroll.offset, 20);
        // One step back down moves immediately (no hidden overshoot to burn through).
        scroll.scroll_down(1);
        assert_eq!(scroll.offset, 19);
    }

    #[test]
    fn content_that_fits_cannot_scroll() {
        let mut scroll = scrolled(5, 10);
        scroll.scroll_up(3);
        assert_eq!(scroll.offset, 0);
        assert!(scroll.follow);
    }

    #[test]
    fn scroll_down_to_the_bottom_resumes_follow() {
        let mut scroll = scrolled(100, 10);
        scroll.scroll_up(4);
        scroll.scroll_down(4);
        assert!(scroll.follow);
        assert_eq!(scroll.offset, 0);
    }

    #[test]
    fn scroll_to_top_and_bottom() {
        let mut scroll = scrolled(100, 10);
        scroll.scroll_to_top();
        assert_eq!(scroll.offset, 90);
        assert!(!scroll.follow);
        scroll.scroll_to_bottom();
        assert!(scroll.follow);
        assert_eq!(scroll.offset, 0);
    }

    #[test]
    fn page_is_a_screenful_minus_one() {
        assert_eq!(scrolled(100, 10).page(), 9);
        assert_eq!(scrolled(100, 0).page(), 1);
    }

    #[test]
    fn parse_slash_recognizes_commands() {
        assert_eq!(parse_slash("/quit"), Some(SlashCommand::Quit));
        assert_eq!(parse_slash("/exit"), Some(SlashCommand::Quit));
        assert_eq!(parse_slash("/q"), Some(SlashCommand::Quit));
        assert_eq!(parse_slash("/yolo"), Some(SlashCommand::Yolo));
        assert_eq!(parse_slash("/model"), Some(SlashCommand::Model(None)));
        assert_eq!(
            parse_slash("/model deepseek"),
            Some(SlashCommand::Model(Some("deepseek".into())))
        );
        assert_eq!(
            parse_slash("/m gpt-4o"),
            Some(SlashCommand::Model(Some("gpt-4o".into())))
        );
        assert_eq!(parse_slash("/models"), Some(SlashCommand::PickModel));
        assert_eq!(
            parse_slash("/frobnicate"),
            Some(SlashCommand::Unknown("frobnicate".into()))
        );
        // Not a slash command
        assert_eq!(parse_slash("hello"), None);
        assert_eq!(parse_slash(""), None);
        // Leading whitespace tolerated
        assert_eq!(parse_slash("  /yolo  "), Some(SlashCommand::Yolo));
    }

    #[test]
    fn parse_skill_command() {
        assert_eq!(
            parse_slash("/skill:tdd"),
            Some(SlashCommand::Skill("tdd".into(), None))
        );
        assert_eq!(
            parse_slash("/skill:tdd fix the parser"),
            Some(SlashCommand::Skill("tdd".into(), Some("fix the parser".into())))
        );
        assert_eq!(
            parse_slash("/skill:"),
            Some(SlashCommand::Skill(String::new(), None))
        );
    }

    #[test]
    fn skill_message_wraps_content_and_appends_args() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("SKILL.md");
        std::fs::write(&skill_md, "# TDD\nRed-green-refactor.").unwrap();
        let skill = crab_core::skills::Skill {
            name: "tdd".into(),
            description: "d".into(),
            dir: dir.path().to_path_buf(),
            skill_md: skill_md.clone(),
        };
        let msg = skill_message(&skill, "red-green the parser");
        assert!(msg.contains("<Skill instructions: tdd ("));
        assert!(msg.contains(")>\n# TDD\nRed-green-refactor.\n</Skill instructions>"));
        assert!(msg.ends_with("red-green the parser"));
        let no_args = skill_message(&skill, "");
        assert!(no_args.ends_with("</Skill instructions>"));
    }

    #[test]
    fn model_picker_navigation() {
        let items = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut p = ModelPicker::new(items, 0);
        assert_eq!(p.selected_name(), Some("a"));
        p.move_down();
        assert_eq!(p.selected_name(), Some("b"));
        p.move_down();
        p.move_down(); // clamps at end
        assert_eq!(p.selected_name(), Some("c"));
        p.move_up();
        assert_eq!(p.selected_name(), Some("b"));
    }

    #[test]
    fn model_picker_selected_clamped_to_nonempty() {
        let items = vec!["only".to_string()];
        let p = ModelPicker::new(items, 99);
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_name(), Some("only"));
    }
}
