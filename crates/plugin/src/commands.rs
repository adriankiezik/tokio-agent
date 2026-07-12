use std::path::Path;
use tokio_agent_extension_api::{CommandDescriptor, CommandId, CommandSource, SessionCommand};

use crate::{CatalogError, CommandCatalog, PromptCommand};

pub const CLEAR_COMMAND_ID: &str = "tokio.builtin:clear";
pub const MODEL_COMMAND_ID: &str = "tokio.builtin:model";
pub const PROVIDERS_COMMAND_ID: &str = "tokio.builtin:providers";
pub const EXTENSIONS_COMMAND_ID: &str = "tokio.builtin:extensions";

#[must_use]
pub fn builtin_command_catalog() -> Vec<CommandDescriptor> {
    [
        (
            CLEAR_COMMAND_ID,
            "/clear",
            "Clear the conversation and start fresh",
            None,
            false,
        ),
        (
            MODEL_COMMAND_ID,
            "/model",
            "Switch models for this session",
            None,
            true,
        ),
        (
            PROVIDERS_COMMAND_ID,
            "/providers",
            "Connect or switch AI providers",
            None,
            true,
        ),
        (
            EXTENSIONS_COMMAND_ID,
            "/extensions",
            "Manage installed extensions",
            None,
            true,
        ),
    ]
    .into_iter()
    .map(
        |(id, name, description, usage, available_while_running)| CommandDescriptor {
            id: CommandId::new(id),
            name: name.to_owned(),
            description: description.to_owned(),
            usage: usage.map(str::to_owned),
            source: CommandSource::BuiltIn,
            available_while_running,
        },
    )
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltInCommand {
    Clear,
    OpenModelPicker,
    OpenProviderPicker,
    OpenExtensionManager,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedCommand {
    SubmitMessage(String),
    SubmitPrompt(String),
    BuiltIn(BuiltInCommand),
    Extension { id: CommandId, arguments: String },
    Interrupt,
    RespondToInteraction(tokio_agent_extension_api::InteractionResponse),
    Shutdown,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("{0}")]
    InvalidArguments(String),
}

/// Frontend-neutral command catalog and invocation router.
///
/// Prompt expansion happens here, at an idle request boundary chosen by the
/// caller, rather than in `Agent`. Both interactive and headless frontends can
/// therefore use exactly the same command semantics.
#[derive(Debug, Clone)]
pub struct CommandRouter {
    catalog: CommandCatalog,
}

impl CommandRouter {
    pub fn new(commands: impl IntoIterator<Item = PromptCommand>) -> Result<Self, CatalogError> {
        let mut catalog = CommandCatalog::new();
        for descriptor in builtin_command_catalog() {
            catalog.register_builtin(descriptor)?;
        }
        for command in commands {
            catalog.register_prompt(command)?;
        }
        Ok(Self { catalog })
    }

    pub fn register_extension(
        &mut self,
        descriptor: CommandDescriptor,
    ) -> Result<(), CatalogError> {
        self.catalog.register_extension(descriptor)
    }

    pub fn catalog(&self) -> Vec<CommandDescriptor> {
        self.catalog.descriptors()
    }

    #[must_use]
    pub fn find_name(&self, name: &str) -> Option<&CommandDescriptor> {
        self.catalog.find_name(name)
    }

    pub fn route(&self, command: SessionCommand, cwd: &Path) -> Result<RoutedCommand, RouteError> {
        match command {
            SessionCommand::SubmitMessage(text) => Ok(RoutedCommand::SubmitMessage(text)),
            SessionCommand::Interrupt => Ok(RoutedCommand::Interrupt),
            SessionCommand::RespondToInteraction(response) => {
                Ok(RoutedCommand::RespondToInteraction(response))
            }
            SessionCommand::Shutdown => Ok(RoutedCommand::Shutdown),
            SessionCommand::InvokeCommand { id, arguments } => {
                if let Some(prompt) = self.catalog.invoke(&id, &arguments, cwd)? {
                    return Ok(RoutedCommand::SubmitPrompt(prompt));
                }
                match route_builtin(id.as_str()) {
                    Ok(command) => Ok(RoutedCommand::BuiltIn(command)),
                    Err(RouteError::Catalog(CatalogError::Unknown(_))) => {
                        Ok(RoutedCommand::Extension { id, arguments })
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
}

fn route_builtin(id: &str) -> Result<BuiltInCommand, RouteError> {
    let command = match id {
        CLEAR_COMMAND_ID => BuiltInCommand::Clear,
        MODEL_COMMAND_ID => BuiltInCommand::OpenModelPicker,
        PROVIDERS_COMMAND_ID => BuiltInCommand::OpenProviderPicker,
        EXTENSIONS_COMMAND_ID => BuiltInCommand::OpenExtensionManager,
        _ => return Err(CatalogError::Unknown(id.to_owned()).into()),
    };
    Ok(command)
}
