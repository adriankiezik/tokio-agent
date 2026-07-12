use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureEntry {
    pub key_id: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedEnvelope<T> {
    pub signed: T,
    pub signatures: Vec<SignatureEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootMetadata {
    pub version: u64,
    pub expires_unix: u64,
    pub registry_name: String,
    pub operator: String,
    pub keys: BTreeMap<String, String>,
    pub threshold: u32,
    pub index_keys: BTreeMap<String, String>,
    pub index_threshold: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TufError {
    #[error("metadata has expired")]
    Expired,
    #[error("metadata rollback from version {trusted} to {received}")]
    Rollback { trusted: u64, received: u64 },
    #[error("metadata version {0} was reused with different contents")]
    VersionReuse(u64),
    #[error("invalid signing key `{0}`")]
    Key(String),
    #[error("metadata signature threshold was not met")]
    Threshold,
    #[error("could not serialize signed metadata")]
    Serialization,
    #[error("root fingerprint does not match the explicitly trusted fingerprint")]
    Fingerprint,
}

#[must_use]
pub fn root_fingerprint(root: &RootMetadata) -> String {
    // Root identity is stable across expiry renewal and metadata version bumps;
    // it changes only when the registry identity/key set changes.
    let identity = (
        root.registry_name.as_str(),
        root.operator.as_str(),
        &root.keys,
        root.threshold,
    );
    let bytes = serde_json::to_vec(&identity).expect("root identity is serializable");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn verify_initial_root(
    root: &SignedEnvelope<RootMetadata>,
    expected_fingerprint: &str,
    now: SystemTime,
) -> Result<(), TufError> {
    if root_fingerprint(&root.signed) != expected_fingerprint {
        return Err(TufError::Fingerprint);
    }
    verify_expiry(root.signed.expires_unix, now)?;
    verify_signatures(root, &root.signed.keys, root.signed.threshold)
}

pub fn verify_root_rotation(
    trusted: &SignedEnvelope<RootMetadata>,
    next: &SignedEnvelope<RootMetadata>,
    now: SystemTime,
) -> Result<(), TufError> {
    if next.signed.version <= trusted.signed.version {
        return Err(TufError::Rollback {
            trusted: trusted.signed.version,
            received: next.signed.version,
        });
    }
    verify_expiry(next.signed.expires_unix, now)?;
    verify_signatures(next, &trusted.signed.keys, trusted.signed.threshold)?;
    verify_signatures(next, &next.signed.keys, next.signed.threshold)
}

pub fn verify_role<T: Serialize + DeserializeOwned>(
    envelope: &SignedEnvelope<T>,
    keys: &BTreeMap<String, String>,
    threshold: u32,
) -> Result<(), TufError> {
    verify_signatures(envelope, keys, threshold)
}

fn verify_signatures<T: Serialize>(
    envelope: &SignedEnvelope<T>,
    keys: &BTreeMap<String, String>,
    threshold: u32,
) -> Result<(), TufError> {
    let payload = serde_json::to_vec(&envelope.signed).map_err(|_| TufError::Serialization)?;
    let mut valid = BTreeSet::new();
    for entry in &envelope.signatures {
        let Some(encoded_key) = keys.get(&entry.key_id) else {
            continue;
        };
        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded_key)
            .map_err(|_| TufError::Key(entry.key_id.clone()))?;
        let key_array: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| TufError::Key(entry.key_id.clone()))?;
        let key = VerifyingKey::from_bytes(&key_array)
            .map_err(|_| TufError::Key(entry.key_id.clone()))?;
        let signature_bytes = base64::engine::general_purpose::STANDARD
            .decode(&entry.signature)
            .map_err(|_| TufError::Key(entry.key_id.clone()))?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|_| TufError::Key(entry.key_id.clone()))?;
        if key.verify(&payload, &signature).is_ok() {
            valid.insert(&entry.key_id);
        }
    }
    if valid.len() >= threshold as usize {
        Ok(())
    } else {
        Err(TufError::Threshold)
    }
}

fn verify_expiry(expires: u64, now: SystemTime) -> Result<(), TufError> {
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    if expires > now {
        Ok(())
    } else {
        Err(TufError::Expired)
    }
}
