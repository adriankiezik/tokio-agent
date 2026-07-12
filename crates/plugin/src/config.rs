use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionScope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enablement {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEnablement {
    pub enabled: bool,
    pub source: Option<ExtensionScope>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    #[serde(default)]
    pub extensions: BTreeMap<String, Enablement>,
    #[serde(default)]
    pub linked: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub granted_capabilities: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub capability_grants: Vec<CapabilityGrant>,
    #[serde(default)]
    pub registries: Vec<RegistryReference>,
    #[serde(default)]
    pub command_aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGrant {
    pub registry_identity: String,
    pub extension_id: String,
    pub publisher: String,
    pub capabilities: BTreeSet<tokio_agent_extension_api::Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryReference {
    pub url: String,
    pub fingerprint: String,
}

impl ExtensionConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(ConfigError::Parse)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, text).map_err(|source| ConfigError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    #[must_use]
    pub fn resolve(id: &str, user: &Self, project: &Self) -> ResolvedEnablement {
        if let Some(value) = project.extensions.get(id) {
            return ResolvedEnablement {
                enabled: *value == Enablement::Enabled,
                source: Some(ExtensionScope::Project),
            };
        }
        if let Some(value) = user.extensions.get(id) {
            return ResolvedEnablement {
                enabled: *value == Enablement::Enabled,
                source: Some(ExtensionScope::User),
            };
        }
        ResolvedEnablement {
            enabled: false,
            source: None,
        }
    }

    pub fn set(&mut self, id: impl Into<String>, enablement: Enablement) {
        self.extensions.insert(id.into(), enablement);
    }

    #[must_use]
    pub fn grant_matches(&self, requested: &CapabilityGrant) -> bool {
        self.capability_grants
            .iter()
            .any(|grant| grant == requested)
    }

    #[must_use]
    pub fn resolve_alias<'a>(
        command_id: &str,
        user: &'a Self,
        project: &'a Self,
    ) -> Option<&'a str> {
        project
            .command_aliases
            .get(command_id)
            .or_else(|| user.command_aliases.get(command_id))
            .map(String::as_str)
    }

    pub fn approve_capabilities(&mut self, grant: CapabilityGrant) {
        self.capability_grants.retain(|existing| {
            existing.registry_identity != grant.registry_identity
                || existing.extension_id != grant.extension_id
        });
        self.capability_grants.push(grant);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read extension configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid extension configuration: {0}")]
    Parse(toml::de::Error),
    #[error("could not serialize extension configuration: {0}")]
    Serialize(toml::ser::Error),
    #[error("could not write extension configuration {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}
