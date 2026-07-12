use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use tokio_agent_extension_api::Capability;

use crate::{RootMetadata, SignedEnvelope, TufError, verify_initial_root, verify_role};

pub const OFFICIAL_REGISTRY_URL: &str = "https://adriankiezik.github.io/tokio-agent";

/// Returns the official root established by the production root ceremony.
/// Release builds may override the checked-in trust anchor during a root rotation.
pub fn builtin_official_root() -> Result<Option<SignedEnvelope<RootMetadata>>, RegistryError> {
    let json = option_env!("TOKIO_AGENT_OFFICIAL_ROOT_JSON")
        .unwrap_or(include_str!("../official-root.json"));
    let root: SignedEnvelope<RootMetadata> = serde_json::from_str(json)?;
    let fingerprint = crate::root_fingerprint(&root.signed);
    verify_initial_root(&root, &fingerprint, SystemTime::now())?;
    Ok(Some(root))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryIndex {
    pub version: u64,
    pub expires_unix: u64,
    pub registry_identity: String,
    pub publisher: String,
    #[serde(default)]
    pub packages: Vec<RegistryPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryPackage {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub license: String,
    pub publisher: String,
    pub readme_url: String,
    pub host_api: String,
    pub minimum_app_version: String,
    pub artifact_url: String,
    pub digest: String,
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub deprecated: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryTrust {
    Official,
    ThirdParty { name: String, operator: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub registry_identity: String,
    pub trust: RegistryTrust,
    pub package: RegistryPackage,
}

#[derive(Debug, Clone)]
pub struct CachedRegistry {
    pub identity: String,
    pub trust: RegistryTrust,
    pub index: RegistryIndex,
}

#[derive(Debug, Default)]
pub struct RegistryCatalog {
    registries: BTreeMap<String, CachedRegistry>,
    official_identity: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry cache I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid registry metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Tuf(#[from] TufError),
    #[error("registry metadata identity does not match the trusted root")]
    Identity,
    #[error("registry package has invalid compatibility metadata")]
    Compatibility,
    #[error("no compatible non-yanked version of `{0}` was found")]
    NotFound(String),
}

impl RegistryCatalog {
    #[must_use]
    pub fn new(official_identity: impl Into<String>) -> Self {
        Self {
            registries: BTreeMap::new(),
            official_identity: Some(official_identity.into()),
        }
    }

    pub fn add_verified(
        &mut self,
        expected_identity: &str,
        trust: RegistryTrust,
        envelope: SignedEnvelope<RegistryIndex>,
        keys: &BTreeMap<String, String>,
        threshold: u32,
    ) -> Result<(), RegistryError> {
        verify_role(&envelope, keys, threshold)?;
        let is_official_identity = self.official_identity.as_deref() == Some(expected_identity);
        if matches!(trust, RegistryTrust::Official) != is_official_identity {
            return Err(RegistryError::Identity);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if envelope.signed.expires_unix <= now {
            return Err(TufError::Expired.into());
        }
        if let Some(cached) = self.registries.get(expected_identity) {
            if envelope.signed.version < cached.index.version {
                return Err(TufError::Rollback {
                    trusted: cached.index.version,
                    received: envelope.signed.version,
                }
                .into());
            }
            if envelope.signed.version == cached.index.version && envelope.signed != cached.index {
                return Err(TufError::VersionReuse(envelope.signed.version).into());
            }
        }
        if envelope.signed.registry_identity != expected_identity {
            return Err(RegistryError::Identity);
        }
        validate_index(&envelope.signed, &trust)?;
        self.registries.insert(
            expected_identity.to_owned(),
            CachedRegistry {
                identity: expected_identity.to_owned(),
                trust,
                index: envelope.signed,
            },
        );
        Ok(())
    }

    pub fn remove(&mut self, identity: &str) -> Option<CachedRegistry> {
        self.registries.remove(identity)
    }

    #[must_use]
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        let query = query.to_ascii_lowercase();
        let mut results = Vec::new();
        for registry in self.registries.values() {
            for package in &registry.index.packages {
                if package.id.to_ascii_lowercase().contains(&query)
                    || package.name.to_ascii_lowercase().contains(&query)
                    || package.description.to_ascii_lowercase().contains(&query)
                    || package
                        .keywords
                        .iter()
                        .any(|keyword| keyword.to_ascii_lowercase().contains(&query))
                {
                    results.push(SearchResult {
                        registry_identity: registry.identity.clone(),
                        trust: registry.trust.clone(),
                        package: package.clone(),
                    });
                }
            }
        }
        results.sort_by(|a, b| {
            (&a.package.id, &a.package.version, &a.registry_identity).cmp(&(
                &b.package.id,
                &b.package.version,
                &b.registry_identity,
            ))
        });
        results
    }

    pub fn resolve(
        &self,
        identity: &str,
        id: &str,
        host_api: &Version,
        app_version: &Version,
    ) -> Result<&RegistryPackage, RegistryError> {
        self.registries
            .get(identity)
            .into_iter()
            .flat_map(|registry| &registry.index.packages)
            .filter(|package| package.id == id && !package.yanked)
            .filter(|package| {
                VersionReq::parse(&package.host_api)
                    .is_ok_and(|requirement| requirement.matches(host_api))
            })
            .filter(|package| {
                Version::parse(&package.minimum_app_version)
                    .is_ok_and(|minimum| app_version >= &minimum)
            })
            .max_by_key(|package| Version::parse(&package.version).ok())
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))
    }

    pub fn save_cache(
        envelope: &SignedEnvelope<RegistryIndex>,
        path: &Path,
    ) -> Result<(), RegistryError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| RegistryError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(envelope)?).map_err(|source| {
            RegistryError::Io {
                path: temporary.clone(),
                source,
            }
        })?;
        fs::rename(&temporary, path).map_err(|source| RegistryError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn load_cache(path: &Path) -> Result<SignedEnvelope<RegistryIndex>, RegistryError> {
        let bytes = fs::read(path).map_err(|source| RegistryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn validate_index(index: &RegistryIndex, trust: &RegistryTrust) -> Result<(), RegistryError> {
    for package in &index.packages {
        Version::parse(&package.version).map_err(|_| RegistryError::Compatibility)?;
        VersionReq::parse(&package.host_api).map_err(|_| RegistryError::Compatibility)?;
        Version::parse(&package.minimum_app_version).map_err(|_| RegistryError::Compatibility)?;
        if !package.digest.starts_with("sha256:")
            || !(package.artifact_url.starts_with("https://")
                || package.artifact_url.starts_with("file://"))
        {
            return Err(RegistryError::Compatibility);
        }
        if package.id.starts_with("tokio.official.") && !matches!(trust, RegistryTrust::Official) {
            return Err(RegistryError::Identity);
        }
    }
    Ok(())
}
