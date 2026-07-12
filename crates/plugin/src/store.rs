use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};

use crate::{ExtensionManifest, validate_package};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("package store I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Manifest(#[from] crate::ManifestError),
    #[error("package digest mismatch: expected {expected}, got {actual}")]
    Digest { expected: String, actual: String },
    #[error("package version {id}@{version} is already installed with different contents")]
    ImmutableVersion { id: String, version: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub manifest: ExtensionManifest,
    pub path: PathBuf,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct PackageStore {
    root: PathBuf,
    host_api: Version,
}

impl PackageStore {
    #[must_use]
    pub fn new(root: PathBuf, host_api: Version) -> Self {
        Self { root, host_api }
    }

    pub fn user_default(host_api: Version) -> Result<Self, StoreError> {
        let root = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tokio-agent/extensions");
        Ok(Self::new(root, host_api))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn install_directory(
        &self,
        source: &Path,
        expected_digest: Option<&str>,
    ) -> Result<InstalledPackage, StoreError> {
        let manifest = validate_package(source, &self.host_api)?;
        let digest = package_digest(source)?;
        if let Some(expected) = expected_digest
            && !constant_time_eq(expected.as_bytes(), digest.as_bytes())
        {
            return Err(StoreError::Digest {
                expected: expected.to_owned(),
                actual: digest,
            });
        }
        let destination = self.root.join(&manifest.id).join(&manifest.version);
        if destination.exists() {
            let installed_digest = package_digest(&destination)?;
            if installed_digest != digest {
                return Err(StoreError::ImmutableVersion {
                    id: manifest.id,
                    version: manifest.version,
                });
            }
            return Ok(InstalledPackage {
                manifest,
                path: destination,
                digest,
            });
        }
        fs::create_dir_all(destination.parent().expect("version has parent"))
            .map_err(|source| io(destination.clone(), source))?;
        let temporary = destination.with_extension(format!("tmp-{}", std::process::id()));
        if temporary.exists() {
            fs::remove_dir_all(&temporary).map_err(|source| io(temporary.clone(), source))?;
        }
        copy_package(source, &temporary)?;
        if let Err(source) = fs::rename(&temporary, &destination) {
            let _ = fs::remove_dir_all(&temporary);
            return Err(io(destination, source));
        }
        Ok(InstalledPackage {
            manifest,
            path: destination,
            digest,
        })
    }

    pub fn list(&self) -> Result<Vec<InstalledPackage>, StoreError> {
        let mut packages = Vec::new();
        if !self.root.exists() {
            return Ok(packages);
        }
        for id in read_dirs(&self.root)? {
            for version in read_dirs(&id)? {
                if !version.join("extension.toml").is_file() {
                    continue;
                }
                let manifest = validate_package(&version, &self.host_api)?;
                let digest = package_digest(&version)?;
                packages.push(InstalledPackage {
                    manifest,
                    path: version,
                    digest,
                });
            }
        }
        packages.sort_by(|a, b| {
            (&a.manifest.id, &a.manifest.version).cmp(&(&b.manifest.id, &b.manifest.version))
        });
        Ok(packages)
    }

    pub fn remove(&self, id: &str, version: &str) -> Result<bool, StoreError> {
        let path = self.root.join(id).join(version);
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&path).map_err(|source| io(path.clone(), source))?;
        if path.parent().is_some_and(|parent| {
            fs::read_dir(parent).is_ok_and(|mut entries| entries.next().is_none())
        }) {
            let _ = fs::remove_dir(path.parent().expect("checked"));
        }
        Ok(true)
    }
}

pub fn package_digest(root: &Path) -> Result<String, StoreError> {
    let canonical = fs::canonicalize(root).map_err(|source| io(root.to_path_buf(), source))?;
    let mut files = Vec::new();
    collect_files(&canonical, &canonical, &mut files)?;
    files.sort();
    let mut hash = Sha256::new();
    for relative in files {
        let bytes = relative.to_string_lossy().as_bytes().to_vec();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
        let path = canonical.join(&relative);
        let mut file = fs::File::open(&path).map_err(|source| io(path.clone(), source))?;
        let mut buffer = [0_u8; 8192];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|source| io(path.clone(), source))?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn collect_files(
    root: &Path,
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(directory).map_err(|source| io(directory.to_path_buf(), source))? {
        let entry = entry.map_err(|source| io(directory.to_path_buf(), source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| io(entry.path(), source))?;
        if file_type.is_symlink() {
            return Err(io(
                entry.path(),
                std::io::Error::other("package symlinks are not allowed"),
            ));
        }
        if file_type.is_dir() {
            collect_files(root, &entry.path(), output)?;
        } else if file_type.is_file() {
            output.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("descendant")
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

fn copy_package(source: &Path, destination: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(destination).map_err(|error| io(destination.to_path_buf(), error))?;
    for entry in fs::read_dir(source).map_err(|error| io(source.to_path_buf(), error))? {
        let entry = entry.map_err(|error| io(source.to_path_buf(), error))?;
        let kind = entry.file_type().map_err(|error| io(entry.path(), error))?;
        let target = destination.join(entry.file_name());
        if kind.is_symlink() {
            return Err(io(
                entry.path(),
                std::io::Error::other("package symlinks are not allowed"),
            ));
        }
        if kind.is_dir() {
            copy_package(&entry.path(), &target)?;
        } else if kind.is_file() {
            let bytes = fs::read(entry.path()).map_err(|error| io(entry.path(), error))?;
            let mut output =
                fs::File::create(&target).map_err(|error| io(target.clone(), error))?;
            output
                .write_all(&bytes)
                .map_err(|error| io(target, error))?;
        }
    }
    Ok(())
}

fn read_dirs(path: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut result = Vec::new();
    for entry in fs::read_dir(path).map_err(|source| io(path.to_path_buf(), source))? {
        let entry = entry.map_err(|source| io(path.to_path_buf(), source))?;
        if entry
            .file_type()
            .map_err(|source| io(entry.path(), source))?
            .is_dir()
        {
            result.push(entry.path());
        }
    }
    Ok(result)
}
fn io(path: PathBuf, source: std::io::Error) -> StoreError {
    StoreError::Io { path, source }
}
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}
