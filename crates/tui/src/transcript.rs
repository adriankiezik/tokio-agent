use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio_agent_core::message::ToolCallId;

use crate::{markdown::MarkdownBuffer, theme};

const TOOL_PREVIEW_LINES: usize = 3;
const TOOL_BLINK_FRAMES: usize = 24;

enum ToolStatus {
    Running,
    Success,
    Error,
}

enum Cell {
    User(String),
    Assistant(MarkdownBuffer),
    Thinking {
        text: String,
        active: bool,
    },
    Tool {
        id: ToolCallId,
        name: String,
        args: String,
        status: ToolStatus,
        result: Vec<String>,
        total_lines: usize,
        expanded: bool,
    },
}

#[derive(Default)]
pub(super) struct Transcript {
    cells: Vec<Cell>,
    assistant_open: bool,
    thinking_open: bool,
    hovered_tool: Option<usize>,
    tool_areas: Vec<(usize, Rect)>,
    previous_max_scroll: Option<usize>,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn push_user(&mut self, text: String) {
        self.close_streams();
        self.cells.push(Cell::User(text));
    }

    pub(super) fn clear(&mut self) {
        self.cells.clear();
        self.hovered_tool = None;
        self.tool_areas.clear();
        self.previous_max_scroll = None;
        self.close_streams();
    }

    pub(super) fn text_delta(&mut self, text: &str) {
        self.close_thinking();
        if self.assistant_open
            && let Some(Cell::Assistant(buffer)) = self.cells.last_mut()
        {
            buffer.push(text);
            return;
        }
        let mut cell = MarkdownBuffer::default();
        cell.push(text);
        self.assistant_open = true;
        self.cells.push(Cell::Assistant(cell));
    }

    pub(super) fn thinking_delta(&mut self, text: &str) {
        self.assistant_open = false;
        if self.thinking_open
            && let Some(Cell::Thinking { text: buffer, .. }) = self.cells.last_mut()
        {
            buffer.push_str(text);
            return;
        }
        self.thinking_open = true;
        self.cells.push(Cell::Thinking {
            text: text.to_owned(),
            active: true,
        });
    }

    pub(super) fn tool_start(&mut self, id: ToolCallId, name: String, summary: String) {
        self.close_streams();
        self.cells.push(Cell::Tool {
            id,
            name,
            args: summary,
            status: ToolStatus::Running,
            result: Vec::new(),
            total_lines: 0,
            expanded: false,
        });
    }

    pub(super) fn tool_result(&mut self, id: &ToolCallId, is_error: bool, text: &str) {
        self.tool_result_with_summary(id, is_error, text, None);
    }

    pub(super) fn tool_result_with_summary(
        &mut self,
        id: &ToolCallId,
        is_error: bool,
        text: &str,
        summary: Option<&str>,
    ) {
        self.close_streams();
        if let Some(Cell::Tool {
            args,
            status,
            result,
            total_lines,
            ..
        }) = self
            .cells
            .iter_mut()
            .find(|cell| matches!(cell, Cell::Tool { id: cell_id, .. } if cell_id == id))
        {
            if let Some(summary) = summary {
                *args = summary.to_owned();
            }
            *status = if is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
            *result = text.lines().map(str::to_owned).collect();
            *total_lines = result.len();
        }
    }

    pub(super) fn push_error(&mut self, text: &str) {
        self.close_streams();
        let mut cell = MarkdownBuffer::default();
        cell.push(text);
        self.cells.push(Cell::Assistant(cell));
    }

    fn close_streams(&mut self) {
        self.assistant_open = false;
        self.close_thinking();
    }

    fn close_thinking(&mut self) {
        if self.thinking_open
            && let Some(Cell::Thinking { active, .. }) = self.cells.last_mut()
        {
            *active = false;
        }
        self.thinking_open = false;
    }

    pub(super) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        spinner: usize,
        scroll_up: &mut usize,
    ) {
        let width = usize::from(area.width);
        let mut lines = Vec::new();
        let mut tool_ranges = Vec::new();
        for (index, cell) in self.cells.iter().enumerate() {
            lines.push(Line::default());
            let start = lines.len();
            let expandable = is_expandable_tool(cell);
            append_cell(
                &mut lines,
                cell,
                width,
                spinner,
                expandable && self.hovered_tool == Some(index),
            );
            if expandable {
                tool_ranges.push((index, start, lines.len()));
            }
        }
        let max_scroll = lines.len().saturating_sub(usize::from(area.height));
        *scroll_up = anchored_scroll(*scroll_up, self.previous_max_scroll, max_scroll);
        self.previous_max_scroll = Some(max_scroll);
        let viewport_start = max_scroll - *scroll_up;
        let viewport_end = viewport_start + usize::from(area.height);
        self.tool_areas = tool_ranges
            .into_iter()
            .filter_map(|(index, start, end)| {
                let visible_start = start.max(viewport_start);
                let visible_end = end.min(viewport_end);
                (visible_start < visible_end).then(|| {
                    (
                        index,
                        Rect::new(
                            area.x,
                            area.y + u16::try_from(visible_start - viewport_start).unwrap_or(0),
                            area.width,
                            u16::try_from(visible_end - visible_start).unwrap_or(u16::MAX),
                        ),
                    )
                })
            })
            .collect();
        if self
            .hovered_tool
            .is_some_and(|hovered| !self.tool_areas.iter().any(|(index, _)| *index == hovered))
        {
            self.hovered_tool = None;
        }
        frame.render_widget(
            Paragraph::new(lines).scroll((
                u16::try_from(max_scroll - *scroll_up).unwrap_or(u16::MAX),
                0,
            )),
            area,
        );
    }

    pub(super) fn on_mouse(&mut self, event: MouseEvent) -> bool {
        let hovered = self
            .tool_areas
            .iter()
            .find(|(_, area)| area.contains((event.column, event.row).into()))
            .map(|(index, _)| *index);
        self.hovered_tool = hovered;

        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(index) = hovered
            && let Some(Cell::Tool { expanded, .. }) = self.cells.get_mut(index)
        {
            *expanded = !*expanded;
            return true;
        }
        false
    }
}

fn anchored_scroll(
    scroll_up: usize,
    previous_max_scroll: Option<usize>,
    max_scroll: usize,
) -> usize {
    if scroll_up == 0 {
        return 0;
    }
    let viewport_start = previous_max_scroll
        .unwrap_or(max_scroll)
        .saturating_sub(scroll_up);
    max_scroll.saturating_sub(viewport_start).min(max_scroll)
}

fn append_cell(
    lines: &mut Vec<Line<'static>>,
    cell: &Cell,
    width: usize,
    spinner: usize,
    hovered: bool,
) {
    let start = lines.len();
    if let Cell::Tool {
        name, args, status, ..
    } = cell
        && name == "web_search"
    {
        append_hosted_search(lines, args, status, width);
        return;
    }
    match cell {
        Cell::User(text) => append_user(lines, text, width),
        Cell::Assistant(markdown) => append_markdown(lines, markdown, width),
        Cell::Thinking { text, active } => {
            let summary = thinking_summary(text, 72);
            let style = if *active {
                theme::thinking_pulse(spinner)
            } else {
                theme::dim()
            }
            .add_modifier(ratatui::style::Modifier::ITALIC);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(summary, style),
            ]));
        }
        Cell::Tool {
            name,
            args,
            status,
            result,
            total_lines,
            expanded,
            ..
        } => {
            let bullet = match status {
                ToolStatus::Running => Span::styled(
                    if (spinner / TOOL_BLINK_FRAMES).is_multiple_of(2) {
                        "⏺ "
                    } else {
                        "  "
                    },
                    theme::tool_running(),
                ),
                ToolStatus::Success => Span::styled("⏺ ", theme::success()),
                ToolStatus::Error => Span::styled("⏺ ", theme::error()),
            };
            lines.push(Line::from(vec![
                bullet,
                Span::styled(name.clone(), theme::bold()),
                Span::raw(format!("({args})")),
            ]));

            if name == "read" && matches!(status, ToolStatus::Success) && !expanded {
                let read_lines = if result.as_slice() == ["(no lines in range)"] {
                    0
                } else {
                    *total_lines
                };
                let summary = if is_empty_read_result(result) {
                    "Read empty file".to_owned()
                } else {
                    format!(
                        "Read {read_lines} {}",
                        if read_lines == 1 { "line" } else { "lines" }
                    )
                };
                lines.push(Line::styled(format!("  ⎿  {summary}"), theme::dim()));
            } else {
                let shown = if *expanded {
                    *total_lines
                } else {
                    (*total_lines).min(TOOL_PREVIEW_LINES)
                };
                for (i, line) in result.iter().take(shown).enumerate() {
                    let style = if matches!(status, ToolStatus::Error) {
                        theme::error()
                    } else {
                        theme::dim()
                    };
                    let wrapped = if *expanded && name == "read" {
                        wrap_read_output_line(
                            line,
                            width.saturating_sub(5),
                            read_line_number_width(result),
                        )
                    } else if *expanded {
                        wrap(line, width.saturating_sub(5))
                    } else {
                        vec![line.clone()]
                    };
                    for (part_index, part) in wrapped.into_iter().enumerate() {
                        let prefix = if i == 0 && part_index == 0 {
                            "  ⎿  "
                        } else {
                            "     "
                        };
                        lines.push(Line::styled(format!("{prefix}{part}"), style));
                    }
                }
                if shown < *total_lines {
                    lines.push(Line::styled(
                        format!("     … {} more lines", total_lines - shown),
                        theme::dim(),
                    ));
                }
            }
        }
    }
    if hovered && matches!(cell, Cell::Tool { .. }) {
        for line in &mut lines[start..] {
            let padding = " ".repeat(width.saturating_sub(line.width()));
            line.spans.push(Span::raw(padding));
            line.style = line.style.patch(theme::tool_hover());
        }
    }
}

fn append_hosted_search(
    lines: &mut Vec<Line<'static>>,
    query: &str,
    status: &ToolStatus,
    width: usize,
) {
    let (title, bullet_style) = match status {
        ToolStatus::Running => ("Searching the web", theme::tool_running()),
        ToolStatus::Success => ("Web search", theme::success()),
        ToolStatus::Error => ("Web search", theme::error()),
    };
    lines.push(Line::from(vec![
        Span::styled("⏺ ", bullet_style),
        Span::styled(title, theme::bold()),
    ]));
    if matches!(status, ToolStatus::Running) {
        return;
    }
    let detail = match status {
        ToolStatus::Success if query.is_empty() || query == "search completed" => {
            "Searched the web".to_owned()
        }
        ToolStatus::Success => format!("Searched the web for {query}"),
        ToolStatus::Error if query.is_empty() => "Web search failed".to_owned(),
        ToolStatus::Error => format!("Web search failed for {query}"),
        ToolStatus::Running => unreachable!("running status returned above"),
    };
    let detail_style = match status {
        ToolStatus::Success => theme::dim(),
        ToolStatus::Error => theme::error(),
        ToolStatus::Running => unreachable!("running status returned above"),
    };
    for (index, part) in wrap(&detail, width.saturating_sub(5))
        .into_iter()
        .enumerate()
    {
        let prefix = if index == 0 { "  ⎿  " } else { "     " };
        lines.push(Line::styled(format!("{prefix}{part}"), detail_style));
    }
}

fn is_expandable_tool(cell: &Cell) -> bool {
    let Cell::Tool {
        name,
        status,
        result,
        total_lines,
        ..
    } = cell
    else {
        return false;
    };
    if name == "web_search" || matches!(status, ToolStatus::Running) {
        return false;
    }
    if name == "read" && matches!(status, ToolStatus::Success) {
        return *total_lines > 0
            && result.as_slice() != ["(no lines in range)"]
            && !is_empty_read_result(result);
    }
    *total_lines > TOOL_PREVIEW_LINES
}

fn is_empty_read_result(result: &[String]) -> bool {
    result == ["(empty file)"]
        || (!result.is_empty()
            && result.iter().all(|line| {
                line.split_once('\t').is_some_and(|(number, content)| {
                    !number.trim().is_empty()
                        && number
                            .trim()
                            .chars()
                            .all(|character| character.is_ascii_digit())
                        && content.is_empty()
                })
            }))
}

fn read_line_number_width(result: &[String]) -> usize {
    result
        .iter()
        .filter_map(|line| line.split_once('\t').map(|(number, _)| number.trim().len()))
        .max()
        .unwrap_or(1)
}

fn wrap_read_output_line(line: &str, width: usize, number_width: usize) -> Vec<String> {
    let Some((number, content)) = line.split_once('\t') else {
        return wrap_verbatim(line, width);
    };
    let number = number.trim();
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        return wrap_verbatim(line, width);
    }
    let prefix = format!("{number:>number_width$}  ");
    let content_width = width.saturating_sub(prefix.chars().count()).max(1);
    let mut chunks = content.chars();
    let mut lines = Vec::new();
    loop {
        let chunk = chunks.by_ref().take(content_width).collect::<String>();
        if chunk.is_empty() {
            break;
        }
        let line_prefix = if lines.is_empty() {
            prefix.clone()
        } else {
            " ".repeat(prefix.chars().count())
        };
        lines.push(format!("{line_prefix}{chunk}"));
    }
    if lines.is_empty() {
        lines.push(prefix);
    }
    lines
}

fn wrap_verbatim(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chars = text.chars();
    let mut lines = Vec::new();
    loop {
        let line = chars.by_ref().take(width).collect::<String>();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn append_user(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let bg = theme::composer_bg();
    let blank = Line::styled(" ".repeat(width), bg);
    lines.push(blank.clone());
    for (i, l) in wrap(text, width.saturating_sub(3)).into_iter().enumerate() {
        let prefix = if i == 0 { "› " } else { "  " };
        let pad = " ".repeat(width.saturating_sub(prefix.chars().count() + l.chars().count()));
        lines.push(Line::from(vec![
            Span::styled(prefix, theme::prompt().patch(bg)),
            Span::styled(l, bg),
            Span::styled(pad, bg),
        ]));
    }
    lines.push(blank);
}

fn append_markdown(lines: &mut Vec<Line<'static>>, markdown: &MarkdownBuffer, width: usize) {
    lines.extend(markdown.render(width));
}

fn thinking_summary(text: &str, max: usize) -> String {
    let mut line = text.lines().next().unwrap_or_default().trim();
    for marker in ["**", "__"] {
        if let Some(inner) = line
            .strip_prefix(marker)
            .and_then(|line| line.strip_suffix(marker))
        {
            line = inner.trim();
        }
    }
    if line.chars().count() <= max {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split(' ') {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(value: &str) -> ToolCallId {
        ToolCallId(value.into())
    }

    #[test]
    fn transcript_growth_preserves_a_manually_scrolled_viewport() {
        let previous_max_scroll = 100;
        let scroll_up = 25;
        let previous_viewport_start = previous_max_scroll - scroll_up;

        let new_max_scroll = 107;
        let anchored = anchored_scroll(scroll_up, Some(previous_max_scroll), new_max_scroll);

        assert_eq!(new_max_scroll - anchored, previous_viewport_start);
        assert_eq!(anchored, 32);
    }

    #[test]
    fn transcript_growth_still_follows_output_when_at_the_bottom() {
        assert_eq!(anchored_scroll(0, Some(100), 107), 0);
    }

    #[test]
    fn thinking_is_separate_from_visible_answer() {
        let mut t = Transcript::new();
        t.thinking_delta("reason");
        t.text_delta("answer");
        assert!(matches!(t.cells[0], Cell::Thinking { active: false, .. }));
        assert!(matches!(t.cells[1], Cell::Assistant(_)));
    }

    #[test]
    fn active_thinking_has_no_spinner_and_keeps_its_pulsing_summary() {
        let mut t = Transcript::new();
        t.thinking_delta("**Inspecting CLI source and help commands**");

        let mut lines = Vec::new();
        append_cell(&mut lines, &t.cells[0], 120, 0, false);
        let rendered = lines[0].to_string();
        assert!(rendered.contains("Inspecting CLI source and help commands"));
        assert!(!rendered.contains("thinking"));
        assert!(!rendered.contains("**"));
        assert_eq!(rendered, "  Inspecting CLI source and help commands");
        assert_eq!(lines[0].spans[0].content, "  ");
        assert_eq!(
            lines[0].spans[1].style,
            theme::thinking_pulse(0).add_modifier(ratatui::style::Modifier::ITALIC)
        );
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
        );

        let mut next_frame = Vec::new();
        append_cell(&mut next_frame, &t.cells[0], 120, 24, false);
        assert_eq!(next_frame[0].to_string(), rendered);
        assert_ne!(lines[0].spans[1].style, next_frame[0].spans[1].style);
    }

    #[test]
    fn tool_results_correlate_by_id_out_of_order() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "read".into(), "one".into());
        t.tool_start(id("b"), "bash".into(), "two".into());
        t.tool_result(&id("a"), false, "first");
        assert!(matches!(
            t.cells[0],
            Cell::Tool {
                status: ToolStatus::Success,
                ..
            }
        ));
        assert!(matches!(
            t.cells[1],
            Cell::Tool {
                status: ToolStatus::Running,
                ..
            }
        ));
    }

    #[test]
    fn running_tool_blinks_white_then_settles_to_its_result_color() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "bash".into(), "ls".into());

        let mut visible = Vec::new();
        append_cell(&mut visible, &t.cells[0], 120, 0, false);
        assert!(visible[0].to_string().starts_with("⏺ bash(ls)"));
        assert_eq!(visible[0].spans[0].style, theme::tool_running());

        let mut hidden = Vec::new();
        append_cell(&mut hidden, &t.cells[0], 120, TOOL_BLINK_FRAMES, false);
        assert!(!hidden[0].to_string().contains('⏺'));

        t.tool_result(&id("a"), false, "done");
        let mut success = Vec::new();
        append_cell(&mut success, &t.cells[0], 120, TOOL_BLINK_FRAMES, false);
        assert!(success[0].to_string().starts_with("⏺ bash(ls)"));
        assert_eq!(success[0].spans[0].style, theme::success());

        t.tool_start(id("b"), "bash".into(), "false".into());
        t.tool_result(&id("b"), true, "failed");
        let mut error = Vec::new();
        append_cell(&mut error, &t.cells[1], 120, 0, false);
        assert!(error[0].to_string().starts_with("⏺ bash(false)"));
        assert_eq!(error[0].spans[0].style, theme::error());
    }

    #[test]
    fn completed_hosted_tool_can_replace_its_running_summary() {
        let mut t = Transcript::new();
        t.tool_start(id("ws"), "web_search".into(), "searching the web".into());

        let mut running = Vec::new();
        append_cell(&mut running, &t.cells[0], 120, 0, false);
        assert_eq!(running[0].to_string(), "⏺ Searching the web");

        t.tool_result_with_summary(
            &id("ws"),
            false,
            "searched: latest OpenAI models",
            Some("latest OpenAI models"),
        );

        let mut lines = Vec::new();
        append_cell(&mut lines, &t.cells[0], 120, TOOL_BLINK_FRAMES, false);
        assert_eq!(lines[0].to_string(), "⏺ Web search");
        assert_eq!(
            lines[1].to_string(),
            "  ⎿  Searched the web for latest OpenAI models"
        );
        assert!(!lines[0].to_string().contains("searching the web"));
        assert!(!is_expandable_tool(&t.cells[0]));
    }

    #[test]
    fn long_hosted_search_query_wraps_without_duplicate_tool_output() {
        let mut t = Transcript::new();
        let query = "site:platform.openai.com/docs/models OpenAI models official";
        t.tool_start(id("ws"), "web_search".into(), query.into());
        t.tool_result(&id("ws"), false, &format!("searched: {query}"));

        let mut lines = Vec::new();
        append_cell(&mut lines, &t.cells[0], 30, TOOL_BLINK_FRAMES, false);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join(" ")
                .contains("platform.openai.com/docs/models")
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.to_string().contains("Searched the web"))
                .count(),
            1
        );
    }

    #[test]
    fn output_is_retained_beyond_preview_limit() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "bash".into(), "many".into());
        let output = (0..25)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        t.tool_result(&id("a"), false, &output);
        let Cell::Tool {
            result,
            total_lines,
            ..
        } = &t.cells[0]
        else {
            panic!()
        };
        assert_eq!((*total_lines, result.len()), (25, 25));
    }

    #[test]
    fn successful_read_is_collapsed_to_its_line_count() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "read".into(), "src/main.rs".into());
        t.tool_result(&id("a"), false, "     1\tfirst\n     2\tsecond");

        let rendered = render_cell(&t.cells[0]);
        assert!(rendered.contains("Read 2 lines"));
        assert!(!rendered.contains("first"));
    }

    #[test]
    fn bash_preview_only_shows_the_first_few_lines() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "bash".into(), "cargo test".into());
        t.tool_result(&id("a"), false, "one\ntwo\nthree\nfour\nfive");

        let rendered = render_cell(&t.cells[0]);
        assert!(rendered.contains("one"));
        assert!(rendered.contains("three"));
        assert!(!rendered.contains("four"));
        assert!(rendered.contains("… 2 more lines"));
    }

    #[test]
    fn hovering_styles_the_entire_tool_width() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "bash".into(), "cargo test".into());
        t.tool_result(&id("a"), false, "1\n2\n3\n4");
        t.tool_areas.push((0, Rect::new(4, 7, 20, 2)));

        assert!(!t.on_mouse(mouse(MouseEventKind::Moved, 8, 7)));

        let mut lines = Vec::new();
        append_cell(&mut lines, &t.cells[0], 20, 0, t.hovered_tool == Some(0));
        assert_eq!(lines[0].width(), 20);
        assert!(lines[0].style.bg.is_some());
    }

    #[test]
    fn fully_visible_tool_results_are_not_expandable() {
        let mut t = Transcript::new();
        t.tool_start(id("glob"), "glob".into(), "**/package.json".into());
        t.tool_result(&id("glob"), false, "(no matches)");
        assert!(!is_expandable_tool(&t.cells[0]));

        t.tool_start(id("short"), "bash".into(), "printf ok".into());
        t.tool_result(&id("short"), false, "one\ntwo\nthree");
        assert!(!is_expandable_tool(&t.cells[1]));

        t.tool_start(id("long"), "bash".into(), "many lines".into());
        t.tool_result(&id("long"), false, "one\ntwo\nthree\nfour");
        assert!(is_expandable_tool(&t.cells[2]));
    }

    #[test]
    fn read_is_expandable_only_when_it_has_content_to_reveal() {
        let mut t = Transcript::new();
        t.tool_start(id("empty"), "read".into(), "empty.txt".into());
        t.tool_result(&id("empty"), false, "(empty file)");
        assert!(!is_expandable_tool(&t.cells[0]));
        assert!(render_cell(&t.cells[0]).contains("Read empty file"));

        t.tool_start(id("legacy-empty"), "read".into(), "blank.txt".into());
        t.tool_result(&id("legacy-empty"), false, "     1\t");
        assert!(!is_expandable_tool(&t.cells[1]));
        assert!(render_cell(&t.cells[1]).contains("Read empty file"));

        t.tool_start(id("content"), "read".into(), "src/main.rs".into());
        t.tool_result(&id("content"), false, "     1\tfn main() {}");
        assert!(is_expandable_tool(&t.cells[2]));
    }

    #[test]
    fn expanded_read_separates_line_numbers_from_source() {
        let mut t = Transcript::new();
        t.tool_start(id("source"), "read".into(), "src/lib.rs".into());
        t.tool_result(
            &id("source"),
            false,
            "     1\tmod app;\n     2\t    mod nested;",
        );
        let Cell::Tool { expanded, .. } = &mut t.cells[0] else {
            panic!();
        };
        *expanded = true;

        let rendered = render_cell(&t.cells[0]);
        assert!(rendered.contains("1  mod app;"));
        assert!(rendered.contains("2      mod nested;"));
        assert!(!rendered.contains("1mod app;"));
    }

    #[test]
    fn clicking_toggles_only_the_target_tool_output() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "bash".into(), "first".into());
        t.tool_result(&id("a"), false, "1\n2\n3\n4\n5");
        t.tool_start(id("b"), "bash".into(), "second".into());
        t.tool_result(&id("b"), false, "a\nb\nc\nd\ne");
        t.tool_areas = vec![(0, Rect::new(0, 0, 40, 5)), (1, Rect::new(0, 6, 40, 5))];

        assert!(t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1)));
        assert!(render_cell(&t.cells[0]).contains("5"));
        assert!(render_cell(&t.cells[1]).contains("… 2 more lines"));

        assert!(t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1)));
        assert!(render_cell(&t.cells[0]).contains("… 2 more lines"));
    }

    #[test]
    fn expanding_a_read_reveals_its_retained_content() {
        let mut t = Transcript::new();
        t.tool_start(id("a"), "read".into(), "src/main.rs".into());
        t.tool_result(&id("a"), false, "     1\tfirst\n     2\tsecond");
        t.tool_areas.push((0, Rect::new(0, 0, 40, 2)));

        t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1));

        let rendered = render_cell(&t.cells[0]);
        assert!(rendered.contains("first"));
        assert!(!rendered.contains("Read 2 lines"));
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn render_cell(cell: &Cell) -> String {
        let mut lines = Vec::new();
        append_cell(&mut lines, cell, 120, 0, false);
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_wraps_and_preserves_code_newlines() {
        let mut lines = Vec::new();
        let mut markdown = MarkdownBuffer::default();
        markdown.push("abcdefghi\n\n```rust\na();\nb();\n```");
        append_markdown(&mut lines, &markdown, 10);
        assert!(lines.iter().all(|line| line.width() <= 10));
        let rendered = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("a();"));
        assert!(rendered.contains("b();"));
    }
}
