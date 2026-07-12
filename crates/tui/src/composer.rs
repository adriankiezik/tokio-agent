use std::cell::RefCell;
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::theme;

const MAX_VISIBLE_ROWS: usize = 8;
const PROMPT_COLS: u16 = 2;
const RIGHT_MARGIN: u16 = 1;

pub(super) struct Composer {
    buffer: String,
    cursor: usize,
    view: RefCell<View>,
}

#[derive(Default)]
struct View {
    rows: Vec<Range<usize>>,
    scroll: usize,
}

impl Composer {
    pub(super) fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            view: RefCell::new(View::default()),
        }
    }

    pub(super) fn insert(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(super) fn insert_str(&mut self, text: &str) {
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub(super) fn insert_newline(&mut self) {
        self.insert('\n');
    }

    pub(super) fn backspace(&mut self) {
        if let Some(c) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.buffer.remove(self.cursor);
        }
    }

    pub(super) fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    pub(super) fn move_left(&mut self) {
        if let Some(c) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub(super) fn move_right(&mut self) {
        if let Some(c) = self.buffer[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub(super) fn move_home(&mut self) {
        if let Some(row) = self.current_row() {
            self.cursor = row.start.min(self.buffer.len());
        }
    }

    pub(super) fn move_end(&mut self) {
        if let Some(row) = self.current_row() {
            self.cursor = row.end.min(self.buffer.len());
        }
    }

    pub(super) fn move_up(&mut self) {
        self.move_vertical(-1);
    }

    pub(super) fn move_down(&mut self) {
        self.move_vertical(1);
    }

    pub(super) fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    pub(super) fn text(&self) -> &str {
        &self.buffer
    }

    pub(super) fn is_single_line(&self) -> bool {
        !self.buffer.contains('\n')
    }

    pub(super) fn replace(&mut self, text: &str) {
        self.buffer.clear();
        self.buffer.push_str(text);
        self.cursor = self.buffer.len();
    }

    pub(super) fn height(&self, total_width: u16) -> u16 {
        let inner = total_width
            .saturating_sub(PROMPT_COLS + RIGHT_MARGIN)
            .max(1);
        let rows = wrap_rows(&self.buffer, usize::from(inner)).len();
        u16::try_from(rows.clamp(1, MAX_VISIBLE_ROWS) + 2)
            .expect("composer height is bounded by MAX_VISIBLE_ROWS")
    }

    pub(super) fn render(&self, frame: &mut Frame, area: Rect, placeholder: &str) {
        frame.render_widget(Block::default().style(theme::composer_bg()), area);
        let inner = Rect {
            x: area.x + PROMPT_COLS,
            y: area.y + 1,
            width: area.width.saturating_sub(PROMPT_COLS + RIGHT_MARGIN),
            height: area.height.saturating_sub(2),
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Line::styled("› ", theme::prompt())),
            Rect {
                x: area.x,
                y: inner.y,
                width: PROMPT_COLS,
                height: 1,
            },
        );
        if self.buffer.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(placeholder, theme::placeholder()))),
                inner,
            );
            frame.set_cursor_position(Position::new(inner.x, inner.y));
            self.view.replace(View::default());
            return;
        }
        let rows = wrap_rows(&self.buffer, usize::from(inner.width));
        let (cursor_row, cursor_col) = cursor_row_col(&self.buffer, &rows, self.cursor);
        let scroll = effective_scroll(
            rows.len(),
            usize::from(inner.height),
            cursor_row,
            self.view.borrow().scroll,
        );
        let visible: Vec<Line> = rows
            .iter()
            .skip(scroll)
            .take(usize::from(inner.height))
            .map(|r| Line::raw(self.buffer[r.clone()].to_string()))
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);
        frame.set_cursor_position(Position::new(
            inner.x
                + u16::try_from(cursor_col)
                    .unwrap_or(u16::MAX)
                    .min(inner.width - 1),
            inner.y + u16::try_from(cursor_row - scroll).unwrap_or(u16::MAX),
        ));
        self.view.replace(View { rows, scroll });
    }

    fn current_row(&self) -> Option<Range<usize>> {
        let view = self.view.borrow();
        let idx = row_index(&view.rows, self.cursor)?;
        Some(view.rows[idx].clone())
    }

    fn move_vertical(&mut self, delta: isize) {
        let view = self.view.borrow();
        let Some(idx) = row_index(&view.rows, self.cursor) else {
            return;
        };
        let Some(target) = idx.checked_add_signed(delta) else {
            return;
        };
        if target >= view.rows.len() {
            return;
        }
        let len = self.buffer.len();
        let row = &view.rows[idx];
        let col = self.buffer[row.start.min(len)..self.cursor.min(len)]
            .chars()
            .count();
        let target = &view.rows[target];
        let range = target.start.min(len)..target.end.min(len);
        let offset: usize = self.buffer[range.clone()]
            .chars()
            .take(col)
            .map(char::len_utf8)
            .sum();
        let cursor = range.start + offset;
        drop(view);
        self.cursor = cursor;
    }
}

fn row_index(rows: &[Range<usize>], cursor: usize) -> Option<usize> {
    rows.iter().rposition(|r| r.start <= cursor)
}

fn cursor_row_col(text: &str, rows: &[Range<usize>], cursor: usize) -> (usize, usize) {
    let Some(idx) = row_index(rows, cursor) else {
        return (0, 0);
    };
    let row = &rows[idx];
    let col = text[row.start..cursor.max(row.start).min(text.len())]
        .chars()
        .count();
    (idx, col)
}

fn effective_scroll(total: usize, height: usize, cursor_row: usize, current: usize) -> usize {
    if total <= height {
        return 0;
    }
    let mut scroll = current.min(total - height);
    if cursor_row < scroll {
        scroll = cursor_row;
    } else if cursor_row >= scroll + height {
        scroll = cursor_row + 1 - height;
    }
    scroll
}

fn wrap_rows(text: &str, width: usize) -> Vec<Range<usize>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        wrap_line(text, line_start, line_start + line.len(), width, &mut rows);
        line_start += line.len() + 1;
    }
    rows
}

fn wrap_line(text: &str, start: usize, end: usize, width: usize, rows: &mut Vec<Range<usize>>) {
    let mut row_start = start;
    let mut col = 0;
    let mut last_space = None;
    for (off, c) in text[start..end].char_indices() {
        let i = start + off;
        if col == width {
            if c == ' ' {
                rows.push(row_start..i);
                row_start = i + 1;
                col = 0;
                last_space = None;
                continue;
            }
            let break_at = match last_space {
                Some(sp) if sp > row_start => sp,
                _ => i,
            };
            rows.push(row_start..break_at);
            row_start = if break_at == i { i } else { break_at + 1 };
            col = text[row_start..i].chars().count();
            last_space = None;
        }
        if c == ' ' {
            last_space = Some(i);
        }
        col += 1;
    }
    rows.push(row_start..end);
}
