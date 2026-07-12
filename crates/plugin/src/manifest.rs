use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid extension manifest: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported manifest version {0}")]
    ManifestVersion(u32),
    #[error("invalid extension ID `{0}`")]
    Id(String),
    #[error("invalid semantic version `{0}`")]
    Version(String),
    #[error("invalid host API requirement `{0}`")]
    HostApi(String),
    #[error("invalid contribution name `{0}`")]
    ContributionName(String),
    #[error("duplicate contribution name `{0}`")]
    DuplicateContribution(String),
    #[error("command `{0}` must declare exactly one of `prompt` or `handler`")]
    CommandHandler(String),
    #[error("unsafe package path `{0}`")]
    UnsafePath(String),
    #[error("package file does not exist: {0}")]
    MissingFile(PathBuf),
    #[error("a runtime component is present but no executable capability was declared")]
    RuntimeWithoutCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub license: String,
    pub host_api: String,
    #[serde(default)]
    pub runtime: Option<RuntimeContribution>,
    #[serde(default)]
    pub commands: Vec<CommandContribution>,
    #[serde(default)]
    pub skills: Vec<SkillContribution>,
    #[serde(default)]
    pub tools: Vec<ToolContribution>,
    #[serde(default, rename = "status")]
    pub status: Vec<StatusContribution>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeContribution {
    pub component: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContribution {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub usage: Option<String>,
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub available_while_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillContribution {
    pub name: String,
    pub description: String,
    pub instructions: String,
    #[serde(default)]
    pub usage: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolContribution {
    pub name: String,
    pub description: String,
    pub handler: String,
    #[serde(default)]
    pub activation: ToolActivation,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivation {
    #[default]
    Enabled,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusContribution {
    pub id: String,
    #[serde(default)]
    pub side: StatusSide,
    #[serde(default)]
    pub priority: i16,
    pub handler: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusSide {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub session_observe: bool,
    pub session_submit_automatic: bool,
    pub session_schedule: bool,
    pub tools_dynamic: bool,
    pub status_write: bool,
    pub storage_session: bool,
    pub storage_user: bool,
    pub subagents_spawn: bool,
    pub filesystem_read: bool,
    pub filesystem_edit: bool,
    pub process_request: bool,
    pub network_request: bool,
}

impl Capabilities {
    #[must_use]
    pub fn has_executable_capability(&self) -> bool {
        self.session_observe
            || self.session_submit_automatic
            || self.session_schedule
            || self.tools_dynamic
            || self.status_write
            || self.storage_session
            || self.storage_user
            || self.subagents_spawn
            || self.filesystem_read
            || self.filesystem_edit
            || self.process_request
            || self.network_request
    }

    #[must_use]
    pub fn as_set(&self) -> BTreeSet<tokio_agent_extension_api::Capability> {
        use tokio_agent_extension_api::Capability;
        [
            (self.session_observe, Capability::SessionObserve),
            (
                self.session_submit_automatic,
                Capability::SessionSubmitAutomatic,
            ),
            (self.session_schedule, Capability::SessionSchedule),
            (self.tools_dynamic, Capability::ToolsDynamic),
            (self.status_write, Capability::StatusWrite),
            (self.storage_session, Capability::StorageSession),
            (self.storage_user, Capability::StorageUser),
            (self.subagents_spawn, Capability::SubagentsSpawn),
            (self.filesystem_read, Capability::FilesystemRead),
            (self.filesystem_edit, Capability::FilesystemEdit),
            (self.process_request, Capability::ProcessRequest),
            (self.network_request, Capability::NetworkRequest),
        ]
        .into_iter()
        .filter_map(|(enabled, capability)| enabled.then_some(capability))
        .collect()
    }
}

impl ExtensionManifest {
    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let text = fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let manifest: Self = toml::from_str(&text)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != 1 {
            return Err(ManifestError::ManifestVersion(self.manifest_version));
        }
        validate_id(&self.id)?;
        Version::parse(&self.version).map_err(|_| ManifestError::Version(self.version.clone()))?;
        VersionReq::parse(&normalize_requirement(&self.host_api))
            .map_err(|_| ManifestError::HostApi(self.host_api.clone()))?;
        let mut commands = BTreeSet::new();
        for command in &self.commands {
            validate_name(&command.name)?;
            if !commands.insert(&command.name) {
                return Err(ManifestError::DuplicateContribution(command.name.clone()));
            }
            if command.prompt.is_some() == command.handler.is_some() {
                return Err(ManifestError::CommandHandler(command.name.clone()));
            }
            if let Some(prompt) = &command.prompt {
                validate_relative(prompt)?;
            }
            if let Some(handler) = &command.handler {
                validate_symbol_name(handler)?;
            }
        }
        let mut skills = BTreeSet::new();
        for skill in &self.skills {
            validate_name(&skill.name)?;
            validate_relative(&skill.instructions)?;
            if commands.contains(&skill.name) || !skills.insert(&skill.name) {
                return Err(ManifestError::DuplicateContribution(skill.name.clone()));
            }
        }
        let mut tools = BTreeSet::new();
        for tool in &self.tools {
            validate_symbol_name(&tool.name)?;
            validate_symbol_name(&tool.handler)?;
            if !tools.insert(&tool.name) {
                return Err(ManifestError::DuplicateContribution(tool.name.clone()));
            }
        }
        let mut statuses = BTreeSet::new();
        for status in &self.status {
            validate_name(&status.id)?;
            validate_symbol_name(&status.handler)?;
            if !statuses.insert(&status.id) {
                return Err(ManifestError::DuplicateContribution(status.id.clone()));
            }
        }
        if let Some(runtime) = &self.runtime {
            validate_relative(&runtime.component)?;
            if !self.capabilities.has_executable_capability() {
                return Err(ManifestError::RuntimeWithoutCapability);
            }
        } else if self
            .commands
            .iter()
            .any(|command| command.handler.is_some())
            || !self.tools.is_empty()
            || !self.status.is_empty()
        {
            return Err(ManifestError::MissingFile(PathBuf::from(
                "runtime.component",
            )));
        }
        Ok(())
    }

    #[must_use]
    pub fn is_host_compatible(&self, host_api: &Version) -> bool {
        VersionReq::parse(&normalize_requirement(&self.host_api))
            .is_ok_and(|requirement| requirement.matches(host_api))
    }
}

pub fn validate_package(
    root: &Path,
    host_api: &Version,
) -> Result<ExtensionManifest, ManifestError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| ManifestError::Read {
        path: root.to_path_buf(),
        source,
    })?;
    let manifest = ExtensionManifest::from_path(&root.join("extension.toml"))?;
    if !manifest.is_host_compatible(host_api) {
        return Err(ManifestError::HostApi(manifest.host_api.clone()));
    }
    let files = manifest
        .commands
        .iter()
        .filter_map(|command| command.prompt.as_ref())
        .chain(manifest.skills.iter().map(|skill| &skill.instructions))
        .chain(manifest.runtime.iter().map(|runtime| &runtime.component));
    for relative in files {
        let candidate = root.join(relative);
        let canonical = fs::canonicalize(&candidate)
            .map_err(|_| ManifestError::MissingFile(candidate.clone()))?;
        if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
            return Err(ManifestError::UnsafePath(relative.clone()));
        }
    }
    Ok(manifest)
}

fn validate_id(id: &str) -> Result<(), ManifestError> {
    let valid = id.len() <= 128 && id.split('.').count() >= 2 && id.split('.').all(is_name);
    if valid {
        Ok(())
    } else {
        Err(ManifestError::Id(id.to_owned()))
    }
}
fn validate_name(name: &str) -> Result<(), ManifestError> {
    if is_name(name) && name.len() <= 64 {
        Ok(())
    } else {
        Err(ManifestError::ContributionName(name.to_owned()))
    }
}
fn validate_symbol_name(name: &str) -> Result<(), ManifestError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(ManifestError::ContributionName(name.to_owned()))
    }
}
fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn validate_relative(value: &str) -> Result<(), ManifestError> {
    let path = Path::new(value);
    if !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(ManifestError::UnsafePath(value.to_owned()))
    }
}
fn normalize_requirement(value: &str) -> String {
    value.replace(", <", ",<").replace(", >", ",>")
}

#[cfg(test)]
mod tests {
    use super::validate_id;

    #[test]
    fn accepts_two_part_extension_ids() {
        assert!(validate_id("tokio.goal").is_ok());
        assert!(validate_id("tokio.loop").is_ok());
        assert!(validate_id("goal").is_err());
    }
}
