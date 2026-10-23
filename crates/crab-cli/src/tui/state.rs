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
pub struct ListPicker {
    /// Available display items.
    pub items: Vec<String>,
    /// Currently highlighted index.
    pub selected: usize,
}

impl ListPicker {
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

/// The active overlay picker: model profiles, or skills to load.
pub enum Picker {
    Model(ListPicker),
    /// Skill display items are "name — description" (description truncated);
    /// `names` holds the plain names, indexed like `list`, for the Enter action.
    Skill {
        list: ListPicker,
        names: Vec<String>,
    },
}

impl Picker {
    pub fn items(&self) -> &[String] {
        match self {
            Picker::Model(l) => &l.items,
            Picker::Skill { list, .. } => &list.items,
        }
    }

    pub fn selected(&self) -> usize {
        match self {
            Picker::Model(l) => l.selected,
            Picker::Skill { list, .. } => list.selected,
        }
    }

    pub fn move_up(&mut self) {
        match self {
            Picker::Model(l) => l.move_up(),
            Picker::Skill { list, .. } => list.move_up(),
        }
    }

    pub fn move_down(&mut self) {
        match self {
            Picker::Model(l) => l.move_down(),
            Picker::Skill { list, .. } => list.move_down(),
        }
    }
}

/// First visible index of a scrolling window of `capacity` items that keeps
/// `selected` in view: centered when possible, clamped at both ends.
pub fn visible_window(selected: usize, len: usize, capacity: usize) -> usize {
    if len <= capacity {
        return 0;
    }
    selected.saturating_sub(capacity / 2).min(len - capacity)
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
    /// Open the interactive skill picker (`/skills`).
    PickSkill,
    /// Compact the conversation now (`/compact`).
    Compact,
    /// Self-update a managed install (`/update`).
    Update,
    /// Unrecognized `/something`.
    Unknown(String),
}

/// A slash command offered by the input hint (pi-style autocomplete).
pub struct SlashCmd {
    /// All spellings; the first is canonical. Aliases match too (`/q`).
    pub names: &'static [&'static str],
    pub desc: &'static str,
    /// What completing fills the input with; a trailing space/`:` means
    /// arguments are expected after it.
    pub completion: &'static str,
}

/// The command catalog for the slash hint, in display order.
pub const SLASH_COMMANDS: &[SlashCmd] = &[
    SlashCmd {
        names: &["quit", "exit", "q"],
        desc: "leave crab",
        completion: "/quit",
    },
    SlashCmd {
        names: &["yolo"],
        desc: "auto-approve every tool call for the session",
        completion: "/yolo",
    },
    SlashCmd {
        names: &["model"],
        desc: "switch provider profile: /model <name>",
        completion: "/model ",
    },
    SlashCmd {
        names: &["models"],
        desc: "pick a provider profile from a list",
        completion: "/models",
    },
    SlashCmd {
        names: &["skills"],
        desc: "pick a skill from a list",
        completion: "/skills",
    },
    SlashCmd {
        names: &["skill"],
        desc: "load a skill: /skill:<name> [args]",
        completion: "/skill:",
    },
    SlashCmd {
        names: &["compact"],
        desc: "compact the conversation now",
        completion: "/compact",
    },
];

/// Live slash-command hint: what filter produced it + the selection.
/// Synced from the input at draw/key time; the selection survives only
/// while the filter text is unchanged.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SlashHint {
    pub kind: HintKind,
    pub selected: usize,
}

/// What the hint is completing: a command name, or the argument of
/// `/model <name>` / `/skill:<name>` (listed once the separator is typed).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HintKind {
    Command(String),
    ModelArg(String),
    SkillArg(String),
}

impl HintKind {
    /// Identity for reset-on-change: kind tag + filter text.
    fn key(&self) -> (u8, &str) {
        match self {
            HintKind::Command(f) => (0, f),
            HintKind::ModelArg(f) => (1, f),
            HintKind::SkillArg(f) => (2, f),
        }
    }
}

/// Classify the input for hint purposes: once the argument separator is
/// typed, the hint switches to completing that argument.
fn hint_kind(input: &str) -> HintKind {
    for prefix in ["/model ", "/m "] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return HintKind::ModelArg(rest.to_string());
        }
    }
    if let Some(rest) = input.strip_prefix("/skill:") {
        return HintKind::SkillArg(rest.to_string());
    }
    HintKind::Command(input.trim_start_matches('/').to_string())
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
        "skills" => SlashCommand::PickSkill,
        "compact" => SlashCommand::Compact,
        "update" => SlashCommand::Update,
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
    let content = std::fs::read_to_string(&skill.skill_md)
        .unwrap_or_else(|e| format!("(failed to read {}: {e})", skill.skill_md.display()));
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

/// The input-box line an Enter'd skill picker selection produces: `/skill:<name> `
/// with a trailing space so the user types straight into the args.
pub fn skill_slash_line(name: &str) -> String {
    format!("/skill:{name} ")
}

/// Build the skill picker overlay. None when no skills are discovered.
/// Display items are "name — description" (description truncated to 60 cells);
/// `names` parallels the items for the Enter action.
pub fn skill_picker(skills: &[crab_core::skills::Skill]) -> Option<Picker> {
    if skills.is_empty() {
        return None;
    }
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    let items: Vec<String> = skills
        .iter()
        .map(|s| {
            format!(
                "{} — {}",
                s.name,
                crate::width::truncate_to_width(&s.description, 60)
            )
        })
        .collect();
    Some(Picker::Skill {
        list: ListPicker::new(items, 0),
        names,
    })
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
    pub picker: Option<Picker>,
    /// Yolo (allow-all) flag, mirrored from the permission gate for the status line.
    pub yolo: bool,
    /// Current provider label for the picker (e.g. "openai/gpt-4o-mini").
    pub provider_label: String,
    /// Cached tool args from AssistantMessage (for showing in tool bubbles).
    tool_args_cache: HashMap<ToolCallId, String>,
    /// In-progress text selection (press-drag-release), absolute row indexes.
    pub selection: Option<super::select::Selection>,
    /// What the last draw rendered: selectable rows + screen geometry.
    /// Refreshed every frame; mouse hit-testing runs against it.
    pub select_grid: Option<super::select::SelectGrid>,
    /// Live cost fold for the status line's `$` segment (pi parity). None in
    /// contexts without pricing (created by the TUI after construction).
    pub pricer: Option<crate::metrics::Pricer>,
    /// Slash-command hint (pi-style autocomplete), synced from the input.
    pub slash_hint: Option<SlashHint>,
    /// Input text whose hint the user dismissed with Esc; the hint stays
    /// hidden while the input is unchanged and returns when it changes.
    pub slash_dismissed: Option<String>,
    /// Provider profile names for `/model ` argument completion.
    pub model_names: Vec<String>,
    /// Discovered skills for `/skill:` argument completion.
    pub skills: Vec<crab_core::skills::Skill>,
}

impl TuiState {
    pub fn new(
        provider_model: String,
        cwd: std::path::PathBuf,
        context_window: Option<u64>,
        // Occupancy replayed from the session log at startup (`--continue`).
        // None for a fresh session or a log with no usage yet; live usage
        // events take over from here.
        context_used: Option<u64>,
    ) -> Self {
        let mut status = StatusLine::new(provider_model.clone(), cwd);
        status.set_context(context_used, context_window);
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
            selection: None,
            select_grid: None,
            pricer: None,
            slash_hint: None,
            slash_dismissed: None,
            model_names: Vec::new(),
            skills: Vec::new(),
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

    /// Replay one persisted session message into transcript bubbles (`--continue`).
    /// Mirrors the live event flow: user text → `User` bubble, assistant text →
    /// a complete `Assistant` bubble, tool_use → `Tool` bubble whose result is
    /// filled by the following message's matching tool_result block.
    pub fn replay_message(&mut self, msg: &crab_core::types::Message) {
        use crab_core::types::{ContentBlock, Role};
        match msg.role {
            Role::User => {
                let mut texts = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => texts.push(text.clone()),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            // Fill the matching Tool bubble (live path: ToolCallFinished).
                            for bubble in self.bubbles.iter_mut().rev() {
                                if let Bubble::Tool {
                                    id: bid, result: r, ..
                                } = bubble
                                    && bid == tool_use_id
                                {
                                    let (text, truncated) = truncate_result(content);
                                    *r = Some(ToolResult {
                                        text,
                                        is_error: *is_error,
                                        truncated,
                                    });
                                    break;
                                }
                            }
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !texts.is_empty() {
                    self.bubbles.push(Bubble::User(texts.join("\n")));
                }
            }
            Role::Assistant => {
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    self.bubbles.push(Bubble::Assistant {
                        text,
                        complete: true,
                    });
                }
                // Tool bubbles come after the text (live order: text streams,
                // then ToolCallStarted fires per tool_use block).
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let args_str = compact_json(input);
                        self.tool_args_cache
                            .insert(id.clone(), format!("{name} {args_str}"));
                        self.bubbles.push(Bubble::Tool {
                            id: id.clone(),
                            name: name.clone(),
                            args: format!("{name} {args_str}"),
                            result: None,
                        });
                    }
                }
            }
        }
        if self.scroll.follow {
            self.scroll.offset = 0;
        }
    }

    /// Mouse press: start a selection at the clicked cell (pi-style copy).
    pub fn select_begin(&mut self, col: u16, row: u16) {
        match self.select_grid.as_ref().and_then(|g| g.hit(col, row)) {
            Some(pos) => {
                self.selection = Some(super::select::Selection {
                    anchor: pos,
                    focus: pos,
                });
            }
            None => self.selection = None,
        }
    }

    /// Mouse drag while pressed: extend the selection.
    pub fn select_update(&mut self, col: u16, row: u16) {
        if let (Some(sel), Some(pos)) = (
            &mut self.selection,
            self.select_grid.as_ref().and_then(|g| g.hit(col, row)),
        ) {
            sel.focus = pos;
        }
    }

    /// Mouse release: return the selected text when the drag was non-trivial.
    /// The highlight stays until the next press (pi behavior).
    pub fn select_finish(&self) -> Option<String> {
        let grid = self.select_grid.as_ref()?;
        let sel = self.selection?;
        (sel.anchor != sel.focus).then(|| super::select::extract(&grid.rows, &sel))
    }

    // ---- slash-command hint (pi-style autocomplete) ----

    /// Commands matching a command-name filter (text after `/`).
    pub fn slash_matches_for(rest: &str) -> Vec<&'static SlashCmd> {
        SLASH_COMMANDS
            .iter()
            .filter(|c| c.names.iter().any(|n| n.starts_with(rest)))
            .collect()
    }

    /// Commands matching the input's command token (`/` + prefix, no args
    /// yet). Empty for non-slash input or once arguments are being typed.
    /// (Test-facing convenience over `slash_matches_for`.)
    #[cfg(test)]
    pub fn slash_matches(input: &str) -> Vec<&'static SlashCmd> {
        let Some(rest) = input.strip_prefix('/') else {
            return Vec::new();
        };
        if rest.contains(char::is_whitespace) || rest.contains(':') {
            return Vec::new();
        }
        Self::slash_matches_for(rest)
    }

    /// Completions for a hint kind: what Enter would fill the input with.
    fn hint_completions(&self, kind: &HintKind) -> Vec<String> {
        match kind {
            HintKind::Command(_) => Self::slash_matches_for(kind.key().1)
                .iter()
                .map(|c| c.completion.to_string())
                .collect(),
            HintKind::ModelArg(prefix) => self
                .model_names
                .iter()
                .filter(|n| n.starts_with(prefix.as_str()))
                .map(|n| format!("/model {n}"))
                .collect(),
            HintKind::SkillArg(prefix) => self
                .skills
                .iter()
                .filter(|s| s.name.starts_with(prefix.as_str()))
                .map(|s| format!("/skill:{} ", s.name))
                .collect(),
        }
    }

    /// Refresh the hint from the current input. Keeps the selection while the
    /// filter is unchanged, resets it when the text changes, hides the hint
    /// when nothing matches, when a no-arg command is fully spelled, or when
    /// the user dismissed this exact input with Esc.
    pub fn sync_slash_hint(&mut self) {
        let input = self.input.text().to_string();
        if self.slash_dismissed.as_deref() == Some(input.as_str()) {
            self.slash_hint = None;
            return;
        }
        self.slash_dismissed = None;
        if !input.starts_with('/') {
            self.slash_hint = None;
            return;
        }
        let kind = hint_kind(&input);
        let matches = self.hint_completions(&kind);
        let done_no_arg = matches!(kind, HintKind::Command(_)) && matches.len() == 1 && {
            // 完整输入了规范形且无参：隐藏提示，Enter 直接执行。
            // （别名如 /q 保留提示，便于补全为规范形 /quit。）
            let rest = input.trim_start_matches('/');
            let cmd = Self::slash_matches_for(kind.key().1);
            !cmd[0].completion.ends_with(' ')
                && !cmd[0].completion.ends_with(':')
                && cmd[0].names.first() == Some(&rest)
        };
        if matches.is_empty() || done_no_arg {
            self.slash_hint = None;
            return;
        }
        match &mut self.slash_hint {
            Some(h) if h.kind.key() == kind.key() => {
                h.selected = h.selected.min(matches.len().saturating_sub(1));
            }
            _ => self.slash_hint = Some(SlashHint { kind, selected: 0 }),
        }
    }

    /// Esc: dismiss the hint for this exact input (it returns if the text
    /// changes — the dismissal is tied to what was on screen).
    pub fn dismiss_slash_hint(&mut self) {
        if self.slash_hint.is_some() {
            self.slash_dismissed = Some(self.input.text().to_string());
        }
        self.slash_hint = None;
    }

    /// Move the hint selection (clamp at both ends; no wrap).
    pub fn slash_hint_move(&mut self, delta: isize) {
        let Some(h) = &self.slash_hint else { return };
        let len = self.hint_completions(&h.kind).len();
        if len == 0 {
            return;
        }
        let h = self.slash_hint.as_mut().unwrap();
        h.selected = (h.selected as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// Fill the input with the selected completion. Returns false when there
    /// was nothing to complete — or the completion equals the input, in which
    /// case the Enter should fall through and execute the command.
    pub fn slash_hint_complete(&mut self) -> bool {
        let Some(h) = &self.slash_hint else {
            return false;
        };
        let completions = self.hint_completions(&h.kind);
        let Some(completion) = completions.get(h.selected) else {
            self.slash_hint = None;
            return false;
        };
        if completion == self.input.text() {
            return false; // already complete: let Enter execute
        }
        let completion = completion.clone();
        self.input.take();
        self.input.insert_str(&completion);
        self.slash_hint = None;
        true
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
                cache_read_tokens,
                cache_write_tokens,
            } => {
                let u = crab_core::obs::LlmUsage {
                    prompt: prompt_tokens,
                    completion: completion_tokens,
                    cache_read: cache_read_tokens,
                    cache_write: cache_write_tokens,
                };
                self.status.add_usage(u);
                // Same pricing fold as the metrics sink, for the `$` segment.
                if let Some(p) = &mut self.pricer {
                    p.on_usage(crab_core::util::now_ms() as i64, &u);
                    self.status.set_cost(p.total());
                }
            }
            Compacted {
                strategy,
                shed_tool_results,
                turns_affected,
            } => {
                let mut msg = format!(
                    "compacted {turns_affected} turns, shed {shed_tool_results} tool results"
                );
                if matches!(strategy, crab_core::obs::CompactionStrategy::Summarize) {
                    msg.push_str(" + summary");
                }
                self.notice(msg);
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

    fn msg(
        role: crab_core::types::Role,
        content: Vec<crab_core::types::ContentBlock>,
    ) -> crab_core::types::Message {
        crab_core::types::Message { role, content }
    }

    #[test]
    fn slash_matches_filters_by_prefix_and_stops_at_args() {
        // "/" alone: full catalog.
        assert_eq!(TuiState::slash_matches("/").len(), SLASH_COMMANDS.len());
        // 前缀过滤：/m → model + models。
        let m: Vec<_> = TuiState::slash_matches("/m")
            .iter()
            .map(|c| c.names[0])
            .collect();
        assert_eq!(m, vec!["model", "models"]);
        // 别名匹配：/q → quit。
        assert_eq!(TuiState::slash_matches("/q")[0].names[0], "quit");
        // 进入参数部分（空格或冒号）后不再提示。
        assert!(TuiState::slash_matches("/model gpt").is_empty());
        assert!(TuiState::slash_matches("/skill:").is_empty());
        // 非 slash 输入没有提示。
        assert!(TuiState::slash_matches("hello").is_empty());
        // 无匹配。
        assert!(TuiState::slash_matches("/zzz").is_empty());
    }

    #[test]
    fn slash_hint_resets_selection_when_filter_changes_and_clamps() {
        let mut s = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        s.input.insert_str("/m");
        s.sync_slash_hint();
        assert_eq!(
            s.slash_hint,
            Some(SlashHint {
                kind: HintKind::Command("m".into()),
                selected: 0
            })
        );
        s.slash_hint_move(1);
        assert_eq!(s.slash_hint.as_ref().unwrap().selected, 1);
        // 过滤器不变：选中保留。
        s.sync_slash_hint();
        assert_eq!(s.slash_hint.as_ref().unwrap().selected, 1);
        // 过滤器变化：重置为 0。
        s.input.take();
        s.input.insert_str("/s");
        s.sync_slash_hint();
        assert_eq!(
            s.slash_hint,
            Some(SlashHint {
                kind: HintKind::Command("s".into()),
                selected: 0
            })
        );
        // 收窄到唯一匹配且是完整无参命令 → 提示隐藏（直接 Enter 执行）。
        s.input.take();
        s.input.insert_str("/quit");
        s.sync_slash_hint();
        assert_eq!(s.slash_hint, None);
        // 非 slash 输入 → 无提示。
        s.input.take();
        s.input.insert_str("hi");
        s.sync_slash_hint();
        assert_eq!(s.slash_hint, None);
    }

    #[test]
    fn slash_hint_complete_fills_input_with_canonical_form() {
        let mut s = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        s.input.insert_str("/mo");
        s.sync_slash_hint();
        assert!(s.slash_hint_complete());
        assert_eq!(s.input.text(), "/model ", "first match, args expected");
        assert_eq!(s.slash_hint, None);
        // 别名也补全为规范形。
        s.input.take();
        s.input.insert_str("/q");
        s.sync_slash_hint();
        assert!(s.slash_hint_complete());
        assert_eq!(s.input.text(), "/quit");
        // 无提示时 complete 返回 false。
        assert!(!s.slash_hint_complete());
    }

    #[test]
    fn slash_hint_completes_model_and_skill_args() {
        let mut s = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        s.model_names = vec!["deepseek".into(), "kimi".into()];
        s.skills = vec![
            crab_core::skills::Skill {
                name: "tdd".into(),
                description: "red-green-refactor".into(),
                dir: std::path::PathBuf::from("/x"),
                skill_md: std::path::PathBuf::from("/x/SKILL.md"),
            },
            crab_core::skills::Skill {
                name: "review".into(),
                description: "review the diff".into(),
                dir: std::path::PathBuf::from("/y"),
                skill_md: std::path::PathBuf::from("/y/SKILL.md"),
            },
        ];
        // /model <prefix> → provider profiles。
        s.input.insert_str("/model d");
        s.sync_slash_hint();
        assert!(matches!(
            s.slash_hint,
            Some(SlashHint {
                kind: HintKind::ModelArg(_),
                ..
            })
        ));
        assert!(s.slash_hint_complete());
        assert_eq!(s.input.text(), "/model deepseek");
        // 已完成的输入再按 Enter 不应被吞（complete 返回 false → 执行）。
        s.sync_slash_hint();
        assert!(!s.slash_hint_complete(), "completion equals input: run it");
        // /skill:<prefix> → skills（补全带尾随空格等参数）。
        s.input.take();
        s.input.insert_str("/skill:t");
        s.sync_slash_hint();
        assert!(s.slash_hint_complete());
        assert_eq!(s.input.text(), "/skill:tdd ");
        // 别名 /m 同样进入 model 参数模式。
        s.input.take();
        s.input.insert_str("/m k");
        s.sync_slash_hint();
        assert!(matches!(
            s.slash_hint,
            Some(SlashHint {
                kind: HintKind::ModelArg(_),
                ..
            })
        ));
        // 没有配置任何 model/skill 时不弹参数提示。
        let mut bare = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        bare.input.insert_str("/model ");
        bare.sync_slash_hint();
        assert_eq!(bare.slash_hint, None);
    }

    #[test]
    fn slash_hint_esc_dismissal_survives_until_input_changes() {
        let mut s = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        s.input.insert_str("/m");
        s.sync_slash_hint();
        assert!(s.slash_hint.is_some());
        s.dismiss_slash_hint();
        assert_eq!(s.slash_hint, None);
        // 同一输入再 sync（下一帧）不会把弹层拉回来。
        s.sync_slash_hint();
        assert_eq!(s.slash_hint, None, "dismissed for this exact input");
        // 输入一变，提示恢复。
        s.input.insert_str("o");
        s.sync_slash_hint();
        assert!(s.slash_hint.is_some());
    }

    #[test]
    fn replay_rebuilds_user_and_assistant_bubbles() {
        use crab_core::types::{ContentBlock, Role};
        let mut state = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        state.replay_message(&msg(
            Role::User,
            vec![ContentBlock::Text { text: "hi".into() }],
        ));
        state.replay_message(&msg(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        ));
        assert_eq!(state.bubbles.len(), 2);
        assert!(matches!(&state.bubbles[0], Bubble::User(t) if t == "hi"));
        assert!(
            matches!(&state.bubbles[1], Bubble::Assistant { text, complete: true } if text == "hello"),
            "{:?}",
            state.bubbles[1]
        );
    }

    #[test]
    fn replay_pairs_tool_use_with_result() {
        use crab_core::types::{ContentBlock, Role};
        let mut state = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        state.replay_message(&msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "count".into(),
            }],
        ));
        state.replay_message(&msg(
            Role::Assistant,
            vec![
                ContentBlock::Text {
                    text: "counting".into(),
                },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
        ));
        state.replay_message(&msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "total 3".into(),
                is_error: false,
            }],
        ));
        // 顺序：User(question) → Assistant(text) → Tool(带结果)。
        assert_eq!(state.bubbles.len(), 3, "{:?}", state.bubbles);
        assert!(matches!(&state.bubbles[1], Bubble::Assistant { text, .. } if text == "counting"));
        match &state.bubbles[2] {
            Bubble::Tool {
                name, args, result, ..
            } => {
                assert_eq!(name, "bash");
                assert!(args.contains("bash"), "{args}");
                let r = result.as_ref().expect("result filled");
                assert_eq!(r.text, "total 3");
                assert!(!r.is_error);
            }
            other => panic!("expected Tool bubble, got {other:?}"),
        }
    }

    #[test]
    fn replay_marks_error_results() {
        use crab_core::types::{ContentBlock, Role};
        let mut state = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        state.replay_message(&msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "c9".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/x"}),
            }],
        ));
        state.replay_message(&msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "c9".into(),
                content: "no such file".into(),
                is_error: true,
            }],
        ));
        let r = match &state.bubbles[0] {
            Bubble::Tool { result, .. } => result.as_ref().unwrap(),
            other => panic!("{other:?}"),
        };
        assert!(r.is_error);
        assert_eq!(r.text, "no such file");
    }

    #[test]
    fn replay_without_text_blocks_skips_empty_bubbles() {
        use crab_core::types::{ContentBlock, Role};
        let mut state = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        // 纯 tool_use 的 assistant 消息不产生空 Assistant bubble。
        state.replay_message(&msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "grep".into(),
                input: serde_json::json!({}),
            }],
        ));
        assert_eq!(state.bubbles.len(), 1);
        assert!(matches!(&state.bubbles[0], Bubble::Tool { .. }));
    }

    #[test]
    fn text_delta_appends_to_open_assistant_bubble() {
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None, None);

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
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None, None);

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
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None, None);

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
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None, None);

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
        let mut state = TuiState::new("test/model".into(), std::path::PathBuf::new(), None, None);

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
            Some(SlashCommand::Skill(
                "tdd".into(),
                Some("fix the parser".into())
            ))
        );
        assert_eq!(
            parse_slash("/skill:"),
            Some(SlashCommand::Skill(String::new(), None))
        );
    }

    #[test]
    fn visible_window_keeps_selection_in_view() {
        // Everything fits: never scrolls.
        assert_eq!(visible_window(0, 10, 13), 0);
        assert_eq!(visible_window(9, 10, 13), 0);
        // Top: stays at 0 until the selection passes the middle.
        assert_eq!(visible_window(0, 50, 13), 0);
        assert_eq!(visible_window(6, 50, 13), 0);
        // Middle: follows, keeping the selection centered.
        assert_eq!(visible_window(25, 50, 13), 19);
        assert_eq!(visible_window(26, 50, 13), 20);
        // Bottom: clamped so the last window still fills the capacity.
        assert_eq!(visible_window(49, 50, 13), 37);
        assert_eq!(visible_window(44, 50, 13), 37);
    }

    #[test]
    fn parse_skills_opens_picker() {
        assert_eq!(parse_slash("/skills"), Some(SlashCommand::PickSkill));
        assert_eq!(parse_slash("/compact"), Some(SlashCommand::Compact));
    }

    #[test]
    fn skill_slash_line_has_trailing_space() {
        assert_eq!(skill_slash_line("tdd"), "/skill:tdd ");
    }

    #[test]
    fn skill_picker_empty_when_no_skills() {
        assert!(skill_picker(&[]).is_none());
    }

    #[test]
    fn skill_picker_builds_display_items_and_names() {
        let mk = |name: &str, desc: &str| crab_core::skills::Skill {
            name: name.into(),
            description: desc.into(),
            dir: std::path::PathBuf::from("/x"),
            skill_md: std::path::PathBuf::from("/x/SKILL.md"),
        };
        let long = "y".repeat(200);
        let skills = vec![mk("tdd", "Test-driven development"), mk("rev", &long)];
        let picker = skill_picker(&skills).unwrap();
        let Picker::Skill { list, names } = picker else {
            panic!("must be a Skill picker");
        };
        assert_eq!(names, vec!["tdd".to_string(), "rev".to_string()]);
        assert_eq!(list.items[0], "tdd — Test-driven development");
        assert!(
            crate::width::display_width(&list.items[1]) <= "rev — ".len() + 60,
            "long description must be truncated: {}",
            list.items[1]
        );
        assert!(list.items[1].starts_with("rev — yyy"));
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
        let mut p = ListPicker::new(items, 0);
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
        let p = ListPicker::new(items, 99);
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_name(), Some("only"));
    }
}
