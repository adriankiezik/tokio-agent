use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use tokio_agent_core::agent::{Agent, AgentEvent, UiCommand};
use tokio_agent_core::permission::Mode;
use tokio_agent_core::provider::Provider;

use crate::projection::{FrontendEffect, FrontendProjection};
use crate::provider_setup::configure_provider_in;
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Quit,
    ConfigureProvider,
}

pub fn run<P: Provider + 'static>(agent: Agent<P>) -> io::Result<RunOutcome> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    theme::init_terminal_bg(query_terminal_bg());
    let session = SessionDisplay {
        provider: agent.provider_name().to_owned(),
        model: agent.model().to_owned(),
        effort: agent.reasoning_effort().map(str::to_owned),
        cwd: agent.cwd().to_path_buf(),
        context_window: agent.context_before_compaction(),
        max_output_tokens: agent.max_output_tokens(),
        permission_mode: agent.permission_mode(),
        history: tokio_agent_config::recent_messages().unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to load recent message history");
            Vec::new()
        }),
    };
    let cwd = session.cwd.clone();
    let mut terminal = ratatui::init();
    let restore = TerminalRestore;
    execute!(
        io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
    )?;
    let (commands_tx, commands_rx) = unbounded_channel();
    let (events_tx, events_rx) = unbounded_channel();
    let mut agent_task = handle.spawn(agent.run(commands_rx, events_tx));
    let mut app = App::new(commands_tx, events_rx, session);
    let result = loop {
        match app.event_loop(&mut terminal) {
            Ok(RunOutcome::ConfigureProvider) => {
                let mut draw_background = |frame: &mut Frame, height| {
                    app.drain_events();
                    app.spinner = app.spinner.wrapping_add(1);
                    app.draw_with_provider_panel(frame, height)
                };
                let changed =
                    configure_provider_in(&mut terminal, &runtime, &cwd, &mut draw_background)?;
                let during_turn = app.projection.is_running();
                if changed && app.provider_selection_changed(during_turn) {
                    break Ok(RunOutcome::ConfigureProvider);
                }
                app.quit = false;
                app.outcome = RunOutcome::Quit;
            }
            result => break result,
        }
    };
    drop(restore);
    let (agent_result, forced) = runtime.block_on(async {
        if let Ok(result) = tokio::time::timeout(Duration::from_secs(2), &mut agent_task).await {
            (result, false)
        } else {
            agent_task.abort();
            (agent_task.await, true)
        }
    });
    match (result, agent_result) {
        (Err(err), _) => Err(err),
        (Ok(outcome), Err(_)) if forced => Ok(outcome),
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(err)) => Err(io::Error::other(format!("agent task failed: {err}"))),
    }
}

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        ratatui::restore();
    }
}

fn query_terminal_bg() -> Option<(u8, u8, u8)> {
    terminal_colorsaurus::background_color(terminal_colorsaurus::QueryOptions::default())
        .ok()
        .map(|color| color.scale_to_8bit())
}

struct App {
    projection: FrontendProjection,
    commands_tx: UnboundedSender<UiCommand>,
    events_rx: UnboundedReceiver<AgentEvent>,
    spinner: usize,
    quit: bool,
    outcome: RunOutcome,
    selection: Option<Selection>,
    selection_area: ratatui::layout::Rect,
    visible_text: Vec<Vec<String>>,
    provider_restart_pending: bool,
    provider_restart_ready: bool,
}

struct SessionDisplay {
    provider: String,
    model: String,
    effort: Option<String>,
    cwd: std::path::PathBuf,
    context_window: Option<u64>,
    max_output_tokens: u32,
    permission_mode: Mode,
    history: Vec<String>,
}

impl Default for SessionDisplay {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            effort: None,
            cwd: std::path::PathBuf::new(),
            context_window: None,
            max_output_tokens: 0,
            permission_mode: Mode::Suggest,
            history: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    row: u16,
    column: u16,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: TextPoint,
    head: TextPoint,
    dragged: bool,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.commands_tx.send(UiCommand::Shutdown);
    }
}

impl App {
    fn new(
        commands_tx: UnboundedSender<UiCommand>,
        events_rx: UnboundedReceiver<AgentEvent>,
        session: SessionDisplay,
    ) -> Self {
        Self {
            projection: FrontendProjection::new(
                session.provider,
                session.model,
                session.effort,
                display_path(&session.cwd),
                session.context_window,
                session.max_output_tokens,
                session.permission_mode,
                session.history,
            ),
            commands_tx,
            events_rx,
            spinner: 0,
            quit: false,
            outcome: RunOutcome::Quit,
            selection: None,
            selection_area: ratatui::layout::Rect::default(),
            visible_text: Vec::new(),
            provider_restart_pending: false,
            provider_restart_ready: false,
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> io::Result<RunOutcome> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            self.drain_events();
            if event::poll(Duration::from_millis(16))? {
                self.handle_event(&event::read()?)?;
                for _ in 0..255 {
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                    self.handle_event(&event::read()?)?;
                }
            }
            self.spinner = self.spinner.wrapping_add(1);
        }
        Ok(self.outcome)
    }

    fn handle_event(&mut self, event: &Event) -> io::Result<()> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(*key),
            Event::Paste(text) => {
                self.selection = None;
                self.projection.on_paste(text);
            }
            Event::Mouse(mouse) => self.on_mouse(*mouse)?,
            _ => {}
        }
        Ok(())
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            let turn_done = matches!(&event, AgentEvent::TurnDone(_));
            self.projection.apply(event);
            if turn_done && self.provider_restart_pending {
                self.provider_restart_ready = true;
                self.finish_provider_restart_if_ready();
            }
        }
    }

    fn provider_selection_changed(&mut self, during_turn: bool) -> bool {
        if !during_turn {
            return true;
        }
        self.provider_restart_pending = true;
        false
    }

    fn finish_provider_restart_if_ready(&mut self) {
        if self.provider_restart_ready && !self.projection.provider_change_notice_visible() {
            self.outcome = RunOutcome::ConfigureProvider;
            self.quit = true;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Modifier(_)) {
            return;
        }
        if key.modifiers.contains(KeyModifiers::SUPER) {
            if is_copy_key(key)
                && let Some(selection) = self.selection.filter(|selection| selection.dragged)
            {
                let text = selected_text(&self.visible_text, selection);
                if !text.is_empty() {
                    let _ = copy_text(&text);
                }
            }
            return;
        }
        self.selection = None;
        match self.projection.on_key(key) {
            FrontendEffect::None => {}
            FrontendEffect::Quit => self.quit = true,
            FrontendEffect::ConfigureProvider => {
                self.outcome = RunOutcome::ConfigureProvider;
                self.quit = true;
            }
            FrontendEffect::Copy(text) => {
                let _ = copy_text(&text);
            }
            FrontendEffect::Command(command) => {
                match &command {
                    UiCommand::SetPermissionMode(mode) => {
                        let mode = match mode {
                            Mode::Suggest => tokio_agent_config::PermissionMode::Suggest,
                            Mode::AutoEdit => tokio_agent_config::PermissionMode::AutoEdit,
                            Mode::FullAuto => tokio_agent_config::PermissionMode::FullAuto,
                        };
                        if let Err(error) = tokio_agent_config::store_permission_mode(mode) {
                            tracing::warn!(%error, "failed to persist permission mode");
                        }
                    }
                    UiCommand::SetModel(model) => {
                        if let Err(error) = tokio_agent_config::store_model_selection(model) {
                            tracing::warn!(%error, "failed to persist model selection");
                        }
                    }
                    UiCommand::SetReasoningEffort(Some(effort)) => {
                        if let Err(error) = tokio_agent_config::store_reasoning_effort(effort) {
                            tracing::warn!(%error, "failed to persist reasoning effort");
                        }
                    }
                    UiCommand::SetReasoningEffort(None) => {}
                    UiCommand::UserMessage(message) => {
                        if let Err(error) = tokio_agent_config::store_recent_message(message) {
                            tracing::warn!(%error, "failed to persist recent message");
                        }
                    }
                    _ => {}
                }
                let submission = matches!(command, UiCommand::UserMessage(_));
                if self.commands_tx.send(command).is_err() && submission {
                    self.projection.submission_failed();
                }
            }
        }
        self.finish_provider_restart_if_ready();
    }

    fn on_mouse(&mut self, event: MouseEvent) -> io::Result<()> {
        if self.projection.on_mouse(event) {
            self.selection = None;
            return Ok(());
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = self.text_point(event).map(|point| Selection {
                    anchor: point,
                    head: point,
                    dragged: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(point) = self.text_point(event)
                    && let Some(selection) = &mut self.selection
                {
                    selection.head = point;
                    selection.dragged |= selection.head != selection.anchor;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(point) = self.text_point(event)
                    && let Some(selection) = &mut self.selection
                {
                    selection.head = point;
                    selection.dragged |= selection.head != selection.anchor;
                }
                if let Some(selection) = self.selection.filter(|selection| selection.dragged) {
                    let text = selected_text(&self.visible_text, selection);
                    if !text.is_empty() {
                        copy_text(&text)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn text_point(&self, event: MouseEvent) -> Option<TextPoint> {
        let area = self.selection_area;
        if !area.contains((event.column, event.row).into()) || area.width == 0 || area.height == 0 {
            return None;
        }
        Some(TextPoint {
            row: event.row - area.y,
            column: event.column - area.x,
        })
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.draw_with_provider_panel(frame, 0);
    }

    fn draw_with_provider_panel(&mut self, frame: &mut Frame, provider_height: u16) -> Rect {
        let [
            transcript_area,
            _transcript_spacing,
            scroll_button_area,
            working_area,
            _working_spacing,
            permissions_area,
            picker_area,
            provider_notice_area,
            provider_area,
            interaction_area,
            _footer_spacing,
            footer_area,
        ] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(self.projection.scroll_button_height()),
            Constraint::Length(self.projection.working_indicator_height()),
            Constraint::Length(self.projection.working_indicator_height()),
            Constraint::Length(self.projection.permissions_panel_height()),
            Constraint::Length(self.projection.slash_picker_height()),
            Constraint::Length(self.projection.provider_change_notice_height()),
            Constraint::Length(provider_height),
            Constraint::Length(self.projection.composer_height(frame.area().width)),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.projection
            .render_transcript(frame, transcript_area, self.spinner);
        self.projection
            .render_scroll_button(frame, scroll_button_area);
        self.projection
            .render_working_indicator(frame, working_area);
        self.projection
            .render_permissions_panel(frame, permissions_area);
        self.projection.render_slash_picker(frame, picker_area);
        self.projection
            .render_provider_change_notice(frame, provider_notice_area);
        self.projection.render_interaction(frame, interaction_area);
        self.projection.render_footer(frame, footer_area);
        self.selection_area = frame.area();
        self.visible_text = snapshot_and_highlight(frame, self.selection_area, self.selection);
        provider_area
    }
}

fn is_copy_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::SUPER)
        && matches!(key.code, KeyCode::Char(character) if character.eq_ignore_ascii_case(&'c'))
}

fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(home)
    {
        return if relative.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", relative.display())
        };
    }
    path.display().to_string()
}

fn snapshot_and_highlight(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    selection: Option<Selection>,
) -> Vec<Vec<String>> {
    let mut visible = Vec::with_capacity(usize::from(area.height));
    for row in 0..area.height {
        let mut cells = Vec::with_capacity(usize::from(area.width));
        let mut column = 0;
        while column < area.width {
            let symbol = frame.buffer_mut()[(area.x + column, area.y + row)]
                .symbol()
                .to_owned();
            let width = Line::raw(&symbol)
                .width()
                .max(1)
                .min(usize::from(area.width - column));
            cells.push(symbol);
            cells.extend((1..width).map(|_| String::new()));
            column += u16::try_from(width).expect("cell width is bounded by the terminal width");
        }
        visible.push(cells);
    }
    if let Some(selection) = selection {
        let (start, end) = ordered(selection);
        for row in start.row..=end.row.min(area.height.saturating_sub(1)) {
            let first = if row == start.row { start.column } else { 0 };
            let last = if row == end.row {
                end.column.min(area.width.saturating_sub(1))
            } else {
                area.width.saturating_sub(1)
            };
            for column in first..=last {
                if let Some(cell) = frame.buffer_mut().cell_mut((area.x + column, area.y + row)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
    visible
}

fn selected_text(visible: &[Vec<String>], selection: Selection) -> String {
    let (start, end) = ordered(selection);
    let mut lines = Vec::new();
    for row in start.row..=end.row {
        let Some(cells) = visible.get(row as usize) else {
            break;
        };
        let first = if row == start.row {
            start.column as usize
        } else {
            0
        };
        let last = if row == end.row {
            end.column as usize + 1
        } else {
            cells.len()
        };
        let line = cells
            .get(first.min(cells.len())..last.min(cells.len()))
            .unwrap_or_default()
            .concat();
        lines.push(line.trim_end().to_owned());
    }
    lines.join("\n").trim_end_matches('\n').to_owned()
}

fn ordered(selection: Selection) -> (TextPoint, TextPoint) {
    if selection.anchor <= selection.head {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    }
}

fn copy_text(text: &str) -> io::Result<()> {
    if copy_native(text).is_ok() {
        return Ok(());
    }
    copy_osc52(text)
}

#[cfg(target_os = "macos")]
fn copy_native(text: &str) -> io::Result<()> {
    copy_with_command("pbcopy", &[], text)
}

#[cfg(target_os = "windows")]
fn copy_native(text: &str) -> io::Result<()> {
    copy_with_command("cmd", &["/C", "clip"], text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn copy_native(text: &str) -> io::Result<()> {
    copy_with_command("wl-copy", &[], text)
        .or_else(|_| copy_with_command("xclip", &["-selection", "clipboard"], text))
        .or_else(|_| copy_with_command("xsel", &["--clipboard", "--input"], text))
}

fn copy_with_command(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("clipboard process has no stdin"))?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "clipboard command exited with {status}"
        )))
    }
}

fn copy_osc52(text: &str) -> io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::ModifierKeyCode;

    fn app() -> (App, UnboundedReceiver<UiCommand>) {
        let (commands_tx, commands_rx) = unbounded_channel();
        let (_events_tx, events_rx) = unbounded_channel();
        (
            App::new(commands_tx, events_rx, SessionDisplay::default()),
            commands_rx,
        )
    }

    #[test]
    fn second_ctrl_c_forces_exit_after_interrupt() {
        let (mut app, mut commands) = app();
        app.projection.begin_turn("test");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        app.on_key(ctrl_c);
        assert!(app.projection.is_interrupting());
        assert!(!app.quit);
        assert!(matches!(commands.try_recv(), Ok(UiCommand::Interrupt)));

        app.on_key(ctrl_c);
        assert!(app.quit);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn escape_interrupt_then_ctrl_c_forces_exit() {
        let (mut app, mut commands) = app();
        app.projection.begin_turn("test");

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.projection.is_interrupting());
        assert!(matches!(commands.try_recv(), Ok(UiCommand::Interrupt)));

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn provider_change_waits_for_active_turn_to_finish() {
        let (mut app, _commands) = app();
        app.projection.begin_turn("test");

        assert!(!app.provider_selection_changed(true));
        assert!(app.provider_restart_pending);
        assert!(!app.quit);

        app.projection.apply(AgentEvent::TurnDone(Ok(
            tokio_agent_core::event::StopReason::EndTurn,
        )));
        app.provider_restart_ready = true;
        app.finish_provider_restart_if_ready();

        assert!(app.quit);
        assert_eq!(app.outcome, RunOutcome::ConfigureProvider);
    }

    #[test]
    fn provider_change_restarts_immediately_between_turns() {
        let (mut app, _commands) = app();

        assert!(app.provider_selection_changed(false));
        assert!(!app.provider_restart_pending);
    }

    #[test]
    fn selection_extracts_multiple_visual_rows_in_both_directions() {
        let visible = vec![
            vec!["a".into(), "b".into(), "c".into(), " ".into()],
            vec!["d".into(), "e".into(), "f".into(), " ".into()],
        ];
        let forward = Selection {
            anchor: TextPoint { row: 0, column: 1 },
            head: TextPoint { row: 1, column: 1 },
            dragged: true,
        };
        let reverse = Selection {
            anchor: forward.head,
            head: forward.anchor,
            dragged: true,
        };

        assert_eq!(selected_text(&visible, forward), "bc\nde");
        assert_eq!(selected_text(&visible, reverse), "bc\nde");
    }

    #[test]
    fn selection_preserves_leading_whitespace() {
        let visible = vec![vec![" ".into(), " ".into(), "x".into()]];
        let selection = Selection {
            anchor: TextPoint { row: 0, column: 0 },
            head: TextPoint { row: 0, column: 2 },
            dragged: true,
        };

        assert_eq!(selected_text(&visible, selection), "  x");
    }

    #[test]
    fn command_modifier_press_preserves_selection() {
        let (mut app, _commands) = app();
        let selection = Selection {
            anchor: TextPoint { row: 0, column: 0 },
            head: TextPoint { row: 0, column: 1 },
            dragged: true,
        };
        app.selection = Some(selection);

        app.on_key(KeyEvent::new(
            KeyCode::Modifier(ModifierKeyCode::LeftSuper),
            KeyModifiers::SUPER,
        ));

        assert!(app.selection.is_some());
    }

    #[test]
    fn command_c_is_recognized_as_a_copy_shortcut() {
        assert!(is_copy_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::SUPER,
        )));
        assert!(is_copy_key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        )));
        assert!(!is_copy_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn enhanced_keyboard_reporting_requests_shifted_characters() {
        assert!(
            keyboard_enhancement_flags().contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS)
        );
    }
}
