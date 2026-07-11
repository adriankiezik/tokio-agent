use base64::Engine;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(
        rename = "OPENAI_API_KEY",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl Tokens {
    #[must_use]
    pub fn resolved_account_id(&self) -> Option<String> {
        self.account_id
            .clone()
            .or_else(|| account_id_from_id_token(&self.id_token))
    }
}

fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    jwt_claims(id_token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

pub fn jwt_expires_at(jwt: &str) -> Option<u64> {
    jwt_claims(jwt)?.get("exp")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn jwt(payload: &serde_json::Value) -> String {
        let encode = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            encode(b"{\"alg\":\"none\"}"),
            encode(payload.to_string().as_bytes()),
            encode(b"sig"),
        )
    }

    #[test]
    fn account_id_is_read_from_nested_auth_claim() {
        let token = jwt(&serde_json::json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_123" }
        }));
        assert_eq!(
            account_id_from_id_token(&token).as_deref(),
            Some("acct_123")
        );
    }

    #[test]
    fn resolved_account_id_falls_back_to_id_token() {
        let tokens = Tokens {
            id_token: jwt(&serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "acct_456" }
            })),
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            account_id: None,
        };
        assert_eq!(tokens.resolved_account_id().as_deref(), Some("acct_456"));
    }

    #[test]
    fn jwt_expiry_is_parsed() {
        let token = jwt(&serde_json::json!({ "exp": 1_900_000_000u64 }));
        assert_eq!(jwt_expires_at(&token), Some(1_900_000_000));
    }
}
