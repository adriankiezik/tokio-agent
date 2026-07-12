use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{
    RootMetadata, SignedEnvelope, TufError, root_fingerprint, verify_initial_root,
    verify_root_rotation,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryTrustStore {
    #[serde(default)]
    pub registries: BTreeMap<String, TrustedRegistry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedRegistry {
    pub url: String,
    pub fingerprint: String,
    pub root: SignedEnvelope<RootMetadata>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("registry trust-store I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid registry trust store: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize registry trust store: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error(transparent)]
    Tuf(#[from] TufError),
    #[error("registry URL is already trusted with a different root identity")]
    IdentityChanged,
    #[error("registry root identity is already associated with another URL")]
    DuplicateIdentity,
}

impl RegistryTrustStore {
    pub fn load(path: &Path) -> Result<Self, TrustError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|source| TrustError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> Result<(), TrustError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| TrustError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, toml::to_string_pretty(self)?).map_err(|source| TrustError::Io {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| TrustError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Adds a third-party root only after its out-of-band fingerprint has been
    /// explicitly supplied by the caller.
    pub fn trust(
        &mut self,
        url: String,
        expected_fingerprint: &str,
        root: SignedEnvelope<RootMetadata>,
        now: SystemTime,
    ) -> Result<String, TrustError> {
        verify_initial_root(&root, expected_fingerprint, now)?;
        let identity = root_fingerprint(&root.signed);
        if self
            .registries
            .values()
            .any(|registry| registry.url == url && registry.fingerprint != identity)
        {
            return Err(TrustError::IdentityChanged);
        }
        if self
            .registries
            .get(&identity)
            .is_some_and(|registry| registry.url != url)
        {
            return Err(TrustError::DuplicateIdentity);
        }
        self.registries.insert(
            identity.clone(),
            TrustedRegistry {
                url,
                fingerprint: identity.clone(),
                root,
            },
        );
        Ok(identity)
    }

    pub fn rotate(
        &mut self,
        identity: &str,
        next: SignedEnvelope<RootMetadata>,
        now: SystemTime,
    ) -> Result<String, TrustError> {
        let trusted = self
            .registries
            .get(identity)
            .ok_or(TrustError::IdentityChanged)?
            .clone();
        verify_root_rotation(&trusted.root, &next, now)?;
        // The accepted initial-root fingerprint remains the registry identity
        // across a correctly cross-signed key rotation.
        self.registries.insert(
            identity.to_owned(),
            TrustedRegistry {
                url: trusted.url,
                fingerprint: identity.to_owned(),
                root: next,
            },
        );
        Ok(identity.to_owned())
    }

    pub fn remove(&mut self, identity: &str) -> Option<TrustedRegistry> {
        self.registries.remove(identity)
    }
}
