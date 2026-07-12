use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio_agent_extension_api::Capability;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionLock {
    pub lock_version: u32,
    #[serde(default, rename = "extension")]
    pub extensions: Vec<LockedExtension>,
}

impl Default for ExtensionLock {
    fn default() -> Self {
        Self {
            lock_version: 1,
            extensions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedExtension {
    pub id: String,
    pub version: String,
    pub source: LockedSource,
    pub digest: String,
    pub host_api: String,
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LockedSource {
    Registry { root_identity: String, url: String },
    Local { path: PathBuf, reproducible: bool },
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("could not read extension lock {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid extension lock: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported extension lock version {0}")]
    Version(u32),
    #[error("duplicate locked package identity `{0}`")]
    Duplicate(String),
    #[error("could not serialize extension lock: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("could not write extension lock {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl ExtensionLock {
    pub fn load(path: &Path) -> Result<Self, LockError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|source| LockError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let lock: Self = toml::from_str(&text)?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<(), LockError> {
        if self.lock_version != 1 {
            return Err(LockError::Version(self.lock_version));
        }
        let mut identities = BTreeSet::new();
        for extension in &self.extensions {
            let source = match &extension.source {
                LockedSource::Registry { root_identity, .. } => root_identity.as_str(),
                LockedSource::Local { path, .. } => path.to_str().unwrap_or("<non-utf8>"),
            };
            let identity = format!("{source}:{}", extension.id);
            if !identities.insert(identity.clone()) {
                return Err(LockError::Duplicate(identity));
            }
        }
        Ok(())
    }

    pub fn upsert(&mut self, entry: LockedExtension) {
        let position = self
            .extensions
            .iter()
            .position(|existing| same_identity(existing, &entry));
        if let Some(position) = position {
            self.extensions[position] = entry;
        } else {
            self.extensions.push(entry);
        }
        self.extensions
            .sort_by(|a, b| (&a.id, &a.version).cmp(&(&b.id, &b.version)));
    }

    pub fn remove_registry(&mut self, root_identity: &str, id: &str) -> bool {
        let before = self.extensions.len();
        self.extensions.retain(|entry| !(entry.id == id && matches!(&entry.source, LockedSource::Registry { root_identity: root, .. } if root == root_identity)));
        before != self.extensions.len()
    }

    pub fn save(&self, path: &Path) -> Result<(), LockError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| LockError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;
        let temporary = path.with_extension("lock.tmp");
        fs::write(&temporary, text).map_err(|source| LockError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| LockError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn same_identity(a: &LockedExtension, b: &LockedExtension) -> bool {
    a.id == b.id
        && match (&a.source, &b.source) {
            (
                LockedSource::Registry {
                    root_identity: a, ..
                },
                LockedSource::Registry {
                    root_identity: b, ..
                },
            ) => a == b,
            (LockedSource::Local { path: a, .. }, LockedSource::Local { path: b, .. }) => a == b,
            _ => false,
        }
}
