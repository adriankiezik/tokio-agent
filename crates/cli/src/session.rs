use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio_agent_config::{AuthKind, Config, ProviderKind, ResolvedConfig};
use tokio_agent_core::agent::{Agent, ModelConfig};
use tokio_agent_provider::{Anthropic, AnyProvider, DeepSeek, OpenAi};

const DEFAULT_SYSTEM_PROMPT: &str = include_str!("default_system_prompt.md");

static STARTUP_EXTENSION_SETTINGS: std::sync::OnceLock<BTreeMap<String, serde_json::Value>> =
    std::sync::OnceLock::new();

pub fn set_startup_extension_settings(settings: BTreeMap<String, serde_json::Value>) {
    let _ = STARTUP_EXTENSION_SETTINGS.set(settings);
}

pub fn extension_cli_options(
    cwd: &Path,
) -> anyhow::Result<Vec<(String, tokio_agent_plugin::CliOptionContribution)>> {
    let config = Config::load(cwd)?.resolve()?;
    Ok(load_programmable_packages(cwd, &config.extensions)?
        .into_iter()
        .flat_map(|package| {
            let id = package.manifest.id.clone();
            package
                .manifest
                .cli_options
                .into_iter()
                .map(move |option| (id.clone(), option))
        })
        .collect())
}

pub fn build_session(cwd: &Path) -> anyhow::Result<Agent<AnyProvider>> {
    let config = Config::load(cwd).context("loading config")?;
    let config = config.resolve().context("validating config")?;
    SessionBuilder::new(config, cwd).build()
}

struct SessionBuilder<'a> {
    config: ResolvedConfig,
    cwd: &'a Path,
}

impl<'a> SessionBuilder<'a> {
    fn new(config: ResolvedConfig, cwd: &'a Path) -> Self {
        Self { config, cwd }
    }

    fn build(self) -> anyhow::Result<Agent<AnyProvider>> {
        ensure_project_registries_trusted(self.cwd)?;
        let command_router = build_command_router(self.cwd)?;
        let command_catalog = command_router.catalog();
        let command_router = Arc::new(std::sync::RwLock::new(command_router));
        let extension_catalog = load_extension_summaries(self.cwd)?;
        let command_cwd = self.cwd.to_path_buf();
        let ResolvedConfig {
            provider: provider_kind,
            model,
            api_base,
            auth,
            max_tokens,
            context_window_tokens,
            reasoning_effort,
            extensions,
            system_prompt,
            bash_yield_time_ms,
            bash_timeout_ms,
        } = self.config;
        let supports_reasoning_effort = matches!(
            provider_kind,
            ProviderKind::Anthropic | ProviderKind::OpenAi | ProviderKind::DeepSeek
        );
        let tools = tools_for_provider(provider_kind, bash_yield_time_ms, bash_timeout_ms);

        let provider = match provider_kind {
            ProviderKind::Anthropic => {
                let api_key = tokio_agent_config::api_key(provider_kind.as_str())
                    .context("resolving API key")?;
                AnyProvider::Anthropic(Anthropic::new(api_key, api_base))
            }
            ProviderKind::OpenAi => {
                AnyProvider::OpenAi(Self::build_openai(provider_kind, auth, api_base)?)
            }
            ProviderKind::DeepSeek => {
                let api_key = tokio_agent_config::api_key(provider_kind.as_str())
                    .context("resolving API key")?;
                AnyProvider::DeepSeek(DeepSeek::new(api_key, api_base))
            }
        };
        let system = system_prompt.unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());

        let mut agent = Agent::new(
            provider,
            tools,
            None,
            ModelConfig {
                model,
                system,
                max_tokens,
                reasoning_effort,
            },
            self.cwd.to_path_buf(),
        );
        let extension_runtime = crate::extension_runtime::ExtensionRuntime::start(
            load_programmable_packages(self.cwd, &extensions)?,
            agent.dynamic_tools(),
        )?;
        let gate_slot = agent.tool_gate_slot();
        if let Some(gate) = extension_runtime.tool_gate(gate_slot.clone()) {
            gate_slot.attach(gate);
        }
        let interaction_runtime = extension_runtime.clone();
        agent = agent.with_interaction_responder(move |response| {
            interaction_runtime
                .respond_to_interaction(response)
                .map_err(|error| error.to_string())
        });
        let route_runtime = extension_runtime.clone();
        let route_router = Arc::clone(&command_router);
        agent = agent
            .with_command_router(command_catalog.clone(), move |id, arguments| {
                let router = route_router
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                route_command(&router, &command_cwd, &route_runtime, id, arguments)
            })
            .with_extension_catalog(extension_catalog.clone())
            .with_reasoning_effort_support(supports_reasoning_effort)
            .with_provider_name(provider_kind.as_str())
            .with_context_window(context_window_tokens);
        let hook = extension_runtime.clone();
        agent = agent.with_session_hook(move |event| hook.event(event));
        let watcher_router = Arc::clone(&command_router);
        let watcher_runtime = extension_runtime.clone();
        let shutdown_runtime = extension_runtime.clone();
        let watcher_cwd = self.cwd.to_path_buf();
        let watcher_settings = extensions.clone();
        let watcher_state = Arc::new(std::sync::Mutex::new((
            std::time::Instant::now(),
            command_catalog,
            std::time::Instant::now(),
            extension_catalog,
        )));
        agent = agent.with_session_poll(move || {
            let mut effects = extension_runtime.poll();
            let mut state = watcher_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.0.elapsed() >= std::time::Duration::from_millis(500) {
                state.0 = std::time::Instant::now();
                if let Ok(router) = build_command_router(&watcher_cwd)
                    && let Ok(packages) =
                        load_programmable_packages(&watcher_cwd, &watcher_settings)
                    && watcher_runtime.load(packages).is_ok()
                {
                    let catalog = router.catalog();
                    *watcher_router
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = router;
                    if catalog != state.1 {
                        state.1 = catalog.clone();
                        effects.push(tokio_agent_core::SessionHookEffect::CommandCatalog(catalog));
                    }
                }
            }
            if state.2.elapsed() >= std::time::Duration::from_secs(2) {
                state.2 = std::time::Instant::now();
                if let Ok(catalog) = load_extension_summaries(&watcher_cwd)
                    && catalog != state.3
                {
                    state.3 = catalog.clone();
                    effects.push(tokio_agent_core::SessionHookEffect::ExtensionCatalog(
                        catalog,
                    ));
                }
            }
            effects
        });
        agent = agent.with_shutdown_hook(move || shutdown_runtime.shutdown());
        Ok(agent)
    }

    fn build_openai(
        provider: ProviderKind,
        auth: Option<AuthKind>,
        api_base: Option<String>,
    ) -> anyhow::Result<OpenAi> {
        let use_chatgpt = match auth {
            Some(AuthKind::ChatGpt) => true,
            Some(AuthKind::ApiKey) => false,
            None => tokio_agent_auth::is_signed_in(),
        };

        if use_chatgpt {
            let auth = tokio_agent_auth::load()
                .context("no ChatGPT login found — run `tokio-agent login`")?;
            Ok(OpenAi::chatgpt(Arc::new(auth), api_base))
        } else {
            let api_key =
                tokio_agent_config::api_key(provider.as_str()).context("resolving API key")?;
            Ok(OpenAi::new(api_key, api_base))
        }
    }
}

fn build_command_router(cwd: &Path) -> anyhow::Result<tokio_agent_plugin::CommandRouter> {
    let (prompt_commands, programmable_commands) = load_prompt_commands(cwd)?;
    let mut router = tokio_agent_plugin::CommandRouter::new(prompt_commands)
        .context("building command catalog")?;
    for descriptor in programmable_commands {
        router
            .register_extension(descriptor)
            .context("registering programmable command")?;
    }
    Ok(router)
}

fn ensure_project_registries_trusted(cwd: &Path) -> anyhow::Result<()> {
    let project =
        tokio_agent_plugin::ExtensionConfig::load(&cwd.join(".tokio-agent/extensions.toml"))?;
    if project.registries.is_empty() {
        return Ok(());
    }
    let trust_path = dirs::config_dir()
        .context("locating user configuration for registry trust")?
        .join("tokio-agent/registry-trust.toml");
    let trust = tokio_agent_plugin::RegistryTrustStore::load(&trust_path)?;
    for reference in project.registries {
        let trusted = trust
            .registries
            .get(&reference.fingerprint)
            .is_some_and(|registry| {
                registry.url == reference.url && registry.fingerprint == reference.fingerprint
            });
        if !trusted {
            anyhow::bail!(
                "project registry {} is not trusted; explicitly run `tokio-agent extension registry add {} --fingerprint {}` first",
                reference.url,
                reference.url,
                reference.fingerprint
            );
        }
    }
    Ok(())
}

fn load_prompt_commands(
    cwd: &Path,
) -> anyhow::Result<(
    Vec<tokio_agent_plugin::PromptCommand>,
    Vec<tokio_agent_extension_api::CommandDescriptor>,
)> {
    use std::collections::BTreeMap;
    use tokio_agent_plugin::ExtensionConfig;

    let mut commands =
        tokio_agent_plugin::discover_prompt_commands(cwd).context("discovering local commands")?;
    let mut programmable_commands = Vec::new();
    let user_path = dirs::config_dir().map(|path| path.join("tokio-agent/extensions.toml"));
    let user = match user_path {
        Some(path) => ExtensionConfig::load(&path)?,
        None => ExtensionConfig::default(),
    };
    let project = ExtensionConfig::load(&cwd.join(".tokio-agent/extensions.toml"))?;
    let ids = installed_extension_ids(&user, &project)?;
    for id in ids {
        let root = resolve_extension_root(cwd, &id, &user, &project)?
            .with_context(|| format!("extension `{id}` is not installed or linked"))?;
        let manifest = tokio_agent_plugin::validate_package(&root, &semver::Version::new(1, 0, 0))?;
        if manifest.id != id {
            anyhow::bail!(
                "linked extension ID `{id}` does not match manifest ID `{}`",
                manifest.id
            );
        }
        let mut package_commands = tokio_agent_plugin::commands_from_package(&root, &manifest)?;
        for command in &mut package_commands {
            if let Some(alias) =
                ExtensionConfig::resolve_alias(command.descriptor.id.as_str(), &user, &project)
            {
                command.descriptor.name = format!("/{}", alias.trim_start_matches('/'));
            }
        }
        commands.extend(package_commands);
        programmable_commands.extend(
            manifest
                .commands
                .iter()
                .filter(|command| command.handler.is_some())
                .map(|command| {
                    let id = tokio_agent_extension_api::CommandId::new(format!(
                        "{}:{}",
                        manifest.id, command.name
                    ));
                    let name = ExtensionConfig::resolve_alias(id.as_str(), &user, &project)
                        .map_or_else(
                            || format!("/{}", command.name),
                            |alias| format!("/{}", alias.trim_start_matches('/')),
                        );
                    tokio_agent_extension_api::CommandDescriptor {
                        id,
                        name,
                        description: command.description.clone(),
                        usage: command.usage.clone(),
                        source: tokio_agent_extension_api::CommandSource::Extension {
                            id: tokio_agent_extension_api::ExtensionId::new(&manifest.id),
                            version: manifest.version.clone(),
                        },
                        available_while_running: command.available_while_running,
                    }
                }),
        );
    }
    let reserved = ["/clear", "/model", "/providers", "/extensions"];
    let mut names = BTreeMap::new();
    for command in &commands {
        if reserved.contains(&command.descriptor.name.as_str()) {
            anyhow::bail!(
                "extension command `{}` conflicts with a built-in command",
                command.descriptor.name
            );
        }
        if let Some(previous) = names.insert(command.descriptor.name.clone(), command.path.clone())
        {
            anyhow::bail!(
                "command `{}` is provided by both {} and {}",
                command.descriptor.name,
                previous.display(),
                command.path.display()
            );
        }
    }
    for descriptor in &programmable_commands {
        if reserved.contains(&descriptor.name.as_str()) {
            anyhow::bail!(
                "extension command `{}` conflicts with a built-in command",
                descriptor.name
            );
        }
        if let Some(previous) = names.insert(
            descriptor.name.clone(),
            std::path::PathBuf::from(descriptor.id.as_str()),
        ) {
            anyhow::bail!(
                "command `{}` is provided by both {} and {}",
                descriptor.name,
                previous.display(),
                descriptor.id
            );
        }
    }
    Ok((commands, programmable_commands))
}

fn load_programmable_packages(
    cwd: &Path,
    extension_settings: &BTreeMap<String, toml::Value>,
) -> anyhow::Result<Vec<crate::extension_runtime::ProgrammablePackage>> {
    use tokio_agent_plugin::{CapabilityGrant, ExtensionConfig, LockedSource};
    let user_path = dirs::config_dir().map(|path| path.join("tokio-agent/extensions.toml"));
    let user = match user_path {
        Some(path) => ExtensionConfig::load(&path)?,
        None => ExtensionConfig::default(),
    };
    let project = ExtensionConfig::load(&cwd.join(".tokio-agent/extensions.toml"))?;
    let ids = installed_extension_ids(&user, &project)?;
    let store = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
    let records =
        tokio_agent_plugin::ExtensionLock::load(&store.root().join("installations.lock"))?;
    let official_identity = tokio_agent_plugin::builtin_official_root()?
        .map(|root| tokio_agent_plugin::root_fingerprint(&root.signed));
    let mut packages = Vec::new();
    for id in ids {
        let Some(root) = resolve_extension_root(cwd, &id, &user, &project)? else {
            continue;
        };
        let manifest = tokio_agent_plugin::validate_package(&root, &semver::Version::new(1, 0, 0))?;
        if manifest.runtime.is_none() {
            continue;
        }
        let (registry_identity, publisher, privileged_source) =
            if project.linked.contains_key(&id) || user.linked.contains_key(&id) {
                ("local".to_owned(), "local".to_owned(), true)
            } else {
                let record = records
                    .extensions
                    .iter()
                    .find(|entry| entry.id == id)
                    .with_context(|| format!("missing installation identity for `{id}`"))?;
                let (identity, privileged_source) = match &record.source {
                    LockedSource::Registry { root_identity, .. } => (
                        root_identity.clone(),
                        registry_is_official(root_identity, official_identity.as_deref()),
                    ),
                    LockedSource::Local { .. } => ("local".to_owned(), true),
                };
                (
                    identity,
                    record.publisher.clone().unwrap_or_default(),
                    privileged_source,
                )
            };
        if manifest.tool_gate.is_some()
            && (!manifest.id.starts_with("tokio.") || !privileged_source)
        {
            anyhow::bail!("extension `{id}` cannot claim privileged global tool-gating authority");
        }
        let grant = CapabilityGrant {
            registry_identity,
            extension_id: id.clone(),
            publisher,
            capabilities: manifest.capabilities.as_set(),
        };
        if !grant.capabilities.is_empty()
            && !user.grant_matches(&grant)
            && !project.grant_matches(&grant)
        {
            anyhow::bail!(
                "extension `{id}` capabilities have not been approved for this exact source and publisher"
            );
        }
        let settings = extension_settings
            .get(&id)
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or_else(|| serde_json::json!({}));
        let startup_settings = STARTUP_EXTENSION_SETTINGS
            .get()
            .and_then(|all| all.get(&id))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let script = manifest
            .runtime
            .as_ref()
            .map(|runtime| root.join(&runtime.javascript));
        let metadata = script.as_ref().map(std::fs::metadata).transpose()?;
        let modified = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        let fingerprint =
            modified ^ u128::from(metadata.as_ref().map_or(0, std::fs::Metadata::len));
        packages.push(crate::extension_runtime::ProgrammablePackage {
            root,
            manifest,
            settings,
            startup_settings,
            fingerprint,
        });
    }
    Ok(packages)
}

fn registry_is_official(root_identity: &str, official_identity: Option<&str>) -> bool {
    official_identity == Some(root_identity)
}

fn installed_extension_ids(
    user: &tokio_agent_plugin::ExtensionConfig,
    project: &tokio_agent_plugin::ExtensionConfig,
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let mut ids: std::collections::BTreeSet<String> = user
        .linked
        .keys()
        .chain(project.linked.keys())
        .cloned()
        .collect();
    let store = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
    ids.extend(store.list()?.into_iter().map(|package| package.manifest.id));
    Ok(ids)
}

fn resolve_extension_root(
    cwd: &Path,
    id: &str,
    user: &tokio_agent_plugin::ExtensionConfig,
    project: &tokio_agent_plugin::ExtensionConfig,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    if let Some(path) = project.linked.get(id).or_else(|| user.linked.get(id)) {
        return Ok(Some(path.clone()));
    }
    let store = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
    let mut packages: Vec<_> = store
        .list()?
        .into_iter()
        .filter(|package| package.manifest.id == id)
        .collect();
    let project_lock =
        tokio_agent_plugin::ExtensionLock::load(&cwd.join(".tokio-agent/extensions.lock"))?;
    if let Some(locked) = project_lock.extensions.iter().find(|entry| entry.id == id) {
        return Ok(packages
            .into_iter()
            .find(|package| package.manifest.version == locked.version)
            .map(|package| package.path));
    }
    packages.sort_by_key(|package| semver::Version::parse(&package.manifest.version).ok());
    Ok(packages.pop().map(|package| package.path))
}

fn load_extension_summaries(
    cwd: &Path,
) -> anyhow::Result<Vec<tokio_agent_extension_api::ExtensionSummary>> {
    use std::collections::BTreeMap;
    use tokio_agent_extension_api::{ExtensionId, ExtensionOrigin, ExtensionSummary};
    use tokio_agent_plugin::ExtensionConfig;

    let user_path = dirs::config_dir().map(|path| path.join("tokio-agent/extensions.toml"));
    let user = match user_path {
        Some(path) => ExtensionConfig::load(&path)?,
        None => ExtensionConfig::default(),
    };
    let project = ExtensionConfig::load(&cwd.join(".tokio-agent/extensions.toml"))?;
    let mut summaries = BTreeMap::new();
    let registry_installed_ids: std::collections::BTreeSet<_> =
        tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))
            .ok()
            .and_then(|store| store.list().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|package| package.manifest.id)
            .collect();
    let ids: std::collections::BTreeSet<_> = user
        .linked
        .keys()
        .chain(project.linked.keys())
        .cloned()
        .collect();
    for id in ids {
        let root = project.linked.get(&id).or_else(|| user.linked.get(&id));
        let Some(root) = root else { continue };
        let manifest = tokio_agent_plugin::validate_package(root, &semver::Version::new(1, 0, 0))?;
        summaries.insert(
            id.clone(),
            ExtensionSummary {
                id: ExtensionId::new(id.clone()),
                name: manifest.name,
                version: manifest.version,
                description: manifest.description,
                origin: ExtensionOrigin::Local {
                    path: root.to_string_lossy().into_owned(),
                },
                installed: true,
                local_override: registry_installed_ids.contains(&id),
                capabilities: manifest.capabilities.as_set().into_iter().collect(),
                commands: manifest
                    .commands
                    .into_iter()
                    .map(|command| format!("/{}", command.name))
                    .chain(
                        manifest
                            .skills
                            .into_iter()
                            .map(|skill| format!("/{}", skill.name)),
                    )
                    .collect(),
                tools: manifest.tools.into_iter().map(|tool| tool.name).collect(),
                status_segments: manifest
                    .status
                    .into_iter()
                    .map(|status| status.id)
                    .collect(),
            },
        );
    }
    if let Ok(store) = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))
    {
        let records =
            tokio_agent_plugin::ExtensionLock::load(&store.root().join("installations.lock"))
                .unwrap_or_default();
        let official_identity = tokio_agent_plugin::builtin_official_root()
            .ok()
            .flatten()
            .map(|root| tokio_agent_plugin::root_fingerprint(&root.signed));
        let trust = dirs::config_dir()
            .map(|path| path.join("tokio-agent/registry-trust.toml"))
            .and_then(|path| tokio_agent_plugin::RegistryTrustStore::load(&path).ok())
            .unwrap_or_default();
        for package in store.list().unwrap_or_default() {
            let manifest = package.manifest;
            let id = manifest.id.clone();
            let origin = records
                .extensions
                .iter()
                .find(|entry| entry.id == id && entry.version == manifest.version)
                .map(|entry| match &entry.source {
                    tokio_agent_plugin::LockedSource::Registry { root_identity, .. }
                        if official_identity.as_ref() == Some(root_identity) =>
                    {
                        ExtensionOrigin::OfficialRegistry {
                            registry: root_identity.clone(),
                        }
                    }
                    tokio_agent_plugin::LockedSource::Registry { root_identity, .. } => {
                        let operator = trust.registries.get(root_identity).map_or_else(
                            || "Unavailable registry".to_owned(),
                            |registry| registry.root.signed.operator.clone(),
                        );
                        ExtensionOrigin::ThirdPartyRegistry {
                            registry: root_identity.clone(),
                            operator,
                        }
                    }
                    tokio_agent_plugin::LockedSource::Local { path, .. } => {
                        ExtensionOrigin::Local {
                            path: path.to_string_lossy().into_owned(),
                        }
                    }
                })
                .unwrap_or_else(|| ExtensionOrigin::Local {
                    path: package.path.to_string_lossy().into_owned(),
                });
            summaries
                .entry(id.clone())
                .or_insert_with(|| ExtensionSummary {
                    id: ExtensionId::new(id.clone()),
                    name: manifest.name,
                    version: manifest.version,
                    description: manifest.description,
                    origin,
                    installed: true,
                    local_override: false,
                    capabilities: manifest.capabilities.as_set().into_iter().collect(),
                    commands: manifest
                        .commands
                        .into_iter()
                        .map(|command| format!("/{}", command.name))
                        .collect(),
                    tools: manifest.tools.into_iter().map(|tool| tool.name).collect(),
                    status_segments: manifest
                        .status
                        .into_iter()
                        .map(|status| status.id)
                        .collect(),
                });
        }
    }
    if let Ok(catalog) = crate::load_registry_catalog(false) {
        for result in catalog.search("") {
            let key = format!(
                "{}:{}@{}",
                result.registry_identity, result.package.id, result.package.version
            );
            let origin = match result.trust {
                tokio_agent_plugin::RegistryTrust::Official => ExtensionOrigin::OfficialRegistry {
                    registry: result.registry_identity,
                },
                tokio_agent_plugin::RegistryTrust::ThirdParty { operator, .. } => {
                    ExtensionOrigin::ThirdPartyRegistry {
                        registry: result.registry_identity,
                        operator,
                    }
                }
            };
            summaries.entry(key).or_insert_with(|| ExtensionSummary {
                id: ExtensionId::new(result.package.id),
                name: result.package.name,
                version: result.package.version,
                description: result.package.description,
                origin,
                installed: false,
                local_override: false,
                capabilities: result.package.capabilities.into_iter().collect(),
                commands: result
                    .package
                    .commands
                    .into_iter()
                    .chain(result.package.skills.into_iter())
                    .map(|name| format!("/{name}"))
                    .collect(),
                tools: result.package.tools,
                status_segments: Vec::new(),
            });
        }
    }
    Ok(summaries.into_values().collect())
}

fn route_command(
    router: &tokio_agent_plugin::CommandRouter,
    cwd: &Path,
    extension_runtime: &crate::extension_runtime::ExtensionRuntime,
    id: tokio_agent_extension_api::CommandId,
    arguments: String,
) -> Result<tokio_agent_core::agent::UiCommand, String> {
    use tokio_agent_core::agent::UiCommand;
    use tokio_agent_extension_api::SessionCommand;
    use tokio_agent_plugin::{BuiltInCommand, RoutedCommand};

    let routed = router
        .route(SessionCommand::InvokeCommand { id, arguments }, cwd)
        .map_err(|error| error.to_string())?;
    Ok(match routed {
        RoutedCommand::SubmitPrompt(text) | RoutedCommand::SubmitMessage(text) => {
            UiCommand::UserMessage(text)
        }
        RoutedCommand::BuiltIn(BuiltInCommand::Clear) => UiCommand::Clear,
        RoutedCommand::BuiltIn(BuiltInCommand::OpenModelPicker) => {
            return Err("/model requires the interactive frontend".to_owned());
        }
        RoutedCommand::BuiltIn(BuiltInCommand::OpenProviderPicker) => {
            return Err("/providers requires the interactive frontend".to_owned());
        }
        RoutedCommand::BuiltIn(BuiltInCommand::OpenExtensionManager) => {
            return Err("the extension manager is not available in headless mode".to_owned());
        }
        RoutedCommand::Extension { id, arguments } => {
            let prompt = extension_runtime
                .route(&id, arguments)
                .map_err(|error| error.to_string())?;
            extension_command_result(&id, prompt)
        }
        RoutedCommand::Interrupt
        | RoutedCommand::RespondToInteraction(_)
        | RoutedCommand::Shutdown => {
            return Err("invalid routed command".to_owned());
        }
    })
}

fn extension_command_result(
    id: &tokio_agent_extension_api::CommandId,
    prompt: Option<(String, bool)>,
) -> tokio_agent_core::agent::UiCommand {
    use tokio_agent_core::agent::UiCommand;

    match prompt {
        Some((prompt, true)) => {
            let source = tokio_agent_extension_api::ExtensionId::new(
                id.as_str()
                    .split_once(':')
                    .map_or(id.as_str(), |(owner, _)| owner),
            );
            UiCommand::AutomaticMessage {
                source,
                text: prompt,
            }
        }
        Some((prompt, false)) => UiCommand::UserMessage(prompt),
        None => UiCommand::CommandHandled(None),
    }
}

fn tools_for_provider(
    provider: ProviderKind,
    bash_yield_time_ms: u64,
    bash_timeout_ms: u64,
) -> Vec<Arc<dyn tokio_agent_core::Tool>> {
    let mut tools = tokio_agent_tools::builtins_with_bash_config(tokio_agent_tools::BashConfig {
        yield_time_ms: bash_yield_time_ms,
        timeout_ms: bash_timeout_ms,
    });
    if !matches!(provider, ProviderKind::OpenAi) {
        tools.push(Arc::new(tokio_agent_tools::WebSearch::new()));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programmable_command_without_a_prompt_is_still_handled() {
        let command = extension_command_result(
            &tokio_agent_extension_api::CommandId::new("tokio.loop:loop"),
            None,
        );

        assert!(matches!(
            command,
            tokio_agent_core::agent::UiCommand::CommandHandled(None)
        ));
    }

    #[test]
    fn search_tool_is_selected_automatically_by_provider() {
        let anthropic_names: Vec<_> = tools_for_provider(ProviderKind::Anthropic, 10_000, 600_000)
            .into_iter()
            .map(|tool| tool.schema().name)
            .collect();
        let openai_names: Vec<_> = tools_for_provider(ProviderKind::OpenAi, 10_000, 600_000)
            .into_iter()
            .map(|tool| tool.schema().name)
            .collect();

        assert!(anthropic_names.iter().any(|name| name == "websearch"));
        assert!(!openai_names.iter().any(|name| name == "websearch"));
    }

    #[test]
    fn privileged_registry_source_is_matched_by_signed_root_identity() {
        let identity = tokio_agent_plugin::builtin_official_root()
            .unwrap()
            .map(|root| tokio_agent_plugin::root_fingerprint(&root.signed))
            .expect("official registry root is embedded");

        assert!(registry_is_official(&identity, Some(&identity)));
        assert!(!registry_is_official("sha256:third-party", Some(&identity)));
        assert!(!registry_is_official(&identity, None));
    }
}
