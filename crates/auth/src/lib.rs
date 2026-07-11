mod oauth;
mod pkce;
mod server;
mod store;
mod token;

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

pub use oauth::{CLIENT_ID, ISSUER, ORIGINATOR, REDIRECT_PORT};
pub use token::{AuthFile, Tokens};

const REFRESH_LEEWAY_SECS: u64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("network error talking to the auth service: {0}")]
    Network(String),
    #[error("token endpoint error: {0}")]
    Token(String),
    #[error("callback error: {0}")]
    Callback(String),
    #[error("local login server error: {0}")]
    Server(String),
    #[error("failed to persist credentials: {0}")]
    Storage(String),
}

pub struct ChatGptAuth {
    client: reqwest::Client,
    tokens: Mutex<Tokens>,
    account_id: Option<String>,
}

impl ChatGptAuth {
    fn new(tokens: Tokens) -> Self {
        let account_id = tokens.resolved_account_id();
        Self {
            client: reqwest::Client::new(),
            tokens: Mutex::new(tokens),
            account_id,
        }
    }

    #[must_use]
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub async fn access_token(&self) -> Result<String, AuthError> {
        let mut guard = self.tokens.lock().await;
        if is_expiring(&guard.access_token) {
            let refreshed = oauth::refresh(&self.client, &guard.refresh_token).await?;
            guard.id_token = refreshed.id_token;
            guard.access_token = refreshed.access_token;
            guard.refresh_token = refreshed.refresh_token;
            if let Err(e) = persist(&guard) {
                tracing::warn!("could not persist refreshed credentials: {e}");
            }
        }
        Ok(guard.access_token.clone())
    }
}

#[must_use]
pub fn load() -> Option<ChatGptAuth> {
    let file = store::load()?;
    let tokens = file.tokens?;
    Some(ChatGptAuth::new(tokens))
}

#[must_use]
pub fn is_signed_in() -> bool {
    store::load().and_then(|f| f.tokens).is_some()
}

pub fn logout() -> Result<Option<std::path::PathBuf>, AuthError> {
    store::clear().map_err(|e| AuthError::Storage(e.to_string()))
}

pub struct LoginOutcome {
    pub email: Option<String>,
    pub account_id: Option<String>,
}

pub async fn login() -> Result<LoginOutcome, AuthError> {
    login_inner(true).await
}

pub async fn login_silent() -> Result<LoginOutcome, AuthError> {
    login_inner(false).await
}

async fn login_inner(announce: bool) -> Result<LoginOutcome, AuthError> {
    let client = reqwest::Client::new();
    let pkce = pkce::generate();
    let state = pkce::random_state();

    let url = oauth::authorize_url(&pkce.challenge, &state);
    if announce {
        println!("Opening your browser to sign in with ChatGPT.");
        println!("If it does not open, visit this URL:\n{url}\n");
    }
    open_browser(&url);

    let code = server::wait_for_code(&state).await?;
    let token_set = oauth::exchange_code(&client, &code, &pkce).await?;

    let account_id = token::account_id_from_id_token(&token_set.id_token);
    let email = jwt_email(&token_set.id_token);
    let tokens = Tokens {
        id_token: token_set.id_token,
        access_token: token_set.access_token,
        refresh_token: token_set.refresh_token,
        account_id: account_id.clone(),
    };
    persist(&tokens)?;

    Ok(LoginOutcome { email, account_id })
}

fn persist(tokens: &Tokens) -> Result<(), AuthError> {
    let file = AuthFile {
        openai_api_key: None,
        tokens: Some(tokens.clone()),
        last_refresh: None,
    };
    store::save(&file).map_err(|e| AuthError::Storage(e.to_string()))?;
    Ok(())
}

fn is_expiring(access_token: &str) -> bool {
    let Some(exp) = token::jwt_expires_at(access_token) else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now + REFRESH_LEEWAY_SECS >= exp
}

fn jwt_email(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        tracing::debug!("could not open browser automatically: {e}");
    }
}
