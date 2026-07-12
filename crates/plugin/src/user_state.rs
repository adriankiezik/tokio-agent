use std::io::Write;
use std::path::{Path, PathBuf};

use tokio_agent_extension_api::ExtensionId;

const MAX_USER_STATE_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UserStateError {
    #[error("extension user state exceeds {MAX_USER_STATE_BYTES} bytes")]
    TooLarge,
    #[error("extension ID is not safe for user storage")]
    InvalidId,
    #[error("extension user storage is unavailable")]
    Unavailable,
    #[error("failed to access extension user state at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub fn load_user_state(id: &ExtensionId) -> Result<Vec<u8>, UserStateError> {
    let path = state_path(id)?;
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() <= MAX_USER_STATE_BYTES => Ok(bytes),
        Ok(_) => Err(UserStateError::TooLarge),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(UserStateError::Io { path, source }),
    }
}

pub fn store_user_state(id: &ExtensionId, bytes: &[u8]) -> Result<(), UserStateError> {
    if bytes.len() > MAX_USER_STATE_BYTES {
        return Err(UserStateError::TooLarge);
    }
    let path = state_path(id)?;
    let parent = path.parent().ok_or(UserStateError::Unavailable)?;
    std::fs::create_dir_all(parent).map_err(|source| UserStateError::Io {
        path: parent.into(),
        source,
    })?;
    atomic_write(&path, bytes)
}

fn state_path(id: &ExtensionId) -> Result<PathBuf, UserStateError> {
    if id.as_str().is_empty()
        || !id
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
    {
        return Err(UserStateError::InvalidId);
    }
    let root = dirs::config_dir().ok_or(UserStateError::Unavailable)?;
    Ok(root
        .join("tokio-agent")
        .join("extensions")
        .join(id.as_str())
        .join("state.bin"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), UserStateError> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|source| UserStateError::Io {
            path: temporary.clone(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| UserStateError::Io {
            path: temporary.clone(),
            source,
        })?;
    std::fs::rename(&temporary, path).map_err(|source| UserStateError::Io {
        path: path.into(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_state_is_bounded_and_extension_scoped() {
        assert!(matches!(
            store_user_state(&ExtensionId::new("../other"), b"x"),
            Err(UserStateError::InvalidId)
        ));
        assert!(matches!(
            store_user_state(
                &ExtensionId::new("example.safe"),
                &vec![0; MAX_USER_STATE_BYTES + 1]
            ),
            Err(UserStateError::TooLarge)
        ));
    }

    #[test]
    fn atomic_write_replaces_the_complete_value() {
        let root = std::env::temp_dir().join(format!(
            "tokio-agent-user-state-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("state.bin");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(root);
    }
}
