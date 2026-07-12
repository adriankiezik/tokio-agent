use std::collections::{BTreeMap, BTreeSet};

use tokio_agent_extension_api::{CommandDescriptor, CommandId};

use crate::PromptCommand;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("invalid slash command name `{0}`")]
    InvalidName(String),
    #[error("command name `{0}` is already registered")]
    NameCollision(String),
    #[error("command ID `{0}` is already registered")]
    IdCollision(CommandId),
    #[error("unknown command `{0}`")]
    Unknown(String),
}

#[derive(Debug, Clone)]
pub enum CommandHandler {
    BuiltIn,
    Prompt(PromptCommand),
    Extension,
}

#[derive(Debug, Clone)]
struct Entry {
    descriptor: CommandDescriptor,
    handler: CommandHandler,
}

#[derive(Debug, Clone, Default)]
pub struct CommandCatalog {
    entries: BTreeMap<String, Entry>,
    ids: BTreeSet<CommandId>,
}

impl CommandCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_builtin(&mut self, descriptor: CommandDescriptor) -> Result<(), CatalogError> {
        self.register(descriptor, CommandHandler::BuiltIn)
    }

    pub fn register_prompt(&mut self, command: PromptCommand) -> Result<(), CatalogError> {
        self.register(command.descriptor.clone(), CommandHandler::Prompt(command))
    }

    pub fn register_extension(
        &mut self,
        descriptor: CommandDescriptor,
    ) -> Result<(), CatalogError> {
        self.register(descriptor, CommandHandler::Extension)
    }

    fn register(
        &mut self,
        descriptor: CommandDescriptor,
        handler: CommandHandler,
    ) -> Result<(), CatalogError> {
        validate_slash_name(&descriptor.name)?;
        if self.entries.contains_key(&descriptor.name) {
            return Err(CatalogError::NameCollision(descriptor.name));
        }
        if !self.ids.insert(descriptor.id.clone()) {
            return Err(CatalogError::IdCollision(descriptor.id));
        }
        self.entries.insert(
            descriptor.name.clone(),
            Entry {
                descriptor,
                handler,
            },
        );
        Ok(())
    }

    pub fn remove_extension(&mut self, extension_id: &str) {
        let removed: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(name, entry)| match &entry.descriptor.source {
                tokio_agent_extension_api::CommandSource::Extension { id, .. }
                    if id.as_str() == extension_id =>
                {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        for name in removed {
            if let Some(entry) = self.entries.remove(&name) {
                self.ids.remove(&entry.descriptor.id);
            }
        }
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<CommandDescriptor> {
        self.entries
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    #[must_use]
    pub fn autocomplete(&self, prefix: &str, running: bool) -> Vec<CommandDescriptor> {
        self.entries
            .values()
            .filter(|entry| entry.descriptor.name.starts_with(prefix))
            .filter(|entry| !running || entry.descriptor.available_while_running)
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    pub fn invoke(
        &self,
        id: &CommandId,
        arguments: &str,
        cwd: &std::path::Path,
    ) -> Result<Option<String>, CatalogError> {
        let entry = self
            .entries
            .values()
            .find(|entry| &entry.descriptor.id == id)
            .ok_or_else(|| CatalogError::Unknown(id.to_string()))?;
        match &entry.handler {
            CommandHandler::Prompt(command) => Ok(Some(command.render(arguments, cwd))),
            CommandHandler::BuiltIn | CommandHandler::Extension => Ok(None),
        }
    }

    #[must_use]
    pub fn find_name(&self, name: &str) -> Option<&CommandDescriptor> {
        self.entries.get(name).map(|entry| &entry.descriptor)
    }
}

fn validate_slash_name(name: &str) -> Result<(), CatalogError> {
    let Some(bare) = name.strip_prefix('/') else {
        return Err(CatalogError::InvalidName(name.to_owned()));
    };
    if !bare.is_empty()
        && bare.len() <= 64
        && bare
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        Ok(())
    } else {
        Err(CatalogError::InvalidName(name.to_owned()))
    }
}
