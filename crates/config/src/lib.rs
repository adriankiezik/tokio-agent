use std::path::{Path, PathBuf};

use serde::Deserialize;

const KEYCHAIN_SERVICE: &str = "tokio-agent";
const DEFAULT_MODEL: &str = "claude-sonnet-5";
const DEFAULT_MAX_TOKENS: u32 = 8192;
const DEFAULT_REASONING_EFFORT: &str = "medium";
const DEFAULT_PERMISSION_MODE: &str = "suggest";
const MAX_RECENT_MESSAGES: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to parse state file {path}: {source}")]
    StateParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to write config file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("no API key for provider '{0}': set {1} or store one in the keychain")]
    MissingKey(String, String),
    #[error("keychain error: {0}")]
    Keychain(String),
    #[error("unknown provider '{0}' — supported providers: anthropic, openai")]
    UnknownProvider(String),
    #[error("unknown auth '{0}' — use \"chatgpt\" or \"api_key\"")]
    UnknownAuth(String),
    #[error("unknown permission_mode '{0}'")]
    UnknownPermissionMode(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
}

impl ProviderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    ChatGpt,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Suggest,
    AutoEdit,
    FullAuto,
}

impl PermissionMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::AutoEdit => "auto-edit",
            Self::FullAuto => "full-auto",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub api_base: Option<String>,
    pub auth: Option<AuthKind>,
    pub max_tokens: u32,
    pub context_window_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub permission_mode: PermissionMode,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    provider: Option<String>,
    model: Option<String>,
    api_base: Option<String>,
    auth: Option<String>,
    max_tokens: Option<u32>,
    context_window_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    permission_mode: Option<String>,
    system_prompt: Option<String>,
}

impl Layer {
    fn merge(&mut self, other: Layer) {
        if other.provider.is_some() {
            self.provider = other.provider;
        }
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.api_base.is_some() {
            self.api_base = other.api_base;
        }
        if other.auth.is_some() {
            self.auth = other.auth;
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
        if other.context_window_tokens.is_some() {
            self.context_window_tokens = other.context_window_tokens;
        }
        if other.reasoning_effort.is_some() {
            self.reasoning_effort = other.reasoning_effort;
        }
        if other.permission_mode.is_some() {
            self.permission_mode = other.permission_mode;
        }
        if other.system_prompt.is_some() {
            self.system_prompt = other.system_prompt;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub model: String,
    pub api_base: Option<String>,
    pub auth: Option<String>,
    pub max_tokens: u32,
    pub context_window_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub permission_mode: String,
    pub system_prompt: Option<String>,
}

impl Config {
    pub fn load(cwd: &Path) -> Result<Self, ConfigError> {
        let mut merged = Layer::default();

        if let Some(user) = user_config_path()
            && let Some(layer) = read_layer(&user)?
        {
            merged.merge(layer);
        }

        let project = cwd.join(".tokio-agent").join("config.toml");
        if let Some(layer) = read_layer(&project)? {
            merged.merge(layer);
        }

        if let Some(selection) = selection_config_path()
            && let Some(layer) = read_layer(&selection)?
        {
            merged.merge(layer);
        }

        if let Some(runtime) = runtime_config_path()
            && let Some(layer) = read_layer(&runtime)?
        {
            merged.merge(layer);
        }

        Ok(Self::from_layer(merged))
    }

    fn from_layer(layer: Layer) -> Self {
        Self {
            provider: layer.provider.unwrap_or_else(|| "anthropic".to_owned()),
            model: layer.model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            api_base: layer.api_base,
            auth: layer.auth,
            max_tokens: layer.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            context_window_tokens: layer.context_window_tokens,
            reasoning_effort: Some(
                layer
                    .reasoning_effort
                    .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_owned()),
            ),
            permission_mode: layer
                .permission_mode
                .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_owned()),
            system_prompt: layer.system_prompt,
        }
    }

    pub fn api_key(&self) -> Result<String, ConfigError> {
        api_key(&self.provider)
    }

    pub fn resolve(self) -> Result<ResolvedConfig, ConfigError> {
        let provider = match self.provider.as_str() {
            "anthropic" => ProviderKind::Anthropic,
            "openai" => ProviderKind::OpenAi,
            other => return Err(ConfigError::UnknownProvider(other.to_owned())),
        };
        let auth = match self.auth.as_deref() {
            Some("chatgpt") => Some(AuthKind::ChatGpt),
            Some("api_key") => Some(AuthKind::ApiKey),
            Some(other) => return Err(ConfigError::UnknownAuth(other.to_owned())),
            None => None,
        };
        let permission_mode = match self.permission_mode.as_str() {
            "suggest" => PermissionMode::Suggest,
            "auto-edit" | "auto_edit" => PermissionMode::AutoEdit,
            "full-auto" | "full_auto" => PermissionMode::FullAuto,
            other => return Err(ConfigError::UnknownPermissionMode(other.to_owned())),
        };
        let context_window_tokens = self
            .context_window_tokens
            .or_else(|| known_context_window(provider, &self.model));
        Ok(ResolvedConfig {
            provider,
            model: self.model,
            api_base: self.api_base,
            auth,
            max_tokens: self.max_tokens,
            context_window_tokens,
            reasoning_effort: Some(
                self.reasoning_effort
                    .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_owned()),
            ),
            permission_mode,
            system_prompt: self.system_prompt,
        })
    }
}

pub fn store_provider_selection(
    provider: ProviderKind,
    auth: AuthKind,
    model: &str,
) -> Result<(), ConfigError> {
    let path = selection_config_path().ok_or_else(|| ConfigError::Write {
        path: PathBuf::from("selection.toml"),
        source: std::io::Error::other("configuration directory is unavailable"),
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.clone(),
            source,
        })?;
    }
    let auth = match auth {
        AuthKind::ChatGpt => "chatgpt",
        AuthKind::ApiKey => "api_key",
    };
    let text = format!(
        "provider = {:?}\nauth = {:?}\nmodel = {:?}\n",
        provider.as_str(),
        auth,
        model
    );
    std::fs::write(&path, text).map_err(|source| ConfigError::Write { path, source })
}

pub fn store_permission_mode(mode: PermissionMode) -> Result<(), ConfigError> {
    let path = runtime_config_path().ok_or_else(|| ConfigError::Write {
        path: PathBuf::from("runtime.toml"),
        source: std::io::Error::other("configuration directory is unavailable"),
    })?;
    store_permission_mode_at(&path, mode)
}

pub fn recent_messages() -> Result<Vec<String>, ConfigError> {
    let Some(path) = history_path() else {
        return Ok(Vec::new());
    };
    recent_messages_at(&path)
}

pub fn store_recent_message(message: &str) -> Result<(), ConfigError> {
    let path = history_path().ok_or_else(|| ConfigError::Write {
        path: PathBuf::from("history.json"),
        source: std::io::Error::other("configuration directory is unavailable"),
    })?;
    store_recent_message_at(&path, message)
}

fn known_context_window(provider: ProviderKind, model: &str) -> Option<u64> {
    match provider {
        ProviderKind::Anthropic
            if model.starts_with("claude-sonnet-5")
                || model.starts_with("claude-sonnet-4-6")
                || model.starts_with("claude-opus-4-6")
                || model.starts_with("claude-opus-4-7")
                || model.starts_with("claude-opus-4-8") =>
        {
            Some(1_000_000)
        }
        ProviderKind::Anthropic if model.starts_with("claude-") => Some(200_000),
        ProviderKind::OpenAi if model.starts_with("gpt-5.4") || model.starts_with("gpt-5.6") => {
            Some(1_050_000)
        }
        _ => None,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_layer(Layer::default())
    }
}

fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokio-agent").join("config.toml"))
}

fn selection_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokio-agent").join("selection.toml"))
}

fn runtime_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokio-agent").join("runtime.toml"))
}

fn history_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokio-agent").join("history.json"))
}

fn ensure_parent(path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_owned(),
            source,
        })?;
    }
    Ok(())
}

fn store_permission_mode_at(path: &Path, mode: PermissionMode) -> Result<(), ConfigError> {
    ensure_parent(path)?;
    let text = format!("permission_mode = {:?}\n", mode.as_str());
    std::fs::write(path, text).map_err(|source| ConfigError::Write {
        path: path.to_owned(),
        source,
    })
}

fn recent_messages_at(path: &Path) -> Result<Vec<String>, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| ConfigError::StateParse {
            path: path.to_owned(),
            source,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn store_recent_message_at(path: &Path, message: &str) -> Result<(), ConfigError> {
    let mut messages = recent_messages_at(path)?;
    if messages.last().is_none_or(|last| last != message) {
        messages.push(message.to_owned());
    }
    if messages.len() > MAX_RECENT_MESSAGES {
        messages.drain(..messages.len() - MAX_RECENT_MESSAGES);
    }
    ensure_parent(path)?;
    let bytes = serde_json::to_vec(&messages).map_err(|source| ConfigError::StateParse {
        path: path.to_owned(),
        source,
    })?;
    std::fs::write(path, bytes).map_err(|source| ConfigError::Write {
        path: path.to_owned(),
        source,
    })
}

fn read_layer(path: &Path) -> Result<Option<Layer>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let layer = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })?;
            Ok(Some(layer))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn env_var_for(provider: &str) -> String {
    format!("{}_API_KEY", provider.to_uppercase())
}

pub fn api_key(provider: &str) -> Result<String, ConfigError> {
    let var = env_var_for(provider);
    if let Ok(key) = std::env::var(&var)
        && !key.is_empty()
    {
        return Ok(key);
    }

    match keyring::Entry::new(KEYCHAIN_SERVICE, provider) {
        Ok(entry) => match entry.get_password() {
            Ok(key) => Ok(key),
            Err(keyring::Error::NoEntry) => Err(ConfigError::MissingKey(provider.to_owned(), var)),
            Err(e) => Err(ConfigError::Keychain(e.to_string())),
        },
        Err(e) => Err(ConfigError::Keychain(e.to_string())),
    }
}

pub fn store_api_key(provider: &str, key: &str) -> Result<(), ConfigError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, provider)
        .map_err(|e| ConfigError::Keychain(e.to_string()))?;
    entry
        .set_password(key)
        .map_err(|e| ConfigError::Keychain(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tokio-agent-config-test-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn resolve_types_runtime_policy() {
        let config = Config {
            provider: "openai".into(),
            model: "model".into(),
            api_base: None,
            auth: Some("chatgpt".into()),
            max_tokens: 42,
            context_window_tokens: None,
            reasoning_effort: None,
            permission_mode: "auto-edit".into(),
            system_prompt: None,
        };
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.provider, ProviderKind::OpenAi);
        assert_eq!(resolved.auth, Some(AuthKind::ChatGpt));
        assert_eq!(resolved.permission_mode, PermissionMode::AutoEdit);
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn resolve_rejects_unknown_runtime_policy() {
        let config = Config {
            provider: "unknown".into(),
            ..Config::default()
        };
        assert!(matches!(
            config.resolve(),
            Err(ConfigError::UnknownProvider(name)) if name == "unknown"
        ));
    }

    #[test]
    fn resolves_known_context_windows_and_preserves_overrides() {
        let claude = Config::default().resolve().unwrap();
        assert_eq!(claude.context_window_tokens, Some(1_000_000));

        let custom = Config {
            context_window_tokens: Some(64_000),
            ..Config::default()
        }
        .resolve()
        .unwrap();
        assert_eq!(custom.context_window_tokens, Some(64_000));
    }

    #[test]
    fn selected_permission_mode_round_trips_as_a_config_layer() {
        let path = temp_path("runtime.toml");

        store_permission_mode_at(&path, PermissionMode::FullAuto).unwrap();
        let layer = read_layer(&path).unwrap().unwrap();

        assert_eq!(layer.permission_mode.as_deref(), Some("full-auto"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recent_messages_preserve_multiline_text_and_are_bounded() {
        let path = temp_path("history.json");
        store_recent_message_at(&path, "first\nmessage").unwrap();
        store_recent_message_at(&path, "first\nmessage").unwrap();
        for index in 0..MAX_RECENT_MESSAGES {
            store_recent_message_at(&path, &format!("message {index}")).unwrap();
        }

        let messages = recent_messages_at(&path).unwrap();

        assert_eq!(messages.len(), MAX_RECENT_MESSAGES);
        assert_eq!(messages.first().map(String::as_str), Some("message 0"));
        assert_eq!(messages.last().map(String::as_str), Some("message 99"));
        let _ = std::fs::remove_file(path);
    }
}
