use serde::Deserialize;

use crate::AuthError;
use crate::pkce::Pkce;

pub const ISSUER: &str = "https://auth.openai.com";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ORIGINATOR: &str = "codex_cli_rs";
pub const REDIRECT_PORT: u16 = 1455;
const SCOPE: &str = "openid profile email offline_access";

pub fn redirect_uri() -> String {
    format!("http://localhost:{REDIRECT_PORT}/auth/callback")
}

pub fn authorize_url(challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", &redirect_uri()),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", ORIGINATOR),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{ISSUER}/oauth/authorize?{query}")
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct TokenSet {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    pkce: &Pkce,
) -> Result<TokenSet, AuthError> {
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(&redirect_uri()),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(&pkce.verifier),
    );
    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    parse_token_response(resp, None).await
}

pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<TokenSet, AuthError> {
    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    parse_token_response(resp, Some(refresh_token)).await
}

async fn parse_token_response(
    resp: reqwest::Response,
    fallback_refresh: Option<&str>,
) -> Result<TokenSet, AuthError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AuthError::Token(format!("HTTP {status}: {body}")));
    }
    let parsed: TokenResponse = resp
        .json()
        .await
        .map_err(|e| AuthError::Token(e.to_string()))?;
    Ok(TokenSet {
        id_token: parsed
            .id_token
            .ok_or_else(|| AuthError::Token("missing id_token".to_owned()))?,
        access_token: parsed
            .access_token
            .ok_or_else(|| AuthError::Token("missing access_token".to_owned()))?,
        refresh_token: parsed
            .refresh_token
            .or_else(|| fallback_refresh.map(str::to_owned))
            .ok_or_else(|| AuthError::Token("missing refresh_token".to_owned()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_pkce_and_codex_flow_params() {
        let url = authorize_url("CHALLENGE", "STATE");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=CHALLENGE"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }
}
