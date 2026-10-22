//! Rendering logic for the TUI.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use super::select;
use super::state::{Picker, Theme, TuiState, visible_window};

/// Draw the full TUI.
pub fn draw(f: &mut Frame, state: &mut TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // Transcript
            Constraint::Length(3), // Input box
            Constraint::Length(1), // cwd row
            Constraint::Length(1), // status row
        ])
        .split(f.area());

    draw_transcript(f, state, chunks[0]);
    draw_input(f, state, chunks[1]);
    draw_status_rows(f, state, &chunks[2..]);

    // Draw confirm modal on top if active
    if state.confirm.is_some() {
        draw_confirm_modal(f, state);
    }

    // Draw model picker on top if open
    if state.picker.is_some() {
        draw_picker(f, state);
    }
}

/// Draw the transcript area.
///
/// Long lines wrap, and scrolling is measured in *rendered* lines, so every line of
/// every message is reachable. The scroll state is clamped and re-anchored here
/// because only the draw knows the wrapped line count.
fn draw_transcript(f: &mut Frame, state: &mut TuiState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // One blank row between bubbles (pi-style breathing room) and at the end.
    let mut lines: Vec<Line> = Vec::new();
    for (i, bubble) in state.bubbles.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.extend(bubble.to_text(area.width as usize).lines);
    }
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }

    // Pre-wrap so screen rows match `rows` 1:1 — the same WordWrapper
    // semantics crab rendered with before (see tui::select), minus the
    // over-wide-row clipping that could drop a character off screen.
    let height = area.height as usize;
    let mut width = area.width as usize;
    let mut rows = select::wrap_styled(&lines, width);
    if rows.len() > height && area.width > 1 {
        width -= 1; // reserve the scrollbar column
        rows = select::wrap_styled(&lines, width);
    }
    let total = rows.len();
    let mut text_area = area;
    if width != area.width as usize {
        text_area.width -= 1;
    }

    // A width change re-wraps everything — absolute row indexes shift, so an
    // existing selection would point at the wrong text.
    if state
        .select_grid
        .as_ref()
        .is_some_and(|g| g.width != text_area.width)
    {
        state.selection = None;
    }

    let max_offset = total.saturating_sub(height);
    let scroll = &mut state.scroll;
    // While scrolled up, keep the same text on screen as output keeps arriving.
    if !scroll.follow && scroll.last_width == text_area.width && total > scroll.last_total {
        scroll.offset = scroll.offset.saturating_add(total - scroll.last_total);
    }
    scroll.offset = scroll.offset.min(max_offset);
    scroll.follow = scroll.offset == 0;
    scroll.last_total = total;
    scroll.last_width = text_area.width;
    scroll.viewport = height;

    // `Paragraph::scroll` takes a u16 row; clamp rather than wrap around.
    let top = (max_offset - scroll.offset).min(u16::MAX as usize);

    // Snapshot the rows for hit-testing/copy before painting the highlight.
    state.select_grid = Some(select::SelectGrid {
        rows: rows.clone(),
        top,
        x: text_area.x,
        y: text_area.y,
        width: text_area.width,
        height: area.height,
    });
    if let Some(sel) = state.selection {
        select::highlight(
            &mut rows,
            &sel,
            Style::default().add_modifier(Modifier::REVERSED),
        );
    }

    let paragraph = Paragraph::new(rows);
    f.render_widget(paragraph.scroll((top as u16, 0)), text_area);

    if max_offset > 0 {
        let mut bar = ScrollbarState::new(max_offset).position(top);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .style(Theme::dim());
        f.render_stateful_widget(scrollbar, area, &mut bar);
    }
}

/// Draw the two status rows below the input box (pi-style): the cwd+branch
/// row, then the usage row (left) with the model label right-aligned.
fn draw_status_rows(f: &mut Frame, state: &TuiState, areas: &[Rect]) {
    let cwd = Line::styled(
        state.status.format_cwd_row(areas[0].width as usize),
        Theme::status(),
    );
    f.render_widget(Paragraph::new(cwd), areas[0]);
    let row = Line::styled(
        state.status.format_status_row(areas[1].width as usize),
        Theme::status(),
    );
    f.render_widget(Paragraph::new(row), areas[1]);
}

/// Braille spinner frames, one per [`SPINNER_FRAME_MS`].
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_FRAME_MS: u128 = 100;

/// Input-box title shown while the agent runs: an animated spinner and an
/// elapsed timer, so a long turn reads as "working", not "frozen".
fn running_title(state: &TuiState) -> String {
    if state.confirm.is_some() {
        return " waiting for confirmation ".to_string();
    }
    let elapsed = state.run_elapsed().unwrap_or_default();
    let frame = SPINNER[((elapsed.as_millis() / SPINNER_FRAME_MS) as usize) % SPINNER.len()];
    format!(
        " {frame} running... {elapsed:.0}s (Esc to abort) ",
        elapsed = elapsed.as_secs()
    )
}

/// Cursor column within the input area: the 6-column prompt prefix plus the
/// display width of the current line up to the cursor. Display width, not
/// char count — a CJK char occupies two columns, so counting `chars()` puts
/// the cursor one cell short per wide char.
fn cursor_column(input_text: &str, cursor_byte: usize) -> usize {
    let line_start = input_text[..cursor_byte]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    6 + crate::width::display_width(&input_text[line_start..cursor_byte])
}

/// Draw the input box.
fn draw_input(f: &mut Frame, state: &TuiState, area: Rect) {
    let is_running = state.is_running();

    let style = if is_running {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let border_style = if is_running {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Blue)
    };

    let input_lines = state.input.to_lines(area.width);
    let title = if is_running {
        running_title(state)
    } else {
        String::new()
    };
    let paragraph = Paragraph::new(input_lines).style(style).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );

    f.render_widget(paragraph, area);

    // Position cursor (if not running)
    if !is_running {
        // Calculate cursor position in the input area
        let input_text = state.input.text();
        let cursor_byte = state.input.cursor();

        let cursor_line = input_text[..cursor_byte].lines().count();
        let cursor_col = cursor_column(input_text, cursor_byte);

        // The input box has a border, so add 1 to x and y
        let cursor_x = area.x + 1 + cursor_col as u16;
        let cursor_y = area.y + 1 + cursor_line as u16;

        // Clamp to area
        let cursor_x = cursor_x.min(area.x + area.width.saturating_sub(2));
        let cursor_y = cursor_y.min(area.y + area.height.saturating_sub(2));

        f.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Draw the confirmation modal overlay.
fn draw_confirm_modal(f: &mut Frame, state: &TuiState) {
    let pending = match &state.confirm {
        Some(p) => p,
        None => return,
    };

    // Generous modal: approving a command you cannot read is a security UX
    // failure, so the modal takes most of the screen (old 60x20% clipped long
    // commands — and the key hint with them).
    let area = centered_rect(80, 60, f.area());

    // Clear the area first
    f.render_widget(Clear, area);

    // The key hint LEADS: it must stay visible even when the command overflows.
    let text = format!(
        "[y]es  [n]o  [a]ll\n\n⚠ {} — {}\n",
        pending.prompt.tool, pending.prompt.summary
    );

    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Permission "),
    );

    f.render_widget(paragraph, area);
}

/// Draw the picker overlay (model profiles or skills).
fn draw_picker(f: &mut Frame, state: &TuiState) {
    let picker = match &state.picker {
        Some(p) => p,
        None => return,
    };
    let title = match picker {
        Picker::Model(_) => " Select model (↑↓/jk, Enter, Esc) ",
        Picker::Skill { .. } => " Select skill (↑↓/jk, Enter, Esc) ",
    };

    // Size to content, capped.
    let height = (picker.items().len() as u16 + 2).min(15);
    let width = picker
        .items()
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(10)
        .max(20) as u16
        + 4;
    let area = centered_rect_abs(width, height, f.area());
    f.render_widget(Clear, area);

    // Scroll the window so the selection stays visible (centered when possible).
    let capacity = height.saturating_sub(2) as usize;
    let selected = picker.selected();
    let offset = visible_window(selected, picker.items().len(), capacity);
    let lines: Vec<Line> = picker
        .items()
        .iter()
        .enumerate()
        .skip(offset)
        .take(capacity)
        .map(|(i, name)| {
            if i == selected {
                Line::from(vec![Span::styled(
                    format!("> {name}"),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )])
            } else {
                Line::from(format!("  {name}"))
            }
        })
        .collect();

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );
    f.render_widget(paragraph, area);
}

/// Centered rect with absolute width/height.
fn centered_rect_abs(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + r.width.saturating_sub(width) / 2;
    let y = r.y + r.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(r.width), height.min(r.height))
}

/// Helper to create a centered rect.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::Bubble;
    use ratatui::{Terminal, backend::TestBackend};

    fn state_with(n: usize) -> TuiState {
        let mut s = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        push_entries(&mut s, 0..n);
        s
    }

    #[test]
    fn cursor_column_counts_display_width_not_chars() {
        // 3 ASCII chars: 6 (prompt) + 3.
        assert_eq!(cursor_column("abc", 3), 9);
        // "输" is one char but two columns: cursor after it sits at 6 + 2.
        assert_eq!(cursor_column("输", 3), 8);
        // "输入框" fully typed: 6 + 6.
        assert_eq!(cursor_column("输入框", 9), 12);
        // Mixed: 6 + 1 + 4 = 11.
        assert_eq!(cursor_column("a输入", "a输入".len()), 11);
        // Second line after a newline: prefix still applies.
        assert_eq!(cursor_column("ab\n输c", "ab\n输c".len()), 6 + 3);
    }

    #[test]
    fn running_title_animates_and_shows_elapsed() {
        let mut s = state_with(0);
        s.push_user("go"); // idle -> streaming, starts the clock
        let a = running_title(&s);
        assert!(a.contains("running..."), "{a:?}");
        assert!(a.contains("Esc"), "{a:?}");
        assert!(SPINNER.contains(&a.chars().nth(1).unwrap()), "{a:?}");

        // A later moment picks a different frame (time-driven, not state-driven).
        s.run_started = Some(std::time::Instant::now() - std::time::Duration::from_millis(3100));
        let b = running_title(&s);
        assert!(b.contains("3s"), "{b:?}");
        assert_ne!(a, b, "spinner frame should advance with elapsed time");

        // Confirm prompt replaces the spinner with a static hint.
        let (tx, _rx) = std::sync::mpsc::channel();
        s.begin_confirm(
            crab_core::gate::Prompt {
                tool: "bash",
                summary: "ls".into(),
            },
            tx,
        );
        assert!(running_title(&s).contains("confirmation"));
    }

    fn push_entries(s: &mut TuiState, range: std::ops::Range<usize>) {
        for i in range {
            s.bubbles.push(Bubble::System(format!("entry-{i:03}")));
        }
    }

    /// Draw and return every row of the frame (transcript, input box, status).
    fn render_frame(state: &mut TuiState, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, state)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w - 1).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// Permission modal UX: the key hint must lead (visible even for long
    /// commands) and the modal must be tall enough to inspect what you approve.
    #[test]
    fn confirm_modal_leads_with_hint_and_fits_long_commands() {
        use crab_core::gate::Prompt;
        let mut s = TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        let long_cmd = format!(
            "run: cd /tmp && python3 - <<'PYEOF'\n{}\nPYEOF",
            "x".repeat(400)
        );
        let (tx, _rx) = std::sync::mpsc::channel::<crab_core::gate::Decision>();
        s.begin_confirm(Prompt { tool: "bash", summary: long_cmd }, tx);

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut s)).unwrap();
        let buf = term.backend().buffer();

        let row_text = |y: u16| -> String {
            (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>()
        };
        let hint_row = (0..24).find(|y| row_text(*y).contains("[y]es"));
        let cmd_row = (0..24).find(|y| row_text(*y).contains("python3"));
        let hint = hint_row.expect("hint must be visible");
        let cmd = cmd_row.expect("command start must be visible");
        assert!(
            hint < cmd,
            "hint ({hint}) must lead the command ({cmd}): users must always see how to respond"
        );
        // Modal must be tall enough to actually inspect the command: the old
        // 20%-height modal clipped it (and the hint) entirely.
        let visible_cmd_rows = (0..24).filter(|y| row_text(*y).contains("xxxx")).count();
        assert!(
            visible_cmd_rows >= 5,
            "only {visible_cmd_rows} command rows visible — modal too short"
        );
    }

    /// The two status rows sit at the very bottom, below the input box:
    /// cwd (+branch) on the first, usage left / model right on the second.
    #[test]
    fn status_rows_sit_below_the_input_box() {
        let mut s = TuiState::new(
            "deepseek/deepseek-chat".into(),
            std::path::PathBuf::from("/x"),
            Some(128_000),
            None,
        );
        s.status.add_usage(crab_core::obs::LlmUsage {
            prompt: 8500,
            completion: 100,
            cache_read: 0,
            cache_write: 0,
        }); // makes the context segment appear
        let rows = render_frame(&mut s, 80, 12);
        let cwd_row = &rows[rows.len() - 2];
        let status_row = &rows[rows.len() - 1];
        assert!(cwd_row.contains("/x"), "{rows:?}");
        assert!(status_row.contains('↑'), "{rows:?}");
        assert!(status_row.contains("6.6%/128.0k"), "{rows:?}");
        assert!(
            status_row.trim_end().ends_with("deepseek/deepseek-chat"),
            "{status_row:?}"
        );
        // Input box bottom border sits directly above the cwd row.
        assert!(rows[rows.len() - 3].contains('─'), "{rows:?}");
    }

    /// A `--continue` seed renders the CH segment on the very first frame —
    /// no Usage event required (meter replay, not live accumulation).
    #[test]
    fn seeded_context_occupancy_shows_ch_before_any_usage_event() {
        let mut s = TuiState::new(
            "deepseek/deepseek-chat".into(),
            std::path::PathBuf::from("/x"),
            Some(128_000),
            Some(85_000),
        );
        let rows = render_frame(&mut s, 80, 12);
        let status_row = &rows[rows.len() - 1];
        assert!(status_row.contains("66.4%/128.0k"), "{status_row:?}");
    }

    /// `--continue` also seeds the cumulative counters: the first frame shows
    /// ↑↓ and R/W from the replayed log, not zeros.
    #[test]
    fn seeded_usage_counters_show_on_first_frame() {
        let mut s = TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.status.seed_usage(crab_core::obs::LlmUsage {
            prompt: 50_000,
            completion: 5_000,
            cache_read: 40_000,
            cache_write: 100,
        });
        let rows = render_frame(&mut s, 80, 12);
        let status_row = &rows[rows.len() - 1];
        assert!(
            status_row.contains("↑50.0k ↓5.0k R40.0k W100"),
            "{status_row:?}"
        );
    }

    /// 消息之间有空行分隔（pi 式呼吸感），bubble 内部行保持紧凑。
    #[test]
    fn bubbles_are_separated_by_a_blank_row() {
        let mut s = TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.bubbles.push(Bubble::User("first question".into()));
        s.bubbles.push(Bubble::Assistant { text: "the answer".into(), complete: true });
        let rows = render_frame(&mut s, 40, 12);
        let pos_user = rows.iter().position(|r| r.contains("first question")).unwrap();
        let pos_asst = rows.iter().position(|r| r.contains("the answer")).unwrap();
        assert_eq!(pos_asst, pos_user + 2, "expected one blank row between bubbles: {rows:?}");
        assert!(rows[pos_user + 1].trim().is_empty());
    }

    /// pi-style copy: press-drag-release over the transcript selects and
    /// extracts the text. The grid comes from a real draw, so wrapping is
    /// exercised end to end.
    #[test]
    fn drag_select_extracts_wrapped_text() {
        let mut s = TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.bubbles.push(Bubble::Assistant {
            text: "alpha beta gamma delta".into(),
            complete: true,
        });
        // Draw once so select_grid exists. Area: 12 rows; transcript is the top
        // (12 - 5) rows; wrap at width 20 wraps the text into two rows.
        render_frame(&mut s, 20, 12);
        let grid = s.select_grid.clone().unwrap();
        let (row_alpha, row_delta) = {
            let a = grid.rows.iter().position(|r| super::select::line_text(r).starts_with("alpha")).unwrap();
            let b = grid.rows.iter().position(|r| super::select::line_text(r).starts_with("delta")).unwrap();
            (a, b)
        };
        // 屏幕坐标：transcript 从 (0,0) 开始；选第一行 col 6 到第二行 col 5。
        let top = grid.top;
        s.select_begin(6, (row_alpha - top) as u16);
        s.select_update(5, (row_delta - top) as u16);
        let text = s.select_finish().unwrap();
        assert_eq!(text, "beta gamma\ndelta", "{text:?}");
    }

    /// 单击（无拖拽）不产生拷贝文本，且保持不选中状态。
    #[test]
    fn click_without_drag_selects_nothing() {
        let mut s = TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.bubbles.push(Bubble::User("hello".into()));
        render_frame(&mut s, 20, 12);
        s.select_begin(2, 0);
        s.select_update(2, 0);
        assert_eq!(s.select_finish(), None);
    }

    /// Draw and return the transcript rows, without the scrollbar column.
    fn render(state: &mut TuiState, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, state)).unwrap();
        let buf = term.backend().buffer();
        let rows = h - 5; // input box (3) + status rows (2)
        (0..rows)
            .map(|y| (0..w - 1).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn follows_the_latest_output_by_default() {
        let mut s = state_with(30);
        let rows = render(&mut s, 40, 12);
        assert!(rows.iter().any(|r| r.contains("entry-029")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("entry-000")), "{rows:?}");
    }

    #[test]
    fn scrolling_up_reveals_older_messages() {
        let mut s = state_with(30);
        render(&mut s, 40, 12); // first draw learns the viewport size
        s.scroll_to_top();
        let rows = render(&mut s, 40, 12);
        assert!(rows[0].contains("entry-000"), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("entry-029")), "{rows:?}");

        s.scroll_to_bottom();
        let rows = render(&mut s, 40, 12);
        assert!(rows.iter().any(|r| r.contains("entry-029")), "{rows:?}");
    }

    #[test]
    fn view_stays_put_when_output_arrives_while_scrolled_up() {
        let mut s = state_with(30);
        render(&mut s, 40, 12);
        s.scroll_up(5);
        let before = render(&mut s, 40, 12);
        push_entries(&mut s, 30..33);
        let after = render(&mut s, 40, 12);
        assert_eq!(before, after);
        assert!(!s.scroll.follow);
    }

    #[test]
    fn wrapped_lines_count_towards_the_scroll_range() {
        let mut s = TuiState::new("t/m".into(), std::path::PathBuf::new(), None, None);
        s.bubbles.push(Bubble::Assistant {
            text: "word ".repeat(200),
            complete: true,
        });
        render(&mut s, 20, 12);
        assert!(s.scroll.last_total > 40, "total {}", s.scroll.last_total);
        s.scroll_to_top();
        // Terminal height 12: transcript = 12 - 5 (input box 3 + 2 status rows).
        assert_eq!(s.scroll.offset, s.scroll.last_total - 7);
        let rows = render(&mut s, 20, 12);
        assert!(rows[0].starts_with("word"), "{rows:?}");
    }

    #[test]
    fn centered_rect_is_centered() {
        let area = Rect::new(0, 0, 100, 50);
        let centered = centered_rect(50, 50, area);

        // Should be roughly centered
        assert!(centered.x > 20);
        assert!(centered.x < 60);
        assert!(centered.y > 10);
        assert!(centered.y < 30);
    }
}

