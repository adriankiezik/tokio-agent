use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::token::AuthFile;

pub fn our_auth_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokio-agent").join("auth.json"))
}

pub fn codex_auth_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    dirs::home_dir().map(|d| d.join(".codex").join("auth.json"))
}

fn codex_import_disabled_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokio-agent").join("codex-import-disabled"))
}

pub fn load() -> Option<AuthFile> {
    if let Some(file) = our_auth_path().as_deref().and_then(read_auth) {
        return Some(file);
    }
    if codex_import_disabled_path().is_some_and(|path| path.exists()) {
        return None;
    }
    codex_auth_path().as_deref().and_then(read_auth)
}

fn read_auth(path: &Path) -> Option<AuthFile> {
    let Ok(text) = fs::read_to_string(path) else {
        return None;
    };
    serde_json::from_str(&text).ok()
}

pub fn save(file: &AuthFile) -> io::Result<PathBuf> {
    let path = our_auth_path()
        .ok_or_else(|| io::Error::other("could not determine a config directory"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
    write_private(&path, &json)?;
    if let Some(disabled) = codex_import_disabled_path() {
        match fs::remove_file(disabled) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(path)
}

pub fn clear() -> io::Result<Option<PathBuf>> {
    let Some(path) = our_auth_path() else {
        return Ok(None);
    };
    let removed = match fs::remove_file(&path) {
        Ok(()) => Some(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let disabled = codex_import_disabled_path()
        .ok_or_else(|| io::Error::other("could not determine a config directory"))?;
    if let Some(parent) = disabled.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private(&disabled, "")?;
    Ok(removed.or(Some(disabled)))
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    let mut file = opts.open(path)?;
    io::Write::write_all(&mut file, contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, contents: &str) -> io::Result<()> {
    fs::write(path, contents)
}
