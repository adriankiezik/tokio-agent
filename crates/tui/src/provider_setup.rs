use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio_agent_config::{AuthKind, Config, ProviderKind};

use crate::theme;

type DrawBackground<'a> = &'a mut dyn FnMut(&mut Frame, u16) -> Rect;

#[derive(Clone, Copy)]
struct ProviderOption {
    name: &'static str,
    provider: ProviderKind,
    auth: AuthKind,
    default_model: &'static str,
}

const OPTIONS: [ProviderOption; 3] = [
    ProviderOption {
        name: "ChatGPT subscription",
        provider: ProviderKind::OpenAi,
        auth: AuthKind::ChatGpt,
        default_model: "gpt-5.6-sol",
    },
    ProviderOption {
        name: "OpenAI API key",
        provider: ProviderKind::OpenAi,
        auth: AuthKind::ApiKey,
        default_model: "gpt-5.4",
    },
    ProviderOption {
        name: "Anthropic API key",
        provider: ProviderKind::Anthropic,
        auth: AuthKind::ApiKey,
        default_model: "claude-sonnet-5",
    },
];

enum Screen {
    Providers,
    ApiKey { option: usize, key: String },
    Success { title: String, message: String },
}

struct Setup {
    selected: usize,
    screen: Screen,
    active: Option<(ProviderKind, AuthKind)>,
    connected: [bool; 3],
    message: Option<(String, bool)>,
}

pub fn configure_provider(cwd: &Path) -> io::Result<bool> {
    theme::init_terminal_bg(
        terminal_colorsaurus::background_color(terminal_colorsaurus::QueryOptions::default())
            .ok()
            .map(|color| color.scale_to_8bit()),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut terminal = ratatui::init();
    let result = configure_provider_on(&mut terminal, &runtime, cwd, false, None);
    ratatui::restore();
    result
}

pub(crate) fn configure_provider_in(
    terminal: &mut DefaultTerminal,
    runtime: &tokio::runtime::Runtime,
    cwd: &Path,
    draw_background: &mut dyn FnMut(&mut Frame, u16) -> Rect,
) -> io::Result<bool> {
    configure_provider_on(terminal, runtime, cwd, true, Some(draw_background))
}

fn configure_provider_on(
    terminal: &mut DefaultTerminal,
    runtime: &tokio::runtime::Runtime,
    cwd: &Path,
    restore_mouse_capture: bool,
    draw_background: Option<DrawBackground<'_>>,
) -> io::Result<bool> {
    let config = Config::load(cwd)
        .ok()
        .and_then(|config| config.resolve().ok());
    let active = config.map(|config| {
        let auth = config.auth.unwrap_or_else(|| match config.provider {
            ProviderKind::OpenAi if tokio_agent_auth::is_signed_in() => AuthKind::ChatGpt,
            _ => AuthKind::ApiKey,
        });
        (config.provider, auth)
    });
    let mut setup = Setup {
        selected: active
            .and_then(|active| {
                OPTIONS
                    .iter()
                    .position(|option| (option.provider, option.auth) == active)
            })
            .unwrap_or(0),
        screen: Screen::Providers,
        active,
        connected: [
            tokio_agent_auth::is_signed_in(),
            tokio_agent_config::api_key("openai").is_ok(),
            tokio_agent_config::api_key("anthropic").is_ok(),
        ],
        message: None,
    };
    execute!(io::stdout(), DisableMouseCapture)?;
    let result = setup.run(terminal, runtime, draw_background);
    if restore_mouse_capture {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    result
}

impl Setup {
    #[allow(clippy::too_many_lines)]
    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        runtime: &tokio::runtime::Runtime,
        mut draw_background: Option<DrawBackground<'_>>,
    ) -> io::Result<bool> {
        loop {
            terminal.draw(|frame| {
                if let Some(draw_background) = &mut draw_background {
                    let area = draw_background(frame, self.panel_height());
                    self.render_in(frame, area);
                } else {
                    self.render(frame);
                }
            })?;
            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            let event = event::read()?;
            if let Event::Paste(text) = event {
                if let Screen::ApiKey { key, .. } = &mut self.screen {
                    key.push_str(text.trim());
                }
                continue;
            }
            let Event::Key(event_key) = event else {
                continue;
            };
            if event_key.kind != KeyEventKind::Press {
                continue;
            }
            match &mut self.screen {
                Screen::Providers => match event_key.code {
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Char('c') if event_key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(false);
                    }
                    KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                    KeyCode::Down => self.selected = (self.selected + 1).min(OPTIONS.len() - 1),
                    KeyCode::Enter => {
                        let option = OPTIONS[self.selected];
                        if self.active == Some((option.provider, option.auth))
                            && self.connected[self.selected]
                        {
                            return Ok(false);
                        }
                        if option.auth == AuthKind::ChatGpt {
                            if self.connected[0] {
                                if let Err(error) = self.activate(option) {
                                    self.message = Some((error, true));
                                } else {
                                    return Ok(true);
                                }
                                continue;
                            }
                            self.message = Some(("Waiting for browser sign-in…".to_owned(), false));
                            terminal.draw(|frame| self.render(frame))?;
                            match runtime.block_on(tokio_agent_auth::login_silent()) {
                                Ok(outcome) => {
                                    let identity = outcome.email.map_or_else(
                                        || "Your ChatGPT account is ready".to_owned(),
                                        |email| format!("Connected as {email}"),
                                    );
                                    self.connected[0] = true;
                                    match self.activate(option) {
                                        Ok(()) => {
                                            self.screen = Screen::Success {
                                                title: option.name.to_owned(),
                                                message: identity,
                                            };
                                            self.message = None;
                                        }
                                        Err(error) => {
                                            self.message = Some((error, true));
                                        }
                                    }
                                }
                                Err(error) => self.message = Some((error.to_string(), true)),
                            }
                        } else if self.connected[self.selected] {
                            if let Err(error) = self.activate(option) {
                                self.message = Some((error, true));
                            } else {
                                return Ok(true);
                            }
                        } else {
                            self.screen = Screen::ApiKey {
                                option: self.selected,
                                key: String::new(),
                            };
                            self.message = None;
                        }
                    }
                    _ => {}
                },
                Screen::ApiKey { option, key } => {
                    match key_event(event_key.code, event_key.modifiers) {
                        KeyInput::Cancel => {
                            self.screen = Screen::Providers;
                            self.message = None;
                        }
                        KeyInput::Backspace => {
                            key.pop();
                        }
                        KeyInput::Character(character) => key.push(character),
                        KeyInput::Submit if !key.trim().is_empty() => {
                            let selected = *option;
                            let option = OPTIONS[selected];
                            match tokio_agent_config::store_api_key(
                                option.provider.as_str(),
                                key.trim(),
                            )
                            .map_err(|error| error.to_string())
                            .and_then(|()| self.activate(option))
                            {
                                Ok(()) => {
                                    self.screen = Screen::Success {
                                        title: option.name.to_owned(),
                                        message: "API key saved securely".to_owned(),
                                    };
                                    self.message = None;
                                }
                                Err(error) => self.message = Some((error, true)),
                            }
                        }
                        _ => {}
                    }
                }
                Screen::Success { .. } => match event_key.code {
                    KeyCode::Enter | KeyCode::Esc => return Ok(true),
                    KeyCode::Char('c') if event_key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(true);
                    }
                    _ => {}
                },
            }
        }
    }

    fn activate(&mut self, option: ProviderOption) -> Result<(), String> {
        tokio_agent_config::store_provider_selection(
            option.provider,
            option.auth,
            option.default_model,
        )
        .map_err(|error| error.to_string())?;
        self.active = Some((option.provider, option.auth));
        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        let height = self.panel_height();
        let [_, area, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Length(2),
        ])
        .areas(frame.area());
        frame.render_widget(Block::default().style(theme::picker_bg()), frame.area());
        self.render_in(frame, area);
    }

    fn panel_height(&self) -> u16 {
        match self.screen {
            Screen::Providers => 10,
            Screen::ApiKey { .. } | Screen::Success { .. } => 7,
        }
    }

    fn render_in(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Block::default().style(theme::picker_bg()), area);
        match &self.screen {
            Screen::Providers => self.render_providers(frame, area),
            Screen::ApiKey { option, key } => self.render_api_key(frame, area, *option, key),
            Screen::Success { title, message } => Self::render_success(frame, area, title, message),
        }
    }

    fn render_providers(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::styled("Subscriptions", theme::bold())];
        for (index, option) in OPTIONS.iter().enumerate() {
            if index == 1 {
                lines.push(Line::default());
                lines.push(Line::styled("API keys", theme::bold()));
            }
            let marker = if index == self.selected { "› " } else { "  " };
            let status =
                if self.active == Some((option.provider, option.auth)) && self.connected[index] {
                    "Active"
                } else if self.connected[index] {
                    "Connected"
                } else {
                    "Not configured"
                };
            let style = if index == self.selected {
                theme::picker_selected()
            } else {
                theme::picker_muted()
            };
            lines.push(Line::from(vec![
                Span::styled(marker, theme::running()),
                Span::styled(format!("{:<25}", option.name), style),
                Span::styled(status, style),
            ]));
        }
        lines.push(Line::default());
        lines.push(self.message_line("↑↓ select · Enter configure · Esc close"));
        frame.render_widget(Paragraph::new(lines), inset(area, 1, 1));
    }

    fn render_api_key(&self, frame: &mut Frame, area: Rect, option: usize, key: &str) {
        let option = OPTIONS[option];
        let masked = "•".repeat(key.chars().count());
        let lines = vec![
            Line::styled(option.name, theme::bold()),
            Line::default(),
            Line::from(vec![
                Span::styled("API key  ", theme::picker_muted()),
                Span::styled(masked, theme::picker_selected()),
            ]),
            Line::default(),
            self.message_line("Enter save and use · Esc back"),
        ];
        frame.render_widget(Paragraph::new(lines), inset(area, 1, 1));
    }

    fn render_success(frame: &mut Frame, area: Rect, title: &str, message: &str) {
        let lines = vec![
            Line::styled(title.to_owned(), theme::bold()),
            Line::default(),
            Line::from(vec![
                Span::styled("✓ ", theme::success()),
                Span::styled(message.to_owned(), theme::picker_selected()),
            ]),
            Line::default(),
            Line::styled("Enter continue", theme::picker_muted()),
        ];
        frame.render_widget(Paragraph::new(lines), inset(area, 1, 1));
    }

    fn message_line(&self, fallback: &str) -> Line<'static> {
        match &self.message {
            Some((message, true)) => Line::styled(message.clone(), theme::error()),
            Some((message, false)) => Line::styled(message.clone(), theme::running()),
            None => Line::styled(fallback.to_owned(), theme::picker_muted()),
        }
    }
}

enum KeyInput {
    Cancel,
    Backspace,
    Character(char),
    Submit,
    None,
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyInput {
    match code {
        KeyCode::Esc => KeyInput::Cancel,
        KeyCode::Backspace => KeyInput::Backspace,
        KeyCode::Enter => KeyInput::Submit,
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
            KeyInput::Character(character)
        }
        _ => KeyInput::None,
    }
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y.saturating_add(vertical),
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}
