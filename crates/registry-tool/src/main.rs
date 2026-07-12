#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use base64::Engine;
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::Digest;
use tokio_agent_plugin::{
    RegistryIndex, RegistryPackage, RootMetadata, SignatureEntry, SignedEnvelope, root_fingerprint,
    validate_package,
};

#[derive(Parser)]
#[command(name = "tokio-agent-registry-tool")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create offline-signed root metadata that delegates registry indexes to a separate key.
    InitRoot {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        root_signing_key: PathBuf,
        #[arg(long)]
        index_signing_key: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        operator: String,
        #[arg(long, default_value_t = 3650)]
        expires_days: u64,
    },
    /// Generate a signed static registry and immutable package archives.
    Generate {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        signing_key: PathBuf,
        /// Existing offline-signed root metadata. Production publication must provide it.
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        operator: String,
        #[arg(long, default_value_t = 30)]
        expires_days: u64,
    },
}

fn main() -> anyhow::Result<()> {
    let Args { command } = Args::parse();
    match command {
        Command::InitRoot {
            output,
            root_signing_key,
            index_signing_key,
            name,
            operator,
            expires_days,
        } => init_root(
            &output,
            &root_signing_key,
            &index_signing_key,
            &name,
            &operator,
            expires_days,
        ),
        Command::Generate {
            source,
            output,
            signing_key,
            root,
            base_url,
            name,
            operator,
            expires_days,
        } => generate(
            &source,
            &output,
            &signing_key,
            root.as_deref(),
            &base_url,
            &name,
            &operator,
            expires_days,
        ),
    }
}

fn init_root(
    output: &Path,
    root_signing_key_path: &Path,
    index_signing_key_path: &Path,
    name: &str,
    operator: &str,
    expires_days: u64,
) -> anyhow::Result<()> {
    let root_signing = load_signing_key(root_signing_key_path)?;
    let index_signing = load_signing_key(index_signing_key_path)?;
    let root_key_id = key_id(&root_signing);
    let index_key_id = key_id(&index_signing);
    if root_key_id == index_key_id {
        bail!("root and index signing keys must be different");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let root = RootMetadata {
        version: 1,
        expires_unix: now.saturating_add(expires_days.saturating_mul(86_400)),
        registry_name: name.to_owned(),
        operator: operator.to_owned(),
        keys: BTreeMap::from([(
            root_key_id.clone(),
            base64::engine::general_purpose::STANDARD
                .encode(root_signing.verifying_key().as_bytes()),
        )]),
        threshold: 1,
        index_keys: BTreeMap::from([(
            index_key_id,
            base64::engine::general_purpose::STANDARD
                .encode(index_signing.verifying_key().as_bytes()),
        )]),
        index_threshold: 1,
    };
    let envelope = sign(root, &root_key_id, &root_signing)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, serde_json::to_vec_pretty(&envelope)?)?;
    println!(
        "Created root metadata with identity {}.",
        root_fingerprint(&envelope.signed)
    );
    Ok(())
}

fn key_id(signing: &SigningKey) -> String {
    format!(
        "sha256:{:x}",
        sha2::Sha256::digest(signing.verifying_key().as_bytes())
    )
}

#[allow(clippy::too_many_arguments)]
fn generate(
    source: &Path,
    output: &Path,
    signing_key_path: &Path,
    existing_root: Option<&Path>,
    base_url: &str,
    name: &str,
    operator: &str,
    expires_days: u64,
) -> anyhow::Result<()> {
    if !base_url.starts_with("https://") && !base_url.starts_with("file://") {
        bail!("registry base URL must use HTTPS (or file:// for fixtures)");
    }
    let signing = load_signing_key(signing_key_path)?;
    let key_id = key_id(&signing);
    let mut keys = BTreeMap::new();
    keys.insert(
        key_id.clone(),
        base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes()),
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let expires = now.saturating_add(expires_days.saturating_mul(86_400));
    let root_envelope = if let Some(path) = existing_root {
        let envelope: SignedEnvelope<RootMetadata> = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        let fingerprint = root_fingerprint(&envelope.signed);
        tokio_agent_plugin::verify_initial_root(&envelope, &fingerprint, SystemTime::now())?;
        if !envelope.signed.index_keys.contains_key(&key_id) {
            bail!("index signing key is not delegated by the supplied root metadata");
        }
        envelope
    } else {
        // Fixture/local convenience. Production publication supplies an
        // offline-signed root and uses only a delegated index key here.
        let root = RootMetadata {
            version: now,
            expires_unix: expires,
            registry_name: name.to_owned(),
            operator: operator.to_owned(),
            keys: keys.clone(),
            threshold: 1,
            index_keys: keys.clone(),
            index_threshold: 1,
        };
        sign(root, &key_id, &signing)?
    };
    let identity = root_fingerprint(&root_envelope.signed);
    let packages_dir = output.join("packages");
    fs::create_dir_all(&packages_dir)?;
    let mut packages = Vec::new();
    let mut roots: Vec<_> = fs::read_dir(source)
        .with_context(|| format!("reading {}", source.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("extension.toml").is_file())
        .map(|entry| entry.path())
        .collect();
    roots.sort();
    for root_path in roots {
        let manifest = validate_package(&root_path, &semver::Version::new(1, 0, 0))?;
        let archive_name = format!("{}-{}.tar.gz", manifest.id, manifest.version);
        let archive_path = packages_dir.join(&archive_name);
        write_archive(&root_path, &archive_path)?;
        let bytes = fs::read(&archive_path)?;
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
        packages.push(RegistryPackage {
            id: manifest.id,
            version: manifest.version,
            name: manifest.name,
            description: manifest.description,
            keywords: Vec::new(),
            license: manifest.license,
            publisher: root_envelope.signed.operator.clone(),
            readme_url: format!("{}/README.md", base_url.trim_end_matches('/')),
            host_api: manifest.host_api,
            minimum_app_version: env!("CARGO_PKG_VERSION").to_owned(),
            artifact_url: format!("{}/{archive_name}", base_url.trim_end_matches('/')),
            digest,
            capabilities: manifest.capabilities.as_set(),
            commands: manifest
                .commands
                .into_iter()
                .map(|command| command.name)
                .collect(),
            skills: manifest
                .skills
                .into_iter()
                .map(|skill| skill.name)
                .collect(),
            tools: manifest.tools.into_iter().map(|tool| tool.name).collect(),
            yanked: false,
            deprecated: None,
        });
    }
    let index = RegistryIndex {
        version: now,
        expires_unix: expires,
        registry_identity: identity.clone(),
        publisher: root_envelope.signed.operator.clone(),
        packages,
    };
    fs::write(
        output.join("root.json"),
        serde_json::to_vec_pretty(&root_envelope)?,
    )?;
    fs::write(
        output.join("index.json"),
        serde_json::to_vec_pretty(&sign(index, &key_id, &signing)?)?,
    )?;
    println!("Generated registry {identity} with signed metadata and immutable packages.");
    Ok(())
}

fn sign<T: serde::Serialize>(
    signed: T,
    key_id: &str,
    key: &SigningKey,
) -> anyhow::Result<SignedEnvelope<T>> {
    let payload = serde_json::to_vec(&signed)?;
    Ok(SignedEnvelope {
        signed,
        signatures: vec![SignatureEntry {
            key_id: key_id.to_owned(),
            signature: base64::engine::general_purpose::STANDARD
                .encode(key.sign(&payload).to_bytes()),
        }],
    })
}

fn load_signing_key(path: &Path) -> anyhow::Result<SigningKey> {
    let encoded = fs::read_to_string(path)
        .with_context(|| format!("reading signing key {}", path.display()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("decoding signing key")?;
    let seed: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!("signing key must be a base64-encoded 32-byte Ed25519 seed")
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

fn write_archive(root: &Path, destination: &Path) -> anyhow::Result<()> {
    let file = fs::File::create(destination)?;
    let encoder = GzEncoder::new(file, Compression::best());
    let mut archive = tar::Builder::new(encoder);
    archive.mode(tar::HeaderMode::Deterministic);
    for name in [
        "extension.toml",
        "README.md",
        "LICENSE",
        "commands",
        "component",
        "assets",
    ] {
        let path = root.join(name);
        if path.is_file() {
            append_file(&mut archive, root, &path)?;
        } else if path.is_dir() {
            append_tree(&mut archive, root, &path)?;
        }
    }
    let encoder = archive.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn append_file<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    path: &Path,
) -> anyhow::Result<()> {
    if path.symlink_metadata()?.file_type().is_symlink() {
        bail!("package symlinks are not allowed: {}", path.display());
    }
    let relative = path.strip_prefix(root)?;
    let mut header = tar::Header::new_gnu();
    let metadata = path.metadata()?;
    header.set_size(metadata.len());
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, relative, fs::File::open(path)?)?;
    Ok(())
}

fn append_tree<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    directory: &Path,
) -> anyhow::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("package symlinks are not allowed: {}", path.display());
        }
        if kind.is_dir() {
            append_tree(archive, root, &path)?;
        } else if kind.is_file() {
            append_file(archive, root, &path)?;
        }
    }
    Ok(())
}
