use std::borrow::Cow;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use tokio_agent_core::agent::{AgentEvent, UiCommand};
use tokio_agent_core::message::{ToolOutput, Usage};
use tokio_agent_core::permission::{Decision, Mode, PermissionId};
use tokio_agent_core::tool::{Action, PermissionRequest};
use tokio_agent_extension_api::{
    CommandDescriptor, CommandId, CommandSource, ExtensionSummary, StatusSegment,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme;

#[path = "composer.rs"]
mod composer;
#[path = "transcript.rs"]
mod transcript;

use composer::Composer;
use transcript::Transcript;

#[derive(Debug)]
pub(crate) enum ExtensionOperation {
    Install { id: String, registry: String },
    SetEnabled { id: String, enabled: bool },
}

pub(crate) enum FrontendEffect {
    None,
    Quit,
    ConfigureProvider,
    Copy(String),
    Extension(ExtensionOperation),
    Command(UiCommand),
}

#[derive(Clone)]
struct SlashCommand {
    name: Cow<'static, str>,
    description: Cow<'static, str>,
    usage: Option<Cow<'static, str>>,
    action: SlashAction,
}

#[derive(Clone)]
enum SlashAction {
    Clear,
    Model,
    Permissions,
    Provider,
    Extensions,
    Extension(CommandId),
}

#[derive(Clone, Copy)]
struct PermissionModeOption {
    mode: Mode,
    name: &'static str,
    description: &'static str,
}

const PERMISSION_MODES: [PermissionModeOption; 3] = [
    PermissionModeOption {
        mode: Mode::Suggest,
        name: "Suggest",
        description: "Ask before edits and commands",
    },
    PermissionModeOption {
        mode: Mode::AutoEdit,
        name: "Auto-edit",
        description: "Allow edits; ask before commands",
    },
    PermissionModeOption {
        mode: Mode::FullAuto,
        name: "Full-auto",
        description: "Allow edits and commands without asking",
    },
];

const QUIT_CONFIRMATION: Duration = Duration::from_secs(3);
const COMMAND_NOTICE_DURATION: Duration = Duration::from_secs(3);

struct Pending {
    id: PermissionId,
    request: PermissionRequest,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExtensionTab {
    Discover,
    Installed,
    Updates,
}

#[derive(Clone, Copy)]
enum SettingPicker {
    Effort(usize),
    Model(usize),
}

pub(crate) struct FrontendProjection {
    transcript: Transcript,
    composer: Composer,
    pending: Option<Pending>,
    running: bool,
    interrupting: bool,
    started_at: Option<Instant>,
    elapsed: Duration,
    usage: Usage,
    scroll_up: usize,
    provider: String,
    model: String,
    effort: Option<String>,
    cwd: String,
    context_window: Option<u64>,
    max_output_tokens: u32,
    slash_selected: usize,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    permission_mode: Mode,
    permissions_selected: Option<usize>,
    setting_picker: Option<SettingPicker>,
    provider_change_notice: bool,
    last_request_usage: Usage,
    context_usage_known: bool,
    quit_armed_until: Option<Instant>,
    scroll_button_area: Option<Rect>,
    command_error: Option<String>,
    command_notice: Option<(String, Instant)>,
    extension_commands: Vec<SlashCommand>,
    status_segments: Vec<StatusSegment>,
    extensions: Vec<ExtensionSummary>,
    extension_manager: Option<usize>,
    extension_tab: ExtensionTab,
    extension_search: String,
    extension_disable_confirmation: Option<String>,
}

impl FrontendProjection {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        provider: String,
        model: String,
        effort: Option<String>,
        cwd: String,
        context_window: Option<u64>,
        max_output_tokens: u32,
        permission_mode: Mode,
        history: Vec<String>,
        extension_commands: Vec<CommandDescriptor>,
        extensions: Vec<ExtensionSummary>,
    ) -> Self {
        Self {
            transcript: Transcript::new(),
            composer: Composer::new(),
            pending: None,
            running: false,
            interrupting: false,
            started_at: None,
            elapsed: Duration::ZERO,
            usage: Usage::default(),
            scroll_up: 0,
            provider,
            model,
            effort,
            cwd,
            context_window,
            max_output_tokens,
            slash_selected: 0,
            history,
            history_cursor: None,
            history_draft: String::new(),
            permission_mode,
            permissions_selected: None,
            setting_picker: None,
            provider_change_notice: false,
            last_request_usage: Usage::default(),
            context_usage_known: true,
            quit_armed_until: None,
            scroll_button_area: None,
            command_error: None,
            command_notice: None,
            extension_commands: extension_commands
                .into_iter()
                .map(extension_slash_command)
                .collect(),
            status_segments: Vec::new(),
            extensions,
            extension_manager: None,
            extension_tab: ExtensionTab::Installed,
            extension_search: String::new(),
            extension_disable_confirmation: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_session(
        &mut self,
        provider: String,
        model: String,
        effort: Option<String>,
        cwd: String,
        context_window: Option<u64>,
        max_output_tokens: u32,
        permission_mode: Mode,
        commands: Vec<CommandDescriptor>,
        extensions: Vec<ExtensionSummary>,
    ) {
        self.provider = provider;
        self.model = model;
        self.effort = effort;
        self.cwd = cwd;
        self.context_window = context_window;
        self.max_output_tokens = max_output_tokens;
        self.permission_mode = permission_mode;
        self.extension_commands = commands.into_iter().map(extension_slash_command).collect();
        self.extensions = extensions;
        self.extension_manager = None;
        self.pending = None;
        self.running = false;
        self.interrupting = false;
        self.setting_picker = None;
        self.permissions_selected = None;
        self.provider_change_notice = false;
        self.started_at = None;
        self.elapsed = Duration::ZERO;
        self.last_request_usage = Usage::default();
        self.context_usage_known = self.transcript.len() == 0;
        self.command_error = None;
        self.command_notice = None;
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) -> FrontendEffect {
        self.command_error = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(id) = self.extension_disable_confirmation.clone() {
            match key.code {
                KeyCode::Esc => self.extension_disable_confirmation = None,
                KeyCode::Enter | KeyCode::Char('e') => {
                    self.extension_disable_confirmation = None;
                    return FrontendEffect::Extension(ExtensionOperation::SetEnabled {
                        id,
                        enabled: false,
                    });
                }
                _ => {}
            }
            return FrontendEffect::None;
        }
        if let Some(selected) = self.extension_manager {
            let selected_extension = self.visible_extensions().get(selected).copied().cloned();
            match key.code {
                KeyCode::Esc => self.extension_manager = None,
                KeyCode::Up => self.extension_manager = Some(selected.saturating_sub(1)),
                KeyCode::Down => {
                    self.extension_manager =
                        Some((selected + 1).min(self.visible_extensions().len().saturating_sub(1)));
                }
                KeyCode::Left => {
                    self.extension_tab = match self.extension_tab {
                        ExtensionTab::Discover => ExtensionTab::Updates,
                        ExtensionTab::Installed => ExtensionTab::Discover,
                        ExtensionTab::Updates => ExtensionTab::Installed,
                    };
                    self.extension_manager = Some(0);
                }
                KeyCode::Right | KeyCode::Tab => {
                    self.extension_tab = match self.extension_tab {
                        ExtensionTab::Discover => ExtensionTab::Installed,
                        ExtensionTab::Installed => ExtensionTab::Updates,
                        ExtensionTab::Updates => ExtensionTab::Discover,
                    };
                    self.extension_manager = Some(0);
                }
                KeyCode::Backspace => {
                    self.extension_search.pop();
                    self.extension_manager = Some(0);
                }
                KeyCode::Char('i')
                    if selected_extension
                        .as_ref()
                        .is_some_and(|extension| !extension.installed) =>
                {
                    if let Some(extension) = selected_extension {
                        let registry = match extension.origin {
                            tokio_agent_extension_api::ExtensionOrigin::OfficialRegistry {
                                registry,
                            }
                            | tokio_agent_extension_api::ExtensionOrigin::ThirdPartyRegistry {
                                registry,
                                ..
                            } => registry,
                            tokio_agent_extension_api::ExtensionOrigin::Local { .. } => {
                                return FrontendEffect::None;
                            }
                        };
                        return FrontendEffect::Extension(ExtensionOperation::Install {
                            id: extension.id.to_string(),
                            registry,
                        });
                    }
                }
                KeyCode::Char('e')
                    if selected_extension
                        .as_ref()
                        .is_some_and(|extension| extension.installed) =>
                {
                    if let Some(extension) = selected_extension {
                        if extension.enabled
                            && extension.capabilities.contains(
                                &tokio_agent_extension_api::Capability::SessionSubmitAutomatic,
                            )
                        {
                            self.extension_disable_confirmation = Some(extension.id.to_string());
                            return FrontendEffect::None;
                        }
                        return FrontendEffect::Extension(ExtensionOperation::SetEnabled {
                            id: extension.id.to_string(),
                            enabled: !extension.enabled,
                        });
                    }
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    self.extension_search.push(character);
                    self.extension_manager = Some(0);
                }
                _ => {}
            }
            return FrontendEffect::None;
        }
        if self.provider_change_notice {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.provider_change_notice = false;
                return FrontendEffect::ConfigureProvider;
            }
            return FrontendEffect::None;
        }
        if let Some(effect) = self.handle_permissions_key(key) {
            return effect;
        }
        if let Some(effect) = self.handle_setting_key(key) {
            return effect;
        }
        if ctrl && key.code == KeyCode::Char('c') && self.interrupting {
            return FrontendEffect::Quit;
        }
        if let Some(effect) = self.handle_pending_key(key.code, ctrl) {
            return effect;
        }
        if let Some(effect) = self.handle_slash_key(key) {
            return effect;
        }

        self.handle_editor_key(key, ctrl)
    }

    pub(crate) fn on_paste(&mut self, text: &str) {
        if self.provider_change_notice || self.pending.is_some() || text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.quit_armed_until = None;
        self.permissions_selected = None;
        self.command_error = None;
        self.composer.insert_str(&normalized);
        self.leave_history();
        self.slash_selected = 0;
    }

    fn handle_pending_key(&mut self, code: KeyCode, ctrl: bool) -> Option<FrontendEffect> {
        let pending = self.pending.as_ref()?;
        if code == KeyCode::Char('c') && !ctrl {
            return Some(FrontendEffect::Copy(permission_copy_text(&pending.request)));
        }
        let id = pending.id;
        let decision = match code {
            KeyCode::Char('c') if ctrl => {
                self.interrupting = true;
                return Some(FrontendEffect::Command(UiCommand::Interrupt));
            }
            KeyCode::Char('y') => Decision::AllowOnce,
            KeyCode::Char('a') => Decision::AllowAlways,
            KeyCode::Char('n') | KeyCode::Esc => Decision::Deny,
            _ => return Some(FrontendEffect::None),
        };
        self.pending = None;
        self.resume_elapsed_timer();
        Some(FrontendEffect::Command(UiCommand::Approve { id, decision }))
    }

    fn handle_permissions_key(&mut self, key: KeyEvent) -> Option<FrontendEffect> {
        let selected = self.permissions_selected?;
        match key.code {
            KeyCode::Up => {
                self.permissions_selected = Some(selected.saturating_sub(1));
                Some(FrontendEffect::None)
            }
            KeyCode::Down => {
                self.permissions_selected = Some((selected + 1).min(PERMISSION_MODES.len() - 1));
                Some(FrontendEffect::None)
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let mode = PERMISSION_MODES[selected].mode;
                self.permission_mode = mode;
                self.permissions_selected = None;
                Some(FrontendEffect::Command(UiCommand::SetPermissionMode(mode)))
            }
            KeyCode::Esc => {
                self.permissions_selected = None;
                Some(FrontendEffect::None)
            }
            _ => Some(FrontendEffect::None),
        }
    }

    fn handle_setting_key(&mut self, key: KeyEvent) -> Option<FrontendEffect> {
        let picker = self.setting_picker?;
        let (selected, count) = match picker {
            SettingPicker::Effort(selected) => (selected, self.effort_options().len()),
            SettingPicker::Model(selected) => (selected, self.model_options().len()),
        };
        match key.code {
            KeyCode::Up => {
                self.setting_picker = Some(match picker {
                    SettingPicker::Effort(_) => SettingPicker::Effort(selected.saturating_sub(1)),
                    SettingPicker::Model(_) => SettingPicker::Model(selected.saturating_sub(1)),
                });
                Some(FrontendEffect::None)
            }
            KeyCode::Down => {
                let selected = (selected + 1).min(count.saturating_sub(1));
                self.setting_picker = Some(match picker {
                    SettingPicker::Effort(_) => SettingPicker::Effort(selected),
                    SettingPicker::Model(_) => SettingPicker::Model(selected),
                });
                Some(FrontendEffect::None)
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.setting_picker = None;
                match picker {
                    SettingPicker::Effort(selected) => {
                        let effort = self.effort_options()[selected].to_owned();
                        self.effort = Some(effort.clone());
                        Some(FrontendEffect::Command(UiCommand::SetReasoningEffort(
                            Some(effort),
                        )))
                    }
                    SettingPicker::Model(selected) => {
                        let model = self.model_options()[selected].to_owned();
                        self.context_window =
                            model_context_window(&self.provider, &model).map(|window| {
                                context_before_compaction(
                                    &self.provider,
                                    window,
                                    self.max_output_tokens,
                                )
                            });
                        self.model = model.clone();
                        let efforts = self.effort_options();
                        if !efforts.is_empty() {
                            let selected = efforts
                                .iter()
                                .position(|effort| Some(*effort) == self.effort.as_deref())
                                .unwrap_or_default();
                            self.setting_picker = Some(SettingPicker::Effort(selected));
                        }
                        Some(FrontendEffect::Command(UiCommand::SetModel(model)))
                    }
                }
            }
            KeyCode::Esc => {
                self.setting_picker = None;
                Some(FrontendEffect::None)
            }
            _ => Some(FrontendEffect::None),
        }
    }

    fn model_options(&self) -> &'static [&'static str] {
        model_options(&self.provider)
    }

    fn effort_options(&self) -> &'static [&'static str] {
        effort_options(&self.provider, &self.model)
    }

    fn handle_slash_key(&mut self, key: KeyEvent) -> Option<FrontendEffect> {
        if !self.slash_picker_open() {
            return None;
        }
        match key.code {
            KeyCode::Up => {
                let count = self.slash_matches().len();
                self.slash_selected = self.slash_selected.saturating_sub(1);
                if count > 0 {
                    self.slash_selected = self.slash_selected.min(count - 1);
                }
                Some(FrontendEffect::None)
            }
            KeyCode::Down => {
                let count = self.slash_matches().len();
                if count > 0 {
                    self.slash_selected = (self.slash_selected + 1).min(count - 1);
                }
                Some(FrontendEffect::None)
            }
            KeyCode::Tab => {
                if let Some(command) = self.selected_slash_command() {
                    self.composer.replace(&command.name);
                }
                Some(FrontendEffect::None)
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let allowed = !self.running
                    || self.selected_slash_command().is_some_and(|command| {
                        matches!(
                            command.action,
                            SlashAction::Permissions | SlashAction::Provider
                        )
                    });
                Some(if allowed {
                    self.run_selected_slash_command()
                } else {
                    FrontendEffect::None
                })
            }
            KeyCode::Esc => {
                self.composer.clear();
                self.slash_selected = 0;
                Some(FrontendEffect::None)
            }
            _ => None,
        }
    }

    fn handle_editor_key(&mut self, key: KeyEvent, ctrl: bool) -> FrontendEffect {
        if ctrl && key.code == KeyCode::Char('c') {
            return self.handle_ctrl_c();
        }
        match key.code {
            KeyCode::Char('j') if ctrl => {
                self.composer.insert_newline();
                FrontendEffect::None
            }
            KeyCode::Enter | KeyCode::Char('\n' | '\r')
                if key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.composer.insert_newline();
                FrontendEffect::None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.composer.backspace();
                self.leave_history();
                self.slash_selected = 0;
                FrontendEffect::None
            }
            KeyCode::Delete => {
                self.composer.delete();
                self.leave_history();
                self.slash_selected = 0;
                FrontendEffect::None
            }
            KeyCode::Left => {
                self.composer.move_left();
                FrontendEffect::None
            }
            KeyCode::Right => {
                self.composer.move_right();
                FrontendEffect::None
            }
            KeyCode::Up => {
                if self.composer.is_single_line() {
                    self.history_previous();
                } else {
                    self.composer.move_up();
                }
                FrontendEffect::None
            }
            KeyCode::Down => {
                if self.composer.is_single_line() && self.history_cursor.is_some() {
                    self.history_next();
                } else {
                    self.composer.move_down();
                }
                FrontendEffect::None
            }
            KeyCode::Home => {
                self.composer.move_home();
                FrontendEffect::None
            }
            KeyCode::End => {
                self.composer.move_end();
                FrontendEffect::None
            }
            KeyCode::PageUp => {
                self.scroll_up();
                FrontendEffect::None
            }
            KeyCode::PageDown => {
                self.scroll_down();
                FrontendEffect::None
            }
            KeyCode::Esc if self.running && self.composer.text().is_empty() => {
                self.interrupting = true;
                FrontendEffect::Command(UiCommand::Interrupt)
            }
            KeyCode::Esc => {
                self.composer.clear();
                self.leave_history();
                self.slash_selected = 0;
                FrontendEffect::None
            }
            KeyCode::Char(c) if !ctrl => {
                self.quit_armed_until = None;
                self.composer.insert(c);
                self.leave_history();
                self.slash_selected = 0;
                FrontendEffect::None
            }
            _ => FrontendEffect::None,
        }
    }

    fn handle_ctrl_c(&mut self) -> FrontendEffect {
        if self.running {
            self.interrupting = true;
            FrontendEffect::Command(UiCommand::Interrupt)
        } else if self.quit_confirmation_active() || self.composer.text().is_empty() {
            FrontendEffect::Quit
        } else {
            self.composer.clear();
            self.leave_history();
            self.slash_selected = 0;
            self.quit_armed_until = Some(Instant::now() + QUIT_CONFIRMATION);
            FrontendEffect::None
        }
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = if let Some(index) = self.history_cursor {
            index.saturating_sub(1)
        } else {
            self.history_draft = self.composer.text().to_owned();
            self.history.len() - 1
        };
        self.history_cursor = Some(index);
        self.composer.replace(&self.history[index]);
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_cursor = Some(next);
            self.composer.replace(&self.history[next]);
        } else {
            self.history_cursor = None;
            self.composer.replace(&self.history_draft);
        }
    }

    fn leave_history(&mut self) {
        self.history_cursor = None;
        self.history_draft.clear();
    }

    fn quit_confirmation_active(&self) -> bool {
        self.quit_armed_until
            .is_some_and(|until| until > Instant::now())
    }

    fn slash_picker_open(&self) -> bool {
        let text = self.composer.text();
        text.starts_with('/') && !text.chars().any(char::is_whitespace)
    }

    fn slash_matches(&self) -> Vec<SlashCommand> {
        let query = self.composer.text();
        self.extension_commands
            .iter()
            .filter(|command| command.name.starts_with(query))
            .filter(|command| {
                !matches!(command.action, SlashAction::Model) || !self.model_options().is_empty()
            })
            .cloned()
            .collect()
    }

    fn slash_display_matches(&self) -> Vec<SlashCommand> {
        if !self.composer.text().starts_with('/') {
            return Vec::new();
        }

        let matches = self.slash_matches();
        if !matches.is_empty() || self.slash_picker_open() {
            return matches;
        }

        self.extension_commands
            .iter()
            .filter(|command| {
                command.usage.is_some()
                    && command_usage_visible(&command.name, self.composer.text())
            })
            .cloned()
            .collect()
    }

    fn selected_slash_command(&self) -> Option<SlashCommand> {
        let matches = self.slash_matches();
        matches
            .get(self.slash_selected.min(matches.len().saturating_sub(1)))
            .cloned()
    }

    fn run_selected_slash_command(&mut self) -> FrontendEffect {
        let Some(command) = self.selected_slash_command() else {
            return self.submit();
        };
        self.composer.clear();
        self.slash_selected = 0;
        match command.action {
            SlashAction::Clear => {
                self.transcript.clear();
                self.scroll_up = 0;
                self.last_request_usage = Usage::default();
                self.context_usage_known = true;
                FrontendEffect::Command(UiCommand::Clear)
            }
            SlashAction::Model => {
                let selected = self
                    .model_options()
                    .iter()
                    .position(|model| *model == self.model)
                    .unwrap_or_default();
                self.setting_picker = Some(SettingPicker::Model(selected));
                FrontendEffect::None
            }
            SlashAction::Permissions => {
                self.permissions_selected = Some(
                    PERMISSION_MODES
                        .iter()
                        .position(|option| option.mode == self.permission_mode)
                        .unwrap_or_default(),
                );
                FrontendEffect::None
            }
            SlashAction::Provider if self.running => {
                self.show_provider_change_notice();
                FrontendEffect::None
            }
            SlashAction::Provider => FrontendEffect::ConfigureProvider,
            SlashAction::Extensions => {
                self.extension_manager = Some(0);
                FrontendEffect::None
            }
            SlashAction::Extension(_) => {
                self.composer.replace(&format!("{} ", command.name));
                FrontendEffect::None
            }
        }
    }

    fn submit(&mut self) -> FrontendEffect {
        let text = self.composer.text().to_owned();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.composer.clear();
            return FrontendEffect::None;
        }
        let message = trimmed.to_owned();
        if message.starts_with('/') {
            let (name, arguments) = message
                .split_once(char::is_whitespace)
                .unwrap_or((&message, ""));
            if let Some(command) = self
                .extension_commands
                .iter()
                .find(|command| command.name == name)
                .cloned()
                && let SlashAction::Extension(id) = command.action
            {
                if self.running {
                    self.command_error =
                        Some(format!("{name} is unavailable while a turn is running"));
                    return FrontendEffect::None;
                }
                self.composer.clear();
                if self.history.last() != Some(&message) {
                    self.history.push(message.clone());
                }
                self.leave_history();
                self.begin_turn(&message);
                return FrontendEffect::Command(UiCommand::InvokeCommand {
                    id,
                    arguments: arguments.trim().to_owned(),
                });
            }
            if let Some(extension) = self.extensions.iter().find(|extension| {
                extension.installed
                    && !extension.enabled
                    && extension.commands.iter().any(|command| command == name)
            }) {
                self.command_error = Some(format!(
                    "{name} is provided by “{}”, which is installed but disabled · open /extensions to enable it",
                    extension.name
                ));
            } else {
                self.command_error = Some(format!("Unknown command: {name}"));
            }
            return FrontendEffect::None;
        }
        self.composer.clear();
        if self.history.last() != Some(&message) {
            self.history.push(message.clone());
        }
        self.leave_history();
        if self.running {
            self.transcript.push_user(message.clone());
            self.scroll_up = 0;
            FrontendEffect::Command(UiCommand::Steer(message))
        } else {
            self.begin_turn(&message);
            FrontendEffect::Command(UiCommand::UserMessage(message))
        }
    }

    pub(crate) fn composer_height(&self, width: u16) -> u16 {
        match &self.pending {
            Some(pending) => permission_height(&pending.request, width),
            None => self.composer.height(width),
        }
    }

    fn active_command_notice(&self) -> Option<&str> {
        self.command_notice
            .as_ref()
            .filter(|(_, expires_at)| *expires_at > Instant::now())
            .map(|(message, _)| message.as_str())
    }

    pub(crate) fn command_feedback_height(&self) -> u16 {
        if (self.command_error.is_some() || self.active_command_notice().is_some())
            && self.pending.is_none()
        {
            2
        } else {
            0
        }
    }

    pub(crate) fn render_command_feedback(&self, frame: &mut Frame, area: Rect) {
        let feedback = self
            .command_error
            .as_deref()
            .map(|error| (error, theme::error()))
            .or_else(|| {
                self.active_command_notice()
                    .map(|notice| (notice, theme::success()))
            });
        if let Some((message, style)) = feedback {
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::raw("  "), Span::raw(message)])).style(style),
                area,
            );
        }
    }

    pub(crate) fn working_indicator_height(&self) -> u16 {
        u16::from(self.running && self.pending.is_none())
    }

    pub(crate) fn slash_picker_height(&self) -> u16 {
        if self.pending.is_some() {
            return 0;
        }
        let count = u16::try_from(self.slash_display_matches().len()).unwrap_or(u16::MAX);
        if count == 0 { 0 } else { count + 2 }
    }

    pub(crate) fn render_slash_picker(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        frame.render_widget(Block::default().style(theme::picker_bg()), area);
        let matches = self.slash_display_matches();
        let name_width = slash_command_name_width(&matches);
        let query = self.composer.text();
        let lines = matches
            .into_iter()
            .enumerate()
            .map(|(index, command)| {
                let marker = if index == self.slash_selected {
                    "› "
                } else {
                    "  "
                };
                let (marker_style, command_style) = if index == self.slash_selected {
                    (theme::running(), theme::picker_selected())
                } else {
                    (theme::picker_muted(), theme::picker_muted())
                };
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        format!("{:<width$}", command.name, width = name_width),
                        command_style,
                    ),
                    Span::raw("  "),
                    Span::styled(
                        slash_command_description(
                            &command,
                            command_usage_visible(&command.name, query),
                        ),
                        theme::picker_muted(),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        let inner = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    pub(crate) fn permissions_panel_height(&self) -> u16 {
        let count = if self.permissions_selected.is_some() {
            PERMISSION_MODES.len()
        } else {
            match self.setting_picker {
                Some(SettingPicker::Effort(_)) => self.effort_options().len(),
                Some(SettingPicker::Model(_)) => self.model_options().len(),
                None => 0,
            }
        };
        u16::try_from(count)
            .unwrap_or(u16::MAX)
            .saturating_add(u16::from(count > 0) * 2)
    }

    pub(crate) fn show_provider_change_notice(&mut self) {
        self.provider_change_notice = true;
    }

    pub(crate) fn provider_change_notice_height(&self) -> u16 {
        u16::from(self.provider_change_notice) * 6
    }

    pub(crate) fn provider_change_notice_visible(&self) -> bool {
        self.provider_change_notice
    }

    pub(crate) fn render_provider_change_notice(&self, frame: &mut Frame, area: Rect) {
        if !self.provider_change_notice || area.height == 0 {
            return;
        }
        frame.render_widget(Block::default().style(theme::picker_bg()), area);
        let lines = vec![
            Line::styled("Provider changes are deferred", theme::bold()),
            Line::default(),
            Line::styled(
                "The current work will finish with the active provider. Any provider change will be used afterward.",
                theme::picker_muted(),
            ),
            Line::styled("Enter or Esc dismiss", theme::picker_muted()),
        ];
        let inner = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
    }

    pub(crate) fn render_permissions_panel(&self, frame: &mut Frame, area: Rect) {
        if let Some(picker) = self.setting_picker {
            self.render_setting_picker(frame, area, picker);
            return;
        }
        let Some(selected) = self.permissions_selected else {
            return;
        };
        frame.render_widget(Block::default().style(theme::picker_bg()), area);
        let lines = PERMISSION_MODES
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let marker = if index == selected { "› " } else { "  " };
                let active = permission_mode_indicator(option.mode, self.permission_mode);
                let style = if index == selected {
                    theme::picker_selected()
                } else {
                    theme::picker_muted()
                };
                Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(active, theme::running()),
                    Span::styled(format!("{:<12}", option.name), style),
                    Span::styled(option.description, theme::picker_muted()),
                ])
            })
            .collect::<Vec<_>>();
        let inner = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_setting_picker(&self, frame: &mut Frame, area: Rect, picker: SettingPicker) {
        let (options, selected, active) = match picker {
            SettingPicker::Effort(selected) => {
                (self.effort_options(), selected, self.effort.as_deref())
            }
            SettingPicker::Model(selected) => {
                (self.model_options(), selected, Some(self.model.as_str()))
            }
        };
        frame.render_widget(Block::default().style(theme::picker_bg()), area);
        let lines = options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let marker = if index == selected { "› " } else { "  " };
                let current = if active == Some(*option) {
                    "● "
                } else {
                    "  "
                };
                let style = if index == selected {
                    theme::picker_selected()
                } else {
                    theme::picker_muted()
                };
                Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(current, theme::running()),
                    Span::styled(*option, style),
                ])
            })
            .collect::<Vec<_>>();
        let inner = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    pub(crate) fn render_interaction(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(pending) = &self.pending {
            render_permission(frame, area, &pending.request);
        } else {
            let placeholder = if self.quit_confirmation_active() {
                "Press again to quit"
            } else if self.running {
                "Send a message to steer agent"
            } else {
                "Ask agent to do anything"
            };
            self.composer.render(frame, area, placeholder);
        }
    }

    pub(crate) fn render_working_indicator(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(working_indicator_line(
                self.current_elapsed(),
                self.interrupting,
            )),
            area,
        );
    }

    pub(crate) fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AutomaticTurnStarted(_) => self.begin_automatic_turn(),
            AgentEvent::TextDelta(text) => self.transcript.text_delta(&text),
            AgentEvent::ThinkingDelta(text) => self.transcript.thinking_delta(&text),
            AgentEvent::ToolStarted { id, name, summary } => {
                self.transcript.tool_start(id, name, summary);
            }
            AgentEvent::ToolFinished { id, name, result } => {
                let ToolOutput::Text(text) = &result.output;
                let summary = (name == "web_search")
                    .then(|| text.strip_prefix("searched: ").unwrap_or(text).trim());
                if let Some(summary) = summary {
                    self.transcript.tool_result_with_summary(
                        &id,
                        result.is_error,
                        text,
                        Some(summary),
                    );
                } else {
                    self.transcript.tool_result(&id, result.is_error, text);
                }
            }
            AgentEvent::TurnUsage(usage) => self.usage = usage,
            AgentEvent::RequestUsage(usage) => {
                self.last_request_usage = usage;
                self.context_usage_known = true;
            }
            AgentEvent::PermissionNeeded { id, request } => {
                self.pause_elapsed_timer();
                self.pending = Some(Pending { id, request });
            }
            AgentEvent::ExtensionCatalog(catalog) => {
                self.extensions = catalog;
                self.extension_manager = self.extension_manager.map(|selected| {
                    selected.min(self.visible_extensions().len().saturating_sub(1))
                });
            }
            AgentEvent::CommandCatalog(catalog) => {
                self.extension_commands =
                    catalog.into_iter().map(extension_slash_command).collect();
                self.slash_selected = 0;
            }
            AgentEvent::StatusSegments(mut segments) => {
                segments.sort_by_key(|segment| std::cmp::Reverse(segment.priority));
                self.status_segments = segments;
            }
            AgentEvent::CommandHandled(result) => {
                match result {
                    Ok(notice) => {
                        self.command_notice = notice
                            .map(|message| (message, Instant::now() + COMMAND_NOTICE_DURATION));
                    }
                    Err(error) => self.command_error = Some(error),
                }
                self.pause_elapsed_timer();
                self.running = false;
                self.interrupting = false;
            }
            AgentEvent::TurnDone(result) => {
                if let Err(error) = result {
                    self.transcript.push_error(&format!("error: {error}"));
                }
                self.pause_elapsed_timer();
                self.running = false;
                self.interrupting = false;
                self.pending = None;
            }
        }
    }

    pub(crate) fn begin_turn(&mut self, text: &str) {
        self.transcript.push_user(text.to_owned());
        self.begin_automatic_turn();
    }

    fn begin_automatic_turn(&mut self) {
        self.scroll_up = 0;
        self.running = true;
        self.interrupting = false;
        self.started_at = Some(Instant::now());
        self.elapsed = Duration::ZERO;
        self.usage = Usage::default();
    }

    pub(crate) fn submission_failed(&mut self) {
        self.pause_elapsed_timer();
        self.running = false;
        self.transcript.push_error("error: agent session stopped");
    }

    fn pause_elapsed_timer(&mut self) {
        if let Some(started_at) = self.started_at.take() {
            self.elapsed = self.elapsed.saturating_add(started_at.elapsed());
        }
    }

    fn resume_elapsed_timer(&mut self) {
        if self.running && self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }

    fn current_elapsed(&self) -> Duration {
        self.started_at.map_or(self.elapsed, |started_at| {
            self.elapsed + started_at.elapsed()
        })
    }

    #[cfg(test)]
    pub(crate) fn is_interrupting(&self) -> bool {
        self.interrupting
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running
    }

    pub(crate) fn scroll_up(&mut self) {
        self.scroll_up += 10;
    }

    pub(crate) fn scroll_down(&mut self) {
        self.scroll_up = self.scroll_up.saturating_sub(10);
    }

    pub(crate) fn on_mouse(&mut self, event: MouseEvent) -> bool {
        if self.transcript.on_mouse(event) {
            return true;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .scroll_button_area
                    .is_some_and(|area| area.contains((event.column, event.row).into())) =>
            {
                self.scroll_up = 0;
                self.scroll_button_area = None;
                true
            }
            MouseEventKind::ScrollUp => {
                self.scroll_up = self.scroll_up.saturating_add(1);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll_up = self.scroll_up.saturating_sub(1);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn scroll_button_height(&self) -> u16 {
        u16::from(self.scroll_up > 0)
    }

    pub(crate) fn render_scroll_button(&mut self, frame: &mut Frame, area: Rect) {
        const LABEL: &str = " Click to scroll down ";

        self.scroll_button_area = None;
        if self.scroll_up == 0 || area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width).min(LABEL.len());
        let button = Rect {
            x: area
                .x
                .saturating_add(area.width.saturating_sub(width as u16) / 2),
            y: area.y,
            width: width as u16,
            height: 1,
        };
        let label = LABEL.chars().take(width).collect::<String>();
        frame.render_widget(
            Paragraph::new(Line::styled(label, theme::scroll_button())),
            button,
        );
        self.scroll_button_area = Some(button);
    }

    pub(crate) fn render_transcript(&mut self, frame: &mut Frame, area: Rect, spinner: usize) {
        self.transcript
            .render(frame, area, spinner, &mut self.scroll_up);
    }

    fn visible_extensions(&self) -> Vec<&ExtensionSummary> {
        let query = self.extension_search.to_ascii_lowercase();
        self.extensions
            .iter()
            .filter(|extension| match self.extension_tab {
                ExtensionTab::Discover => !extension.installed,
                ExtensionTab::Installed => extension.installed,
                ExtensionTab::Updates => {
                    !extension.installed
                        && self.extensions.iter().any(|installed| {
                            installed.installed
                                && installed.id == extension.id
                                && same_extension_origin(&installed.origin, &extension.origin)
                                && semver::Version::parse(&extension.version).ok()
                                    > semver::Version::parse(&installed.version).ok()
                        })
                }
            })
            .filter(|extension| {
                query.is_empty()
                    || extension.name.to_ascii_lowercase().contains(&query)
                    || extension.id.as_str().to_ascii_lowercase().contains(&query)
                    || extension.description.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }

    pub(crate) fn render_extension_manager(&self, frame: &mut Frame, area: Rect) {
        let Some(selected) = self.extension_manager else {
            return;
        };
        let width = area.width.saturating_sub(8).min(90);
        let height = area.height.saturating_sub(4).min(30);
        let panel = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, panel);
        let block = Block::default()
            .title(" Extensions ")
            .borders(Borders::ALL)
            .padding(Padding::new(2, 2, 1, 1));
        let inner = block.inner(panel);
        frame.render_widget(block, panel);
        let tabs = match self.extension_tab {
            ExtensionTab::Discover => "[Discover]  Installed  Updates",
            ExtensionTab::Installed => "Discover  [Installed]  Updates",
            ExtensionTab::Updates => "Discover  Installed  [Updates]",
        };
        let mut lines = vec![
            Line::from(tabs),
            Line::styled(
                format!("Search: {}_    ←→ tabs · Esc close", self.extension_search),
                theme::dim(),
            ),
            Line::default(),
        ];
        if self.extension_disable_confirmation.is_some() {
            lines.push(Line::styled(
                "Any active autonomous work will be paused and unloaded. Press Enter to disable or Esc to cancel.",
                theme::error(),
            ));
            lines.push(Line::default());
        }
        let visible = self.visible_extensions();
        if visible.is_empty() {
            lines.push(Line::from("No matching extensions."));
        }
        for (index, extension) in visible.into_iter().enumerate() {
            let marker = if index == selected { "›" } else { " " };
            let state = if self.extension_tab == ExtensionTab::Updates {
                "Update available"
            } else if !extension.installed {
                "Not installed"
            } else if extension.enabled {
                "Enabled"
            } else {
                "Disabled"
            };
            let source = match &extension.origin {
                tokio_agent_extension_api::ExtensionOrigin::OfficialRegistry { .. } => {
                    "Official".to_owned()
                }
                tokio_agent_extension_api::ExtensionOrigin::ThirdPartyRegistry {
                    registry, ..
                } => format!("Third-party · {registry}"),
                tokio_agent_extension_api::ExtensionOrigin::Local { .. } => "Local".to_owned(),
            };
            lines.push(Line::from(format!(
                "{marker} {}  v{}",
                extension.name, extension.version
            )));
            lines.push(Line::styled(
                format!("  {} · {source} · {state}", extension.id),
                theme::dim(),
            ));
            lines.push(Line::from(format!("  {}", extension.description)));
            if index == selected {
                if !extension.commands.is_empty() {
                    lines.push(Line::styled(
                        format!("  Commands: {}", extension.commands.join(", ")),
                        theme::dim(),
                    ));
                }
                if !extension.tools.is_empty() {
                    lines.push(Line::styled(
                        format!("  Model tools: {}", extension.tools.join(", ")),
                        theme::dim(),
                    ));
                    lines.push(Line::styled(
                        "  Context cost: tool schemas only while enabled/active",
                        theme::dim(),
                    ));
                } else {
                    lines.push(Line::styled(
                        "  Context cost: none until invoked",
                        theme::dim(),
                    ));
                }
                if !extension.capabilities.is_empty() {
                    let permissions = extension
                        .capabilities
                        .iter()
                        .map(|capability| format!("{capability:?}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(Line::styled(
                        format!("  Permissions: {permissions}"),
                        theme::dim(),
                    ));
                }
            }
            lines.push(Line::default());
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    pub(crate) fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let left = match &self.pending {
            Some(_) => "approval required · esc to deny".to_owned(),
            None => session_context(&self.model, self.effort.as_deref(), &self.cwd),
        };
        let width = usize::from(area.width);
        let right = footer_status(
            &left,
            width,
            self.context_window.filter(|_| self.context_usage_known),
            self.last_request_usage.input_tokens,
        );
        let left = status_left(&left, &self.status_segments, &right, width);
        frame.render_widget(Paragraph::new(footer_line(&left, &right, width)), area);
    }
}

fn same_extension_origin(
    left: &tokio_agent_extension_api::ExtensionOrigin,
    right: &tokio_agent_extension_api::ExtensionOrigin,
) -> bool {
    match (left, right) {
        (
            tokio_agent_extension_api::ExtensionOrigin::OfficialRegistry { registry: left },
            tokio_agent_extension_api::ExtensionOrigin::OfficialRegistry { registry: right },
        ) => left == right,
        (
            tokio_agent_extension_api::ExtensionOrigin::ThirdPartyRegistry {
                registry: left, ..
            },
            tokio_agent_extension_api::ExtensionOrigin::ThirdPartyRegistry {
                registry: right, ..
            },
        ) => left == right,
        (
            tokio_agent_extension_api::ExtensionOrigin::Local { path: left },
            tokio_agent_extension_api::ExtensionOrigin::Local { path: right },
        ) => left == right,
        _ => false,
    }
}

fn status_left(base: &str, segments: &[StatusSegment], right: &str, width: usize) -> String {
    let mut left = base.to_owned();
    let reserved = right.width().saturating_add(2);
    for segment in segments {
        if segment.text.contains(['\n', '\r', '\u{1b}']) {
            continue;
        }
        let text: String = segment.text.chars().take(160).collect();
        let candidate = format!("{left}   [{text}]");
        if candidate.width().saturating_add(reserved) <= width {
            left = candidate;
        }
    }
    left
}

fn extension_slash_command(descriptor: CommandDescriptor) -> SlashCommand {
    let source = match &descriptor.source {
        CommandSource::Extension { id, .. } => id.to_string(),
        CommandSource::Local { .. } => "Local command".to_owned(),
        CommandSource::BuiltIn => "Built in".to_owned(),
    };
    let action = match descriptor.id.as_str() {
        "tokio.builtin:clear" => SlashAction::Clear,
        "tokio.builtin:model" => SlashAction::Model,
        "tokio.builtin:permissions" => SlashAction::Permissions,
        "tokio.builtin:providers" => SlashAction::Provider,
        "tokio.builtin:extensions" => SlashAction::Extensions,
        _ => SlashAction::Extension(descriptor.id),
    };
    let description = if matches!(descriptor.source, CommandSource::BuiltIn) {
        descriptor.description
    } else {
        format!("{} · {source}", descriptor.description)
    };
    SlashCommand {
        name: Cow::Owned(descriptor.name),
        description: Cow::Owned(description),
        usage: descriptor.usage.map(Cow::Owned),
        action,
    }
}

fn command_usage_visible(command_name: &str, input: &str) -> bool {
    input == command_name
        || input
            .strip_prefix(command_name)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn slash_command_description(command: &SlashCommand, fully_typed: bool) -> Cow<'static, str> {
    if fully_typed {
        command
            .usage
            .as_ref()
            .map(|usage| Cow::Owned(format!("Usage: {usage}")))
            .unwrap_or_else(|| command.description.clone())
    } else {
        command.description.clone()
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    let days = total_seconds / (24 * 60 * 60);
    let hours = (total_seconds / (60 * 60)) % 24;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{days}d {hours}h {minutes}m {seconds}s")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn working_indicator_line(elapsed: Duration, interrupting: bool) -> Line<'static> {
    let elapsed = format_elapsed(elapsed);
    let (activity, hint) = if interrupting {
        ("Interrupting…", "ctrl-c to force exit")
    } else {
        ("Working", "esc to interrupt")
    };
    Line::from(vec![
        Span::styled("• ", theme::assistant_bullet()),
        Span::styled(activity, theme::bold()),
        Span::styled(format!(" ({elapsed} • {hint})"), theme::dim()),
    ])
}

fn model_options(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &[
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-sonnet-5",
            "claude-haiku-4-5",
        ],
        "openai" => &[
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.2",
        ],
        "deepseek" => &[
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "deepseek-chat",
            "deepseek-reasoner",
        ],
        _ => &[],
    }
}

fn model_context_window(provider: &str, model: &str) -> Option<u64> {
    match provider {
        "anthropic" if model.contains("haiku") => Some(200_000),
        "anthropic" => Some(1_000_000),
        "openai" if model.starts_with("gpt-5.6") => Some(372_000),
        "openai" if model == "gpt-5.4" => Some(1_000_000),
        "openai" => Some(272_000),
        "deepseek" => Some(1_000_000),
        _ => None,
    }
}

fn context_before_compaction(provider: &str, window: u64, max_output_tokens: u32) -> u64 {
    if provider == "openai" {
        window.saturating_mul(9) / 10
    } else {
        window.saturating_sub(u64::from(max_output_tokens).min(20_000))
    }
}

fn effort_options(provider: &str, model: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" if model == "claude-haiku-4-5" => &[],
        "anthropic" => &["low", "medium", "high", "xhigh", "max"],
        "openai" if model == "gpt-5.6-sol" || model == "gpt-5.6-terra" => {
            &["low", "medium", "high", "xhigh", "max", "ultra"]
        }
        "openai" if model == "gpt-5.6-luna" => &["low", "medium", "high", "xhigh", "max"],
        "openai" => &["low", "medium", "high", "xhigh"],
        "deepseek" => &["high", "max"],
        _ => &[],
    }
}

fn slash_command_name_width(commands: &[SlashCommand]) -> usize {
    commands
        .iter()
        .map(|command| command.name.width())
        .max()
        .unwrap_or_default()
}

fn permission_mode_indicator(mode: Mode, active: Mode) -> &'static str {
    if mode == active { "✓ " } else { "  " }
}

fn permission_height(request: &PermissionRequest, width: u16) -> u16 {
    let content_width = width.saturating_sub(4).max(1);
    let (intro, summary, actions) = permission_sections(request, content_width);
    u16::try_from(intro.len() + summary.len() + actions.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
}

fn render_permission(frame: &mut Frame, area: Rect, request: &PermissionRequest) {
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ◆ ", theme::approval()),
            Span::styled("Permission required ", theme::bold()),
        ]))
        .borders(Borders::ALL)
        .border_style(theme::approval())
        .padding(Padding::horizontal(1))
        .style(theme::composer_bg());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let (intro, summary, actions) = permission_sections(request, inner.width);
    let intro_rows = u16::try_from(intro.len()).unwrap_or(u16::MAX);
    let summary_rows = u16::try_from(summary.len()).unwrap_or(u16::MAX);
    let action_rows = u16::try_from(actions.len()).unwrap_or(u16::MAX);
    let intro_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: intro_rows,
    };
    let summary_area = Rect {
        x: inner.x,
        y: inner.y.saturating_add(intro_rows).saturating_add(1),
        width: inner.width,
        height: summary_rows,
    };
    let action_area = Rect {
        x: inner.x,
        y: inner.bottom().saturating_sub(action_rows),
        width: inner.width,
        height: action_rows,
    };
    frame.render_widget(Paragraph::new(intro), intro_area);
    frame.render_widget(Paragraph::new(summary), summary_area);
    frame.render_widget(Paragraph::new(actions), action_area);
}

fn permission_sections(
    request: &PermissionRequest,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let width = usize::from(width.max(1));
    let verb = match request.action {
        Action::Read => "read data",
        Action::Edit => "make changes",
        Action::Execute => "run a command",
    };
    let intro = wrap_permission_text(
        &format!("The agent wants to {verb} using {}:", request.tool),
        width,
        usize::MAX,
    )
    .into_iter()
    .map(Line::from)
    .collect();
    let summary = wrap_permission_text(&request.summary, width, 3)
        .into_iter()
        .map(|line| Line::styled(line, theme::code()))
        .collect();
    let actions = if width >= 76 {
        vec![permission_action_line(&[
            ("Y", "Allow once"),
            ("A", "Allow for session"),
            ("N", "Deny"),
            ("C", "Copy details"),
        ])]
    } else if width >= 30 {
        vec![
            permission_action_line(&[("Y", "Allow once"), ("A", "Session")]),
            permission_action_line(&[("N", "Deny"), ("C", "Copy")]),
        ]
    } else {
        [
            ("Y", "Allow once"),
            ("A", "Allow for session"),
            ("N", "Deny"),
            ("C", "Copy details"),
        ]
        .into_iter()
        .map(|action| permission_action_line(&[action]))
        .collect()
    };
    (intro, summary, actions)
}

fn permission_action_line(actions: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in actions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(format!(" {key} "), theme::key()));
        spans.push(Span::raw(format!(" {label}")));
    }
    Line::from(spans)
}

fn permission_copy_text(request: &PermissionRequest) -> String {
    format!("{}: {}", request.tool, request.summary)
}

fn wrap_permission_text(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for source_line in text.lines() {
        let mut current = String::new();
        for word in source_line.split_whitespace() {
            let separator = usize::from(!current.is_empty());
            if !current.is_empty() && current.width() + separator + word.width() > width {
                rows.push(std::mem::take(&mut current));
            }
            if word.width() > width {
                for grapheme in word.graphemes(true) {
                    if current.width() + grapheme.width() > width && !current.is_empty() {
                        rows.push(std::mem::take(&mut current));
                    }
                    current.push_str(grapheme);
                }
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        let last = rows.last_mut().expect("max_rows is non-zero");
        while last.width() + 1 > width {
            last.pop();
        }
        last.push('…');
    }
    rows
}

fn footer_line(left: &str, right: &str, width: usize) -> Line<'static> {
    const HORIZONTAL_PADDING: usize = 2;

    if width == 0 {
        return Line::default();
    }
    let horizontal_padding = HORIZONTAL_PADDING.min(width / 2);
    let content_width = width.saturating_sub(horizontal_padding * 2);
    let right = truncate_start(right, content_width);
    let right_width = right.chars().count();
    let left_budget = content_width.saturating_sub(right_width + usize::from(right_width > 0));
    let left = truncate_end(left, left_budget);
    let pad = content_width.saturating_sub(left.chars().count() + right_width);
    Line::from(vec![
        Span::raw(" ".repeat(horizontal_padding)),
        Span::styled(left, theme::dim()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, theme::dim()),
        Span::raw(" ".repeat(horizontal_padding)),
    ])
}

fn footer_status(
    left: &str,
    width: usize,
    context_window: Option<u64>,
    input_tokens: u64,
) -> String {
    const HORIZONTAL_PADDING: usize = 2;
    const MIN_LEFT_RESERVE: usize = 16;

    let padding = HORIZONTAL_PADDING.min(width / 2);
    let available = width.saturating_sub(padding * 2);
    let left_reserve = left.width().min(MIN_LEFT_RESERVE);
    let gap = usize::from(left_reserve > 0);
    for expanded in [true, false] {
        let meter = context_meter(context_window, input_tokens, expanded);
        if meter.width() + left_reserve + gap <= available {
            return meter;
        }
    }
    context_meter(context_window, input_tokens, false)
}

fn context_meter(context_window: Option<u64>, input_tokens: u64, expanded: bool) -> String {
    const BAR_WIDTH: usize = 8;

    let Some(capacity) = context_window.filter(|capacity| *capacity > 0) else {
        return format!("{} N/A", "─".repeat(BAR_WIDTH));
    };
    let percent = input_tokens
        .saturating_mul(100)
        .saturating_div(capacity)
        .min(100);
    let filled = usize::try_from(percent.saturating_mul(BAR_WIDTH as u64).saturating_add(50) / 100)
        .unwrap_or(BAR_WIDTH)
        .min(BAR_WIDTH);
    let usage = if expanded {
        format!(" ({} tokens)", compact_token_count(input_tokens))
    } else {
        String::new()
    };
    format!(
        "{}{} {percent}%{usage}",
        "━".repeat(filled),
        "─".repeat(BAR_WIDTH - filled),
    )
}

fn compact_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }

    let (value, suffix) = if tokens < 1_000_000 {
        (tokens, "k")
    } else {
        (tokens / 1_000, "m")
    };
    if value % 1_000 == 0 {
        format!("{}{suffix}", value / 1_000)
    } else {
        format!("{}.{:01}{suffix}", value / 1_000, value % 1_000 / 100)
    }
}

fn session_context(model: &str, effort: Option<&str>, cwd: &str) -> String {
    let model = if model.is_empty() {
        String::new()
    } else {
        format!("{model} {}", effort.unwrap_or("N/A"))
    };
    match (model.is_empty(), cwd.is_empty()) {
        (false, false) => format!("{model} · {cwd}"),
        (false, true) => model,
        (true, false) => cwd.to_owned(),
        (true, true) => String::new(),
    }
}

fn truncate_end(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", text.chars().take(width - 1).collect::<String>())
}

fn truncate_start(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!(
        "…{}",
        text.chars().skip(count - (width - 1)).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_descriptors() -> Vec<CommandDescriptor> {
        [
            (
                "tokio.builtin:clear",
                "/clear",
                "Clear the conversation and start fresh",
            ),
            (
                "tokio.builtin:model",
                "/model",
                "Switch models for this session",
            ),
            (
                "tokio.builtin:goal",
                "/goal",
                "Keep working autonomously until an objective is complete",
            ),
            (
                "tokio.builtin:loop",
                "/loop",
                "Run a prompt repeatedly on an interval",
            ),
            (
                "tokio.builtin:permissions",
                "/permissions",
                "Select how the agent asks for permission",
            ),
            (
                "tokio.builtin:providers",
                "/providers",
                "Connect or switch AI providers",
            ),
        ]
        .into_iter()
        .map(|(id, name, description)| CommandDescriptor {
            id: CommandId::new(id),
            name: name.to_owned(),
            description: description.to_owned(),
            usage: match name {
                "/goal" => Some("/goal <objective> | pause | resume | cancel".to_owned()),
                "/loop" => Some("/loop <10s|5m|2h> <prompt> | cancel".to_owned()),
                _ => None,
            },
            source: CommandSource::BuiltIn,
            available_while_running: false,
        })
        .collect()
    }

    fn projection() -> FrontendProjection {
        FrontendProjection::new(
            String::new(),
            String::new(),
            None,
            String::new(),
            None,
            0,
            Mode::Suggest,
            Vec::new(),
            builtin_descriptors(),
            Vec::new(),
        )
    }

    #[test]
    fn streaming_events_do_not_cancel_manual_scroll() {
        let mut projection = projection();
        projection.scroll_up = 10;
        projection.apply(AgentEvent::TextDelta("more output".into()));
        assert_eq!(projection.scroll_up, 10);
    }

    #[test]
    fn scroll_button_click_jumps_to_the_bottom() {
        let mut projection = projection();
        projection.scroll_up = 20;
        projection.scroll_button_area = Some(Rect::new(10, 5, 22, 1));

        let consumed = projection.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });

        assert!(consumed);
        assert_eq!(projection.scroll_up, 0);
        assert_eq!(projection.scroll_button_height(), 0);
    }

    #[test]
    fn plain_o_is_inserted_into_composer() {
        let mut projection = projection();

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::UserMessage(message)) if message == "o"
        ));
    }

    #[test]
    fn shift_enter_inserts_a_newline() {
        let mut projection = projection();
        projection.composer.replace("first line");

        projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));

        assert_eq!(projection.composer.text(), "first line\n");
    }

    #[test]
    fn shift_enter_does_not_execute_a_slash_command() {
        let mut projection = projection();
        projection.composer.replace("/clear");

        let effect = projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));

        assert!(matches!(effect, FrontendEffect::None));
        assert_eq!(projection.composer.text(), "/clear\n");
    }

    #[test]
    fn multiline_paste_is_inserted_without_submitting() {
        let mut projection = projection();
        let pasted = "╭─ Shell\r\n│ cargo fmt --all\r\n│ cargo test";

        projection.on_paste(pasted);

        assert_eq!(
            projection.composer.text(),
            "╭─ Shell\n│ cargo fmt --all\n│ cargo test"
        );
        assert!(!projection.running);
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::UserMessage(message))
                if message == "╭─ Shell\n│ cargo fmt --all\n│ cargo test"
        ));
    }

    #[test]
    fn ctrl_c_clears_a_draft_then_quits_on_second_press() {
        let mut projection = projection();
        projection.composer.replace("unfinished draft");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(matches!(projection.on_key(ctrl_c), FrontendEffect::None));
        assert!(projection.composer.text().is_empty());
        assert!(projection.quit_confirmation_active());
        assert!(matches!(projection.on_key(ctrl_c), FrontendEffect::Quit));
    }

    #[test]
    fn ctrl_c_confirmation_expires() {
        let mut projection = projection();
        projection.quit_armed_until = Instant::now().checked_sub(Duration::from_secs(1));

        assert!(!projection.quit_confirmation_active());
    }

    #[test]
    fn cumulative_usage_replaces_previous_snapshot() {
        let mut projection = projection();
        let cumulative = Usage {
            input_tokens: 20,
            output_tokens: 4,
            cache_read_tokens: 5,
            cache_write_tokens: 2,
        };
        projection.apply(AgentEvent::TurnUsage(cumulative));
        assert_eq!(projection.usage, cumulative);
    }

    #[test]
    fn elapsed_timer_pauses_and_resumes_without_counting_the_wait() {
        let mut projection = projection();
        projection.running = true;
        projection.elapsed = Duration::from_secs(3);
        projection.started_at = Instant::now().checked_sub(Duration::from_secs(2));

        projection.pause_elapsed_timer();

        assert!(projection.started_at.is_none());
        assert!(projection.elapsed >= Duration::from_secs(5));
        assert!(projection.elapsed < Duration::from_secs(6));
        assert_eq!(projection.current_elapsed(), projection.elapsed);

        let paused = projection.elapsed;
        projection.resume_elapsed_timer();
        assert!(projection.started_at.is_some());
        assert_eq!(projection.elapsed, paused);
    }

    #[test]
    fn elapsed_time_uses_seconds_minutes_hours_and_days() {
        assert_eq!(format_elapsed(Duration::ZERO), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(520)), "8m 40s");
        assert_eq!(format_elapsed(Duration::from_secs(3_661)), "1h 1m 1s");
        assert_eq!(format_elapsed(Duration::from_secs(90_061)), "1d 1h 1m 1s");
    }

    #[test]
    fn working_indicator_contains_elapsed_time_and_interrupt_hint() {
        assert_eq!(
            working_indicator_line(Duration::from_secs(520), false).to_string(),
            "• Working (8m 40s • esc to interrupt)"
        );
        assert_eq!(
            working_indicator_line(Duration::from_secs(18), true).to_string(),
            "• Interrupting… (18s • ctrl-c to force exit)"
        );
    }

    #[test]
    fn footer_never_exceeds_available_width() {
        for width in 0..30 {
            let line = footer_line(
                "bash would run something — y allow",
                "12.3s · ↑123 ↓45",
                width,
            );
            assert!(line.width() <= width);
        }
    }

    #[test]
    fn footer_has_matching_horizontal_insets() {
        let line = footer_line("esc to interrupt", "1.2s", 40).to_string();
        assert!(line.starts_with("  esc to interrupt"));
        assert!(line.ends_with("1.2s  "));
        assert_eq!(line.chars().count(), 40);
    }

    #[test]
    fn context_meter_uses_an_eight_cell_rounded_bar() {
        assert_eq!(
            context_meter(Some(100_000), 62_000, true),
            "━━━━━─── 62% (62k tokens)"
        );
        assert_eq!(
            context_meter(Some(200_000), 124_000, true),
            "━━━━━─── 62% (124k tokens)"
        );
        assert_eq!(context_meter(Some(100_000), 0, false), "──────── 0%");
        assert_eq!(context_meter(None, 10, true), "──────── N/A");
        assert_eq!(
            context_before_compaction("openai", 372_000, 32_000),
            334_800
        );
        assert_eq!(
            context_before_compaction("anthropic", 1_000_000, 32_000),
            980_000
        );
        assert_eq!(
            context_meter(Some(334_800), 334_800, true),
            "━━━━━━━━ 100% (334.8k tokens)"
        );
    }

    #[test]
    fn footer_always_keeps_meter_and_expands_token_count_when_it_fits() {
        let wide = footer_status("gpt-5 · ~/micro", 80, Some(100), 62);
        assert_eq!(wide, "━━━━━─── 62% (62 tokens)");

        let narrow = footer_status("gpt-5 · ~/a/long/project/path", 35, Some(100), 62);
        assert_eq!(narrow, "━━━━━─── 62%");
        assert!(
            footer_line("long left status", &narrow, 20)
                .to_string()
                .contains("━━━━━─── 62%")
        );
    }

    #[test]
    fn session_context_is_compact() {
        assert_eq!(
            session_context("gpt-5", Some("high"), "~/Projects/micro"),
            "gpt-5 high · ~/Projects/micro"
        );
        assert_eq!(session_context("claude", None, "~"), "claude N/A · ~");
    }

    #[test]
    fn slash_picker_filters_and_runs_commands() {
        let mut projection = projection();
        projection.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(projection.slash_matches().len(), 5);
        assert_eq!(projection.slash_picker_height(), 7);
        assert!(
            projection
                .slash_matches()
                .iter()
                .all(|command| command.name != "/context")
        );

        projection.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(projection.slash_matches().len(), 1);
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::Clear)
        ));
        assert_eq!(projection.slash_picker_height(), 0);
    }

    #[test]
    fn clear_resets_context_meter_usage() {
        let mut projection = projection();
        projection.context_window = Some(100_000);
        projection.apply(AgentEvent::RequestUsage(Usage {
            input_tokens: 62_000,
            ..Usage::default()
        }));
        assert_eq!(
            footer_status(
                "",
                80,
                projection.context_window,
                projection.last_request_usage.input_tokens,
            ),
            "━━━━━─── 62% (62k tokens)"
        );

        projection.composer.replace("/clear");
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::Clear)
        ));

        assert_eq!(projection.last_request_usage, Usage::default());
        assert_eq!(
            footer_status(
                "",
                80,
                projection.context_window,
                projection.last_request_usage.input_tokens,
            ),
            "──────── 0% (0 tokens)"
        );
    }

    #[test]
    fn autonomy_command_descriptions_show_usage_only_when_fully_typed() {
        let projection = projection();
        let goal = projection
            .extension_commands
            .iter()
            .find(|command| command.name == "/goal")
            .expect("goal command");
        let loop_command = projection
            .extension_commands
            .iter()
            .find(|command| command.name == "/loop")
            .expect("loop command");

        assert_eq!(
            slash_command_description(goal, false),
            "Keep working autonomously until an objective is complete"
        );
        assert_eq!(
            slash_command_description(loop_command, false),
            "Run a prompt repeatedly on an interval"
        );
        assert_eq!(
            slash_command_description(goal, true),
            "Usage: /goal <objective> | pause | resume | cancel"
        );
        assert_eq!(
            slash_command_description(loop_command, true),
            "Usage: /loop <10s|5m|2h> <prompt> | cancel"
        );
    }

    #[test]
    fn extension_commands_are_invoked_by_stable_id_and_unknown_slashes_stay_local() {
        let mut projection = projection();
        let descriptor = CommandDescriptor {
            id: CommandId::new("local.project.review:review"),
            name: "/review".into(),
            description: "Review changes".into(),
            usage: None,
            source: CommandSource::Local {
                path: "review.md".into(),
            },
            available_while_running: false,
        };
        projection
            .extension_commands
            .push(extension_slash_command(descriptor));
        projection.composer.replace("/review tests");
        assert!(matches!(
            projection.submit(),
            FrontendEffect::Command(UiCommand::InvokeCommand { id, arguments })
                if id.as_str() == "local.project.review:review" && arguments == "tests"
        ));

        projection.running = false;
        projection.composer.replace("/typo");
        assert!(matches!(projection.submit(), FrontendEffect::None));
        assert_eq!(
            projection.command_error.as_deref(),
            Some("Unknown command: /typo")
        );
        assert_eq!(projection.composer.text(), "/typo");
    }

    #[test]
    fn slash_palette_is_hidden_for_an_empty_composer() {
        let projection = projection();

        assert!(projection.slash_display_matches().is_empty());
        assert_eq!(projection.slash_picker_height(), 0);
    }

    #[test]
    fn autonomy_usage_stays_visible_while_typing_arguments() {
        let mut projection = projection();

        projection.composer.replace("/loop 5m check the deploy");
        assert!(!projection.slash_picker_open());
        assert_eq!(projection.slash_picker_height(), 3);
        assert!(matches!(
            projection.slash_display_matches().as_slice(),
            [command] if command.name == "/loop"
        ));
        assert!(command_usage_visible("/loop", projection.composer.text()));

        projection.composer.replace("/goal finish the migration");
        assert_eq!(projection.slash_picker_height(), 3);
        assert!(matches!(
            projection.slash_display_matches().as_slice(),
            [command] if command.name == "/goal"
        ));
    }

    #[test]
    fn command_validation_errors_are_reported_by_the_shared_router() {
        let mut projection = projection();
        projection.composer.replace("/loop .");

        assert!(matches!(
            projection.submit(),
            FrontendEffect::Command(UiCommand::InvokeCommand { id, arguments })
                if id.as_str() == "tokio.builtin:loop" && arguments == "."
        ));
        projection.apply(AgentEvent::CommandHandled(Err("Invalid interval".into())));
        assert_eq!(
            projection.command_error.as_deref(),
            Some("Invalid interval")
        );
        assert_eq!(projection.command_feedback_height(), 2);
        assert!(!projection.running);
    }

    #[test]
    fn goal_and_loop_commands_show_temporary_success_notices() {
        for (input, expected) in [
            ("/goal finish the migration", "Goal started"),
            ("/goal cancel", "Goal cancelled"),
            ("/goal pause", "Goal paused"),
            ("/goal resume", "Goal resumed"),
            ("/loop 10s check the deploy", "Loop started"),
            ("/loop cancel", "Loop cancelled"),
        ] {
            let mut projection = projection();
            projection.composer.replace(input);

            assert!(matches!(projection.submit(), FrontendEffect::Command(_)));
            projection.apply(AgentEvent::CommandHandled(Ok(Some(expected.to_owned()))));
            assert_eq!(projection.active_command_notice(), Some(expected));
            assert_eq!(projection.command_feedback_height(), 2);

            projection
                .command_notice
                .as_mut()
                .expect("command notice")
                .1 = Instant::now();
            assert_eq!(projection.active_command_notice(), None);
            assert_eq!(projection.command_feedback_height(), 0);
        }
    }

    #[test]
    fn slash_picker_spacing_tracks_the_longest_command_name() {
        let width = slash_command_name_width(&projection().extension_commands);
        assert_eq!(width, "/permissions".width());
        assert_eq!(
            format!("{:<width$}  {}", "/clear", "Clear conversation"),
            "/clear        Clear conversation"
        );
        assert_eq!(
            format!("{:<width$}  {}", "/permissions", "Select mode"),
            "/permissions  Select mode"
        );
    }

    #[test]
    fn active_permission_indicator_has_a_fixed_leading_column() {
        assert_eq!(
            permission_mode_indicator(Mode::Suggest, Mode::Suggest),
            "✓ "
        );
        assert_eq!(
            permission_mode_indicator(Mode::AutoEdit, Mode::Suggest),
            "  "
        );
    }

    #[test]
    fn provider_session_update_preserves_rendered_conversation() {
        let mut projection = projection();
        projection.begin_turn("keep this visible");
        projection.apply(AgentEvent::TextDelta("still here".into()));
        projection.apply(AgentEvent::RequestUsage(Usage {
            input_tokens: 62_000,
            ..Usage::default()
        }));
        let cells = projection.transcript.len();

        projection.update_session(
            "openai".into(),
            "gpt-5.6-sol".into(),
            Some("high".into()),
            "/tmp".into(),
            Some(300_000),
            8_192,
            Mode::FullAuto,
            builtin_descriptors(),
            Vec::new(),
        );

        assert_eq!(projection.transcript.len(), cells);
        assert_eq!(projection.provider, "openai");
        assert_eq!(projection.model, "gpt-5.6-sol");
        assert!(!projection.running);
        assert_eq!(
            footer_status(
                "gpt-5.6-sol · /tmp",
                80,
                projection
                    .context_window
                    .filter(|_| projection.context_usage_known),
                projection.last_request_usage.input_tokens,
            ),
            "──────── N/A"
        );

        projection.apply(AgentEvent::RequestUsage(Usage {
            input_tokens: 30_000,
            ..Usage::default()
        }));
        assert_eq!(
            footer_status(
                "gpt-5.6-sol · /tmp",
                80,
                projection
                    .context_window
                    .filter(|_| projection.context_usage_known),
                projection.last_request_usage.input_tokens,
            ),
            "━─────── 10% (30k tokens)"
        );
    }

    #[test]
    fn model_command_selects_a_new_model() {
        let mut projection = projection();
        projection.provider = "openai".into();
        projection.model = "gpt-5.6-sol".into();
        projection.composer.replace("/model");

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::SetModel(model)) if model == "gpt-5.6-terra"
        ));
        assert_eq!(projection.model, "gpt-5.6-terra");
    }

    #[test]
    fn provider_model_and_effort_options_match_the_api_catalogs() {
        assert_eq!(
            model_options("anthropic"),
            &[
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-sonnet-5",
                "claude-haiku-4-5",
            ]
        );
        assert_eq!(
            effort_options("anthropic", "claude-fable-5"),
            &["low", "medium", "high", "xhigh", "max"]
        );
        assert!(effort_options("anthropic", "claude-haiku-4-5").is_empty());

        assert_eq!(
            model_options("openai"),
            &[
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.2",
            ]
        );
        assert_eq!(
            effort_options("openai", "gpt-5.6-sol"),
            &["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(
            effort_options("openai", "gpt-5.6-luna"),
            &["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            effort_options("openai", "gpt-5.5"),
            &["low", "medium", "high", "xhigh"]
        );
        assert_eq!(model_context_window("openai", "gpt-5.6-sol"), Some(372_000));
        assert_eq!(model_context_window("openai", "gpt-5.4"), Some(1_000_000));
        assert_eq!(
            model_context_window("openai", "gpt-5.4-mini"),
            Some(272_000)
        );

        assert_eq!(
            model_options("deepseek"),
            &[
                "deepseek-v4-flash",
                "deepseek-v4-pro",
                "deepseek-chat",
                "deepseek-reasoner",
            ]
        );
        assert_eq!(
            effort_options("deepseek", "deepseek-v4-pro"),
            &["high", "max"]
        );
    }

    #[test]
    fn model_selection_immediately_opens_effort_picker() {
        let mut projection = projection();
        projection.provider = "deepseek".into();
        projection.model = "deepseek-v4-flash".into();
        projection.effort = Some("high".into());
        projection.composer.replace("/model");

        projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::SetModel(model)) if model == "deepseek-v4-pro"
        ));
        assert_eq!(projection.permissions_panel_height(), 4);

        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::SetReasoningEffort(Some(effort))) if effort == "max"
        ));
    }

    #[test]
    fn effort_is_not_a_slash_command() {
        let mut projection = projection();
        projection.composer.replace("/effort");
        assert!(projection.slash_matches().is_empty());
    }

    #[test]
    fn enter_steers_a_running_turn() {
        let mut projection = projection();
        projection.running = true;
        projection.composer.replace("check the failing test first");

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::Steer(message))
                if message == "check the failing test first"
        ));
        assert!(projection.running);
        assert!(projection.composer.text().is_empty());
    }

    #[test]
    fn permissions_command_selects_a_new_runtime_mode() {
        let mut projection = projection();
        projection.composer.replace("/permissions");

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        assert_eq!(projection.permissions_panel_height(), 5);

        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::SetPermissionMode(Mode::AutoEdit))
        ));
        assert_eq!(projection.permission_mode, Mode::AutoEdit);
        assert_eq!(projection.permissions_panel_height(), 0);
    }

    #[test]
    fn configuration_slash_commands_remain_available_while_running() {
        let mut projection = projection();
        projection.running = true;

        projection.composer.replace("/permissions");
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        assert_eq!(projection.permissions_panel_height(), 5);

        projection.permissions_selected = None;
        projection.composer.replace("/providers");
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        assert_eq!(projection.provider_change_notice_height(), 6);

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::ConfigureProvider
        ));
        assert_eq!(projection.provider_change_notice_height(), 0);

        projection.composer.replace("/clear");
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
    }

    #[test]
    fn permissions_picker_can_be_cancelled_without_changing_mode() {
        let mut projection = projection();
        projection.composer.replace("/permissions");
        projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        projection.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(projection.permission_mode, Mode::Suggest);
        assert_eq!(projection.permissions_panel_height(), 0);
    }

    #[test]
    fn escape_clears_a_running_draft_before_interrupting() {
        let mut projection = projection();
        projection.running = true;
        projection.composer.replace("queued follow-up");

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        assert!(projection.composer.text().is_empty());
        assert!(!projection.interrupting);

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::Interrupt)
        ));
        assert!(projection.interrupting);
    }

    #[test]
    fn escape_dismisses_provider_notice_and_continues_to_picker() {
        let mut projection = projection();
        projection.running = true;
        projection.composer.replace("/providers");
        projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            FrontendEffect::ConfigureProvider
        ));
        assert!(!projection.provider_change_notice_visible());
    }

    #[test]
    fn slash_picker_hides_when_there_are_no_matches() {
        let mut projection = projection();
        for character in "/unknown".chars() {
            projection.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(projection.slash_picker_height(), 0);
    }

    #[test]
    fn arrows_recall_history_and_restore_draft() {
        let mut projection = projection();
        for message in ["first", "second"] {
            projection.composer.replace(message);
            assert!(matches!(projection.submit(), FrontendEffect::Command(_)));
            projection.running = false;
        }
        projection.composer.replace("unfinished");

        projection.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "second");
        projection.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "first");
        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "second");
        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "unfinished");
    }

    #[test]
    fn up_arrow_recalls_history_loaded_from_a_previous_session() {
        let mut projection = FrontendProjection::new(
            String::new(),
            String::new(),
            None,
            String::new(),
            None,
            0,
            Mode::Suggest,
            vec![
                "from another directory".to_owned(),
                "most recent".to_owned(),
            ],
            Vec::new(),
            Vec::new(),
        );

        projection.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "most recent");
        projection.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "from another directory");
    }

    #[test]
    fn permission_panel_grows_for_details_but_stays_compact() {
        let mut request = PermissionRequest {
            tool: "bash".into(),
            summary: "run: cargo test".into(),
            action: Action::Execute,
        };
        assert_eq!(permission_height(&request, 80), 7);

        request.summary = "x".repeat(200);
        let height = permission_height(&request, 40);
        assert!(height > 7);
        let (_, _, actions) = permission_sections(&request, 36);
        let rendered = actions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Y"));
        assert!(rendered.contains("A"));
        assert!(rendered.contains("N"));
        assert!(rendered.contains("C"));
    }

    #[test]
    fn permission_details_can_be_copied_without_answering() {
        let request = PermissionRequest {
            tool: "bash".into(),
            summary: "cargo test".into(),
            action: Action::Execute,
        };

        assert_eq!(permission_copy_text(&request), "bash: cargo test");
    }
}
