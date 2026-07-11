use std::sync::OnceLock;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme;

#[derive(Default)]
pub(crate) struct MarkdownBuffer {
    frozen: Vec<Token>,
    open: String,
}

impl MarkdownBuffer {
    pub(crate) fn push(&mut self, delta: &str) {
        self.open.push_str(delta);
        while let Some(end) = completed_block_end(&self.open) {
            self.frozen.extend(parse(&self.open[..end]));
            self.open.drain(..end);
        }
    }

    pub(crate) fn render(&self, width: usize) -> Vec<Line<'static>> {
        let open = parse(&self.open);
        render(self.frozen.iter().chain(&open), width)
    }

    #[cfg(test)]
    pub(crate) fn has_frozen_blocks(&self) -> bool {
        !self.frozen.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn open_source(&self) -> &str {
        &self.open
    }
}

fn completed_block_end(source: &str) -> Option<usize> {
    let mut fence: Option<(char, usize)> = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next();
        if matches!(marker, Some('`' | '~')) {
            let marker = marker.expect("checked above");
            let count = trimmed.chars().take_while(|ch| *ch == marker).count();
            if count >= 3 {
                match fence {
                    None => fence = Some((marker, count)),
                    Some((open_marker, open_count))
                        if marker == open_marker && count >= open_count =>
                    {
                        fence = None;
                    }
                    Some(_) => {}
                }
            }
        }
        offset += line.len();
        if fence.is_none() && source[offset..].starts_with('\n') {
            return Some(offset + 1);
        }
    }
    None
}

#[derive(Clone)]
enum Token {
    Text(String, Style),
    InlineCode(String),
    Break,
    ParagraphEnd,
    HeadingStart(u8),
    HeadingEnd,
    QuoteStart(Option<BlockQuoteKind>),
    QuoteEnd,
    Item(String),
    Task(bool),
    CodeBlock { language: String, code: String },
    Rule,
    Table(Table),
    Footnote(String),
}

#[derive(Clone, Default)]
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Inline>>>,
    header_rows: usize,
}

#[derive(Clone)]
struct Inline {
    text: String,
    style: Style,
}

#[derive(Default)]
struct InlineStyle {
    strong: usize,
    emphasis: usize,
    strike: usize,
    link: usize,
}

impl InlineStyle {
    fn style(&self) -> Style {
        let mut style = Style::new();
        if self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link > 0 {
            style = style.patch(theme::link());
        }
        style
    }
}

struct ListState {
    next: Option<u64>,
}

struct TableBuilder {
    table: Table,
    row: Vec<Vec<Inline>>,
    cell: Vec<Inline>,
    in_head: bool,
}

fn parse(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut inline = InlineStyle::default();
    let mut lists: Vec<ListState> = Vec::new();
    let mut code: Option<(String, String)> = None;
    let mut table: Option<TableBuilder> = None;
    let options = Options::all();

    for event in Parser::new_ext(source, options) {
        if let Some((_, contents)) = &mut code {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let (language, code_text) = code.take().expect("code block is open");
                    tokens.push(Token::CodeBlock {
                        language,
                        code: code_text,
                    });
                }
                Event::Text(text) | Event::Code(text) => contents.push_str(&text),
                _ => {}
            }
            continue;
        }

        if table.is_some() {
            handle_table_event(event, &mut table, &mut tokens, &mut inline);
            continue;
        }

        match event {
            Event::Start(Tag::Strong) => inline.strong += 1,
            Event::End(TagEnd::Strong) => inline.strong = inline.strong.saturating_sub(1),
            Event::Start(Tag::Emphasis) => inline.emphasis += 1,
            Event::End(TagEnd::Emphasis) => inline.emphasis = inline.emphasis.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => inline.strike += 1,
            Event::End(TagEnd::Strikethrough) => inline.strike = inline.strike.saturating_sub(1),
            Event::Start(Tag::Link { .. }) => inline.link += 1,
            Event::End(TagEnd::Link) => inline.link = inline.link.saturating_sub(1),
            Event::Start(Tag::Heading { level, .. }) => {
                tokens.push(Token::HeadingStart(heading_level(level)));
            }
            Event::End(TagEnd::Heading(_)) => tokens.push(Token::HeadingEnd),
            Event::Start(Tag::BlockQuote(kind)) => tokens.push(Token::QuoteStart(kind)),
            Event::End(TagEnd::BlockQuote(_)) => tokens.push(Token::QuoteEnd),
            Event::Start(Tag::List(start)) => lists.push(ListState { next: start }),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                tokens.push(if lists.is_empty() {
                    Token::ParagraphEnd
                } else {
                    Token::Break
                });
            }
            Event::Start(Tag::Item) => {
                let depth = lists.len().saturating_sub(1);
                let marker = match lists.last_mut().and_then(|list| list.next.as_mut()) {
                    Some(next) => {
                        let marker = format!("{next}.");
                        *next = next.saturating_add(1);
                        marker
                    }
                    None => "•".to_owned(),
                };
                tokens.push(Token::Item(format!("{}{} ", "  ".repeat(depth), marker)));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(info) => normalize_language(&info),
                };
                code = Some((language, String::new()));
            }
            Event::Start(Tag::Table(alignments)) => {
                table = Some(TableBuilder {
                    table: Table {
                        alignments,
                        ..Table::default()
                    },
                    row: Vec::new(),
                    cell: Vec::new(),
                    in_head: false,
                });
            }
            Event::Text(text) => tokens.push(Token::Text(text.into_string(), inline.style())),
            Event::Code(text) => tokens.push(Token::InlineCode(text.into_string())),
            Event::SoftBreak => tokens.push(Token::Text(" ".into(), inline.style())),
            Event::HardBreak => tokens.push(Token::Break),
            Event::End(TagEnd::Paragraph) => {
                if lists.is_empty() {
                    tokens.push(Token::ParagraphEnd);
                } else {
                    tokens.push(Token::Break);
                }
            }
            Event::TaskListMarker(checked) => tokens.push(Token::Task(checked)),
            Event::Rule => tokens.push(Token::Rule),
            Event::FootnoteReference(label) => tokens.push(Token::Footnote(label.into_string())),
            Event::InlineHtml(html) | Event::Html(html) => {
                tokens.push(Token::Text(html.into_string(), theme::dim()));
            }
            _ => {}
        }
    }
    if let Some((language, contents)) = code {
        tokens.push(Token::CodeBlock {
            language,
            code: contents,
        });
    }
    tokens
}

fn handle_table_event(
    event: Event<'_>,
    builder: &mut Option<TableBuilder>,
    tokens: &mut Vec<Token>,
    inline: &mut InlineStyle,
) {
    let table = builder.as_mut().expect("table is open");
    match event {
        Event::Start(Tag::TableHead) => table.in_head = true,
        Event::End(TagEnd::TableHead) => {
            if !table.row.is_empty() {
                table.table.rows.push(std::mem::take(&mut table.row));
                table.table.header_rows += 1;
            }
            table.in_head = false;
        }
        Event::Start(Tag::TableRow) => table.row.clear(),
        Event::End(TagEnd::TableRow) => {
            table.table.rows.push(std::mem::take(&mut table.row));
            if table.in_head {
                table.table.header_rows += 1;
            }
        }
        Event::Start(Tag::TableCell) => table.cell.clear(),
        Event::End(TagEnd::TableCell) => table.row.push(std::mem::take(&mut table.cell)),
        Event::Start(Tag::Strong) => inline.strong += 1,
        Event::End(TagEnd::Strong) => inline.strong = inline.strong.saturating_sub(1),
        Event::Start(Tag::Emphasis) => inline.emphasis += 1,
        Event::End(TagEnd::Emphasis) => inline.emphasis = inline.emphasis.saturating_sub(1),
        Event::Start(Tag::Strikethrough) => inline.strike += 1,
        Event::End(TagEnd::Strikethrough) => inline.strike = inline.strike.saturating_sub(1),
        Event::Start(Tag::Link { .. }) => inline.link += 1,
        Event::End(TagEnd::Link) => inline.link = inline.link.saturating_sub(1),
        Event::Text(text) => table.cell.push(Inline {
            text: text.into_string(),
            style: inline.style(),
        }),
        Event::Code(text) => table.cell.push(Inline {
            text: text.into_string(),
            style: theme::inline_code(),
        }),
        Event::SoftBreak | Event::HardBreak => table.cell.push(Inline {
            text: " ".into(),
            style: inline.style(),
        }),
        Event::End(TagEnd::Table) => {
            let completed = builder.take().expect("table is open").table;
            tokens.push(Token::Table(completed));
            tokens.push(Token::ParagraphEnd);
        }
        _ => {}
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn normalize_language(info: &str) -> String {
    info.split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch| matches!(ch, '{' | '}' | '.'))
        .to_owned()
}

struct Renderer {
    lines: Vec<Line<'static>>,
    current: Line<'static>,
    width: usize,
    quote_depth: usize,
    heading: Option<u8>,
    first_line: bool,
}

fn render<'a>(tokens: impl Iterator<Item = &'a Token>, width: usize) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        lines: Vec::new(),
        current: Line::default(),
        width: width.max(1),
        quote_depth: 0,
        heading: None,
        first_line: true,
    };
    renderer.start_line();
    for token in tokens {
        match token {
            Token::Text(text, style) => renderer.text(text, *style),
            Token::InlineCode(text) => renderer.text(text, theme::inline_code()),
            Token::Break => renderer.flush(false),
            Token::ParagraphEnd => renderer.flush(true),
            Token::HeadingStart(level) => {
                renderer.flush_content();
                renderer.heading = Some(*level);
            }
            Token::HeadingEnd => {
                renderer.flush(false);
                renderer.heading = None;
            }
            Token::QuoteStart(kind) => {
                renderer.flush_content();
                renderer.quote_depth += 1;
                renderer.current = Line::default();
                renderer.start_line();
                if let Some(kind) = kind {
                    renderer.text(&format!("{}  ", quote_label(*kind)), theme::bold());
                }
            }
            Token::QuoteEnd => {
                renderer.flush(false);
                renderer.quote_depth = renderer.quote_depth.saturating_sub(1);
                renderer.current = Line::default();
                renderer.start_line();
            }
            Token::Item(prefix) => {
                renderer.flush_content();
                renderer.text(prefix, theme::dim());
            }
            Token::Task(checked) => {
                renderer.text(if *checked { "[✓] " } else { "[ ] " }, theme::dim());
            }
            Token::CodeBlock { language, code } => {
                renderer.flush_content();
                renderer.code_block(language, code);
            }
            Token::Rule => {
                renderer.flush_content();
                let available = renderer.width.saturating_sub(2).max(1);
                renderer
                    .lines
                    .push(Line::styled("─".repeat(available), theme::rule()));
                renderer.start_line();
            }
            Token::Table(table) => {
                renderer.flush_content();
                renderer.table(table);
            }
            Token::Footnote(label) => {
                renderer.text(&format!("[{label}]"), theme::link());
            }
        }
    }
    renderer.flush(false);
    while renderer.lines.last().is_some_and(|line| line.width() == 0) {
        renderer.lines.pop();
    }
    renderer.lines
}

impl Renderer {
    fn start_line(&mut self) {
        if !self.current.spans.is_empty() {
            return;
        }
        if self.first_line {
            self.current
                .spans
                .push(Span::styled("⏺ ", theme::assistant_bullet()));
            self.first_line = false;
        } else {
            self.current.spans.push(Span::raw("  "));
        }
        for _ in 0..self.quote_depth {
            self.current.spans.push(Span::styled("│ ", theme::quote()));
        }
    }

    fn text(&mut self, text: &str, style: Style) {
        let style = match self.heading {
            Some(level) => style.patch(theme::heading(level)),
            None if self.quote_depth > 0 => style.patch(theme::quote()),
            None => style,
        };
        let graphemes = text.graphemes(true).collect::<Vec<_>>();
        let mut index = 0;
        while index < graphemes.len() {
            let grapheme = graphemes[index];
            if grapheme == "\n" {
                self.flush(false);
                index += 1;
                continue;
            }
            let whitespace = grapheme.chars().all(char::is_whitespace);
            if whitespace && self.current.width() <= self.line_prefix_width() {
                index += 1;
                continue;
            }
            if !grapheme.chars().all(char::is_whitespace) {
                let run_width = graphemes[index..]
                    .iter()
                    .take_while(|part| **part != "\n" && !part.chars().all(char::is_whitespace))
                    .map(|part| UnicodeWidthStr::width(*part))
                    .sum::<usize>();
                let content_width = self.width.saturating_sub(self.line_prefix_width());
                if run_width <= content_width
                    && self.current.width() + run_width > self.width
                    && self.current.width() > self.line_prefix_width()
                {
                    self.flush(false);
                }
            }
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if self.current.width() + grapheme_width > self.width
                && self.current.width() > self.line_prefix_width()
            {
                self.flush(false);
                if whitespace {
                    index += 1;
                    continue;
                }
            }
            match self.current.spans.last_mut() {
                Some(span) if span.style == style => span.content.to_mut().push_str(grapheme),
                _ => self
                    .current
                    .spans
                    .push(Span::styled(grapheme.to_owned(), style)),
            }
            index += 1;
        }
    }

    fn line_prefix_width(&self) -> usize {
        2 + self.quote_depth * 2
    }

    fn flush_content(&mut self) {
        if self.current.width() > self.line_prefix_width() {
            self.flush(false);
        }
    }

    fn flush(&mut self, blank_after: bool) {
        if self.current.width() > self.line_prefix_width() {
            self.lines.push(std::mem::take(&mut self.current));
        }
        if blank_after && self.lines.last().is_some_and(|line| line.width() > 0) {
            self.lines.push(Line::default());
        }
        self.start_line();
    }

    fn code_block(&mut self, language: &str, code: &str) {
        if self.lines.last().is_some_and(|line| line.width() > 0) {
            self.lines.push(Line::default());
        }
        let terminal = is_terminal_language(language);
        let plain_surface =
            terminal || is_configuration_language(language) || is_plain_text_language(language);
        let syntax_set = syntax_set();
        let syntax = syntax_set
            .find_syntax_by_token(language)
            .or_else(|| syntax_set.find_syntax_by_extension(language))
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, syntax_theme());
        for source_with_ending in code.split_inclusive('\n') {
            let source_line = source_with_ending
                .strip_suffix('\n')
                .unwrap_or(source_with_ending);
            let regions = match highlighter.highlight_line(source_with_ending, syntax_set) {
                Ok(regions) => regions
                    .into_iter()
                    .map(|(style, text)| {
                        let surface = if plain_surface {
                            Style::new()
                        } else {
                            theme::picker_bg()
                        };
                        (
                            syntect_style(style).patch(surface),
                            text.strip_suffix('\n').unwrap_or(text).to_owned(),
                        )
                    })
                    .filter(|(_, text)| !text.is_empty())
                    .collect(),
                Err(_) => vec![(
                    if plain_surface {
                        Style::new()
                    } else {
                        theme::picker_bg()
                    },
                    source_line.to_owned(),
                )],
            };
            let wrapped = if plain_surface {
                wrap_terminal_regions(&regions, self.width.max(1))
            } else {
                wrap_code_surface_regions(&regions, self.width.max(1))
            };
            self.lines.extend(wrapped);
        }
        if code.is_empty() {
            if plain_surface {
                self.lines.push(Line::from("  "));
            } else {
                self.lines.push(Line::styled(
                    " ".repeat(self.width.max(1)),
                    theme::picker_bg(),
                ));
            }
        }
        self.lines.push(Line::default());
        self.current = Line::default();
        self.start_line();
    }

    fn table(&mut self, table: &Table) {
        if table.rows.is_empty() {
            return;
        }
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let available = self.width.saturating_sub(2 + columns + 1 + columns * 2);
        let natural = (0..columns)
            .map(|column| {
                table
                    .rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| cell.iter().map(|part| part.text.width()).sum::<usize>())
                    .max()
                    .unwrap_or(1)
            })
            .collect::<Vec<_>>();
        let widths = fit_columns(&natural, available.max(columns));
        for (row_index, row) in table.rows.iter().enumerate() {
            let mut line = Line::from(Span::styled("  │", theme::table_border()));
            for (column, column_width) in widths.iter().copied().enumerate() {
                line.spans.push(Span::raw(" "));
                let cell = row.get(column).map(Vec::as_slice).unwrap_or_default();
                append_table_cell(
                    &mut line,
                    cell,
                    column_width,
                    table
                        .alignments
                        .get(column)
                        .copied()
                        .unwrap_or(Alignment::None),
                );
                line.spans.push(Span::styled(" │", theme::table_border()));
            }
            self.lines.push(line);
            if row_index + 1 == table.header_rows {
                let mut separator = Line::from(Span::styled("  ├", theme::table_border()));
                for width in &widths {
                    separator
                        .spans
                        .push(Span::styled("─".repeat(width + 2), theme::table_border()));
                    separator
                        .spans
                        .push(Span::styled("┼", theme::table_border()));
                }
                if let Some(last) = separator.spans.last_mut() {
                    last.content = "┤".into();
                }
                self.lines.push(separator);
            }
        }
        self.current = Line::default();
        self.start_line();
    }
}

fn is_plain_text_language(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "" | "text" | "plaintext" | "plain" | "txt"
    )
}

fn is_configuration_language(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "toml"
            | "yaml"
            | "yml"
            | "json"
            | "jsonc"
            | "ini"
            | "conf"
            | "config"
            | "dotenv"
            | "env"
            | "properties"
            | "xml"
    )
}

fn is_terminal_language(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "sh" | "shell"
            | "bash"
            | "zsh"
            | "fish"
            | "console"
            | "terminal"
            | "command"
            | "cmd"
            | "powershell"
            | "pwsh"
            | "ps1"
    )
}

fn quote_label(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "NOTE",
        BlockQuoteKind::Tip => "TIP",
        BlockQuoteKind::Important => "IMPORTANT",
        BlockQuoteKind::Warning => "WARNING",
        BlockQuoteKind::Caution => "CAUTION",
    }
}

fn fit_columns(natural: &[usize], available: usize) -> Vec<usize> {
    let mut widths = natural
        .iter()
        .map(|width| (*width).max(1))
        .collect::<Vec<_>>();
    while widths.iter().sum::<usize>() > available {
        if let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > 1)
            .max_by_key(|(_, width)| **width)
        {
            widths[index] -= 1;
        } else {
            break;
        }
    }
    widths
}

fn append_table_cell(
    line: &mut Line<'static>,
    cell: &[Inline],
    width: usize,
    alignment: Alignment,
) {
    let text_width = cell
        .iter()
        .map(|part| part.text.width())
        .sum::<usize>()
        .min(width);
    let spare = width.saturating_sub(text_width);
    let left = match alignment {
        Alignment::Right => spare,
        Alignment::Center => spare / 2,
        Alignment::None | Alignment::Left => 0,
    };
    line.spans.push(Span::raw(" ".repeat(left)));
    let mut remaining = width.saturating_sub(left);
    for part in cell {
        if remaining == 0 {
            break;
        }
        let clipped = clip_width(&part.text, remaining);
        remaining = remaining.saturating_sub(clipped.width());
        line.spans.push(Span::styled(clipped, part.style));
    }
    line.spans.push(Span::raw(" ".repeat(remaining)));
}

fn clip_width(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = grapheme.width();
        if width + next > max_width {
            break;
        }
        out.push_str(grapheme);
        width += next;
    }
    out
}

fn wrap_terminal_regions(regions: &[(Style, String)], width: usize) -> Vec<Line<'static>> {
    let indent_width = 2.min(width);
    let content_width = width.saturating_sub(indent_width).max(1);
    let mut lines = Vec::new();
    let mut line = Line::from(" ".repeat(indent_width));
    let mut used = 0;
    for (style, text) in regions {
        for grapheme in text.graphemes(true) {
            let grapheme_width = grapheme.width();
            if used + grapheme_width > content_width && used > 0 {
                lines.push(line);
                line = Line::from(" ".repeat(indent_width));
                used = 0;
            }
            line.spans.push(Span::styled(grapheme.to_owned(), *style));
            used += grapheme_width;
        }
    }
    lines.push(line);
    lines
}

fn wrap_code_surface_regions(regions: &[(Style, String)], width: usize) -> Vec<Line<'static>> {
    let indent_width = 2.min(width);
    let content_width = width.saturating_sub(indent_width).max(1);
    let surface = theme::picker_bg();
    let mut lines = Vec::new();
    let mut line = Line::from(Span::styled(" ".repeat(indent_width), surface));
    let mut used = 0;
    for (style, text) in regions {
        for grapheme in text.graphemes(true) {
            let grapheme_width = grapheme.width();
            if used + grapheme_width > content_width && used > 0 {
                pad_line(&mut line, width, surface);
                lines.push(line);
                line = Line::from(Span::styled(" ".repeat(indent_width), surface));
                used = 0;
            }
            line.spans.push(Span::styled(grapheme.to_owned(), *style));
            used += grapheme_width;
        }
    }
    pad_line(&mut line, width, surface);
    lines.push(line);
    lines
}

fn pad_line(line: &mut Line<'static>, width: usize, style: Style) {
    let padding = width.saturating_sub(line.width());
    if padding > 0 {
        line.spans.push(Span::styled(" ".repeat(padding), style));
    }
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn syntax_theme() -> &'static Theme {
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    let themes = &THEMES.get_or_init(ThemeSet::load_defaults).themes;
    let preferred = if theme::terminal_is_light() {
        "InspiredGitHub"
    } else {
        "base16-ocean.dark"
    };
    themes
        .get(preferred)
        .or_else(|| themes.values().next())
        .expect("syntect ships at least one default theme")
}

fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let mut result = Style::new().fg(theme::code_color((
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    )));
    if style.font_style.contains(FontStyle::BOLD) {
        result = result.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        result = result.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        result = result.add_modifier(Modifier::UNDERLINED);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(source: &str, width: usize) -> String {
        let mut markdown = MarkdownBuffer::default();
        markdown.push(source);
        markdown
            .render(width)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn keeps_blank_lines_inside_streaming_fences_open() {
        let mut markdown = MarkdownBuffer::default();
        markdown.push("```rust\na();\n\nb();");
        assert!(!markdown.has_frozen_blocks());
        markdown.push("\n```\n\nnext");
        assert!(markdown.has_frozen_blocks());
        assert_eq!(markdown.open_source(), "next");
    }

    #[test]
    fn renders_rich_blocks() {
        let output = rendered(
            "# Heading\n\n> quote\n\n1. one\n2. two\n\n- [x] done\n\n---\n",
            60,
        );
        assert!(output.contains("Heading"), "{output:?}");
        assert!(output.contains("│ quote"), "{output:?}");
        assert!(output.contains("1. one"), "{output:?}");
        assert!(output.contains("[✓] done"), "{output:?}");
        assert!(output.contains('─'), "{output:?}");
    }

    #[test]
    fn renders_tables_with_aligned_borders() {
        let output = rendered("| Name | Value |\n|:-----|------:|\n| a | 12 |", 50);
        assert!(output.contains("Name"), "{output:?}");
        assert!(output.contains("Value"), "{output:?}");
        assert!(output.contains('┼'), "{output:?}");
    }

    #[test]
    fn separates_a_top_level_list_from_the_following_paragraph() {
        let output = rendered("- First item\n- Last item\n\nFollowing paragraph", 60);
        assert!(
            output.contains("• Last item\n\n  Following paragraph"),
            "{output:?}"
        );
    }

    #[test]
    fn separates_a_table_from_the_following_paragraph() {
        let output = rendered(
            "| Need | Model |\n|---|---|\n| Coding | Codex |\n\nFollowing paragraph",
            60,
        );
        assert!(
            output.contains("Codex") && output.contains("│\n\n  Following paragraph"),
            "{output:?}"
        );
    }

    #[test]
    fn highlights_known_code_and_falls_back_for_unknown_languages() {
        let rust = rendered("```rust\nfn main() {}\n```", 50);
        let unknown = rendered("```made-up\nhello\n```", 50);
        assert!(rust.contains("fn main() {}"));
        assert!(unknown.contains("hello"));
    }

    #[test]
    fn language_fences_produce_multiple_syntax_colors() {
        for (language, code) in [
            ("rust", "pub fn answer() -> u32 { 42 }"),
            ("typescript", "const answer: number = 42;"),
        ] {
            let mut markdown = MarkdownBuffer::default();
            markdown.push(&format!("```{language}\n{code}\n```"));
            let lines = markdown.render(80);
            let colors = lines
                .iter()
                .find(|line| line.to_string().contains(code))
                .expect("code line")
                .spans
                .iter()
                .skip(1)
                .filter(|span| !span.content.trim().is_empty())
                .filter_map(|span| span.style.fg)
                .collect::<std::collections::HashSet<_>>();

            assert!(
                colors.len() > 1,
                "expected syntax colors for {language}, got {colors:?}"
            );
        }
    }

    #[test]
    fn shell_block_has_no_title_border_or_background() {
        let mut markdown = MarkdownBuffer::default();
        markdown.push("```sh\ncargo test\n```");
        let lines = markdown.render(24);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "  cargo test");
        assert!(
            lines.iter().all(|line| line.style.bg.is_none()
                && line.spans.iter().all(|span| span.style.bg.is_none()))
        );
    }

    #[test]
    fn terminal_fence_aliases_use_plain_rendering() {
        for language in [
            "bash",
            "zsh",
            "fish",
            "console",
            "terminal",
            "cmd",
            "powershell",
            "pwsh",
            "ps1",
        ] {
            let mut markdown = MarkdownBuffer::default();
            markdown.push(&format!("```{language}\necho hello\n```"));
            let lines = markdown.render(24);

            assert_eq!(lines.len(), 1, "{language}");
            assert_eq!(lines[0].to_string(), "  echo hello", "{language}");
            assert!(
                lines[0].spans.iter().all(|span| span.style.bg.is_none()),
                "{language}"
            );
        }
    }

    #[test]
    fn plain_text_and_unlabelled_fences_have_no_background() {
        for source in [
            "```text\ncargo test --workspace\n```",
            "```plaintext\ncargo test --workspace\n```",
            "```\ncargo test --workspace\n```",
        ] {
            let mut markdown = MarkdownBuffer::default();
            markdown.push(source);
            let lines = markdown.render(32);

            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].to_string(), "  cargo test --workspace");
            assert!(
                lines[0].spans.iter().all(|span| span.style.bg.is_none()),
                "plain-text fence unexpectedly had a background: {source}"
            );
        }
    }

    #[test]
    fn configuration_fences_keep_highlighting_without_a_background() {
        let mut markdown = MarkdownBuffer::default();
        markdown.push("```toml\nprovider = \"deepseek\"\nauth = \"api_key\"\n```");
        let lines = markdown.render(32);

        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| line.spans.iter().all(|span| span.style.bg.is_none()))
        );
        let colors = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| !span.content.trim().is_empty())
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(
            colors.len() > 1,
            "configuration syntax should stay highlighted"
        );
    }

    #[test]
    fn non_shell_code_uses_palette_surface_without_chrome() {
        let mut markdown = MarkdownBuffer::default();
        markdown.push("```typescript\nconst answer = 42;\n```");
        let lines = markdown.render(32);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 32);
        assert!(!lines[0].to_string().contains("TypeScript"));
        assert!(!lines[0].to_string().contains('│'));
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.style.bg == theme::picker_bg().bg)
        );
    }

    #[test]
    fn code_block_is_separated_from_following_text() {
        let mut markdown = MarkdownBuffer::default();
        markdown.push("```sh\ncargo test\n```\n\nFollowing paragraph");
        let lines = markdown.render(40);

        let body = lines
            .iter()
            .position(|line| line.to_string().contains("cargo test"))
            .expect("code body");
        assert_eq!(lines[body + 1].width(), 0);
        assert!(lines[body + 2].to_string().contains("Following paragraph"));
    }

    #[test]
    fn code_block_is_separated_from_preceding_non_paragraph_block() {
        let mut markdown = MarkdownBuffer::default();
        markdown.push("## Application commands\n```sh\ncargo run -p tokio-agent\n```");
        let lines = markdown.render(40);

        let title = lines
            .iter()
            .position(|line| line.to_string().contains("Application commands"))
            .expect("heading");
        assert_eq!(lines[title + 1].width(), 0);
        assert_eq!(lines[title + 2].to_string(), "  cargo run -p tokio-agent");
    }

    #[test]
    fn wraps_using_terminal_width_for_wide_unicode() {
        let lines = rendered("界界界", 6);
        assert!(lines.lines().count() >= 2);
    }

    #[test]
    fn wraps_a_word_with_its_trailing_punctuation() {
        let output = rendered("1234 abc.", 10);
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines, ["⏺ 1234 ", "  abc."]);
        assert!(lines.iter().all(|line| *line != "  ."));
    }

    #[test]
    fn prose_wraps_without_leading_separator_spaces() {
        let source = "It lets a developer describe a coding task in natural language. The AI can then inspect the current project, search files, edit code, run shell commands, and report the result.";
        let mut markdown = MarkdownBuffer::default();
        markdown.push(source);

        for width in 20..=100 {
            let lines = markdown.render(width);
            assert!(
                lines
                    .iter()
                    .skip(1)
                    .all(|line| !line.to_string().starts_with("   ")),
                "continuation at width {width} gained an extra leading space: {lines:?}"
            );
        }
    }
}
