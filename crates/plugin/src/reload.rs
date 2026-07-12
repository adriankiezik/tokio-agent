use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{DiscoveryError, PromptCommand, discover_prompt_commands_in};

/// Poll-based declarative command reloader. It performs no work unless the
/// frontend polls it and never executes extension code.
pub struct DeclarativeReloader {
    user: Option<PathBuf>,
    project: PathBuf,
    snapshot: BTreeMap<PathBuf, (u64, SystemTime)>,
}

impl DeclarativeReloader {
    pub fn new(user: Option<PathBuf>, project: PathBuf) -> Result<Self, DiscoveryError> {
        let snapshot = snapshot(user.as_deref(), &project)?;
        Ok(Self {
            user,
            project,
            snapshot,
        })
    }

    pub fn reload_if_changed(&mut self) -> Result<Option<Vec<PromptCommand>>, DiscoveryError> {
        let current = snapshot(self.user.as_deref(), &self.project)?;
        if current == self.snapshot {
            return Ok(None);
        }
        let commands = discover_prompt_commands_in(self.user.as_deref(), &self.project)?;
        self.snapshot = current;
        Ok(Some(commands))
    }
}

pub struct PackageReloader {
    root: PathBuf,
    host_api: semver::Version,
    snapshot: BTreeMap<PathBuf, (u64, SystemTime)>,
}

impl PackageReloader {
    pub fn new(root: PathBuf, host_api: semver::Version) -> Result<Self, crate::ManifestError> {
        crate::validate_package(&root, &host_api)?;
        let snapshot = recursive_snapshot(&root).map_err(|source| crate::ManifestError::Read {
            path: root.clone(),
            source,
        })?;
        Ok(Self {
            root,
            host_api,
            snapshot,
        })
    }

    pub fn reload_if_changed(
        &mut self,
    ) -> Result<Option<(crate::ExtensionManifest, Vec<PromptCommand>)>, crate::ManifestError> {
        let current =
            recursive_snapshot(&self.root).map_err(|source| crate::ManifestError::Read {
                path: self.root.clone(),
                source,
            })?;
        if current == self.snapshot {
            return Ok(None);
        }
        let manifest = crate::validate_package(&self.root, &self.host_api)?;
        let commands = crate::commands_from_package(&self.root, &manifest).map_err(|error| {
            crate::ManifestError::Read {
                path: self.root.clone(),
                source: std::io::Error::other(error.to_string()),
            }
        })?;
        self.snapshot = current;
        Ok(Some((manifest, commands)))
    }
}

fn recursive_snapshot(root: &Path) -> std::io::Result<BTreeMap<PathBuf, (u64, SystemTime)>> {
    fn visit(
        directory: &Path,
        output: &mut BTreeMap<PathBuf, (u64, SystemTime)>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                visit(&entry.path(), output)?;
            } else if metadata.is_file() {
                output.insert(
                    entry.path(),
                    (
                        metadata.len(),
                        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    ),
                );
            }
        }
        Ok(())
    }
    let mut output = BTreeMap::new();
    visit(root, &mut output)?;
    Ok(output)
}

fn snapshot(
    user: Option<&Path>,
    project: &Path,
) -> Result<BTreeMap<PathBuf, (u64, SystemTime)>, DiscoveryError> {
    let mut result = BTreeMap::new();
    for directory in user.into_iter().chain(std::iter::once(project)) {
        if !directory.exists() {
            continue;
        }
        let entries = fs::read_dir(directory).map_err(|source| DiscoveryError::ReadDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| DiscoveryError::ReadDirectory {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|source| DiscoveryError::ReadDirectory {
                    path: path.clone(),
                    source,
                })?;
            result.insert(
                path,
                (
                    metadata.len(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                ),
            );
        }
    }
    Ok(result)
}
