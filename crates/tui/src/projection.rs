use std::borrow::Cow;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use tokio_agent_core::agent::{AgentEvent, UiCommand};
use tokio_agent_core::message::{ToolOutput, Usage};
use tokio_agent_extension_api::{
    CommandDescriptor, CommandId, CommandSource, ExtensionSummary, InteractionRequest,
    InteractionResponse, InteractionSpec, StatusSegment,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::input::is_cancel_key;
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
    Uninstall { id: String },
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
    source: Cow<'static, str>,
    usage: Option<Cow<'static, str>>,
    action: SlashAction,
    available_while_running: bool,
}

#[derive(Clone)]
enum SlashAction {
    Clear,
    Model,
    Provider,
    Extensions,
    Extension(CommandId),
}

const QUIT_CONFIRMATION: Duration = Duration::from_secs(3);
const NOTIFICATION_DURATION: Duration = Duration::from_secs(3);

struct Pending {
    request: InteractionRequest,
    selected: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct Notification {
    message: String,
    expires_at: Instant,
}

impl Notification {
    fn new(message: String) -> Self {
        Self {
            message,
            expires_at: Instant::now() + NOTIFICATION_DURATION,
        }
    }

    fn active_message(&self) -> Option<&str> {
        (self.expires_at > Instant::now()).then_some(self.message.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExtensionTab {
    Discover,
    Installed,
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
    history_entry_to_persist: Option<String>,
    setting_picker: Option<SettingPicker>,
    provider_change_notice: bool,
    last_request_usage: Usage,
    context_usage_known: bool,
    quit_armed_until: Option<Instant>,
    scroll_button_area: Option<Rect>,
    command_error: Option<Notification>,
    command_notice: Option<Notification>,
    extension_commands: Vec<SlashCommand>,
    status_segments: Vec<StatusSegment>,
    extensions: Vec<ExtensionSummary>,
    extension_manager: Option<usize>,
    extension_tab: ExtensionTab,
    extension_details_visible: bool,
    installing_extension: Option<(String, String)>,
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
            history_entry_to_persist: None,
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
            extension_details_visible: false,
            installing_extension: None,
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
        commands: Vec<CommandDescriptor>,
        extensions: Vec<ExtensionSummary>,
    ) {
        self.provider = provider;
        self.model = model;
        self.effort = effort;
        self.cwd = cwd;
        self.context_window = context_window;
        self.max_output_tokens = max_output_tokens;
        self.extension_commands = commands.into_iter().map(extension_slash_command).collect();
        self.extensions = extensions;
        self.installing_extension = None;
        self.extension_manager = self
            .extension_manager
            .map(|selected| selected.min(self.visible_extensions().len().saturating_sub(1)));
        self.pending = None;
        self.running = false;
        self.interrupting = false;
        self.setting_picker = None;
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
        if is_cancel_key(&key)
            && let Some(effect) = self.dismiss_active_menu()
        {
            return effect;
        }
        if let Some(selected) = self.extension_manager {
            let selected_extension = self.visible_extensions().get(selected).copied().cloned();
            match key.code {
                KeyCode::Up => {
                    self.extension_manager = Some(selected.saturating_sub(1));
                    self.extension_details_visible = false;
                }
                KeyCode::Down => {
                    self.extension_manager =
                        Some((selected + 1).min(self.visible_extensions().len().saturating_sub(1)));
                    self.extension_details_visible = false;
                }
                KeyCode::Left => {
                    self.extension_tab = match self.extension_tab {
                        ExtensionTab::Discover => ExtensionTab::Installed,
                        ExtensionTab::Installed => ExtensionTab::Discover,
                    };
                    self.extension_manager = Some(0);
                    self.extension_details_visible = false;
                }
                KeyCode::Right | KeyCode::Tab => {
                    self.extension_tab = match self.extension_tab {
                        ExtensionTab::Discover => ExtensionTab::Installed,
                        ExtensionTab::Installed => ExtensionTab::Discover,
                    };
                    self.extension_manager = Some(0);
                    self.extension_details_visible = false;
                }
                KeyCode::Char('D') => {
                    self.extension_details_visible = !self.extension_details_visible;
                }
                KeyCode::Enter
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
                KeyCode::Char('x')
                    if selected_extension
                        .as_ref()
                        .is_some_and(|extension| extension.installed) =>
                {
                    if let Some(extension) = selected_extension {
                        return FrontendEffect::Extension(ExtensionOperation::Uninstall {
                            id: extension.id.to_string(),
                        });
                    }
                }
                _ => {}
            }
            return FrontendEffect::None;
        }
        if self.provider_change_notice {
            if key.code == KeyCode::Enter {
                self.provider_change_notice = false;
                return FrontendEffect::ConfigureProvider;
            }
            return FrontendEffect::None;
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
        self.command_error = None;
        self.composer.insert_str(&normalized);
        self.leave_history();
        self.slash_selected = 0;
    }

    fn dismiss_active_menu(&mut self) -> Option<FrontendEffect> {
        if self.extension_manager.take().is_some() {
            self.extension_details_visible = false;
            return Some(FrontendEffect::None);
        }
        if self.provider_change_notice {
            self.provider_change_notice = false;
            return Some(FrontendEffect::ConfigureProvider);
        }
        if self.setting_picker.take().is_some() {
            return Some(FrontendEffect::None);
        }
        if self.interrupting || self.pending.is_some() || !self.slash_picker_open() {
            return None;
        }
        self.composer.clear();
        self.slash_selected = 0;
        Some(FrontendEffect::None)
    }

    fn handle_pending_key(&mut self, code: KeyCode, ctrl: bool) -> Option<FrontendEffect> {
        let pending = self.pending.as_mut()?;
        if code == KeyCode::Char('c') && !ctrl {
            if let InteractionSpec::Approval(spec) = &pending.request.spec
                && let Some(text) = &spec.copy_text
            {
                return Some(FrontendEffect::Copy(text.clone()));
            }
        }
        if code == KeyCode::Char('c') && ctrl {
            self.interrupting = true;
            return Some(FrontendEffect::Command(UiCommand::Interrupt));
        }
        let action_id = match &pending.request.spec {
            InteractionSpec::Approval(spec) => {
                let key = match code {
                    KeyCode::Char(value) => Some(value.to_ascii_lowercase().to_string()),
                    KeyCode::Esc => Some("escape".into()),
                    _ => None,
                };
                key.and_then(|key| {
                    spec.actions.iter().find(|action| {
                        action
                            .key_hint
                            .as_ref()
                            .is_some_and(|hint| hint.eq_ignore_ascii_case(&key))
                            || (key == "escape" && action.id == "deny")
                    })
                })
                .map(|action| action.id.clone())
            }
            InteractionSpec::SingleSelect(spec) => match code {
                KeyCode::Up => {
                    pending.selected = pending.selected.saturating_sub(1);
                    return Some(FrontendEffect::None);
                }
                KeyCode::Down => {
                    pending.selected =
                        (pending.selected + 1).min(spec.options.len().saturating_sub(1));
                    return Some(FrontendEffect::None);
                }
                KeyCode::Enter => spec
                    .options
                    .get(pending.selected)
                    .map(|option| option.id.clone()),
                KeyCode::Esc => Some("cancel".into()),
                _ => None,
            },
        };
        let Some(action_id) = action_id else {
            return Some(FrontendEffect::None);
        };
        let response = InteractionResponse {
            id: pending.request.id.clone(),
            owner: pending.request.owner.clone(),
            generation: pending.request.generation,
            action_id,
        };
        self.pending = None;
        self.resume_elapsed_timer();
        Some(FrontendEffect::Command(UiCommand::RespondToInteraction(
            response,
        )))
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
        let match_count = self.slash_matches().len();
        match key.code {
            KeyCode::Up if match_count > 1 => {
                self.slash_selected = self.slash_selected.saturating_sub(1).min(match_count - 1);
                Some(FrontendEffect::None)
            }
            KeyCode::Down if match_count > 1 => {
                self.slash_selected = (self.slash_selected + 1).min(match_count - 1);
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
                    || self
                        .selected_slash_command()
                        .is_some_and(|command| command.available_while_running);
                Some(if allowed {
                    self.run_selected_slash_command()
                } else {
                    self.reject_running_slash_command();
                    FrontendEffect::None
                })
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

    fn record_history(&mut self, entry: String) {
        if self.history.last() != Some(&entry) {
            self.history.push(entry.clone());
            self.history_entry_to_persist = Some(entry);
        }
        self.leave_history();
    }

    pub(crate) fn take_history_entry_to_persist(&mut self) -> Option<String> {
        self.history_entry_to_persist.take()
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

    fn reject_running_slash_command(&mut self) {
        if let Some(command) = self.selected_slash_command() {
            self.composer.clear();
            self.slash_selected = 0;
            self.command_error = Some(Notification::new(format!(
                "{} is unavailable while a turn is running",
                command.name
            )));
        }
    }

    fn run_selected_slash_command(&mut self) -> FrontendEffect {
        let Some(command) = self.selected_slash_command() else {
            return self.submit();
        };
        if !matches!(&command.action, SlashAction::Extension(_)) {
            self.record_history(command.name.to_string());
        }
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
            SlashAction::Provider if self.running => {
                self.show_provider_change_notice();
                FrontendEffect::None
            }
            SlashAction::Provider => FrontendEffect::ConfigureProvider,
            SlashAction::Extensions => {
                self.extension_manager = Some(0);
                self.extension_details_visible = false;
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
                self.composer.clear();
                self.record_history(message.clone());
                if self.running && !command.available_while_running {
                    self.command_error = Some(Notification::new(format!(
                        "{name} is unavailable while a turn is running"
                    )));
                    return FrontendEffect::None;
                }
                return FrontendEffect::Command(UiCommand::InvokeCommand {
                    id,
                    arguments: arguments.trim().to_owned(),
                });
            }
            self.command_error = Some(Notification::new(format!("Unknown command: {name}")));
            return FrontendEffect::None;
        }
        self.composer.clear();
        self.record_history(message.clone());
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
            Some(pending) => interaction_height(&pending.request, width),
            None => self.composer.height(width),
        }
    }

    fn active_command_notice(&self) -> Option<&str> {
        self.command_notice
            .as_ref()
            .and_then(Notification::active_message)
    }

    fn active_command_error(&self) -> Option<&str> {
        self.command_error
            .as_ref()
            .and_then(Notification::active_message)
    }

    pub(crate) fn command_feedback_height(&self) -> u16 {
        if self.active_command_error().is_some() && self.pending.is_none() {
            2
        } else {
            0
        }
    }

    pub(crate) fn render_command_feedback(&self, frame: &mut Frame, area: Rect) {
        if let Some(message) = self.active_command_error() {
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::raw("  "), Span::raw(message)]))
                    .style(theme::error()),
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
        let description_width = slash_command_description_width(&matches, query);
        let has_multiple_matches = matches.len() > 1;
        let lines = matches
            .into_iter()
            .enumerate()
            .map(|(index, command)| {
                let marker = if index == self.slash_selected {
                    "› "
                } else {
                    "  "
                };
                let (marker_style, command_style) =
                    if has_multiple_matches && index == self.slash_selected {
                        (
                            theme::running().add_modifier(ratatui::style::Modifier::BOLD),
                            theme::picker_selected(),
                        )
                    } else {
                        (theme::picker_muted(), theme::picker_muted())
                    };
                let description = slash_command_description(
                    &command,
                    command_usage_visible(&command.name, query),
                );
                let mut spans = vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        format!("{:<width$}", command.name, width = name_width),
                        command_style,
                    ),
                    Span::raw("  "),
                    Span::styled(description.clone(), command_style),
                ];
                let padding = description_width.saturating_sub(description.width());
                spans.push(Span::raw(format!("{:padding$}  ", "")));
                spans.push(Span::styled(command.source, command_style));
                Line::from(spans)
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

    pub(crate) fn settings_panel_height(&self) -> u16 {
        let count = match self.setting_picker {
            Some(SettingPicker::Effort(_)) => self.effort_options().len(),
            Some(SettingPicker::Model(_)) => self.model_options().len(),
            None => 0,
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

    pub(crate) fn render_settings_panel(&self, frame: &mut Frame, area: Rect) {
        if let Some(picker) = self.setting_picker {
            self.render_setting_picker(frame, area, picker);
        }
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
            render_interaction_spec(frame, area, &pending.request, pending.selected);
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
            AgentEvent::ToolOutputDelta { id, text } => {
                self.transcript.tool_output_delta(&id, &text);
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
            AgentEvent::InteractionRequested(request) => {
                self.pause_elapsed_timer();
                let selected = match &request.spec {
                    InteractionSpec::SingleSelect(spec) => spec
                        .selected
                        .as_ref()
                        .and_then(|id| spec.options.iter().position(|option| &option.id == id))
                        .unwrap_or_default(),
                    InteractionSpec::Approval(_) => 0,
                };
                self.pending = Some(Pending { request, selected });
            }
            AgentEvent::InteractionCancelled { id } => {
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.request.id == id)
                {
                    self.pending = None;
                    self.resume_elapsed_timer();
                }
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
                if let Some(segment) = segments
                    .iter()
                    .find(|segment| transient_status(&segment.text))
                {
                    self.command_notice = Some(Notification::new(segment.text.clone()));
                }
                segments.retain(|segment| !transient_status(&segment.text));
                self.status_segments = segments;
            }
            AgentEvent::CommandHandled(result) => match result {
                Ok(notice) => {
                    self.command_notice = notice.map(Notification::new);
                }
                Err(error) => self.command_error = Some(Notification::new(error)),
            },
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
        self.extensions
            .iter()
            .filter(|extension| match self.extension_tab {
                ExtensionTab::Discover => {
                    !extension.installed
                        && self
                            .installing_extension
                            .as_ref()
                            .is_none_or(|(id, _)| id != extension.id.as_str())
                        && !self
                            .extensions
                            .iter()
                            .any(|installed| installed.installed && installed.id == extension.id)
                }
                ExtensionTab::Installed => extension.installed,
            })
            .collect()
    }

    pub(crate) fn begin_extension_operation(&mut self, operation: &ExtensionOperation) {
        if let ExtensionOperation::Install { id, .. } = operation {
            let name = self
                .extensions
                .iter()
                .find(|extension| extension.id.as_str() == id)
                .map_or_else(|| id.clone(), |extension| extension.name.clone());
            self.installing_extension = Some((id.clone(), name));
            self.extension_manager = self
                .extension_manager
                .map(|selected| selected.min(self.visible_extensions().len().saturating_sub(1)));
        }
    }

    pub(crate) fn extension_operation_failed(&mut self) {
        self.installing_extension = None;
    }

    pub(crate) fn extension_manager_height(&self) -> u16 {
        const MAX_VISIBLE: usize = 3;

        let Some(selected) = self.extension_manager else {
            return 0;
        };
        let visible = self.visible_extensions();
        let rows = visible.len().min(MAX_VISIBLE);
        let mut height = if rows == 0 { 5 } else { rows * 3 + 4 };
        if self.extension_details_visible
            && let Some(extension) = visible.get(selected)
        {
            height += 1;
            height += usize::from(!extension.commands.is_empty());
            height += usize::from(!extension.capabilities.is_empty());
        }
        u16::try_from(height).unwrap_or(u16::MAX)
    }

    pub(crate) fn render_extension_manager(&self, frame: &mut Frame, area: Rect) {
        const MAX_VISIBLE: usize = 3;

        let Some(selected) = self.extension_manager else {
            return;
        };
        if area.height == 0 {
            return;
        }
        frame.render_widget(Block::default().style(theme::picker_bg()), area);
        let mut lines = vec![extension_tabs_line(self.extension_tab), Line::default()];
        let visible = self.visible_extensions();
        if visible.is_empty() {
            lines.push(Line::styled("    No extensions", theme::picker_muted()));
        }
        let start = if visible.len() > MAX_VISIBLE {
            selected
                .saturating_sub(MAX_VISIBLE - 1)
                .min(visible.len() - MAX_VISIBLE)
        } else {
            0
        };
        for (index, extension) in visible.iter().enumerate().skip(start).take(MAX_VISIBLE) {
            let is_selected = index == selected;
            let marker = if is_selected { "  › " } else { "    " };
            let style = if is_selected {
                theme::picker_selected()
            } else {
                theme::picker_muted()
            };
            lines.push(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(
                    format!(
                        "{}  v{}{}",
                        extension.name,
                        extension.version,
                        if extension.local_override {
                            "  [override]"
                        } else {
                            ""
                        }
                    ),
                    style,
                ),
            ]));
            let source = match &extension.origin {
                tokio_agent_extension_api::ExtensionOrigin::Local { path }
                    if extension.local_override =>
                {
                    format!(" · {}", compact_home_path(path))
                }
                _ => String::new(),
            };
            lines.push(Line::styled(
                format!("      {}{source}", extension.id),
                theme::picker_muted(),
            ));
            lines.push(Line::styled(
                format!("      {}", extension.description),
                theme::picker_muted(),
            ));

            if is_selected && self.extension_details_visible {
                if !extension.commands.is_empty() {
                    lines.push(Line::styled(
                        format!("      Commands: {}", extension.commands.join(", ")),
                        theme::picker_muted(),
                    ));
                }
                let context = if extension.tools.is_empty() {
                    "none until invoked".to_owned()
                } else {
                    format!("tools: {}", extension.tools.join(", "))
                };
                lines.push(Line::styled(
                    format!("      Context cost: {context}"),
                    theme::picker_muted(),
                ));
                if !extension.capabilities.is_empty() {
                    let permissions = extension
                        .capabilities
                        .iter()
                        .map(|capability| format!("{capability:?}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(Line::styled(
                        format!("      Permissions: {permissions}"),
                        theme::picker_muted(),
                    ));
                }
            }
        }

        let inner = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Paragraph::new(lines), inner);
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
        let status = self
            .active_command_notice()
            .map(str::to_owned)
            .or_else(|| status_text(&self.status_segments))
            .or_else(|| {
                self.installing_extension
                    .as_ref()
                    .map(|(_, name)| format!("Installing {name} extension"))
            });
        let left = status.map_or(left.clone(), |text| {
            append_status_text(left, &text, &right, width)
        });
        frame.render_widget(Paragraph::new(footer_line(&left, &right, width)), area);
    }
}

fn extension_tabs_line(selected: ExtensionTab) -> Line<'static> {
    let style = |tab| {
        if tab == selected {
            theme::picker_selected()
        } else {
            theme::picker_muted()
        }
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled("Discover", style(ExtensionTab::Discover)),
        Span::raw("  "),
        Span::styled("Installed", style(ExtensionTab::Installed)),
    ])
}

fn compact_home_path(path: &str) -> String {
    let path = std::path::Path::new(path);
    let Some(home) = dirs::home_dir() else {
        return path.to_string_lossy().into_owned();
    };
    let Ok(relative) = path.strip_prefix(home) else {
        return path.to_string_lossy().into_owned();
    };
    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", relative.to_string_lossy())
    }
}

fn transient_status(text: &str) -> bool {
    matches!(
        text,
        "goal: complete" | "goal: blocked" | "goal: cancelled" | "loop: stopped"
    )
}

fn status_text(segments: &[StatusSegment]) -> Option<String> {
    segments
        .iter()
        .find(|segment| !segment.text.contains(['\n', '\r', '\u{1b}']))
        .map(|segment| segment.text.chars().take(160).collect())
}

fn append_status_text(left: String, text: &str, right: &str, width: usize) -> String {
    let reserved = right.width().saturating_add(2);
    let candidate = format!("{left}   [{text}]");
    if candidate.width().saturating_add(reserved) <= width {
        candidate
    } else {
        left
    }
}

fn extension_slash_command(descriptor: CommandDescriptor) -> SlashCommand {
    let source = match &descriptor.source {
        CommandSource::Extension { id, .. } => id.to_string(),
        CommandSource::Local { .. } => "Local command".to_owned(),
        CommandSource::BuiltIn => "tokio.core".to_owned(),
    };
    let action = match descriptor.id.as_str() {
        "tokio.builtin:clear" => SlashAction::Clear,
        "tokio.builtin:model" => SlashAction::Model,
        "tokio.builtin:providers" => SlashAction::Provider,
        "tokio.builtin:extensions" => SlashAction::Extensions,
        _ => SlashAction::Extension(descriptor.id),
    };
    SlashCommand {
        name: Cow::Owned(descriptor.name),
        description: Cow::Owned(descriptor.description),
        source: Cow::Owned(source),
        usage: descriptor.usage.map(Cow::Owned),
        action,
        available_while_running: descriptor.available_while_running,
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
        (window.saturating_mul(9) / 10).min(250_000)
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

fn slash_command_description_width(commands: &[SlashCommand], query: &str) -> usize {
    commands
        .iter()
        .map(|command| {
            slash_command_description(command, command_usage_visible(&command.name, query)).width()
        })
        .max()
        .unwrap_or_default()
}

fn interaction_height(request: &InteractionRequest, width: u16) -> u16 {
    let width = usize::from(width.saturating_sub(4).max(1));
    let rows = interaction_lines(request, 0, width).len();
    u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2)
}

fn render_interaction_spec(
    frame: &mut Frame,
    area: Rect,
    request: &InteractionRequest,
    selected: usize,
) {
    let title = match &request.spec {
        InteractionSpec::Approval(spec) => &spec.title,
        InteractionSpec::SingleSelect(spec) => &spec.title,
    };
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ◆ ", theme::approval()),
            Span::styled(format!("{title} "), theme::bold()),
        ]))
        .borders(Borders::ALL)
        .border_style(theme::approval())
        .padding(Padding::horizontal(1))
        .style(theme::composer_bg());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        frame.render_widget(
            Paragraph::new(interaction_lines(
                request,
                selected,
                usize::from(inner.width),
            )),
            inner,
        );
    }
}

fn interaction_lines(
    request: &InteractionRequest,
    selected: usize,
    width: usize,
) -> Vec<Line<'static>> {
    match &request.spec {
        InteractionSpec::Approval(spec) => {
            let mut lines = Vec::new();
            for section in &spec.body {
                if let Some(heading) = &section.heading {
                    lines.push(Line::styled(heading.clone(), theme::bold()));
                }
                lines.extend(
                    wrap_interaction_text(&section.text, width, 3)
                        .into_iter()
                        .map(|line| Line::styled(line, theme::code())),
                );
            }
            let mut actions = Vec::new();
            for action in &spec.actions {
                if !actions.is_empty() {
                    actions.push(Span::raw("   "));
                }
                let key = action.key_hint.as_deref().unwrap_or("Enter");
                actions.push(Span::styled(format!(" {key} "), theme::key()));
                actions.push(Span::raw(format!(" {}", action.label)));
            }
            if spec.copy_text.is_some() {
                actions.push(Span::raw("   "));
                actions.push(Span::styled(" C ", theme::key()));
                actions.push(Span::raw(" Copy"));
            }
            lines.push(Line::from(actions));
            lines
        }
        InteractionSpec::SingleSelect(spec) => spec
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let style = if index == selected {
                    theme::picker_selected()
                } else {
                    theme::picker_muted()
                };
                let mut spans = vec![
                    Span::styled(if index == selected { "› " } else { "  " }, style),
                    Span::styled(option.label.clone(), style),
                ];
                if let Some(description) = &option.description {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(description.clone(), theme::picker_muted()));
                }
                Line::from(spans)
            })
            .collect(),
    }
}

fn wrap_interaction_text(text: &str, width: usize, max_rows: usize) -> Vec<String> {
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
            Vec::new(),
            builtin_descriptors(),
            Vec::new(),
        )
    }

    fn extension(installed: bool) -> ExtensionSummary {
        ExtensionSummary {
            id: tokio_agent_extension_api::ExtensionId::new("example.extension"),
            name: "Example".to_owned(),
            version: "1.0.0".to_owned(),
            description: "An example extension".to_owned(),
            origin: tokio_agent_extension_api::ExtensionOrigin::OfficialRegistry {
                registry: "official".to_owned(),
            },
            installed,
            local_override: false,
            capabilities: Vec::new(),
            commands: Vec::new(),
            tools: Vec::new(),
            status_segments: Vec::new(),
        }
    }

    #[test]
    fn extension_tab_indicator_does_not_shift_labels() {
        let expected = "  Discover  Installed";
        for (tab, selected_span) in [(ExtensionTab::Discover, 1), (ExtensionTab::Installed, 3)] {
            let line = extension_tabs_line(tab);
            assert_eq!(line.to_string(), expected);
            assert_eq!(line.spans[selected_span].style, theme::picker_selected());
        }
    }

    #[test]
    fn extension_manager_has_single_blank_row_between_tabs_and_items() {
        let mut projection = projection();
        projection.extensions.push(extension(false));
        projection.extension_manager = Some(0);
        projection.extension_tab = ExtensionTab::Discover;

        let height = projection.extension_manager_height();
        let backend = TestBackend::new(60, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| projection.render_extension_manager(frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                "",
                "  Discover  Installed",
                "",
                "  › Example  v1.0.0",
                "      example.extension",
                "      An example extension",
                "",
            ]
        );
    }

    #[test]
    fn local_override_paths_are_compacted_under_home() {
        let home = dirs::home_dir().expect("test requires a home directory");
        let path = home.join("code/extensions/goal");

        assert_eq!(
            compact_home_path(&path.to_string_lossy()),
            "~/code/extensions/goal"
        );
        assert_eq!(
            compact_home_path("/workspace/extensions/goal"),
            "/workspace/extensions/goal"
        );
    }

    #[test]
    fn installed_extension_menu_labels_local_overrides() {
        let mut local = extension(true);
        local.local_override = true;
        local.origin = tokio_agent_extension_api::ExtensionOrigin::Local {
            path: "/workspace/registry/extensions/loop".into(),
        };
        let mut projection = projection();
        projection.extensions.push(local);
        projection.extension_manager = Some(0);

        let height = projection.extension_manager_height();
        let backend = TestBackend::new(100, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| projection.render_extension_manager(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("[override]"));
        assert!(rendered.contains("example.extension · /workspace/registry/extensions/loop"));
    }

    #[test]
    fn extension_manager_exposes_install_and_uninstall_actions() {
        let mut projection = projection();
        projection.extensions.push(extension(true));
        projection.extension_manager = Some(0);

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            FrontendEffect::Extension(ExtensionOperation::Uninstall { id })
                if id == "example.extension"
        ));

        projection.extensions[0] = extension(false);
        projection.extension_tab = ExtensionTab::Discover;
        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Extension(ExtensionOperation::Install { id, registry })
                if id == "example.extension" && registry == "official"
        ));
    }

    #[test]
    fn installed_extensions_are_not_shown_in_discover() {
        let mut projection = projection();
        projection.extensions = vec![extension(false), extension(true)];
        projection.extension_tab = ExtensionTab::Discover;

        assert!(projection.visible_extensions().is_empty());
    }

    #[test]
    fn installing_extension_disappears_from_discover_immediately() {
        let mut projection = projection();
        projection.extensions.push(extension(false));
        projection.extension_tab = ExtensionTab::Discover;
        projection.extension_manager = Some(0);
        let operation = ExtensionOperation::Install {
            id: "example.extension".to_owned(),
            registry: "official".to_owned(),
        };

        projection.begin_extension_operation(&operation);

        assert!(projection.visible_extensions().is_empty());
        assert_eq!(
            projection.installing_extension,
            Some(("example.extension".to_owned(), "Example".to_owned()))
        );
        assert!(
            append_status_text("session".to_owned(), "Installing Example extension", "", 80,)
                .contains("[Installing Example extension]")
        );

        projection.extension_operation_failed();
        assert_eq!(projection.visible_extensions().len(), 1);
    }

    #[test]
    fn extension_manager_stays_open_after_session_update() {
        let mut projection = projection();
        projection.extensions.push(extension(true));
        projection.extension_manager = Some(0);

        projection.update_session(
            String::new(),
            String::new(),
            None,
            String::new(),
            None,
            0,
            builtin_descriptors(),
            vec![extension(false)],
        );

        assert_eq!(projection.extension_manager, Some(0));
    }

    #[test]
    fn extension_details_are_only_shown_on_request() {
        let mut projection = projection();
        projection.extensions.push(extension(true));
        projection.extension_manager = Some(0);
        let compact_height = projection.extension_manager_height();

        projection.on_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));

        assert!(projection.extension_details_visible);
        assert!(projection.extension_manager_height() > compact_height);
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
    fn ctrl_c_dismisses_every_projection_menu() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        let mut projection = projection();
        projection.extension_manager = Some(0);
        projection.extension_details_visible = true;
        assert!(matches!(projection.on_key(ctrl_c), FrontendEffect::None));
        assert!(projection.extension_manager.is_none());
        assert!(!projection.extension_details_visible);

        projection.setting_picker = Some(SettingPicker::Model(0));
        assert!(matches!(projection.on_key(ctrl_c), FrontendEffect::None));
        assert!(projection.setting_picker.is_none());

        projection.composer.replace("/model");
        assert!(matches!(projection.on_key(ctrl_c), FrontendEffect::None));
        assert!(projection.composer.text().is_empty());

        projection.provider_change_notice = true;
        assert!(matches!(
            projection.on_key(ctrl_c),
            FrontendEffect::ConfigureProvider
        ));
        assert!(!projection.provider_change_notice);
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
            250_000
        );
        assert_eq!(
            context_before_compaction("openai", 272_000, 32_000),
            244_800
        );
        assert_eq!(
            context_before_compaction("anthropic", 1_000_000, 32_000),
            980_000
        );
        assert_eq!(
            context_meter(Some(250_000), 250_000, true),
            "━━━━━━━━ 100% (250k tokens)"
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
    fn slash_command_sources_use_an_aligned_two_space_gutter() {
        let descriptor = |name: &str, description: &str, source: &str| CommandDescriptor {
            id: CommandId::new(format!("{source}:{}", name.trim_start_matches('/'))),
            name: name.to_owned(),
            description: description.to_owned(),
            usage: None,
            source: CommandSource::Extension {
                id: tokio_agent_extension_api::ExtensionId::new(source),
                version: "1.0.0".to_owned(),
            },
            available_while_running: false,
        };
        let mut projection = projection();
        projection.extension_commands = vec![
            extension_slash_command(descriptor("/goal", "Longer description", "tokio.goal")),
            extension_slash_command(descriptor("/loop", "Short", "tokio.loop")),
        ];
        projection.composer.replace("/");

        let height = projection.slash_picker_height();
        let backend = TestBackend::new(60, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| projection.render_slash_picker(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            [
                "",
                "› /goal  Longer description  tokio.goal",
                "  /loop  Short               tokio.loop",
                "",
            ]
        );
        for x in 0..buffer.area.width {
            let cell = buffer.cell((x, 1)).unwrap();
            if !cell.symbol().trim().is_empty() {
                assert!(
                    cell.modifier.contains(ratatui::style::Modifier::BOLD),
                    "selected cell at column {x} was not bold"
                );
            }
        }
    }

    #[test]
    fn a_single_slash_command_match_is_not_bold() {
        let mut projection = projection();
        projection.composer.replace("/providers");

        let height = projection.slash_picker_height();
        let backend = TestBackend::new(70, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| projection.render_slash_picker(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.width)
            .map(|x| buffer.cell((x, 1)).unwrap().symbol())
            .collect::<String>()
            .trim_end()
            .to_owned();

        assert_eq!(
            row,
            "› /providers  Connect or switch AI providers  tokio.core"
        );
        for x in 0..buffer.area.width {
            let cell = buffer.cell((x, 1)).unwrap();
            if !cell.symbol().trim().is_empty() {
                assert!(
                    !cell.modifier.contains(ratatui::style::Modifier::BOLD),
                    "single matching command cell at column {x} was bold"
                );
            }
        }
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

        assert!(
            projection
                .extension_commands
                .iter()
                .all(|command| command.source == "tokio.core")
        );
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
        assert_eq!(projection.transcript.len(), 0);
        assert!(!projection.running);

        projection.running = false;
        projection.composer.replace("/typo");
        assert!(matches!(projection.submit(), FrontendEffect::None));
        assert_eq!(
            projection.active_command_error(),
            Some("Unknown command: /typo")
        );
        assert_eq!(projection.composer.text(), "/typo");
    }

    #[test]
    fn rejected_extension_commands_always_close_the_usage_palette() {
        let mut unavailable = projection();
        unavailable.running = true;
        unavailable.composer.replace("/goal run ls");

        assert!(matches!(unavailable.submit(), FrontendEffect::None));
        assert_eq!(unavailable.composer.text(), "");
        assert!(unavailable.slash_display_matches().is_empty());
        assert_eq!(
            unavailable.active_command_error(),
            Some("/goal is unavailable while a turn is running")
        );

        let mut autonomy_conflict = projection();
        autonomy_conflict.composer.replace("/goal run ls");
        assert!(matches!(
            autonomy_conflict.submit(),
            FrontendEffect::Command(UiCommand::InvokeCommand { .. })
        ));
        autonomy_conflict.apply(AgentEvent::CommandHandled(Err(
            "another extension owns autonomous work".into(),
        )));
        assert_eq!(autonomy_conflict.composer.text(), "");
        assert!(autonomy_conflict.slash_display_matches().is_empty());
        assert_eq!(
            autonomy_conflict.active_command_error(),
            Some("another extension owns autonomous work")
        );

        unavailable.command_error.as_mut().unwrap().expires_at = Instant::now();
        autonomy_conflict.command_error.as_mut().unwrap().expires_at = Instant::now();
        assert_eq!(unavailable.active_command_error(), None);
        assert_eq!(unavailable.command_feedback_height(), 0);
        assert_eq!(autonomy_conflict.active_command_error(), None);
        assert_eq!(autonomy_conflict.command_feedback_height(), 0);
    }

    #[test]
    fn extensions_command_shows_notice_when_invoked_while_running() {
        let mut projection = projection();
        projection
            .extension_commands
            .push(extension_slash_command(CommandDescriptor {
                id: CommandId::new("tokio.builtin:extensions"),
                name: "/extensions".into(),
                description: "Manage installed extensions".into(),
                usage: None,
                source: CommandSource::BuiltIn,
                available_while_running: true,
            }));
        projection.running = true;
        projection.composer.replace("/extensions");

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::None
        ));
        assert_eq!(projection.composer.text(), "");
        assert_eq!(
            projection.active_command_error(),
            Some("/extensions is unavailable while a turn is running")
        );
        assert!(projection.extension_manager.is_none());
    }

    #[test]
    fn commands_marked_available_while_running_can_be_invoked() {
        let mut projection = projection();
        let loop_command = projection
            .extension_commands
            .iter_mut()
            .find(|command| command.name == "/loop")
            .expect("loop command");
        loop_command.available_while_running = true;
        projection.running = true;
        projection.composer.replace("/loop cancel");

        assert!(matches!(
            projection.submit(),
            FrontendEffect::Command(UiCommand::InvokeCommand { id, arguments })
                if id.as_str() == "tokio.builtin:loop" && arguments == "cancel"
        ));
        assert!(projection.running);
        assert_eq!(projection.transcript.len(), 0);
        assert_eq!(projection.command_error, None);

        projection.apply(AgentEvent::CommandHandled(Ok(None)));
        assert!(projection.running);
        assert_eq!(projection.transcript.len(), 0);
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
        assert_eq!(projection.transcript.len(), 0);
        projection.apply(AgentEvent::CommandHandled(Err("Invalid interval".into())));
        assert_eq!(projection.active_command_error(), Some("Invalid interval"));
        assert_eq!(projection.transcript.len(), 0);
        assert_eq!(projection.command_feedback_height(), 2);
        assert!(!projection.running);
    }

    #[test]
    fn goal_and_loop_commands_show_temporary_footer_notices() {
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
            assert_eq!(projection.command_feedback_height(), 0);

            projection
                .command_notice
                .as_mut()
                .expect("command notice")
                .expires_at = Instant::now();
            assert_eq!(projection.active_command_notice(), None);
            assert_eq!(projection.command_feedback_height(), 0);
        }
    }

    #[test]
    fn terminal_statuses_share_the_temporary_notice_slot() {
        let segment = |text: &str, priority| StatusSegment {
            id: text.to_owned(),
            text: text.to_owned(),
            tone: tokio_agent_extension_api::StatusTone::Normal,
            side: tokio_agent_extension_api::StatusSide::Left,
            priority,
            min_width: 0,
        };
        let mut projection = projection();
        projection.apply(AgentEvent::StatusSegments(vec![
            segment("loop: stopped", 90),
            segment("goal: complete", 100),
        ]));

        assert_eq!(projection.active_command_notice(), Some("goal: complete"));
        assert!(projection.status_segments.is_empty());
        projection.command_notice.as_mut().unwrap().expires_at = Instant::now();
        assert_eq!(projection.active_command_notice(), None);
    }

    #[test]
    fn status_bar_uses_only_the_highest_priority_valid_segment() {
        let segment = |text: &str| StatusSegment {
            id: text.to_owned(),
            text: text.to_owned(),
            tone: tokio_agent_extension_api::StatusTone::Normal,
            side: tokio_agent_extension_api::StatusSide::Left,
            priority: 0,
            min_width: 0,
        };
        let segments = vec![segment("goal: active"), segment("loop: every 10s")];

        assert_eq!(status_text(&segments).as_deref(), Some("goal: active"));
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
        assert_eq!(projection.settings_panel_height(), 4);

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
    fn configuration_slash_commands_remain_available_while_running() {
        let mut projection = projection();
        projection.running = true;

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
    fn single_match_palette_allows_history_navigation() {
        let mut projection = projection();
        projection.history.push("previous prompt".to_owned());
        projection.composer.replace("/providers");
        assert_eq!(projection.slash_matches().len(), 1);

        projection.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "previous prompt");

        projection.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "/providers");
    }

    #[test]
    fn up_arrow_recalls_recently_run_slash_commands() {
        let mut projection = projection();
        projection.composer.replace("/cl");

        assert!(matches!(
            projection.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::Clear)
        ));
        assert_eq!(
            projection.take_history_entry_to_persist().as_deref(),
            Some("/clear")
        );

        projection.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(projection.composer.text(), "/clear");
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
    fn generic_interactions_use_opaque_ids_and_extension_actions() {
        let mut projection = projection();
        let request = InteractionRequest {
            id: tokio_agent_extension_api::InteractionId::new("opaque-7"),
            owner: tokio_agent_extension_api::ExtensionId::new("example.gate"),
            generation: 3,
            spec: InteractionSpec::Approval(tokio_agent_extension_api::ApprovalSpec {
                title: "Confirm operation".into(),
                body: vec![tokio_agent_extension_api::TextSection {
                    heading: None,
                    text: "safe text".into(),
                }],
                actions: vec![tokio_agent_extension_api::InteractionAction {
                    id: "proceed".into(),
                    label: "Proceed".into(),
                    key_hint: Some("p".into()),
                    tone: tokio_agent_extension_api::InteractionTone::Primary,
                }],
                copy_text: Some("copy only".into()),
            }),
        };
        projection.apply(AgentEvent::InteractionRequested(request));
        assert!(
            matches!(projection.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)), FrontendEffect::Copy(text) if text == "copy only")
        );
        assert!(
            projection.pending.is_some(),
            "copy must not resolve the interaction"
        );
        assert!(
            matches!(projection.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            FrontendEffect::Command(UiCommand::RespondToInteraction(response))
                if response.id.as_str() == "opaque-7" && response.action_id == "proceed" && response.generation == 3)
        );
        assert!(projection.pending.is_none());
    }
}
