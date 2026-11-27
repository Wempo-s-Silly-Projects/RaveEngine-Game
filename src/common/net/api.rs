//! VERTEXIA platform HTTP API: origin configuration, typed client and error mapping.
//!
//! Every call here is blocking (reqwest::blocking) and must never run on the Bevy main
//! thread. Use [`ApiTask::spawn`] from systems: it runs the request on a worker thread and
//! is polled once per frame.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default platform origin used by the client, Studio and dedicated servers.
pub const DEFAULT_PLATFORM_ORIGIN: &str = "https://vertexia.wempos-forum.com";
/// Kept for existing callers; same value as [`DEFAULT_PLATFORM_ORIGIN`].
pub const API_BASE_URL: &str = DEFAULT_PLATFORM_ORIGIN;
/// Environment variable overriding the platform origin.
pub const PLATFORM_ORIGIN_ENV: &str = "VERTEXIA_API_URL";
/// Deprecated override still exported by the platform's local gameserver launcher.
pub const LEGACY_PLATFORM_ORIGIN_ENV: &str = "VERTIGO_API_DOMAIN";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The platform may launch a gameserver before answering /join, so allow it more time.
const JOIN_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginError {
    Empty,
    Invalid(String),
    UnsupportedScheme(String),
    InsecureHttp(String),
    HasPath(String),
}

impl std::fmt::Display for OriginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OriginError::Empty => write!(f, "origin is empty"),
            OriginError::Invalid(e) => write!(f, "origin is not a valid URL: {e}"),
            OriginError::UnsupportedScheme(s) => write!(f, "origin scheme `{s}` is not http(s)"),
            OriginError::InsecureHttp(h) => {
                write!(f, "plain http is only allowed for loopback hosts, not `{h}`")
            }
            OriginError::HasPath(p) => {
                write!(f, "origin must not contain a path, query, fragment or credentials (`{p}`)")
            }
        }
    }
}

/// Validates and normalizes a platform origin: `scheme://host[:port]` without a trailing slash.
/// https is always accepted; http only for loopback hosts (localhost, 127.0.0.0/8, ::1).
pub fn parse_platform_origin(raw: &str) -> Result<String, OriginError> {
    let trimmed = raw.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return Err(OriginError::Empty);
    }
    let url = reqwest::Url::parse(trimmed).map_err(|e| OriginError::Invalid(e.to_string()))?;
    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err(OriginError::UnsupportedScheme(scheme));
    }
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| OriginError::Invalid("missing host".to_string()))?
        .to_string();
    if (url.path() != "/" && !url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(OriginError::HasPath(trimmed.to_string()));
    }
    if scheme == "http" {
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(OriginError::InsecureHttp(host));
        }
    }
    let mut origin = format!("{scheme}://{host}");
    if let Some(port) = url.port() {
        origin.push_str(&format!(":{port}"));
    }
    Ok(origin)
}

/// Resolves the origin from the raw override values (primary first). Invalid overrides are
/// reported and ignored so a typo can never silently send credentials to a plaintext host.
pub fn resolve_platform_origin(
    primary: Option<&str>,
    legacy: Option<&str>,
) -> (String, Option<String>) {
    for (name, value) in [(PLATFORM_ORIGIN_ENV, primary), (LEGACY_PLATFORM_ORIGIN_ENV, legacy)] {
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
            continue;
        };
        return match parse_platform_origin(value) {
            Ok(origin) => (origin, None),
            Err(e) => (
                DEFAULT_PLATFORM_ORIGIN.to_string(),
                Some(format!("ignoring {name}: {e}")),
            ),
        };
    }
    (DEFAULT_PLATFORM_ORIGIN.to_string(), None)
}

/// The platform origin, honouring `VERTEXIA_API_URL` (and the deprecated `VERTIGO_API_DOMAIN`).
pub fn api_base() -> String {
    let primary = std::env::var(PLATFORM_ORIGIN_ENV).ok();
    let legacy = std::env::var(LEGACY_PLATFORM_ORIGIN_ENV).ok();
    let (origin, warning) = resolve_platform_origin(primary.as_deref(), legacy.as_deref());
    if let Some(warning) = warning {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| bevy::log::error!("{warning}; using {origin}"));
    }
    origin
}

pub fn gameserver_api_key() -> Result<String, String> {
    let api_key = std::env::var("GAMESERVER_API_KEY")
        .map_err(|_| "GAMESERVER_API_KEY environment variable is not configured".to_string())?;
    Ok(api_key.trim().trim_matches('"').to_string())
}

pub fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// Credentials a player or creator presents to asset endpoints.
#[derive(Clone, PartialEq, Eq)]
pub enum UserAuth {
    /// Opaque access token from `/studio/exchange`, sent as `Authorization: Bearer`.
    Bearer(String),
    /// Long-lived key from platforms that predate access tokens, sent as `?ukey=`.
    LegacyUkey(String),
}

impl std::fmt::Debug for UserAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Self::LegacyUkey(_) => f.write_str("LegacyUkey(<redacted>)"),
        }
    }
}

/// Attaches the best available asset credentials and reports whether any were attached. A
/// configured `GAMESERVER_API_KEY` (dedicated servers) wins over user credentials and is only
/// ever sent as a header.
pub fn try_asset_auth(
    req: reqwest::blocking::RequestBuilder,
    user: Option<&UserAuth>,
) -> (reqwest::blocking::RequestBuilder, bool) {
    if let Ok(api_key) = gameserver_api_key() {
        if !api_key.is_empty() {
            return (req.header("X-Gameserver-Key", api_key), true);
        }
    }
    match user {
        Some(UserAuth::Bearer(token)) if !token.is_empty() => (req.bearer_auth(token), true),
        Some(UserAuth::LegacyUkey(ukey)) if !ukey.is_empty() => {
            (req.query(&[("ukey", ukey)]), true)
        }
        _ => (req, false),
    }
}

/// Like [`try_asset_auth`], but fails when no credentials are available.
pub fn with_asset_auth(
    req: reqwest::blocking::RequestBuilder,
    user: Option<&UserAuth>,
) -> Result<reqwest::blocking::RequestBuilder, String> {
    match try_asset_auth(req, user) {
        (req, true) => Ok(req),
        (_, false) => Err("not logged in and GAMESERVER_API_KEY is not configured".to_string()),
    }
}

static SHARED_AUTH_CLIENT: std::sync::OnceLock<reqwest::blocking::Client> =
    std::sync::OnceLock::new();

pub fn shared_auth_client() -> Result<&'static reqwest::blocking::Client, String> {
    if let Some(client) = SHARED_AUTH_CLIENT.get() {
        return Ok(client);
    }
    let client = blocking_client(Duration::from_secs(5))?;
    Ok(SHARED_AUTH_CLIENT.get_or_init(|| client))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 401: the access token is missing, expired or revoked. Clear it and log in again.
    Unauthorized,
    /// 403
    Forbidden(String),
    /// 404
    NotFound,
    /// 409 `{error:"full"}`: no capacity for this game right now.
    Full,
    /// 429
    RateLimited { retry_after: Option<Duration> },
    /// 503 `{error:"unavailable"}`: no server could be made ready in time.
    Unavailable { retry_after: Option<Duration> },
    /// Any other non-success status.
    Http { status: u16, message: String },
    /// DNS, TLS, connect or timeout failures.
    Network(String),
    /// The response body did not match the contract.
    Decode(String),
}

impl ApiError {
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::RateLimited { retry_after } | ApiError::Unavailable { retry_after } => {
                *retry_after
            }
            _ => None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ApiError::Full
                | ApiError::RateLimited { .. }
                | ApiError::Unavailable { .. }
                | ApiError::Network(_)
        ) || matches!(self, ApiError::Http { status, .. } if *status >= 500)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Your session has expired. Please log in again."),
            ApiError::Forbidden(m) if !m.is_empty() => write!(f, "Not allowed: {m}"),
            ApiError::Forbidden(_) => write!(f, "You are not allowed to do that."),
            ApiError::NotFound => write!(f, "That game does not exist or is not public."),
            ApiError::Full => write!(f, "The game is full right now."),
            ApiError::RateLimited { retry_after } => match retry_after {
                Some(d) => write!(f, "Too many requests. Try again in {}s.", d.as_secs().max(1)),
                None => write!(f, "Too many requests. Try again shortly."),
            },
            ApiError::Unavailable { retry_after } => match retry_after {
                Some(d) => write!(
                    f,
                    "No game server is available right now. Try again in {}s.",
                    d.as_secs().max(1)
                ),
                None => write!(f, "No game server is available right now. Try again shortly."),
            },
            ApiError::Http { status, message } if !message.is_empty() => {
                write!(f, "Platform error {status}: {message}")
            }
            ApiError::Http { status, .. } => write!(f, "Platform error {status}"),
            ApiError::Network(e) => write!(f, "Could not reach VERTEXIA: {e}"),
            ApiError::Decode(e) => write!(f, "Unexpected response from VERTEXIA: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Parses a `Retry-After` header value in delta-seconds form. HTTP dates are not used by the
/// platform and are ignored.
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let secs: u64 = value?.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(3600)))
}

/// Maps a non-success HTTP response to an [`ApiError`].
pub fn map_error_status(status: u16, retry_after: Option<&str>, body: &str) -> ApiError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_default();
    let retry_after = parse_retry_after(retry_after);
    match status {
        401 => ApiError::Unauthorized,
        403 => ApiError::Forbidden(message),
        404 => ApiError::NotFound,
        409 => ApiError::Full,
        429 => ApiError::RateLimited { retry_after },
        503 => ApiError::Unavailable { retry_after },
        _ => ApiError::Http {
            status,
            message: message.chars().take(200).collect(),
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Me {
    pub id: u64,
    pub username: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GameSummary {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub players_online: u32,
    #[serde(default)]
    pub max_players: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GameList {
    pub games: Vec<GameSummary>,
    #[serde(default)]
    pub next_page: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GameDetails {
    #[serde(flatten)]
    pub summary: GameSummary,
    #[serde(default)]
    pub creator: Option<serde_json::Value>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinResponse {
    pub server_id: String,
    pub host: String,
    pub port: u16,
    /// Standard base64 of the 2048-byte lightyear/netcode connect token.
    pub connect_token: String,
    #[serde(default)]
    pub expires_at: Option<serde_json::Value>,
}

/// Result of exchanging a browser-login code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangedLogin {
    pub uid: i32,
    pub username: String,
    pub access_token: String,
    pub expires_at: Option<i64>,
    /// Only set when talking to a platform that predates access tokens (Studio migration).
    pub legacy_ukey: String,
}

/// Which app a login is for; the platform picks the token lifetime from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAudience {
    Client,
    Studio,
}

impl LoginAudience {
    pub fn as_str(self) -> &'static str {
        match self {
            LoginAudience::Client => "client",
            LoginAudience::Studio => "studio",
        }
    }
}

pub fn parse_exchange_response(body: &str) -> Result<ExchangedLogin, ApiError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))?;
    let access_token = v
        .get("access_token")
        .or_else(|| v.get("token"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let legacy_ukey = v
        .get("ukey")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() && legacy_ukey.is_empty() {
        return Err(ApiError::Decode("missing access_token".to_string()));
    }
    let uid = v
        .get("uid")
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i32;
    let username = v
        .get("username")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_at = v.get("expires_at").and_then(|x| {
        x.as_i64().or_else(|| {
            x.as_str()
                .and_then(|s| s.parse::<i64>().ok().or_else(|| parse_rfc3339_unix(s)))
        })
    });
    Ok(ExchangedLogin {
        uid,
        username,
        access_token,
        expires_at,
        legacy_ukey,
    })
}

/// Minimal RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`) to unix seconds.
pub fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 20 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits = stripped.chars().take_while(|c| c.is_ascii_digit()).count();
        rest = &stripped[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
        _ => return None,
    };
    // days_from_civil (Howard Hinnant)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec - offset)
}

/// Blocking client for the public platform API. Never call it on the Bevy main thread.
#[derive(Clone)]
pub struct PlatformClient {
    origin: String,
    http: reqwest::blocking::Client,
    join_http: reqwest::blocking::Client,
}

impl PlatformClient {
    pub fn new() -> Result<Self, ApiError> {
        Self::with_origin(api_base())
    }

    pub fn with_origin(origin: impl Into<String>) -> Result<Self, ApiError> {
        let user_agent = concat!("RaveEngine/", env!("CARGO_PKG_VERSION"));
        let build = |timeout: Duration| {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .connect_timeout(CONNECT_TIMEOUT)
                .user_agent(user_agent)
                .build()
                .map_err(|e| ApiError::Network(e.to_string()))
        };
        Ok(Self {
            origin: origin.into(),
            http: build(REQUEST_TIMEOUT)?,
            join_http: build(JOIN_TIMEOUT)?,
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }

    fn send(req: reqwest::blocking::RequestBuilder) -> Result<String, ApiError> {
        let resp = req.send().map_err(|e| ApiError::Network(network_message(e)))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.text().map_err(|e| ApiError::Network(network_message(e)))?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(map_error_status(status.as_u16(), retry_after.as_deref(), &body))
        }
    }

    fn decode<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, ApiError> {
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
    }

    pub fn me(&self, token: &str) -> Result<Me, ApiError> {
        let body = Self::send(self.http.get(self.url("/api/v1/me")).bearer_auth(token))?;
        Self::decode(&body)
    }

    pub fn games(&self, page: u32, query: &str) -> Result<GameList, ApiError> {
        let page = page.to_string();
        let mut params = vec![("page", page.as_str())];
        let q = query.trim();
        if !q.is_empty() {
            params.push(("q", q));
        }
        let body = Self::send(self.http.get(self.url("/api/v1/games")).query(&params))?;
        Self::decode(&body)
    }

    pub fn game(&self, id: u64) -> Result<GameDetails, ApiError> {
        let body = Self::send(self.http.get(self.url(&format!("/api/v1/games/{id}"))))?;
        let value: serde_json::Value = Self::decode(&body)?;
        // Accept both a bare object and `{ "game": { ... } }`.
        let inner = value.get("game").cloned().unwrap_or(value);
        serde_json::from_value(inner).map_err(|e| ApiError::Decode(e.to_string()))
    }

    pub fn join(&self, id: u64, token: &str) -> Result<JoinResponse, ApiError> {
        let body = Self::send(
            self.join_http
                .post(self.url(&format!("/api/v1/games/{id}/join")))
                .bearer_auth(token)
                .json(&serde_json::json!({})),
        )?;
        Self::decode(&body)
    }

    /// Revokes the access token server-side. 401 means it is already invalid, which is fine.
    pub fn logout(&self, token: &str) -> Result<(), ApiError> {
        match Self::send(
            self.http
                .post(self.url("/api/v1/auth/logout"))
                .bearer_auth(token),
        ) {
            Ok(_) | Err(ApiError::Unauthorized) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn studio_poll(&self, state: &str) -> Result<Option<String>, ApiError> {
        let body = Self::send(
            self.http
                .get(self.url("/api/v1/studio/poll"))
                .query(&[("state", state)]),
        )?;
        let v: serde_json::Value = Self::decode(&body)?;
        if v.get("status").and_then(|x| x.as_str()) == Some("authorized") {
            Ok(v.get("code").and_then(|x| x.as_str()).map(str::to_string))
        } else {
            Ok(None)
        }
    }

    pub fn exchange_code(
        &self,
        code: &str,
        audience: LoginAudience,
    ) -> Result<ExchangedLogin, ApiError> {
        let body = Self::send(
            self.http
                .post(self.url("/api/v1/studio/exchange"))
                .json(&serde_json::json!({ "code": code, "audience": audience.as_str() })),
        )?;
        parse_exchange_response(&body)
    }
}

fn network_message(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        // reqwest errors include the URL; never echo query strings.
        e.without_url().to_string()
    }
}

/// A blocking API call running on its own thread, polled from a Bevy system.
pub struct ApiTask<T> {
    rx: crossbeam_channel::Receiver<T>,
}

impl<T: Send + 'static> ApiTask<T> {
    pub fn spawn(f: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("vertexia-api".to_string())
            .spawn(move || {
                let _ = tx.send(f());
            })
            .expect("failed to spawn API thread");
        Self { rx }
    }

    /// Returns the result once it is ready. A worker that panicked yields
    /// [`TaskPoll::Abandoned`].
    pub fn poll(&self) -> TaskPoll<T> {
        match self.rx.try_recv() {
            Ok(value) => TaskPoll::Ready(value),
            Err(crossbeam_channel::TryRecvError::Empty) => TaskPoll::Pending,
            Err(crossbeam_channel::TryRecvError::Disconnected) => TaskPoll::Abandoned,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TaskPoll<T> {
    Pending,
    Ready(T),
    Abandoned,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_defaults_to_vertexia() {
        assert_eq!(
            resolve_platform_origin(None, None),
            (DEFAULT_PLATFORM_ORIGIN.to_string(), None)
        );
        assert_eq!(
            resolve_platform_origin(Some("  "), None).0,
            DEFAULT_PLATFORM_ORIGIN
        );
    }

    #[test]
    fn origin_override_is_normalized() {
        assert_eq!(
            parse_platform_origin("https://staging.example.com/").unwrap(),
            "https://staging.example.com"
        );
        assert_eq!(
            parse_platform_origin(" HTTPS://Example.COM:8443 ").unwrap(),
            "https://example.com:8443"
        );
        assert_eq!(
            parse_platform_origin("http://127.0.0.1:3000/").unwrap(),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            parse_platform_origin("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert_eq!(
            parse_platform_origin("http://[::1]:3000").unwrap(),
            "http://[::1]:3000"
        );
    }

    #[test]
    fn origin_override_rejects_unsafe_values() {
        assert!(matches!(
            parse_platform_origin("http://vertexia.wempos-forum.com"),
            Err(OriginError::InsecureHttp(_))
        ));
        assert!(matches!(
            parse_platform_origin("http://10.0.0.5:3000"),
            Err(OriginError::InsecureHttp(_))
        ));
        assert!(matches!(
            parse_platform_origin("ftp://example.com"),
            Err(OriginError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            parse_platform_origin("https://example.com/api"),
            Err(OriginError::HasPath(_))
        ));
        assert!(matches!(
            parse_platform_origin("https://user:pw@example.com"),
            Err(OriginError::HasPath(_))
        ));
        assert!(matches!(
            parse_platform_origin("https://example.com?x=1"),
            Err(OriginError::HasPath(_))
        ));
        assert!(matches!(
            parse_platform_origin("vertexia.wempos-forum.com"),
            Err(OriginError::Invalid(_)) | Err(OriginError::UnsupportedScheme(_))
        ));
        assert_eq!(parse_platform_origin(""), Err(OriginError::Empty));
    }

    #[test]
    fn origin_override_precedence_and_fallback() {
        let (origin, warning) =
            resolve_platform_origin(Some("https://a.example"), Some("https://b.example"));
        assert_eq!(origin, "https://a.example");
        assert!(warning.is_none());

        let (origin, _) = resolve_platform_origin(None, Some("http://127.0.0.1:3000"));
        assert_eq!(origin, "http://127.0.0.1:3000");

        let (origin, warning) = resolve_platform_origin(Some("http://evil.example"), None);
        assert_eq!(origin, DEFAULT_PLATFORM_ORIGIN);
        assert!(warning.unwrap().contains(PLATFORM_ORIGIN_ENV));
    }

    #[test]
    fn api_errors_map_from_status() {
        assert_eq!(map_error_status(401, None, ""), ApiError::Unauthorized);
        assert_eq!(map_error_status(404, None, "{}"), ApiError::NotFound);
        assert_eq!(
            map_error_status(409, None, r#"{"error":"full"}"#),
            ApiError::Full
        );
        assert_eq!(
            map_error_status(503, Some("7"), r#"{"error":"unavailable"}"#),
            ApiError::Unavailable {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert_eq!(
            map_error_status(503, Some("Wed, 21 Oct 2015 07:28:00 GMT"), ""),
            ApiError::Unavailable { retry_after: None }
        );
        assert_eq!(
            map_error_status(429, Some("2"), ""),
            ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(2))
            }
        );
        assert_eq!(
            map_error_status(403, None, r#"{"error":"Game is not joinable"}"#),
            ApiError::Forbidden("Game is not joinable".to_string())
        );
        assert_eq!(
            map_error_status(500, None, "not json"),
            ApiError::Http {
                status: 500,
                message: String::new()
            }
        );
    }

    #[test]
    fn api_error_retry_semantics() {
        assert!(!ApiError::Unauthorized.is_retryable());
        assert!(!ApiError::NotFound.is_retryable());
        assert!(ApiError::Full.is_retryable());
        assert!(ApiError::Unavailable { retry_after: None }.is_retryable());
        assert!(ApiError::Network("x".into()).is_retryable());
        assert_eq!(
            ApiError::Unavailable {
                retry_after: Some(Duration::from_secs(3))
            }
            .retry_after(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(parse_retry_after(Some("999999")), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn decodes_contract_payloads() {
        let list: GameList = serde_json::from_str(
            r#"{"games":[{"id":7,"name":"Obby","description":"d","thumbnail_url":null,"players_online":3,"max_players":16}],"next_page":2}"#,
        )
        .unwrap();
        assert_eq!(list.games[0].id, 7);
        assert_eq!(list.next_page, Some(2));
        let last: GameList = serde_json::from_str(r#"{"games":[],"next_page":null}"#).unwrap();
        assert_eq!(last.next_page, None);

        let join: JoinResponse = serde_json::from_str(
            r#"{"server_id":"aB3dE5gH","host":"games.vertexia.wempos-forum.com","port":27015,"connect_token":"AAAA","expires_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(join.port, 27015);
        assert_eq!(join.host, "games.vertexia.wempos-forum.com");

        let me: Me =
            serde_json::from_str(r#"{"id":5,"username":"wempo","display_name":"Wempo"}"#).unwrap();
        assert_eq!(me.id, 5);
    }

    #[test]
    fn exchange_prefers_access_token() {
        let login = parse_exchange_response(
            r#"{"uid":5,"username":"w","access_token":"tok","expires_at":"2026-01-02T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(login.access_token, "tok");
        assert_eq!(login.expires_at, Some(1_767_312_000));
        assert!(login.legacy_ukey.is_empty());

        let legacy = parse_exchange_response(r#"{"uid":5,"username":"w","ukey":"k"}"#).unwrap();
        assert!(legacy.access_token.is_empty());
        assert_eq!(legacy.legacy_ukey, "k");

        assert!(parse_exchange_response(r#"{"uid":5}"#).is_err());
    }

    #[test]
    fn rfc3339_parsing() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_unix("2026-01-01T00:00:00Z"), Some(1_767_225_600));
        assert_eq!(
            parse_rfc3339_unix("2026-01-01T01:00:00.123+01:00"),
            Some(1_767_225_600)
        );
        assert_eq!(parse_rfc3339_unix("garbage"), None);
    }
}
