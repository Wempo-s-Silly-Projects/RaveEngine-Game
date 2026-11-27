use std::time::Duration;
use serde::Deserialize;

pub const API_BASE_URL: &str = "https://vertexia.wempos-forum.com";

#[derive(Deserialize)]
struct ClientAuthExchangeResponse {
    access_token: String,
}

pub fn exchange_client_code(api_origin: &str, code: &str) -> Result<String, String> {
    let origin = api_origin.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(origin)
        .map_err(|e| format!("invalid VERTEXIA API origin: {e}"))?;
    let local_http = parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if parsed.scheme() != "https" && !local_http {
        return Err("VERTEXIA API origin must use HTTPS outside localhost".to_string());
    }
    if code.trim().is_empty() {
        return Err("missing one-time client authorization code".to_string());
    }
    let client = blocking_client(Duration::from_secs(10))?;
    let response = client
        .post(format!("{origin}/api/v1/auth/exchange"))
        .json(&serde_json::json!({"code": code, "client": "client"}))
        .send()
        .map_err(|e| format!("client authorization exchange failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|e| format!("failed to read client authorization response: {e}"))?;
    if !status.is_success() {
        return Err(format!("client authorization exchange returned {status}"));
    }
    let result: ClientAuthExchangeResponse =
        serde_json::from_str(&body).map_err(|e| format!("invalid client authorization response: {e}"))?;
    if !result.access_token.starts_with("vxa_") {
        return Err("client authorization response did not contain a valid access token".to_string());
    }
    Ok(result.access_token)
}

pub fn decode_connect_token(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| format!("invalid connect token encoding: {e}"))?;
    if bytes.len() != 2048 {
        return Err(format!("connect token must be 2048 bytes, received {}", bytes.len()));
    }
    Ok(bytes)
}


pub fn api_base() -> String {
    for key in ["VERTEXIA_API_DOMAIN", "VERTIGO_API_DOMAIN"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim().trim_end_matches('/').to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    API_BASE_URL.to_string()
}

pub fn gameserver_api_key() -> Result<String, String> {
    let api_key = std::env::var("GAMESERVER_API_KEY")
        .map_err(|_| "GAMESERVER_API_KEY environment variable is not configured".to_string())?;
    Ok(api_key.trim().trim_matches('"').to_string())
}

pub fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

static SHARED_AUTH_CLIENT: std::sync::OnceLock<reqwest::blocking::Client> =
    std::sync::OnceLock::new();

pub fn shared_auth_client() -> Result<&'static reqwest::blocking::Client, String> {
    if let Some(client) = SHARED_AUTH_CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    Ok(SHARED_AUTH_CLIENT.get_or_init(|| client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_connect_token_requires_exactly_2048_bytes() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 2048]);
        assert_eq!(decode_connect_token(&encoded).unwrap().len(), 2048);
        assert!(decode_connect_token("not-base64").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 16]);
        assert!(decode_connect_token(&short).is_err());
    }

    #[test]
    fn api_base_prefers_vertexia_domain_and_keeps_legacy_alias() {
        let previous_vertexia = std::env::var("VERTEXIA_API_DOMAIN").ok();
        let previous_vertigo = std::env::var("VERTIGO_API_DOMAIN").ok();

        unsafe {
            std::env::set_var("VERTEXIA_API_DOMAIN", "https://api.example.test/");
            std::env::set_var("VERTIGO_API_DOMAIN", "http://legacy.example.test/");
        }
        assert_eq!(api_base(), "https://api.example.test");

        unsafe {
            std::env::remove_var("VERTEXIA_API_DOMAIN");
        }
        assert_eq!(api_base(), "http://legacy.example.test");

        unsafe {
            std::env::remove_var("VERTIGO_API_DOMAIN");
        }
        assert_eq!(api_base(), API_BASE_URL);

        unsafe {
            if let Some(value) = previous_vertexia {
                std::env::set_var("VERTEXIA_API_DOMAIN", value);
            }
            if let Some(value) = previous_vertigo {
                std::env::set_var("VERTIGO_API_DOMAIN", value);
            }
        }
    }
}
