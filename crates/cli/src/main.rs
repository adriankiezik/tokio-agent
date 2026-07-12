mod extension_runtime;
mod headless;
mod session;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use sha2::Digest;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "tokio-agent",
    version,
    about = "A fast, provider-agnostic terminal coding agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        short = 'p',
        long,
        value_name = "PROMPT",
        help = "Run a single turn non-interactively and print the result to stdout"
    )]
    non_interactive: Option<String>,

    #[arg(long)]
    debug: bool,

    #[arg(
        long,
        help = "Allow all tool actions without permission prompts (dangerous)"
    )]
    yolo: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Sign in to OpenAI with your ChatGPT subscription")]
    Login,
    #[command(about = "Sign out and remove stored ChatGPT credentials")]
    Logout,
    #[command(subcommand, about = "Manage local extensions")]
    Extension(ExtensionCommand),
}

#[derive(Debug, Subcommand)]
enum ExtensionCommand {
    List,
    Search {
        query: String,
    },
    Info {
        id: String,
    },
    Install(InstallArgs),
    Update {
        id: Option<String>,
    },
    Remove {
        id: String,
    },
    #[command(subcommand)]
    Registry(RegistryCommand),
    Check {
        path: PathBuf,
    },
    New {
        name: String,
    },
    Link(ScopeArgs),
    Enable(IdScopeArgs),
    Disable(IdScopeArgs),
    Alias(AliasArgs),
    Import(ImportArgs),
    Dev {
        path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum RegistryCommand {
    List,
    Add {
        url: String,
        #[arg(long)]
        fingerprint: String,
    },
    Remove {
        identity: String,
    },
}

#[derive(Debug, Args)]
struct InstallArgs {
    id: String,
    #[arg(long)]
    registry: Option<String>,
    #[arg(long)]
    approve: bool,
}

#[derive(Debug, Args)]
struct ScopeArgs {
    path: PathBuf,
    #[arg(long)]
    project: bool,
    #[arg(long)]
    approve: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    #[arg(value_parser = ["claude", "codex"])]
    ecosystem: String,
    #[arg(long)]
    project: bool,
}

#[derive(Debug, Args)]
struct AliasArgs {
    id: String,
    command: String,
    alias: String,
    #[arg(long)]
    project: bool,
}

#[derive(Debug, Args)]
struct IdScopeArgs {
    id: String,
    #[arg(long)]
    project: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.debug {
        tracing_subscriber::fmt()
            .with_env_filter("tokio_agent=debug,tokio_agent_core=debug,tokio_agent_provider=debug,tokio_agent_tools=debug,tokio_agent_tui=debug,tokio_agent_config=debug,tokio_agent_mcp=debug,tokio_agent_plugin=debug,tokio_agent_auth=debug")
            .init();
    }

    match cli.command {
        Some(Command::Login) => run_login(),
        Some(Command::Logout) => run_logout(),
        Some(Command::Extension(command)) => run_extension(command),
        None => match cli.non_interactive {
            Some(prompt) => run_headless(prompt, cli.yolo),
            None => run_tui(cli.yolo),
        },
    }
}

fn run_extension(command: ExtensionCommand) -> anyhow::Result<()> {
    use tokio_agent_plugin::{Enablement, ExtensionConfig};
    let cwd = headless::cwd();
    match command {
        ExtensionCommand::List => {
            let user = ExtensionConfig::load(&extension_config_path(false, &cwd))?;
            let project = ExtensionConfig::load(&extension_config_path(true, &cwd))?;
            let ids: std::collections::BTreeSet<_> = user
                .extensions
                .keys()
                .chain(project.extensions.keys())
                .chain(user.linked.keys())
                .chain(project.linked.keys())
                .collect();
            if ids.is_empty() {
                println!("No extensions installed or linked.");
            }
            for id in ids {
                let resolved = ExtensionConfig::resolve(id, &user, &project);
                let scope = match resolved.source {
                    Some(tokio_agent_plugin::ExtensionScope::Project) => "project",
                    Some(tokio_agent_plugin::ExtensionScope::User) => "user",
                    None => "default",
                };
                println!(
                    "{id}\t{} ({scope})",
                    if resolved.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
        }
        ExtensionCommand::Search { query } => {
            for result in registry_search(&query)? {
                println!(
                    "{}\t{}\tv{}\t{}",
                    result.package.id,
                    result.registry_identity,
                    result.package.version,
                    result.package.description
                );
            }
        }
        ExtensionCommand::Info { id } => {
            let results = registry_search(&id)?;
            let matches: Vec<_> = results
                .into_iter()
                .filter(|result| result.package.id == id)
                .collect();
            if matches.is_empty() {
                anyhow::bail!("extension `{id}` was not found in trusted registries");
            }
            for result in matches {
                println!("{} {}", result.package.name, result.package.version);
                println!("  ID: {}", result.package.id);
                println!(
                    "  Registry: {} ({:?})",
                    result.registry_identity, result.trust
                );
                println!("  Publisher: {}", result.package.publisher);
                println!("  License: {}", result.package.license);
                println!("  Capabilities: {:?}", result.package.capabilities);
                println!("  Commands: {}", result.package.commands.join(", "));
                println!("  Skills: {}", result.package.skills.join(", "));
                println!("  Tools: {}", result.package.tools.join(", "));
            }
        }
        ExtensionCommand::Install(args) => install_registry_extension(args)?,
        ExtensionCommand::Update { id } => update_registry_extensions(id.as_deref())?,
        ExtensionCommand::Remove { id } => remove_installed_extension(&id)?,
        ExtensionCommand::Registry(command) => run_registry(command)?,
        ExtensionCommand::Dev { path } => {
            let manifest =
                tokio_agent_plugin::validate_package(&path, &semver::Version::new(1, 0, 0))?;
            println!(
                "Watching {} {}. Declarative files reload when changed; press Ctrl-C to stop.",
                manifest.id, manifest.version
            );
            let mut reloader =
                tokio_agent_plugin::PackageReloader::new(path, semver::Version::new(1, 0, 0))?;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if let Some((manifest, commands)) = reloader.reload_if_changed()? {
                    println!(
                        "Reloaded {} {} ({} declarative contribution(s)).",
                        manifest.id,
                        manifest.version,
                        commands.len()
                    );
                }
            }
        }
        ExtensionCommand::Check { path } => {
            let manifest =
                tokio_agent_plugin::validate_package(&path, &semver::Version::new(1, 0, 0))?;
            println!("✓ {} {} is valid", manifest.id, manifest.version);
        }
        ExtensionCommand::New { name } => {
            create_extension(&cwd.join(&name), &name)?;
            println!("Created {name}/");
        }
        ExtensionCommand::Link(args) => {
            let manifest =
                tokio_agent_plugin::validate_package(&args.path, &semver::Version::new(1, 0, 0))?;
            if manifest.id.starts_with("tokio.official.") {
                anyhow::bail!(
                    "the `tokio.official.*` namespace is reserved for official registry packages"
                );
            }
            let capabilities = manifest.capabilities.as_set();
            if !capabilities.is_empty() && !args.approve {
                anyhow::bail!(
                    "{} requests capabilities {:?}; review them and rerun with --approve",
                    manifest.id,
                    capabilities
                );
            }
            let linked = std::fs::canonicalize(&args.path).context("resolving extension path")?;
            let config_path = extension_config_path(args.project, &cwd);
            let mut config = ExtensionConfig::load(&config_path)?;
            config.linked.insert(manifest.id.clone(), linked.clone());
            config.set(manifest.id.clone(), Enablement::Enabled);
            if args.approve || capabilities.is_empty() {
                config.approve_capabilities(tokio_agent_plugin::CapabilityGrant {
                    registry_identity: "local".to_owned(),
                    extension_id: manifest.id.clone(),
                    publisher: "local".to_owned(),
                    capabilities: capabilities.clone(),
                });
            }
            config.save(&config_path)?;
            if args.project {
                let lock_path = cwd.join(".tokio-agent/extensions.lock");
                let mut lock = tokio_agent_plugin::ExtensionLock::load(&lock_path)?;
                lock.upsert(tokio_agent_plugin::LockedExtension {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source: tokio_agent_plugin::LockedSource::Local {
                        path: linked.clone(),
                        reproducible: false,
                    },
                    digest: tokio_agent_plugin::package_digest(&linked)?,
                    host_api: "1.0.0".to_owned(),
                    capabilities: manifest.capabilities.as_set(),
                    publisher: None,
                });
                lock.save(&lock_path)?;
            }
            println!(
                "Linked and enabled {} for {}.",
                manifest.id,
                if args.project {
                    "this project"
                } else {
                    "all projects"
                }
            );
        }
        ExtensionCommand::Enable(args) => {
            set_extension_enablement(&cwd, args, Enablement::Enabled)?
        }
        ExtensionCommand::Disable(args) => {
            set_extension_enablement(&cwd, args, Enablement::Disabled)?
        }
        ExtensionCommand::Alias(args) => set_command_alias(&cwd, args)?,
        ExtensionCommand::Import(args) => import_ecosystem_commands(&cwd, args)?,
    }
    Ok(())
}

fn registry_trust_path() -> anyhow::Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("locating configuration directory")?
        .join("tokio-agent/registry-trust.toml"))
}

fn registry_cache_root() -> anyhow::Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("locating cache directory")?
        .join("tokio-agent/registries"))
}

fn run_registry(command: RegistryCommand) -> anyhow::Result<()> {
    use tokio_agent_plugin::{RegistryTrustStore, SignedEnvelope};
    let path = registry_trust_path()?;
    let mut trust = RegistryTrustStore::load(&path)?;
    match command {
        RegistryCommand::List => {
            if trust.registries.is_empty() {
                println!("No third-party registries trusted.");
            }
            for (identity, registry) in &trust.registries {
                println!(
                    "{identity}\t{}\t{}\t{}",
                    registry.root.signed.registry_name, registry.root.signed.operator, registry.url
                );
            }
        }
        RegistryCommand::Add { url, fingerprint } => {
            if !(url.starts_with("https://")
                || cfg!(debug_assertions) && url.starts_with("file://"))
            {
                anyhow::bail!("third-party registry URLs must use HTTPS");
            }
            let root_url = format!("{}/root.json", url.trim_end_matches('/'));
            let bytes = http_get(&root_url)?;
            let root: SignedEnvelope<tokio_agent_plugin::RootMetadata> =
                serde_json::from_slice(&bytes).context("decoding registry root metadata")?;
            let actual = tokio_agent_plugin::root_fingerprint(&root.signed);
            println!("Registry: {}", root.signed.registry_name);
            println!("Operator: {}", root.signed.operator);
            println!("Origin: {url}");
            println!("Root fingerprint: {actual}");
            let identity = trust.trust(url, &fingerprint, root, std::time::SystemTime::now())?;
            trust.save(&path)?;
            println!("Trusted registry {identity}.");
        }
        RegistryCommand::Remove { identity } => {
            if trust.remove(&identity).is_none() {
                anyhow::bail!("registry `{identity}` is not trusted");
            }
            trust.save(&path)?;
            println!("Removed registry trust. Installed packages were preserved.");
        }
    }
    Ok(())
}

fn http_get(url: &str) -> anyhow::Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).with_context(|| format!("reading {url}"));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await?
            .error_for_status()?;
        Ok::<_, anyhow::Error>(response.bytes().await?.to_vec())
    })
}

fn load_registry_catalog(refresh: bool) -> anyhow::Result<tokio_agent_plugin::RegistryCatalog> {
    use tokio_agent_plugin::{
        RegistryCatalog, RegistryIndex, RegistryTrust, RegistryTrustStore, SignedEnvelope,
    };
    let trust_path = registry_trust_path()?;
    let mut trust = RegistryTrustStore::load(&trust_path)?;
    if refresh {
        let registries: Vec<_> = trust
            .registries
            .iter()
            .map(|(identity, registry)| {
                (
                    identity.clone(),
                    registry.url.clone(),
                    registry.root.signed.version,
                )
            })
            .collect();
        let mut changed = false;
        for (identity, url, version) in registries {
            let root_url = format!("{}/root.json", url.trim_end_matches('/'));
            if let Ok(bytes) = http_get(&root_url)
                && let Ok(next) = serde_json::from_slice::<
                    SignedEnvelope<tokio_agent_plugin::RootMetadata>,
                >(&bytes)
                && next.signed.version > version
            {
                trust.rotate(&identity, next, std::time::SystemTime::now())?;
                changed = true;
            }
        }
        if changed {
            trust.save(&trust_path)?;
        }
    }
    let official_root = tokio_agent_plugin::builtin_official_root()?;
    let mut catalog = official_root
        .as_ref()
        .map_or_else(RegistryCatalog::default, |root| {
            RegistryCatalog::new(tokio_agent_plugin::root_fingerprint(&root.signed))
        });
    let cache_root = registry_cache_root()?;
    if let Some(root) = official_root {
        let identity = tokio_agent_plugin::root_fingerprint(&root.signed);
        let cache = cache_root.join(&identity).join("index.json");
        if cache.exists() {
            let cached = RegistryCatalog::load_cache(&cache)?;
            catalog.add_verified(
                &identity,
                RegistryTrust::Official,
                cached,
                &root.signed.index_keys,
                root.signed.index_threshold,
            )?;
        }
        if refresh {
            let url = format!("{}/index.json", tokio_agent_plugin::OFFICIAL_REGISTRY_URL);
            match http_get(&url).and_then(|bytes| {
                serde_json::from_slice::<SignedEnvelope<RegistryIndex>>(&bytes).map_err(Into::into)
            }) {
                Ok(envelope) => {
                    catalog.add_verified(
                        &identity,
                        RegistryTrust::Official,
                        envelope.clone(),
                        &root.signed.index_keys,
                        root.signed.index_threshold,
                    )?;
                    RegistryCatalog::save_cache(&envelope, &cache)?;
                }
                Err(error) if cache.exists() => eprintln!(
                    "warning: official registry refresh failed ({error}); using cached metadata"
                ),
                Err(error) => return Err(error),
            }
        }
    }
    for (identity, registry) in trust.registries {
        let cache = cache_root.join(&identity).join("index.json");
        let trust_label = RegistryTrust::ThirdParty {
            name: registry.root.signed.registry_name.clone(),
            operator: registry.root.signed.operator.clone(),
        };
        if cache.exists() {
            let cached = RegistryCatalog::load_cache(&cache)?;
            catalog.add_verified(
                &identity,
                trust_label.clone(),
                cached,
                &registry.root.signed.index_keys,
                registry.root.signed.index_threshold,
            )?;
        }
        if refresh {
            let url = format!("{}/index.json", registry.url.trim_end_matches('/'));
            match http_get(&url).and_then(|bytes| {
                serde_json::from_slice::<SignedEnvelope<RegistryIndex>>(&bytes).map_err(Into::into)
            }) {
                Ok(envelope) => {
                    catalog.add_verified(
                        &identity,
                        trust_label,
                        envelope.clone(),
                        &registry.root.signed.index_keys,
                        registry.root.signed.index_threshold,
                    )?;
                    RegistryCatalog::save_cache(&envelope, &cache)?;
                }
                Err(error) if cache.exists() => {
                    eprintln!("warning: registry refresh failed ({error}); using cached metadata");
                }
                Err(error) => return Err(error),
            }
        } else if !cache.exists() {
            anyhow::bail!("no cached metadata for registry {identity}");
        }
    }
    Ok(catalog)
}

fn registry_search(query: &str) -> anyhow::Result<Vec<tokio_agent_plugin::SearchResult>> {
    Ok(load_registry_catalog(true)?.search(query))
}

fn install_registry_extension(args: InstallArgs) -> anyhow::Result<()> {
    let catalog = load_registry_catalog(true)?;
    let host = semver::Version::new(1, 0, 0);
    let app = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    let candidates = catalog.search(&args.id);
    let identities: std::collections::BTreeSet<_> = candidates
        .iter()
        .filter(|result| result.package.id == args.id)
        .map(|result| result.registry_identity.clone())
        .collect();
    let identity = match args.registry {
        Some(identity) => identity,
        None if identities.len() == 1 => identities.into_iter().next().expect("one identity"),
        None if identities.is_empty() => anyhow::bail!("extension `{}` was not found", args.id),
        None => anyhow::bail!(
            "extension `{}` exists in multiple registries; pass --registry <root-identity>",
            args.id
        ),
    };
    let package = catalog.resolve(&identity, &args.id, &host, &app)?.clone();
    let cwd = headless::cwd();
    let config_path = extension_config_path(false, &cwd);
    let mut user_config = tokio_agent_plugin::ExtensionConfig::load(&config_path)?;
    let grant = tokio_agent_plugin::CapabilityGrant {
        registry_identity: identity.clone(),
        extension_id: package.id.clone(),
        publisher: package.publisher.clone(),
        capabilities: package.capabilities.clone(),
    };
    let already_approved = user_config.grant_matches(&grant);
    if !package.capabilities.is_empty() && !args.approve && !already_approved {
        anyhow::bail!(
            "{} requests capabilities {:?}; review them and rerun with --approve",
            package.id,
            package.capabilities
        );
    }
    let archive = http_get(&package.artifact_url)?;
    let actual = format!("sha256:{:x}", sha2::Sha256::digest(&archive));
    if actual != package.digest {
        anyhow::bail!(
            "package digest mismatch: expected {}, got {actual}",
            package.digest
        );
    }
    let store = tokio_agent_plugin::PackageStore::user_default(host.clone())?;
    let records_path = store.root().join("installations.lock");
    let existing_records = tokio_agent_plugin::ExtensionLock::load(&records_path)?;
    if existing_records.extensions.iter().any(|entry| {
        entry.id == package.id
            && matches!(&entry.source, tokio_agent_plugin::LockedSource::Registry { root_identity, .. } if root_identity != &identity)
    }) {
        anyhow::bail!(
            "extension `{}` is installed from another registry; remove it before explicitly changing source",
            package.id
        );
    }
    let temporary = store
        .root()
        .join(format!(".install-{}", std::process::id()));
    if temporary.exists() {
        std::fs::remove_dir_all(&temporary)?;
    }
    std::fs::create_dir_all(&temporary)?;
    let extraction = extract_package_archive(&archive, &temporary)
        .and_then(|()| {
            validate_programmable_package(
                &temporary,
                &package.id,
                &package.version,
                &package.capabilities,
            )
        })
        .and_then(|()| {
            store
                .install_directory(&temporary, None)
                .map_err(Into::into)
        });
    let _ = std::fs::remove_dir_all(&temporary);
    let installed = extraction?;
    let mut records = existing_records;
    records.upsert(tokio_agent_plugin::LockedExtension {
        id: package.id.clone(),
        version: package.version.clone(),
        source: tokio_agent_plugin::LockedSource::Registry {
            root_identity: identity.clone(),
            url: package.artifact_url.clone(),
        },
        digest: package.digest.clone(),
        host_api: host.to_string(),
        capabilities: package.capabilities.clone(),
        publisher: Some(package.publisher.clone()),
    });
    records.save(&records_path)?;
    if args.approve || package.capabilities.is_empty() || already_approved {
        user_config.approve_capabilities(grant);
        user_config.save(&config_path)?;
    }
    println!(
        "Installed {} {} from {identity}. It is not enabled.",
        installed.manifest.id, installed.manifest.version
    );
    Ok(())
}

fn validate_programmable_package(
    root: &Path,
    expected_id: &str,
    expected_version: &str,
    expected_capabilities: &std::collections::BTreeSet<tokio_agent_extension_api::Capability>,
) -> anyhow::Result<()> {
    let manifest = tokio_agent_plugin::validate_package(root, &semver::Version::new(1, 0, 0))?;
    if manifest.id != expected_id || manifest.version != expected_version {
        anyhow::bail!(
            "downloaded manifest identity {}@{} does not match signed metadata {expected_id}@{expected_version}",
            manifest.id,
            manifest.version
        );
    }
    if &manifest.capabilities.as_set() != expected_capabilities {
        anyhow::bail!("downloaded manifest capabilities do not match signed metadata");
    }
    let Some(runtime) = manifest.runtime else {
        return Ok(());
    };
    let component_path = root.join(runtime.component);
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio_runtime.block_on(async {
        let mut companion = tokio_agent_plugin::CompanionManager::default();
        let response = companion
            .request(&tokio_agent_extension_api::HostRequest::ValidateComponent {
                component_path: component_path.to_string_lossy().into_owned(),
            })
            .await?;
        companion.stop().await;
        match response {
            tokio_agent_extension_api::HostResponse::ComponentValid => Ok(()),
            tokio_agent_extension_api::HostResponse::Error { message, .. } => {
                anyhow::bail!(message)
            }
            _ => anyhow::bail!("extension companion returned an invalid validation response"),
        }
    })
}

fn extract_package_archive(bytes: &[u8], destination: &Path) -> anyhow::Result<()> {
    use std::path::Component;
    const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
    const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        anyhow::bail!("package archive exceeds the 64 MiB download limit");
    }
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut extracted_bytes = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || !path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            anyhow::bail!("package archive contains an unsafe path");
        }
        extracted_bytes = extracted_bytes.saturating_add(entry.header().size()?);
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            anyhow::bail!("package archive exceeds the 256 MiB extracted-size limit");
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            anyhow::bail!("package archive contains links or unsupported entries");
        }
        entry.unpack_in(destination)?;
    }
    Ok(())
}

fn update_registry_extensions(id: Option<&str>) -> anyhow::Result<()> {
    let store = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
    let records =
        tokio_agent_plugin::ExtensionLock::load(&store.root().join("installations.lock"))?;
    let selected: Vec<_> = records
        .extensions
        .into_iter()
        .filter(|entry| id.is_none_or(|id| entry.id == id))
        .collect();
    if selected.is_empty() {
        anyhow::bail!("no matching registry extensions are installed");
    }
    for entry in selected {
        let tokio_agent_plugin::LockedSource::Registry { root_identity, .. } = entry.source else {
            continue;
        };
        let extension_id = entry.id;
        install_registry_extension(InstallArgs {
            id: extension_id.clone(),
            registry: Some(root_identity.clone()),
            approve: false,
        })?;
        let cwd = headless::cwd();
        let lock_path = cwd.join(".tokio-agent/extensions.lock");
        let mut project_lock = tokio_agent_plugin::ExtensionLock::load(&lock_path)?;
        if project_lock.extensions.iter().any(|locked| {
            locked.id == extension_id
                && matches!(&locked.source, tokio_agent_plugin::LockedSource::Registry { root_identity: source, .. } if source == &root_identity)
        }) {
            let current = tokio_agent_plugin::ExtensionLock::load(&store.root().join("installations.lock"))?;
            if let Some(updated) = current.extensions.into_iter().find(|record| {
                record.id == extension_id
                    && matches!(&record.source, tokio_agent_plugin::LockedSource::Registry { root_identity: source, .. } if source == &root_identity)
            }) {
                project_lock.upsert(updated);
                project_lock.save(&lock_path)?;
            }
        }
    }
    Ok(())
}

fn remove_installed_extension(id: &str) -> anyhow::Result<()> {
    let store = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
    let mut removed = false;
    for package in store.list()? {
        if package.manifest.id == id {
            removed |= store.remove(id, &package.manifest.version)?;
        }
    }
    let records_path = store.root().join("installations.lock");
    let mut records = tokio_agent_plugin::ExtensionLock::load(&records_path)?;
    records.extensions.retain(|entry| entry.id != id);
    records.save(&records_path)?;
    if !removed {
        anyhow::bail!("extension `{id}` is not installed");
    }
    let cwd = headless::cwd();
    for project in [false, true] {
        let path = extension_config_path(project, &cwd);
        let mut config = tokio_agent_plugin::ExtensionConfig::load(&path)?;
        config.extensions.remove(id);
        config
            .capability_grants
            .retain(|grant| grant.extension_id != id);
        config.save(&path)?;
    }
    let project_lock_path = cwd.join(".tokio-agent/extensions.lock");
    let mut project_lock = tokio_agent_plugin::ExtensionLock::load(&project_lock_path)?;
    project_lock.extensions.retain(|entry| entry.id != id);
    project_lock.save(&project_lock_path)?;
    println!("Removed {id}.");
    Ok(())
}

fn import_ecosystem_commands(cwd: &Path, args: ImportArgs) -> anyhow::Result<()> {
    let source = if args.project {
        match args.ecosystem.as_str() {
            "claude" => cwd.join(".claude/commands"),
            "codex" => cwd.join(".codex/prompts"),
            _ => unreachable!("clap validates ecosystem"),
        }
    } else {
        let home = dirs::home_dir().context("locating home directory")?;
        match args.ecosystem.as_str() {
            "claude" => home.join(".claude/commands"),
            "codex" => home.join(".codex/prompts"),
            _ => unreachable!("clap validates ecosystem"),
        }
    };
    if !source.is_dir() {
        anyhow::bail!(
            "no {} command directory found at {}",
            args.ecosystem,
            source.display()
        );
    }
    let destination = if args.project {
        cwd.join(".tokio-agent/commands")
    } else {
        dirs::config_dir()
            .context("locating configuration directory")?
            .join("tokio-agent/commands")
    };
    let commands = tokio_agent_plugin::discover_prompt_commands_in(None, &source)?;
    std::fs::create_dir_all(&destination)?;
    let mut imported = 0_usize;
    for command in commands {
        let file_name = command
            .path
            .file_name()
            .context("command has no file name")?;
        let target = destination.join(file_name);
        if target.exists() {
            anyhow::bail!(
                "refusing to overwrite existing command {}",
                target.display()
            );
        }
        std::fs::copy(&command.path, &target)?;
        imported += 1;
    }
    println!(
        "Imported {imported} {} command(s) into {}.",
        args.ecosystem,
        destination.display()
    );
    Ok(())
}

fn set_command_alias(cwd: &Path, args: AliasArgs) -> anyhow::Result<()> {
    let alias = args.alias.trim_start_matches('/');
    if alias.is_empty()
        || alias.len() > 64
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        anyhow::bail!("alias must contain only lowercase letters, digits, and hyphens");
    }
    let command = args.command.trim_start_matches('/');
    let user = tokio_agent_plugin::ExtensionConfig::load(&extension_config_path(false, cwd))?;
    let project = tokio_agent_plugin::ExtensionConfig::load(&extension_config_path(true, cwd))?;
    let root = project
        .linked
        .get(&args.id)
        .or_else(|| user.linked.get(&args.id))
        .cloned()
        .or_else(|| {
            tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))
                .ok()?
                .list()
                .ok()?
                .into_iter()
                .find(|package| package.manifest.id == args.id)
                .map(|package| package.path)
        })
        .with_context(|| format!("extension `{}` is not installed or linked", args.id))?;
    let manifest = tokio_agent_plugin::validate_package(&root, &semver::Version::new(1, 0, 0))?;
    if !manifest
        .commands
        .iter()
        .any(|contribution| contribution.name == command)
        && !manifest.skills.iter().any(|skill| skill.name == command)
    {
        anyhow::bail!(
            "extension `{}` does not contribute command `{command}`",
            args.id
        );
    }
    let path = extension_config_path(args.project, cwd);
    let mut config = tokio_agent_plugin::ExtensionConfig::load(&path)?;
    let command_id = if manifest.skills.iter().any(|skill| skill.name == command) {
        format!("{}:skill-{command}", args.id)
    } else {
        format!("{}:{command}", args.id)
    };
    config.command_aliases.insert(command_id, alias.to_owned());
    config.save(&path)?;
    println!("Aliased {}/{} to /{}.", args.id, command, alias);
    Ok(())
}

fn set_extension_enablement(
    cwd: &Path,
    args: IdScopeArgs,
    value: tokio_agent_plugin::Enablement,
) -> anyhow::Result<()> {
    let config_path = extension_config_path(args.project, cwd);
    let mut config = tokio_agent_plugin::ExtensionConfig::load(&config_path)?;
    config.set(&args.id, value);
    if args.project {
        let project_lock_path = cwd.join(".tokio-agent/extensions.lock");
        let mut project_lock = tokio_agent_plugin::ExtensionLock::load(&project_lock_path)?;
        if value == tokio_agent_plugin::Enablement::Disabled {
            project_lock.extensions.retain(|entry| entry.id != args.id);
        } else {
            let store =
                tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
            let records =
                tokio_agent_plugin::ExtensionLock::load(&store.root().join("installations.lock"))?;
            if let Some(record) = records
                .extensions
                .into_iter()
                .find(|entry| entry.id == args.id)
            {
                let registry_identity = match &record.source {
                    tokio_agent_plugin::LockedSource::Registry { root_identity, .. } => {
                        root_identity
                    }
                    tokio_agent_plugin::LockedSource::Local { .. } => {
                        unreachable!("installation records contain registry packages")
                    }
                };
                let grant = tokio_agent_plugin::CapabilityGrant {
                    registry_identity: registry_identity.clone(),
                    extension_id: record.id.clone(),
                    publisher: record.publisher.clone().unwrap_or_default(),
                    capabilities: record.capabilities.clone(),
                };
                let user =
                    tokio_agent_plugin::ExtensionConfig::load(&extension_config_path(false, cwd))?;
                if !user.grant_matches(&grant) && !config.grant_matches(&grant) {
                    anyhow::bail!(
                        "capabilities or publisher changed; reinstall with explicit approval before enabling"
                    );
                }
                project_lock.upsert(record);
            } else if !config.linked.contains_key(&args.id) {
                anyhow::bail!("extension `{}` is not installed or linked", args.id);
            }
        }
        project_lock.save(&project_lock_path)?;
    }
    config.save(&config_path)?;
    println!(
        "{} {} for {}.",
        if value == tokio_agent_plugin::Enablement::Enabled {
            "Enabled"
        } else {
            "Disabled"
        },
        args.id,
        if args.project {
            "this project"
        } else {
            "all projects"
        }
    );
    Ok(())
}

fn extension_config_path(project: bool, cwd: &Path) -> PathBuf {
    if project {
        cwd.join(".tokio-agent/extensions.toml")
    } else {
        dirs::config_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join("tokio-agent/extensions.toml")
    }
}

fn create_extension(path: &Path, name: &str) -> anyhow::Result<()> {
    if path.exists() {
        anyhow::bail!("{} already exists", path.display());
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        anyhow::bail!("extension name must contain only lowercase letters, digits, and hyphens");
    }
    std::fs::create_dir_all(path.join("commands"))?;
    let manifest = format!(
        "manifest_version = 1\nid = \"local.user.{name}\"\nname = \"{name}\"\nversion = \"0.1.0\"\ndescription = \"Local extension\"\nlicense = \"MIT\"\nhost_api = \">=1.0, <2.0\"\n\n[[commands]]\nname = \"{name}\"\ndescription = \"Run {name}\"\nprompt = \"commands/{name}.md\"\n"
    );
    std::fs::write(path.join("extension.toml"), manifest)?;
    std::fs::write(
        path.join("commands").join(format!("{name}.md")),
        "{{ arguments }}\n",
    )?;
    std::fs::write(path.join("README.md"), format!("# {name}\n"))?;
    Ok(())
}

fn run_login() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?;

    let outcome = runtime
        .block_on(tokio_agent_auth::login())
        .context("signing in with ChatGPT")?;

    match outcome.email {
        Some(email) => println!("Signed in as {email}."),
        None => println!("Signed in."),
    }
    println!("Set `provider = \"openai\"` and `auth = \"chatgpt\"` in your config to use it.");
    Ok(())
}

fn run_logout() -> anyhow::Result<()> {
    match tokio_agent_auth::logout().context("signing out")? {
        Some(path) => println!("Removed stored credentials at {}.", path.display()),
        None => println!("No stored credentials to remove."),
    }
    Ok(())
}

fn run_headless(prompt: String, yolo: bool) -> anyhow::Result<()> {
    let cwd = headless::cwd();
    let agent = session::build_session(&cwd, yolo)?;
    let initial_command = if prompt.starts_with('/') {
        let (name, arguments) = prompt
            .split_once(char::is_whitespace)
            .unwrap_or((&prompt, ""));
        let descriptor = agent
            .command_catalog()
            .into_iter()
            .find(|descriptor| descriptor.name == name)
            .with_context(|| format!("unknown command `{name}`"))?;
        tokio_agent_core::agent::UiCommand::InvokeCommand {
            id: descriptor.id,
            arguments: arguments.trim().to_owned(),
        }
    } else {
        tokio_agent_core::agent::UiCommand::UserMessage(prompt)
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?;

    runtime.block_on(async move {
        let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let turn = tokio::spawn(agent.run(commands_rx, events_tx));
        commands_tx
            .send(initial_command)
            .context("starting the turn")?;
        let mut printer = headless::Printer::new();
        printer
            .consume(events_rx, &commands_tx)
            .await
            .context("running the turn")?;
        turn.await.context("agent task panicked")?;
        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}

fn run_tui(yolo: bool) -> anyhow::Result<()> {
    std::thread::spawn(|| {
        if let Err(error) = load_registry_catalog(true) {
            tracing::debug!(%error, "background registry refresh failed");
        }
    });
    let cwd = headless::cwd();
    let mut agent = loop {
        match session::build_session(&cwd, yolo) {
            Ok(agent) => break agent,
            Err(error) => {
                if tokio_agent_tui::configure_provider(&cwd).context("configuring a provider")? {
                    continue;
                }
                return Err(error);
            }
        }
    };
    let mut tui = tokio_agent_tui::Tui::new().context("starting the terminal UI")?;
    loop {
        match tui.run(agent).context("running the terminal UI")? {
            tokio_agent_tui::RunOutcome::Quit => return Ok(()),
            tokio_agent_tui::RunOutcome::ConfigureProvider
            | tokio_agent_tui::RunOutcome::ExtensionsChanged => {
                agent = loop {
                    match session::build_session(&cwd, yolo) {
                        Ok(agent) => break agent,
                        Err(error) => {
                            if tui
                                .configure_provider(&cwd)
                                .context("configuring a provider")?
                            {
                                continue;
                            }
                            return Err(error);
                        }
                    }
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_flag_is_available_for_interactive_and_non_interactive_modes() {
        let interactive = Cli::try_parse_from(["tokio-agent", "--yolo"]).unwrap();
        assert!(interactive.yolo);

        let long =
            Cli::try_parse_from(["tokio-agent", "--yolo", "--non-interactive", "hello"]).unwrap();
        assert!(long.yolo);
        assert_eq!(long.non_interactive.as_deref(), Some("hello"));

        let short = Cli::try_parse_from(["tokio-agent", "-p", "hello"]).unwrap();
        assert_eq!(short.non_interactive.as_deref(), Some("hello"));
        assert!(Cli::try_parse_from(["tokio-agent", "--print", "hello"]).is_err());
    }
}
