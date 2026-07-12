use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio_agent_config::{AuthKind, Config, PermissionMode, ProviderKind, ResolvedConfig};
use tokio_agent_core::agent::{Agent, ModelConfig};
use tokio_agent_core::permission::{Mode, PermissionEngine};
use tokio_agent_provider::{Anthropic, AnyProvider, DeepSeek, OpenAi};

const DEFAULT_SYSTEM_PROMPT: &str = include_str!("default_system_prompt.md");

pub fn build_session(cwd: &Path, yolo: bool) -> anyhow::Result<Agent<AnyProvider>> {
    let mut config = Config::load(cwd).context("loading config")?;
    apply_yolo_override(&mut config, yolo);
    let config = config.resolve().context("validating config")?;
    SessionBuilder::new(config, cwd).build()
}

fn apply_yolo_override(config: &mut Config, yolo: bool) {
    if yolo {
        config.permission_mode = "full-auto".to_owned();
    }
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
            permission_mode,
            system_prompt,
        } = self.config;
        let supports_reasoning_effort = matches!(
            provider_kind,
            ProviderKind::Anthropic | ProviderKind::OpenAi | ProviderKind::DeepSeek
        );
        let mode = match permission_mode {
            PermissionMode::Suggest => Mode::Suggest,
            PermissionMode::AutoEdit => Mode::AutoEdit,
            PermissionMode::FullAuto => Mode::FullAuto,
        };

        let tools = tools_for_provider(provider_kind);

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
            PermissionEngine::new(mode),
            ModelConfig {
                model,
                system,
                max_tokens,
                reasoning_effort,
            },
            self.cwd.to_path_buf(),
        );
        let extension_runtime = crate::extension_runtime::ExtensionRuntime::start(
            load_programmable_packages(self.cwd)?,
            agent.dynamic_tools(),
        )?;
        let route_runtime = extension_runtime.clone();
        let route_router = Arc::clone(&command_router);
        agent = agent
            .with_command_router(command_catalog.clone(), move |id, arguments| {
                let router = route_router
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                route_command(&router, &command_cwd, route_runtime.as_ref(), id, arguments)
            })
            .with_extension_catalog(extension_catalog.clone())
            .with_reasoning_effort_support(supports_reasoning_effort)
            .with_provider_name(provider_kind.as_str())
            .with_context_window(context_window_tokens);
        if let Some(runtime) = &extension_runtime {
            let hook = runtime.clone();
            agent = agent.with_session_hook(move |event| hook.event(event));
        }
        let watcher_router = Arc::clone(&command_router);
        let watcher_cwd = self.cwd.to_path_buf();
        let watcher_state = Arc::new(std::sync::Mutex::new((
            std::time::Instant::now(),
            command_catalog,
            std::time::Instant::now(),
            extension_catalog,
        )));
        agent = agent.with_session_poll(move || {
            let mut effects = extension_runtime
                .as_ref()
                .map_or_else(Vec::new, |runtime| runtime.poll());
            let mut state = watcher_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.0.elapsed() >= std::time::Duration::from_millis(500) {
                state.0 = std::time::Instant::now();
                if let Ok(router) = build_command_router(&watcher_cwd) {
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
    use std::collections::{BTreeMap, BTreeSet};
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
    let ids: BTreeSet<_> = user
        .linked
        .keys()
        .chain(project.linked.keys())
        .chain(user.extensions.keys())
        .chain(project.extensions.keys())
        .cloned()
        .collect();
    for id in ids {
        if !ExtensionConfig::resolve(&id, &user, &project).enabled {
            continue;
        }
        let root = resolve_extension_root(cwd, &id, &user, &project)?
            .with_context(|| format!("enabled extension `{id}` is not installed or linked"))?;
        let manifest = tokio_agent_plugin::validate_package(&root, &semver::Version::new(1, 0, 0))?;
        if manifest.id.starts_with("tokio.official.")
            && (project.linked.contains_key(&id) || user.linked.contains_key(&id))
        {
            anyhow::bail!(
                "the `tokio.official.*` namespace is reserved for the built-in official registry"
            );
        }
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
    let reserved = [
        "/clear",
        "/model",
        "/permissions",
        "/providers",
        "/extensions",
    ];
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
) -> anyhow::Result<Vec<crate::extension_runtime::ProgrammablePackage>> {
    use std::collections::BTreeSet;
    use tokio_agent_plugin::{CapabilityGrant, ExtensionConfig, LockedSource};
    let user_path = dirs::config_dir().map(|path| path.join("tokio-agent/extensions.toml"));
    let user = match user_path {
        Some(path) => ExtensionConfig::load(&path)?,
        None => ExtensionConfig::default(),
    };
    let project = ExtensionConfig::load(&cwd.join(".tokio-agent/extensions.toml"))?;
    let ids: BTreeSet<_> = user
        .extensions
        .keys()
        .chain(project.extensions.keys())
        .chain(user.linked.keys())
        .chain(project.linked.keys())
        .cloned()
        .collect();
    let store = tokio_agent_plugin::PackageStore::user_default(semver::Version::new(1, 0, 0))?;
    let records =
        tokio_agent_plugin::ExtensionLock::load(&store.root().join("installations.lock"))?;
    let mut packages = Vec::new();
    for id in ids {
        if !ExtensionConfig::resolve(&id, &user, &project).enabled {
            continue;
        }
        let Some(root) = resolve_extension_root(cwd, &id, &user, &project)? else {
            continue;
        };
        let manifest = tokio_agent_plugin::validate_package(&root, &semver::Version::new(1, 0, 0))?;
        if manifest.runtime.is_none() {
            continue;
        }
        let (registry_identity, publisher) =
            if project.linked.contains_key(&id) || user.linked.contains_key(&id) {
                ("local".to_owned(), "local".to_owned())
            } else {
                let record = records
                    .extensions
                    .iter()
                    .find(|entry| entry.id == id)
                    .with_context(|| format!("missing installation identity for `{id}`"))?;
                let identity = match &record.source {
                    LockedSource::Registry { root_identity, .. } => root_identity.clone(),
                    LockedSource::Local { .. } => "local".to_owned(),
                };
                (identity, record.publisher.clone().unwrap_or_default())
            };
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
        packages.push(crate::extension_runtime::ProgrammablePackage { root, manifest });
    }
    Ok(packages)
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
    use std::collections::{BTreeMap, BTreeSet};
    use tokio_agent_extension_api::{ExtensionId, ExtensionOrigin, ExtensionSummary};
    use tokio_agent_plugin::ExtensionConfig;

    let user_path = dirs::config_dir().map(|path| path.join("tokio-agent/extensions.toml"));
    let user = match user_path {
        Some(path) => ExtensionConfig::load(&path)?,
        None => ExtensionConfig::default(),
    };
    let project = ExtensionConfig::load(&cwd.join(".tokio-agent/extensions.toml"))?;
    let mut summaries = BTreeMap::new();
    let ids: BTreeSet<_> = user
        .linked
        .keys()
        .chain(project.linked.keys())
        .chain(user.extensions.keys())
        .chain(project.extensions.keys())
        .cloned()
        .collect();
    for id in ids {
        let root = project.linked.get(&id).or_else(|| user.linked.get(&id));
        let Some(root) = root else { continue };
        let manifest = tokio_agent_plugin::validate_package(root, &semver::Version::new(1, 0, 0))?;
        let enabled = ExtensionConfig::resolve(&id, &user, &project).enabled;
        summaries.insert(
            id.clone(),
            ExtensionSummary {
                id: ExtensionId::new(id),
                name: manifest.name,
                version: manifest.version,
                description: manifest.description,
                origin: ExtensionOrigin::Local {
                    path: root.to_string_lossy().into_owned(),
                },
                installed: true,
                enabled,
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
                    enabled: ExtensionConfig::resolve(&id, &user, &project).enabled,
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
                enabled: false,
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
    extension_runtime: Option<&crate::extension_runtime::ExtensionRuntime>,
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
        RoutedCommand::BuiltIn(BuiltInCommand::OpenPermissionsPicker) => {
            return Err("/permissions requires the interactive frontend".to_owned());
        }
        RoutedCommand::BuiltIn(BuiltInCommand::OpenProviderPicker) => {
            return Err("/providers requires the interactive frontend".to_owned());
        }
        RoutedCommand::BuiltIn(BuiltInCommand::OpenExtensionManager) => {
            return Err("the extension manager is not available in headless mode".to_owned());
        }
        RoutedCommand::Extension { id, arguments } => {
            let runtime = extension_runtime
                .ok_or_else(|| "programmable command runtime is unavailable".to_owned())?;
            let (prompt, automatic) = runtime
                .route(&id, arguments)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("unknown programmable command `{id}`"))?;
            if automatic {
                let source = tokio_agent_extension_api::ExtensionId::new(
                    id.as_str()
                        .split_once(':')
                        .map_or(id.as_str(), |(owner, _)| owner),
                );
                UiCommand::AutomaticMessage {
                    source,
                    text: prompt,
                }
            } else {
                UiCommand::UserMessage(prompt)
            }
        }
        RoutedCommand::Interrupt | RoutedCommand::Approve { .. } | RoutedCommand::Shutdown => {
            return Err("invalid routed command".to_owned());
        }
    })
}

fn tools_for_provider(provider: ProviderKind) -> Vec<Arc<dyn tokio_agent_core::Tool>> {
    let mut tools = tokio_agent_tools::builtins();
    if !matches!(provider, ProviderKind::OpenAi) {
        tools.push(Arc::new(tokio_agent_tools::WebSearch::new()));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(permission_mode: &str) -> Config {
        Config {
            provider: "anthropic".into(),
            model: "test".into(),
            api_base: None,
            auth: None,
            max_tokens: 1024,
            context_window_tokens: None,
            reasoning_effort: None,
            permission_mode: permission_mode.into(),
            system_prompt: None,
        }
    }

    #[test]
    fn yolo_overrides_the_configured_permission_mode() {
        let mut config = config("suggest");
        apply_yolo_override(&mut config, true);
        assert_eq!(
            config.resolve().unwrap().permission_mode,
            PermissionMode::FullAuto
        );
    }

    #[test]
    fn permission_mode_is_unchanged_without_yolo() {
        let mut config = config("auto-edit");
        apply_yolo_override(&mut config, false);
        assert_eq!(
            config.resolve().unwrap().permission_mode,
            PermissionMode::AutoEdit
        );
    }

    #[test]
    fn search_tool_is_selected_automatically_by_provider() {
        let anthropic_names: Vec<_> = tools_for_provider(ProviderKind::Anthropic)
            .into_iter()
            .map(|tool| tool.schema().name)
            .collect();
        let openai_names: Vec<_> = tools_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .map(|tool| tool.schema().name)
            .collect();

        assert!(anthropic_names.iter().any(|name| name == "websearch"));
        assert!(!openai_names.iter().any(|name| name == "websearch"));
    }
}
