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

        Ok(Agent::new(
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
        )
        .with_reasoning_effort_support(supports_reasoning_effort)
        .with_provider_name(provider_kind.as_str())
        .with_context_window(context_window_tokens))
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
